//! `choo push` — push every branch in a train.
//!
//! The whole train goes out in **one** `git push`: one connection and one
//! ref advertisement instead of one per branch, which is what dominates
//! the wall-clock cost on a large repository. The push is `--atomic`, so a
//! stack never lands half-updated on the remote; if the server doesn't
//! implement that capability, choochoo falls back to pushing one branch at
//! a time.
//!
//! When the train has an aggregate branch it is re-synced to the train tip
//! *before* the push and included in it, so the combined PR always shows
//! the same commits the per-branch PRs were just pushed with. Syncing
//! first also means a train whose combined branch can't move (it's the one
//! checked out) fails before anything has reached the remote.

use crate::error::Result;
use crate::git::{self, GitRunner, PushMode};
use crate::report::Reporter;
use crate::state::Store;
use crate::train::aggregate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSummary {
    pub train: String,
    pub pushed: Vec<String>,
    pub mode: PushMode,
    /// The aggregate branch, when one was synced and pushed by this run.
    pub aggregate_pushed: Option<String>,
    /// True when the train went out in one atomic push. False when the
    /// remote didn't support `--atomic` and choochoo fell back to pushing
    /// one branch at a time — worth surfacing, because that path can leave
    /// a train partially pushed if a later branch is rejected.
    pub atomic: bool,
}

pub fn run(
    store: &Store,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    train_name: Option<&str>,
    mode: PushMode,
    remote: &str,
) -> Result<PushSummary> {
    let mut state = store.load()?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    let train = state.train(&train_name)?.clone();
    let mode_label = match mode {
        PushMode::ForceWithLease => "force-with-lease",
        PushMode::Force => "force (no lease)",
        PushMode::Plain => "plain",
    };

    // The aggregate branch is derived state: re-point it at the tip, then
    // let it ride along in the same push as the branches it mirrors.
    let synced = aggregate::sync_train(git, reporter, &train)?;
    let mut targets = train.branches.clone();
    if let Some(outcome) = &synced {
        targets.push(outcome.branch.clone());
    }

    if targets.is_empty() {
        reporter.info(&format!("train `{train_name}` has no branches to push"));
        return Ok(PushSummary {
            train: train_name,
            pushed: Vec::new(),
            mode,
            aggregate_pushed: None,
            atomic: true,
        });
    }

    // `pushed` accumulates what actually reached the remote. It matters on
    // the fallback path, where an early branch can succeed and a later one
    // fail: those SHAs really are on the remote now.
    let mut pushed: Vec<String> = Vec::new();
    let mut atomic = true;
    let refs: Vec<&str> = targets.iter().map(String::as_str).collect();

    reporter.start(&format!(
        "pushing {n} branch{es} to `{remote}` [{mode_label}, atomic]",
        n = targets.len(),
        es = if targets.len() == 1 { "" } else { "es" },
    ));
    let outcome = match git.push_many(&refs, mode, remote, true) {
        Ok(()) => {
            reporter.ok("");
            pushed = targets.clone();
            Ok(())
        }
        Err(e) if git::is_atomic_unsupported(&e) => {
            reporter.fail("remote does not support atomic push");
            reporter.info("falling back to one push per branch");
            atomic = false;
            push_one_at_a_time(git, reporter, &targets, mode, remote, mode_label, &mut pushed)
        }
        Err(e) => {
            reporter.fail(&e.to_string());
            return Err(e);
        }
    };

    // Record the pushed SHAs even when the fallback failed part-way: the
    // next run must not believe branches are unpushed when they aren't.
    let aggregate_branch = synced.as_ref().map(|o| o.branch.clone());
    for branch in &pushed {
        let Ok(sha) = git.rev_parse(branch) else {
            continue;
        };
        let train = state.train_mut(&train_name)?;
        if aggregate_branch.as_deref() == Some(branch.as_str()) {
            if let Some(pr) = train.aggregate.as_mut().and_then(|a| a.pr.as_mut()) {
                pr.last_pushed_sha = Some(sha);
            }
        } else if let Some(pr) = train.prs.get_mut(branch) {
            pr.last_pushed_sha = Some(sha);
        }
    }
    // The push error is the one worth reporting, so it takes precedence
    // over a failure to write the state we just derived from it.
    let saved = store.save(&state);
    outcome?;
    saved?;

    let aggregate_pushed = aggregate_branch.filter(|b| pushed.contains(b));
    pushed.retain(|b| Some(b.as_str()) != aggregate_pushed.as_deref());
    Ok(PushSummary {
        train: train_name,
        pushed,
        mode,
        aggregate_pushed,
        atomic,
    })
}

