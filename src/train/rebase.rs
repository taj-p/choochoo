//! `choo rebase` — restack every branch in a train onto its parent.
//!
//! ## Algorithm
//!
//! For a train `base, b1, b2, ..., bn`:
//!
//! 1. **Snapshot** the tip SHA of every branch (the base + every train
//!    branch) into a [`RebaseProgress`] file at
//!    `.git/choochoo/rebase-progress.json`. The snapshot is the source of
//!    truth for "what the parent tip was *before* its own rebase".
//! 2. For each pair `(parent, child)` in order, run
//!    `git rebase --onto <current parent tip> <snapshot parent tip> <child>`.
//!    The current parent tip is read live (parent may have just been
//!    rebased in step 2); the snapshot tip is the pre-rebase value, used as
//!    `--upstream` so only the commits unique to `child` are replayed.
//! 3. If a rebase exits with conflicts, write the current pair index back
//!    to the progress file, return [`Error::RebaseConflict`], and let the
//!    user resolve the conflicts and run `choo rebase --continue` to pick
//!    up where we left off.
//! 4. On success of the final pair, delete the progress file.
//!
//! Pure helpers (`build_plan`, `next_step`) are unit-tested in this module;
//! orchestration is exercised end-to-end by integration tests using a real
//! temp git repo.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::git::{GitRunner, RebaseOutcome};
use crate::report::Reporter;
use crate::state::{self, StateFile, Store, Train};

/// Persisted state of an in-progress (or interrupted) rebase. Lives at
/// `.git/choochoo/rebase-progress.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebaseProgress {
    pub train: String,
    /// Pre-rebase SHA of every branch (and the base), captured at start.
    pub snapshot: BTreeMap<String, String>,
    /// Index of the next pair to process; pairs come from
    /// [`crate::state::Train::pairs`].
    pub next_pair: usize,
}

fn progress_path(repo_root: &Path) -> PathBuf {
    state::state_dir(repo_root).join("rebase-progress.json")
}

fn load_progress(repo_root: &Path) -> Result<Option<RebaseProgress>> {
    let path = progress_path(repo_root);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|e| Error::Io {
        path: path.clone(),
        source: e,
    })?;
    let p: RebaseProgress = serde_json::from_slice(&bytes)?;
    Ok(Some(p))
}

fn save_progress(repo_root: &Path, p: &RebaseProgress) -> Result<()> {
    let dir = state::state_dir(repo_root);
    fs::create_dir_all(&dir).map_err(|e| Error::Io {
        path: dir,
        source: e,
    })?;
    let path = progress_path(repo_root);
    let mut bytes = serde_json::to_vec_pretty(p)?;
    bytes.push(b'\n');
    fs::write(&path, bytes).map_err(|e| Error::Io { path, source: e })
}

fn clear_progress(repo_root: &Path) -> Result<()> {
    let path = progress_path(repo_root);
    if path.exists() {
        fs::remove_file(&path).map_err(|e| Error::Io { path, source: e })?;
    }
    Ok(())
}

/// One unit of work in a rebase plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairStep {
    pub parent: String,
    pub child: String,
    /// Pre-rebase tip of `parent` at the time we took the snapshot. Passed
    /// to `git rebase --upstream`.
    pub snapshot_parent_tip: String,
}

/// Pure helper: build the ordered plan from a train + a snapshot map.
/// Returns an error if the snapshot is missing any branch we need.
pub fn build_plan(train: &Train, snapshot: &BTreeMap<String, String>) -> Result<Vec<PairStep>> {
    let mut plan = Vec::with_capacity(train.branches.len());
    for (parent, child) in train.pairs() {
        let snap = snapshot.get(parent).ok_or_else(|| {
            Error::CorruptState(format!(
                "rebase snapshot missing tip for `{parent}`"
            ))
        })?;
        plan.push(PairStep {
            parent: parent.to_string(),
            child: child.to_string(),
            snapshot_parent_tip: snap.clone(),
        });
    }
    Ok(plan)
}

/// Initial entry point. Errors with [`Error::RebaseConflict`] on conflict;
/// caller is then expected to call [`continue_run`] after they resolve.
pub fn run(
    store: &Store,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    train_name: Option<&str>,
) -> Result<RebaseOutcomeSummary> {
    if load_progress(store.repo_root())?.is_some() {
        return Err(Error::InvalidArgument(
            "a rebase is already in progress; run `choo rebase --continue` or \
             `choo rebase --abort`".into(),
        ));
    }

    let mut state = store.load()?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    let snapshot = take_snapshot(git, state.train(&train_name)?)?;

    let progress = RebaseProgress {
        train: train_name.clone(),
        snapshot,
        next_pair: 0,
    };
    save_progress(store.repo_root(), &progress)?;

    drive(store, git, reporter, &mut state, progress)
}

/// Resume an in-progress rebase. Assumes the user has already run
/// `git rebase --continue` (or completed conflict resolution another way)
/// and wants choochoo to pick up the next branch.
pub fn continue_run(
    store: &Store,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
) -> Result<RebaseOutcomeSummary> {
    let progress = load_progress(store.repo_root())?.ok_or_else(|| {
        Error::InvalidArgument("no rebase in progress".into())
    })?;
    // Advance past the conflicted pair (assumed resolved).
    let progress = RebaseProgress {
        next_pair: progress.next_pair + 1,
        ..progress
    };
    save_progress(store.repo_root(), &progress)?;
    let mut state = store.load()?;
    drive(store, git, reporter, &mut state, progress)
}

/// Abort an in-progress rebase: tell git to abort, drop the progress file.
pub fn abort(store: &Store, git: &dyn GitRunner) -> Result<()> {
    git.rebase_abort()?;
    clear_progress(store.repo_root())?;
    Ok(())
}

/// Drive the plan from `progress.next_pair` to the end. Persists progress
/// after each successful step so an unexpected crash doesn't lose state.
fn drive(
    store: &Store,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    state: &mut StateFile,
    mut progress: RebaseProgress,
) -> Result<RebaseOutcomeSummary> {
    let train = state.train(&progress.train)?.clone();
    let plan = build_plan(&train, &progress.snapshot)?;
    let total = plan.len();
    let mut summary = RebaseOutcomeSummary {
        train: progress.train.clone(),
        rebased: Vec::new(),
        skipped: Vec::new(),
        aggregate_synced: None,
    };

    let mut aggregate_synced = None;

    while progress.next_pair < plan.len() {
        let step = &plan[progress.next_pair];
        let new_parent_tip = git.rev_parse(&step.parent)?;
        reporter.start(&format!(
            "rebasing `{child}` onto `{parent}` ({n}/{total})",
            child = step.child,
            parent = step.parent,
            n = progress.next_pair + 1,
        ));
        match git.rebase_onto(
            &step.child,
            &new_parent_tip,
            &step.snapshot_parent_tip,
        )? {
            RebaseOutcome::Ok { new_sha: _ } => {
                reporter.ok("");
                summary.rebased.push(step.child.clone());
                progress.next_pair += 1;
                save_progress(store.repo_root(), &progress)?;
            }
            RebaseOutcome::Conflict { stderr: _ } => {
                reporter.fail("conflict");
                save_progress(store.repo_root(), &progress)?;
                return Err(Error::RebaseConflict {
                    branch: step.child.clone(),
                });
            }
        }
    }

    // The restack moved every branch, so the aggregate branch is stale:
    // re-point it at the (new) tip. Only reached once the whole train is
    // restacked — a half-rebased train has nothing meaningful to mirror.
    if let Some(outcome) = crate::train::aggregate::sync_train(git, reporter, &train)? {
        aggregate_synced = Some(outcome.branch);
    }
    summary.aggregate_synced = aggregate_synced;

    clear_progress(store.repo_root())?;
    Ok(summary)
}