/// Fallback for remotes without atomic push: one `git push` per branch, in
/// train order, stopping at the first failure. Every branch pushed before
/// that point is appended to `pushed` so the caller can still record it.
fn push_one_at_a_time(
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    targets: &[String],
    mode: PushMode,
    remote: &str,
    mode_label: &str,
    pushed: &mut Vec<String>,
) -> Result<()> {
    let total = targets.len();
    for (i, branch) in targets.iter().enumerate() {
        reporter.start(&format!(
            "pushing `{branch}` to `{remote}` [{mode_label}] ({n}/{total})",
            n = i + 1,
        ));
        match git.push(branch, mode, remote) {
            Ok(()) => reporter.ok(""),
            Err(e) => {
                reporter.fail(&e.to_string());
                return Err(e);
            }
        }
        pushed.push(branch.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::git::RebaseOutcome;
    use crate::report::{NullReporter, RecordingReporter};
    use crate::state::{PrInfo, StateFile, Train};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    /// One recorded `git push`: the refs it carried, the mode, the remote,
    /// and whether it asked for `--atomic`.
    type Push = (Vec<String>, PushMode, String, bool);

    struct FakeGit {
        tips: RefCell<BTreeMap<String, String>>,
        pushes: RefCell<Vec<Push>>,
        /// Models a server without the atomic-push capability: any push
        /// asking for `--atomic` is rejected the way git words it.
        no_atomic: bool,
        /// Branches this remote refuses (non-fast-forward, stale lease,
        /// hook rejection — the fake doesn't care which).
        rejects: Vec<String>,
    }

    impl FakeGit {
        /// Branch names in push order, flattened across invocations.
        fn pushed_branches(&self) -> Vec<String> {
            self.pushes
                .borrow()
                .iter()
                .flat_map(|(b, _, _, _)| b.clone())
                .collect()
        }
    }

    impl GitRunner for FakeGit {
        fn current_branch(&self) -> Result<String> {
            Ok("a".into())
        }
        fn branch_exists(&self, name: &str) -> Result<bool> {
            Ok(self.tips.borrow().contains_key(name))
        }
        fn checkout(&self, _b: &str) -> Result<()> {
            Ok(())
        }
        fn rev_parse(&self, rev: &str) -> Result<String> {
            self.tips
                .borrow()
                .get(rev)
                .cloned()
                .ok_or_else(|| Error::UnknownBranch(rev.to_string()))
        }
        fn is_ancestor(&self, _a: &str, _d: &str) -> Result<bool> {
            // Pushing never moves commits, so it never picks a boundary.
            unreachable!()
        }
        fn set_branch(&self, branch: &str, to_rev: &str) -> Result<()> {
            // A rev that isn't a branch is a raw SHA, as with real git.
            let target = self
                .rev_parse(to_rev)
                .unwrap_or_else(|_| to_rev.to_string());
            self.tips.borrow_mut().insert(branch.to_string(), target);
            Ok(())
        }
        fn rebase_onto(
            &self,
            _b: &str,
            _o: &str,
            _u: &str,
        ) -> Result<RebaseOutcome> {
            unreachable!()
        }
        fn rebase_abort(&self) -> Result<()> {
            Ok(())
        }
        fn push(&self, branch: &str, mode: PushMode, remote: &str) -> Result<()> {
            self.push_many(&[branch], mode, remote, false)
        }
        fn push_many(
            &self,
            branches: &[&str],
            mode: PushMode,
            remote: &str,
            atomic: bool,
        ) -> Result<()> {
            if atomic && self.no_atomic {
                return Err(Error::Git {
                    code: 128,
                    stderr: "fatal: the receiving end does not support --atomic push".into(),
                });
            }
            if let Some(bad) = branches.iter().find(|b| self.rejects.contains(&b.to_string())) {
                return Err(Error::Git {
                    code: 1,
                    stderr: format!("! [rejected] {bad} (non-fast-forward)"),
                });
            }
            self.pushes.borrow_mut().push((
                branches.iter().map(|b| (*b).to_string()).collect(),
                mode,
                remote.into(),
                atomic,
            ));
            Ok(())
        }
        fn fetch(&self, _r: &str) -> Result<()> {
            Ok(())
        }
        fn ahead_behind(&self, _b: &str, _u: &str) -> Result<Option<(u32, u32)>> {
            Ok(None)
        }
        fn remote_url(&self, _r: &str) -> Result<Option<String>> {
            Ok(None)
        }
        /// These fixtures model repos where every branch is already
        /// local, so the remote-branch paths are never taken. Stubbed
        /// explicitly rather than defaulted: a default `Ok(false)` would
        /// quietly assert something untrue about the fixture.
        fn remote_branch_exists(&self, _r: &str, _b: &str) -> Result<bool> {
            Ok(false)
        }
        fn create_tracking_branch(&self, _b: &str, _r: &str) -> Result<()> {
            unreachable!("fixture branches are always local")
        }
    }

    fn setup() -> (TempDir, Store, FakeGit) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/choochoo")).unwrap();
        let store = Store::local(tmp.path());

        let mut state = StateFile::default();
        let mut t = Train::new("t", "main");
        t.branches = vec!["a".into(), "b".into()];
        t.prs.insert(
            "a".into(),
            PrInfo {
                number: 1,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        state.trains.insert("t".into(), t);
        state.active = Some("t".into());
        store.save(&state).unwrap();

        let git = FakeGit {
            tips: RefCell::new(
                [("main", "M"), ("a", "A1"), ("b", "B1")]
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect(),
            ),
            pushes: RefCell::new(Vec::new()),
            no_atomic: false,
            rejects: Vec::new(),
        };
        (tmp, store, git)
    }

    /// Turn on the aggregate branch for the fixture train.
    fn enable_aggregate(store: &Store, branch: &str) {
        let mut state = store.load().unwrap();
        state.train_mut("t").unwrap().aggregate =
            Some(crate::state::Aggregate::new(branch));
        store.save(&state).unwrap();
    }

    /// Run against a fixture, returning the summary.
    fn push_ok(store: &Store, git: &FakeGit, mode: PushMode) -> PushSummary {
        run(store, git, &mut NullReporter, None, mode, "origin").unwrap()
    }

    #[test]
    fn whole_train_goes_out_in_one_atomic_push() {
        let (_tmp, store, git) = setup();
        let summary = push_ok(&store, &git, PushMode::ForceWithLease);

        assert_eq!(summary.pushed, vec!["a", "b"]);
        assert!(summary.atomic);
        assert_eq!(
            git.pushes.borrow().clone(),
            vec![(
                vec!["a".to_string(), "b".to_string()],
                PushMode::ForceWithLease,
                "origin".to_string(),
                true,
            )],
            "expected a single atomic push carrying both branches"
        );
    }

    #[test]
    fn force_mode_passes_unconditional_force_to_git() {
        let (_tmp, store, git) = setup();
        push_ok(&store, &git, PushMode::Force);
        assert!(git.pushes.borrow().iter().all(|(_, m, _, _)| *m == PushMode::Force));
    }

    #[test]
    fn plain_mode_passes_no_force_flag_to_git() {
        let (_tmp, store, git) = setup();
        push_ok(&store, &git, PushMode::Plain);
        assert!(git.pushes.borrow().iter().all(|(_, m, _, _)| *m == PushMode::Plain));
    }

    #[test]
    fn updates_last_pushed_sha_for_branches_with_prs() {
        let (_tmp, store, git) = setup();
        push_ok(&store, &git, PushMode::ForceWithLease);
        let state = store.load().unwrap();
        let train = state.train("t").unwrap();
        assert_eq!(
            train.prs.get("a").unwrap().last_pushed_sha.as_deref(),
            Some("A1")
        );
    }

    #[test]
    fn emits_a_single_progress_step_for_the_batch() {
        let (_tmp, store, git) = setup();
        let mut rep = RecordingReporter::new();
        run(
            &store,
            &git,
            &mut rep,
            None,
            PushMode::ForceWithLease,
            "origin",
        )
        .unwrap();
        assert_eq!(rep.events.len(), 1, "events: {}", rep.joined());
        assert!(rep.events[0].contains("pushing 2 branches"));
        assert!(rep.events[0].contains("force-with-lease"));
        assert!(rep.events[0].ends_with("ok"));
    }

    #[test]
    fn force_mode_status_label_in_progress() {
        let (_tmp, store, git) = setup();
        let mut rep = RecordingReporter::new();
        run(&store, &git, &mut rep, None, PushMode::Force, "origin").unwrap();
        assert!(
            rep.events[0].contains("force (no lease)"),
            "expected force label, got: {}",
            rep.events[0]
        );
    }

    #[test]
    fn aggregate_branch_is_synced_then_included_in_the_batch() {
        let (_tmp, store, git) = setup();
        enable_aggregate(&store, "choo/t/combined");
        let summary = push_ok(&store, &git, PushMode::ForceWithLease);

        assert_eq!(summary.pushed, vec!["a", "b"]);
        assert_eq!(summary.aggregate_pushed.as_deref(), Some("choo/t/combined"));
        assert_eq!(git.pushes.borrow().len(), 1, "aggregate needs no second push");
        assert_eq!(git.pushed_branches(), vec!["a", "b", "choo/t/combined"]);
        // Synced to the tip (`b`) before being pushed.
        assert_eq!(git.rev_parse("choo/t/combined").unwrap(), "B1");
    }

    #[test]
    fn aggregate_pr_records_the_pushed_sha() {
        let (_tmp, store, git) = setup();
        enable_aggregate(&store, "choo/t/combined");
        let mut state = store.load().unwrap();
        state.train_mut("t").unwrap().aggregate.as_mut().unwrap().pr = Some(PrInfo {
            number: 9,
            url: "u".into(),
            title: None,
            last_pushed_sha: None,
        });
        store.save(&state).unwrap();

        push_ok(&store, &git, PushMode::ForceWithLease);

        let state = store.load().unwrap();
        let agg = state.train("t").unwrap().aggregate.clone().unwrap();
        assert_eq!(agg.pr.unwrap().last_pushed_sha.as_deref(), Some("B1"));
    }

    #[test]
    fn no_aggregate_means_nothing_extra_is_pushed() {
        let (_tmp, store, git) = setup();
        let summary = push_ok(&store, &git, PushMode::ForceWithLease);
        assert!(summary.aggregate_pushed.is_none());
        assert_eq!(git.pushed_branches(), vec!["a", "b"]);
    }

    /// A `git push` with no refspecs would consult `push.default` and send
    /// whatever that names, so an empty train must not push at all.
    #[test]
    fn empty_train_pushes_nothing() {
        let (_tmp, store, git) = setup();
        let mut state = store.load().unwrap();
        let train = state.train_mut("t").unwrap();
        train.branches.clear();
        train.prs.clear(); // PR metadata for a branch not in the train is corrupt state
        store.save(&state).unwrap();

        let summary = push_ok(&store, &git, PushMode::ForceWithLease);
        assert!(summary.pushed.is_empty());
        assert!(git.pushes.borrow().is_empty());
    }

    // -----------------------------------------------------------------
    // Fallback: remotes without the atomic-push capability
    // -----------------------------------------------------------------

    #[test]
    fn falls_back_to_one_push_per_branch_when_remote_lacks_atomic() {
        let (_tmp, store, mut git) = setup();
        git.no_atomic = true;
        let summary = push_ok(&store, &git, PushMode::ForceWithLease);

        assert!(!summary.atomic);
        assert_eq!(summary.pushed, vec!["a", "b"]);
        let pushes = git.pushes.borrow().clone();
        assert_eq!(pushes.len(), 2, "expected one push per branch");
        assert!(
            pushes.iter().all(|(refs, _, _, atomic)| refs.len() == 1 && !atomic),
            "fallback pushes must be single-ref and non-atomic: {pushes:?}"
        );
        assert_eq!(git.pushed_branches(), vec!["a", "b"]);
    }

    #[test]
    fn fallback_still_pushes_and_records_the_aggregate() {
        let (_tmp, store, mut git) = setup();
        git.no_atomic = true;
        enable_aggregate(&store, "choo/t/combined");
        let summary = push_ok(&store, &git, PushMode::ForceWithLease);

        assert_eq!(summary.aggregate_pushed.as_deref(), Some("choo/t/combined"));
        assert_eq!(git.pushed_branches(), vec!["a", "b", "choo/t/combined"]);
    }

    #[test]
    fn fallback_reports_per_branch_progress_after_the_atomic_attempt() {
        let (_tmp, store, mut git) = setup();
        git.no_atomic = true;
        let mut rep = RecordingReporter::new();
        run(
            &store,
            &git,
            &mut rep,
            None,
            PushMode::ForceWithLease,
            "origin",
        )
        .unwrap();

        let log = rep.joined();
        assert!(
            rep.events[0].contains("FAILED: remote does not support atomic push"),
            "log: {log}"
        );
        assert!(log.contains("falling back to one push per branch"), "log: {log}");
        assert!(log.contains("pushing `a`") && log.contains("(1/2)"), "log: {log}");
        assert!(log.contains("pushing `b`") && log.contains("(2/2)"), "log: {log}");
    }

    /// Partial pushes are real: the branches that landed must be recorded
    /// even though the command as a whole fails, or the next run will
    /// believe the remote is further behind than it is.
    #[test]
    fn fallback_records_branches_pushed_before_a_failure() {
        let (_tmp, store, mut git) = setup();
        git.no_atomic = true;
        git.rejects = vec!["b".into()];

        let err = run(
            &store,
            &git,
            &mut NullReporter,
            None,
            PushMode::ForceWithLease,
            "origin",
        )
        .unwrap_err();
        assert!(matches!(err, Error::Git { .. }), "got {err:?}");

        assert_eq!(git.pushed_branches(), vec!["a"]);
        let state = store.load().unwrap();
        assert_eq!(
            state
                .train("t")
                .unwrap()
                .prs
                .get("a")
                .unwrap()
                .last_pushed_sha
                .as_deref(),
            Some("A1"),
            "`a` did reach the remote; its SHA must be persisted"
        );
    }

    /// A push refused on its merits (stale lease, non-fast-forward) must
    /// not be retried in a mode that could let half the train through.
    #[test]
    fn rejected_push_fails_without_falling_back() {
        let (_tmp, store, mut git) = setup();
        git.rejects = vec!["b".into()];

        let err = run(
            &store,
            &git,
            &mut NullReporter,
            None,
            PushMode::ForceWithLease,
            "origin",
        )
        .unwrap_err();
        assert!(matches!(err, Error::Git { .. }), "got {err:?}");
        assert!(
            git.pushes.borrow().is_empty(),
            "nothing should have been pushed, and no retry attempted"
        );
    }
}