/// Snapshot every branch's current tip SHA. Includes the train base as well.
fn take_snapshot(git: &dyn GitRunner, train: &Train) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    map.insert(train.base.clone(), git.rev_parse(&train.base)?);
    for branch in &train.branches {
        if !git.branch_exists(branch)? {
            return Err(Error::UnknownBranch(branch.clone()));
        }
        map.insert(branch.clone(), git.rev_parse(branch)?);
    }
    Ok(map)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseOutcomeSummary {
    pub train: String,
    pub rebased: Vec<String>,
    /// Reserved for future "no-op" detection. Currently always empty.
    pub skipped: Vec<String>,
    /// The aggregate branch, if one was re-pointed at the restacked tip.
    pub aggregate_synced: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{NullReporter, RecordingReporter};
    use std::cell::RefCell;
    use tempfile::TempDir;

    fn train(b: &[&str]) -> Train {
        let mut t = Train::new("t", "main");
        t.branches = b.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn build_plan_walks_pairs_with_snapshot_tips() {
        let t = train(&["a", "b", "c"]);
        let mut snap = BTreeMap::new();
        snap.insert("main".into(), "M".into());
        snap.insert("a".into(), "A".into());
        snap.insert("b".into(), "B".into());
        snap.insert("c".into(), "C".into());
        let plan = build_plan(&t, &snap).unwrap();
        assert_eq!(
            plan,
            vec![
                PairStep {
                    parent: "main".into(),
                    child: "a".into(),
                    snapshot_parent_tip: "M".into(),
                },
                PairStep {
                    parent: "a".into(),
                    child: "b".into(),
                    snapshot_parent_tip: "A".into(),
                },
                PairStep {
                    parent: "b".into(),
                    child: "c".into(),
                    snapshot_parent_tip: "B".into(),
                },
            ]
        );
    }

    #[test]
    fn build_plan_errors_on_missing_snapshot() {
        let t = train(&["a"]);
        let snap = BTreeMap::new(); // no entries
        assert!(build_plan(&t, &snap).is_err());
    }

    /// In-memory fake GitRunner for orchestration tests. Tracks branch tips
    /// and "rebase" by simply pointing the child at its `--onto`. The
    /// `force_conflict_on` field, if set, makes the next rebase of that
    /// branch return Conflict.
    struct FakeGit {
        tips: RefCell<BTreeMap<String, String>>,
        force_conflict_on: RefCell<Option<String>>,
        rebase_calls: RefCell<Vec<(String, String, String)>>,
    }

    impl FakeGit {
        fn new(tips: &[(&str, &str)]) -> Self {
            Self {
                tips: RefCell::new(
                    tips.iter()
                        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                        .collect(),
                ),
                force_conflict_on: RefCell::new(None),
                rebase_calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl GitRunner for FakeGit {
        fn current_branch(&self) -> Result<String> {
            Ok("main".into())
        }
        fn branch_exists(&self, name: &str) -> Result<bool> {
            Ok(self.tips.borrow().contains_key(name))
        }
        fn checkout(&self, _branch: &str) -> Result<()> {
            Ok(())
        }
        fn rev_parse(&self, rev: &str) -> Result<String> {
            self.tips
                .borrow()
                .get(rev)
                .cloned()
                .ok_or_else(|| Error::UnknownBranch(rev.to_string()))
        }
        fn rebase_onto(
            &self,
            branch: &str,
            onto: &str,
            upstream: &str,
        ) -> Result<RebaseOutcome> {
            self.rebase_calls.borrow_mut().push((
                branch.to_string(),
                onto.to_string(),
                upstream.to_string(),
            ));
            if self.force_conflict_on.borrow().as_deref() == Some(branch) {
                return Ok(RebaseOutcome::Conflict {
                    stderr: "fake conflict".into(),
                });
            }
            // Simulate: child now has onto's sha appended with its own name.
            let new_sha = format!("{onto}+{branch}");
            self.tips
                .borrow_mut()
                .insert(branch.to_string(), new_sha.clone());
            Ok(RebaseOutcome::Ok { new_sha })
        }
        fn rebase_abort(&self) -> Result<()> {
            *self.force_conflict_on.borrow_mut() = None;
            Ok(())
        }
        fn set_branch(&self, branch: &str, to_rev: &str) -> Result<()> {
            // A rev that isn't a branch is a raw SHA, as with real git.
            let target = self
                .rev_parse(to_rev)
                .unwrap_or_else(|_| to_rev.to_string());
            self.tips.borrow_mut().insert(branch.to_string(), target);
            Ok(())
        }
        fn push(&self, _b: &str, _m: crate::git::PushMode, _r: &str) -> Result<()> {
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

    fn fake_repo() -> (TempDir, Store, FakeGit, StateFile) {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".git/choochoo")).unwrap();
        let store = Store::local(tmp.path());
        let git = FakeGit::new(&[
            ("main", "M"),
            ("a", "A"),
            ("b", "B"),
            ("c", "C"),
        ]);
        let mut state = StateFile::default();
        let mut t = Train::new("t", "main");
        t.branches = vec!["a".into(), "b".into(), "c".into()];
        state.trains.insert("t".into(), t);
        state.active = Some("t".into());
        store.save(&state).unwrap();
        (tmp, store, git, state)
    }

    #[test]
    fn happy_path_rebases_all_branches() {
        let (_tmp, store, git, _) = fake_repo();
        let summary = run(&store, &git, &mut NullReporter, None).unwrap();
        assert_eq!(summary.rebased, vec!["a", "b", "c"]);
        // Verify the right rebase calls happened with the snapshot upstreams.
        let calls = git.rebase_calls.borrow().clone();
        assert_eq!(
            calls,
            vec![
                ("a".into(), "M".into(), "M".into()),
                // Parent `a` is now at M+a (post-rebase).
                ("b".into(), "M+a".into(), "A".into()),
                ("c".into(), "M+a+b".into(), "B".into()),
            ]
        );
        assert!(!progress_path(store.repo_root()).exists());
    }

    /// Enable the aggregate branch on the fixture train.
    fn enable_aggregate(store: &Store, branch: &str) {
        let mut state = store.load().unwrap();
        state.train_mut("t").unwrap().aggregate =
            Some(crate::state::Aggregate::new(branch));
        store.save(&state).unwrap();
    }

    #[test]
    fn aggregate_branch_follows_the_restacked_tip() {
        let (_tmp, store, git, _) = fake_repo();
        enable_aggregate(&store, "choo/t/combined");
        let summary = run(&store, &git, &mut NullReporter, None).unwrap();
        assert_eq!(
            summary.aggregate_synced.as_deref(),
            Some("choo/t/combined")
        );
        // `c` is the tip and ends at M+a+b+c after the restack.
        assert_eq!(git.rev_parse("c").unwrap(), "M+a+b+c");
        assert_eq!(git.rev_parse("choo/t/combined").unwrap(), "M+a+b+c");
    }

    #[test]
    fn aggregate_branch_is_not_synced_while_a_conflict_is_unresolved() {
        let (_tmp, store, git, _) = fake_repo();
        enable_aggregate(&store, "choo/t/combined");
        *git.force_conflict_on.borrow_mut() = Some("b".into());
        let _ = run(&store, &git, &mut NullReporter, None);
        assert!(
            !git.branch_exists("choo/t/combined").unwrap(),
            "a half-restacked train must not update the combined branch"
        );

        // Once the rest of the train lands, `--continue` syncs it.
        *git.force_conflict_on.borrow_mut() = None;
        git.tips
            .borrow_mut()
            .insert("b".into(), "M+a+b-resolved".into());
        let summary = continue_run(&store, &git, &mut NullReporter).unwrap();
        assert_eq!(
            summary.aggregate_synced.as_deref(),
            Some("choo/t/combined")
        );
        assert_eq!(
            git.rev_parse("choo/t/combined").unwrap(),
            git.rev_parse("c").unwrap()
        );
    }

    #[test]
    fn conflict_preserves_progress_file() {
        let (_tmp, store, git, _) = fake_repo();
        *git.force_conflict_on.borrow_mut() = Some("b".into());

        let err = run(&store, &git, &mut NullReporter, None).unwrap_err();
        assert!(matches!(err, Error::RebaseConflict { ref branch } if branch == "b"));

        let prog = load_progress(store.repo_root()).unwrap().unwrap();
        assert_eq!(prog.train, "t");
        assert_eq!(prog.next_pair, 1);

        // Resolve the conflict, then `continue_run` finishes the chain.
        *git.force_conflict_on.borrow_mut() = None;
        // Manually pretend `git rebase --continue` succeeded: bump b's tip.
        git.tips
            .borrow_mut()
            .insert("b".into(), "M+a+b-resolved".into());

        let summary = continue_run(&store, &git, &mut NullReporter).unwrap();
        assert_eq!(summary.rebased, vec!["c"]);
        assert!(!progress_path(store.repo_root()).exists());
    }

    #[test]
    fn second_run_while_in_progress_errors() {
        let (_tmp, store, git, _) = fake_repo();
        *git.force_conflict_on.borrow_mut() = Some("a".into());
        let _ = run(&store, &git, &mut NullReporter, None);
        // Don't clear; simulate user trying to start over.
        let err = run(&store, &git, &mut NullReporter, None).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
    }

    #[test]
    fn abort_clears_progress() {
        let (_tmp, store, git, _) = fake_repo();
        *git.force_conflict_on.borrow_mut() = Some("a".into());
        let _ = run(&store, &git, &mut NullReporter, None);
        assert!(progress_path(store.repo_root()).exists());
        abort(&store, &git).unwrap();
        assert!(!progress_path(store.repo_root()).exists());
    }

    #[test]
    fn emits_one_progress_step_per_branch() {
        let (_tmp, store, git, _) = fake_repo();
        let mut rep = RecordingReporter::new();
        run(&store, &git, &mut rep, None).unwrap();
        assert_eq!(rep.events.len(), 3, "events: {:?}", rep.events);
        assert!(rep.events[0].contains("rebasing `a` onto `main`"));
        assert!(rep.events[0].contains("(1/3)"));
        assert!(rep.events[1].contains("rebasing `b` onto `a`"));
        assert!(rep.events[2].contains("rebasing `c` onto `b`"));
        assert!(rep.events.iter().all(|e| e.ends_with("ok")));
    }

    #[test]
    fn conflict_step_is_marked_failed_in_progress() {
        let (_tmp, store, git, _) = fake_repo();
        *git.force_conflict_on.borrow_mut() = Some("b".into());
        let mut rep = RecordingReporter::new();
        let _ = run(&store, &git, &mut rep, None);
        let joined = rep.joined();
        assert!(joined.contains("rebasing `a`"));
        assert!(joined.contains("ok"));
        assert!(joined.contains("rebasing `b`"));
        assert!(joined.contains("FAILED: conflict"), "got: {joined}");
    }
}
