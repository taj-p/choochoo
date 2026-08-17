//! The aggregate ("combined") branch: one extra branch per train that
//! holds *all* of the train's changes, plus a draft PR for it against the
//! train's base.
//!
//! ## Why
//!
//! A train splits a big change into reviewable pieces, but reviewers often
//! also want to see the whole thing at once ("does this all add up?"), and
//! CI configured to run on PRs against the default branch only ever sees
//! the pieces. The aggregate branch gives you both without disturbing the
//! stack.
//!
//! ## How
//!
//! The aggregate branch is *derived* state, not a place to commit: it is
//! force-updated to the tip of the train (the last branch), whose diff
//! against `base` is by construction the union of every change in the
//! train. That means:
//!
//! * no extra merge/squash commits to maintain,
//! * nothing to re-resolve when the train is rebased — a fresh
//!   [`sync_train`] after the restack is enough,
//! * the aggregate PR's diff always equals "the whole train".
//!
//! Its PR is always opened as a **draft** and always targets the train's
//! `base` (normally the repo default branch), because it's a review aid:
//! the individual PRs are what get merged. See [`crate::train::pr`].

use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::report::Reporter;
use crate::state::{self, Aggregate, Store, Train};

/// What [`sync_train`] did to the aggregate branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    /// The aggregate branch.
    pub branch: String,
    /// Train branch whose tip the aggregate now mirrors.
    pub tip: String,
    /// SHA both now point at.
    pub sha: String,
    /// False when the aggregate branch was already at `sha`.
    pub moved: bool,
}

/// Enable the aggregate branch for a train.
///
/// `branch` defaults to [`state::default_aggregate_branch`]. Idempotent
/// for the same branch name; naming a *different* branch re-points the
/// aggregate and forgets the old branch's PR (that PR belongs to the old
/// branch, and choochoo no longer manages it).
///
/// The branch is synced immediately when the train has branches, so
/// `choo push` right after `choo aggregate enable` does the right thing.
pub fn enable(
    store: &Store,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    train_name: Option<&str>,
    branch: Option<&str>,
) -> Result<String> {
    let mut state = store.load()?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    let train = state.train_mut(&train_name)?;

    let branch = match branch {
        Some(b) if b.trim().is_empty() => {
            return Err(Error::InvalidArgument(
                "aggregate branch name cannot be empty".into(),
            ));
        }
        Some(b) => b.trim().to_string(),
        None => state::default_aggregate_branch(&train_name),
    };

    if branch == train.base {
        return Err(Error::InvalidArgument(format!(
            "aggregate branch cannot be the train's base `{}`",
            train.base
        )));
    }
    if train.position(&branch).is_some() {
        return Err(Error::InvalidArgument(format!(
            "`{branch}` is already a branch in train `{train_name}`; \
             the aggregate branch must be a separate branch"
        )));
    }

    // Adopting a pre-existing branch means force-moving it: say so, since
    // whatever is on it now will be replaced by the train tip.
    let previous = train.aggregate_branch().map(str::to_string);
    if previous.as_deref() != Some(branch.as_str()) && git.branch_exists(&branch)? {
        reporter.info(&format!(
            "note: branch `{branch}` already exists and will be force-updated \
             to the tip of train `{train_name}`"
        ));
    }

    match &mut train.aggregate {
        Some(existing) if existing.branch == branch => {}
        _ => train.aggregate = Some(Aggregate::new(&branch)),
    }
    let train = train.clone();
    store.save(&state)?;

    sync_train(git, reporter, &train)?;
    Ok(branch)
}

/// Stop managing the train's aggregate branch. The git branch and any PR
/// are left alone — choochoo just forgets about them, mirroring
/// `choo remove`, which also never deletes branches.
pub fn disable(store: &Store, train_name: Option<&str>) -> Result<String> {
    let mut state = store.load()?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    let train = state.train_mut(&train_name)?;
    let branch = train
        .aggregate
        .take()
        .map(|a| a.branch)
        .ok_or_else(|| {
            Error::InvalidArgument(format!(
                "train `{train_name}` has no aggregate branch"
            ))
        })?;
    store.save(&state)?;
    Ok(branch)
}

/// `choo aggregate sync` — update the aggregate branch to the train tip.
///
/// Errors when the train has no aggregate branch configured; returns
/// `Ok(None)` when there is simply nothing to mirror yet (empty train).
pub fn run_sync(
    store: &Store,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    train_name: Option<&str>,
) -> Result<Option<SyncOutcome>> {
    let state = store.load()?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    let train = state.train(&train_name)?;
    if train.aggregate.is_none() {
        return Err(Error::InvalidArgument(format!(
            "train `{train_name}` has no aggregate branch; run \
             `choo aggregate enable` first"
        )));
    }
    sync_train(git, reporter, train)
}

/// Point the train's aggregate branch at the train's tip.
///
/// Returns `Ok(None)` — and touches nothing — when the train has no
/// aggregate branch or no branches to mirror. Callers that already hold a
/// [`Train`] (`push`, `rebase`, `enable`) use this directly; it needs no
/// state file access because the aggregate branch is derived from the tip.
pub fn sync_train(
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    train: &Train,
) -> Result<Option<SyncOutcome>> {
    let Some(branch) = train.aggregate_branch() else {
        return Ok(None);
    };
    let Some(tip) = train.tip() else {
        reporter.info(&format!(
            "train `{}` has no branches; leaving combined branch `{branch}` alone",
            train.name
        ));
        return Ok(None);
    };
    if !git.branch_exists(tip)? {
        return Err(Error::UnknownBranch(tip.to_string()));
    }

    reporter.start(&format!(
        "syncing combined branch `{branch}` to tip of `{tip}`"
    ));
    let sha = match git.rev_parse(tip) {
        Ok(sha) => sha,
        Err(e) => {
            reporter.fail(&e.to_string());
            return Err(e);
        }
    };
    let before = if git.branch_exists(branch)? {
        git.rev_parse(branch).ok()
    } else {
        None
    };
    if let Err(e) = git.set_branch(branch, &sha) {
        reporter.fail(&e.to_string());
        return Err(e);
    }
    let moved = before.as_deref() != Some(sha.as_str());
    reporter.ok(if moved { "" } else { "unchanged" });

    Ok(Some(SyncOutcome {
        branch: branch.to_string(),
        tip: tip.to_string(),
        sha,
        moved,
    }))
}

/// PR title used when choochoo first opens the aggregate PR. Never
/// rewritten afterwards, so renaming it on GitHub sticks.
pub fn pr_title(train: &Train) -> String {
    format!("Combined: {}", train.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{PushMode, RebaseOutcome};
    use crate::report::{NullReporter, RecordingReporter};
    use crate::state::StateFile;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    struct FakeGit {
        tips: RefCell<BTreeMap<String, String>>,
        current: String,
    }

    impl FakeGit {
        fn new(tips: &[(&str, &str)]) -> Self {
            Self {
                tips: RefCell::new(
                    tips.iter()
                        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                        .collect(),
                ),
                current: "main".to_string(),
            }
        }
        fn tip_of(&self, branch: &str) -> Option<String> {
            self.tips.borrow().get(branch).cloned()
        }
    }

    impl GitRunner for FakeGit {
        fn current_branch(&self) -> Result<String> {
            Ok(self.current.clone())
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
            // Aggregate sync only ever force-moves a branch to the train tip;
            // it never picks a rebase boundary, so nothing here asks.
            unreachable!()
        }
        fn rebase_onto(&self, _b: &str, _o: &str, _u: &str) -> Result<RebaseOutcome> {
            unreachable!()
        }
        fn rebase_abort(&self) -> Result<()> {
            Ok(())
        }
        fn set_branch(&self, branch: &str, to_rev: &str) -> Result<()> {
            // Like `git rev-parse`, a rev that isn't a branch is taken as a
            // raw SHA.
            let target = self
                .rev_parse(to_rev)
                .unwrap_or_else(|_| to_rev.to_string());
            if self.current == branch && self.tip_of(branch).as_deref() != Some(&target) {
                return Err(Error::InvalidArgument(format!(
                    "branch `{branch}` is checked out"
                )));
            }
            self.tips.borrow_mut().insert(branch.to_string(), target);
            Ok(())
        }
        fn fast_forward_current(&self, _t: &str) -> Result<()> {
            // The aggregate branch is re-pointed with `git branch -f`; it is
            // never the checked-out branch's business.
            unreachable!()
        }
        fn reset_hard_current(&self, _t: &str) -> Result<()> {
            unreachable!("only `choo pull --reset` resets")
        }
        fn is_dirty(&self) -> Result<bool> {
            Ok(false)
        }
        fn push(&self, _b: &str, _m: PushMode, _r: &str) -> Result<()> {
            Ok(())
        }
        fn push_many(
            &self,
            _b: &[&str],
            _m: PushMode,
            _r: &str,
            _atomic: bool,
        ) -> Result<()> {
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

    /// A tempdir-backed local store holding the fixture train, plus a fake
    /// git. `TempDir` is returned so it outlives the store.
    fn setup() -> (TempDir, Store, FakeGit) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/choochoo")).unwrap();
        let store = Store::local(tmp.path());
        let mut state = StateFile::default();
        let mut t = Train::new("t", "main");
        t.branches = vec!["a".into(), "b".into()];
        state.trains.insert("t".into(), t);
        state.active = Some("t".into());
        store.save(&state).unwrap();
        let git = FakeGit::new(&[("main", "M"), ("a", "A"), ("b", "B")]);
        (tmp, store, git)
    }

    fn train_of(store: &Store) -> Train {
        store.load().unwrap().train("t").unwrap().clone()
    }

    #[test]
    fn enable_defaults_branch_name_and_syncs_to_tip() {
        let (_tmp, store, git) = setup();
        let branch = enable(&store, &git, &mut NullReporter, None, None).unwrap();
        assert_eq!(branch, "choo/t/combined");
        assert_eq!(
            train_of(&store).aggregate_branch(),
            Some("choo/t/combined")
        );
        // Mirrors `b`, the tip: same SHA, so its diff vs `main` is the
        // whole train.
        assert_eq!(git.tip_of("choo/t/combined").as_deref(), Some("B"));
    }

    #[test]
    fn enable_accepts_explicit_branch_name() {
        let (_tmp, store, git) = setup();
        let branch =
            enable(&store, &git, &mut NullReporter, None, Some("all-of-it")).unwrap();
        assert_eq!(branch, "all-of-it");
        assert_eq!(git.tip_of("all-of-it").as_deref(), Some("B"));
    }

    #[test]
    fn enable_is_idempotent_and_keeps_the_pr() {
        let (_tmp, store, git) = setup();
        enable(&store, &git, &mut NullReporter, None, None).unwrap();

        // Pretend `choo pr` recorded the aggregate PR.
        let mut state = store.load().unwrap();
        state.train_mut("t").unwrap().aggregate.as_mut().unwrap().pr =
            Some(crate::state::PrInfo {
                number: 7,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            });
        store.save(&state).unwrap();

        enable(&store, &git, &mut NullReporter, None, None).unwrap();
        let pr = train_of(&store).aggregate.unwrap().pr;
        assert_eq!(pr.map(|p| p.number), Some(7));
    }

    #[test]
    fn renaming_the_aggregate_branch_drops_the_old_pr() {
        let (_tmp, store, git) = setup();
        enable(&store, &git, &mut NullReporter, None, None).unwrap();
        let mut state = store.load().unwrap();
        state.train_mut("t").unwrap().aggregate.as_mut().unwrap().pr =
            Some(crate::state::PrInfo {
                number: 7,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            });
        store.save(&state).unwrap();

        enable(&store, &git, &mut NullReporter, None, Some("other")).unwrap();
        let agg = train_of(&store).aggregate.unwrap();
        assert_eq!(agg.branch, "other");
        assert!(agg.pr.is_none(), "PR #7 belongs to the old branch");
    }

    #[test]
    fn enable_warns_when_adopting_an_existing_branch() {
        let (_tmp, store, git) = setup();
        let mut rep = RecordingReporter::new();
        enable(&store, &git, &mut rep, None, Some("main-ish")).unwrap();
        assert!(!rep.joined().contains("already exists"));

        let mut rep = RecordingReporter::new();
        enable(&store, &git, &mut rep, None, Some("a-copy")).unwrap();
        // `a-copy` doesn't exist yet either.
        assert!(!rep.joined().contains("already exists"));

        // But an existing branch is called out.
        let mut rep = RecordingReporter::new();
        enable(&store, &git, &mut rep, None, Some("main-ish")).unwrap();
        assert!(rep.joined().contains("already exists"), "got: {}", rep.joined());
    }

    #[test]
    fn enable_rejects_base_and_train_branches() {
        let (_tmp, store, git) = setup();
        assert!(matches!(
            enable(&store, &git, &mut NullReporter, None, Some("main")),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            enable(&store, &git, &mut NullReporter, None, Some("b")),
            Err(Error::InvalidArgument(_))
        ));
        assert!(train_of(&store).aggregate.is_none());
    }

    #[test]
    fn disable_forgets_config_but_leaves_the_branch() {
        let (_tmp, store, git) = setup();
        enable(&store, &git, &mut NullReporter, None, None).unwrap();
        let branch = disable(&store, None).unwrap();
        assert_eq!(branch, "choo/t/combined");
        assert!(train_of(&store).aggregate.is_none());
        // git branch still there, exactly like `choo remove`.
        assert!(git.branch_exists("choo/t/combined").unwrap());
    }

    #[test]
    fn disable_without_aggregate_errors() {
        let (_tmp, store, _git) = setup();
        assert!(matches!(
            disable(&store, None),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn sync_follows_the_tip_as_the_train_grows() {
        let (_tmp, store, git) = setup();
        enable(&store, &git, &mut NullReporter, None, None).unwrap();

        // A new branch `c` is appended and becomes the tip.
        let mut state = store.load().unwrap();
        state.train_mut("t").unwrap().branches.push("c".into());
        store.save(&state).unwrap();
        git.tips.borrow_mut().insert("c".into(), "C".into());

        let out = run_sync(&store, &git, &mut NullReporter, None)
            .unwrap()
            .unwrap();
        assert_eq!((out.tip.as_str(), out.sha.as_str()), ("c", "C"));
        assert!(out.moved);
        assert_eq!(git.tip_of("choo/t/combined").as_deref(), Some("C"));
    }

    #[test]
    fn sync_reports_unchanged_when_already_at_tip() {
        let (_tmp, store, git) = setup();
        enable(&store, &git, &mut NullReporter, None, None).unwrap();
        let mut rep = RecordingReporter::new();
        let out = run_sync(&store, &git, &mut rep, None).unwrap().unwrap();
        assert!(!out.moved);
        assert!(rep.joined().contains("unchanged"), "got: {}", rep.joined());
    }

    #[test]
    fn sync_without_aggregate_errors() {
        let (_tmp, store, git) = setup();
        assert!(matches!(
            run_sync(&store, &git, &mut NullReporter, None),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn sync_train_is_a_noop_for_trains_without_an_aggregate() {
        let (_tmp, _store, git) = setup();
        let train = Train::new("t", "main");
        assert_eq!(
            sync_train(&git, &mut NullReporter, &train).unwrap(),
            None
        );
    }

    #[test]
    fn sync_train_skips_empty_trains() {
        let (_tmp, _store, git) = setup();
        let mut train = Train::new("t", "main");
        train.aggregate = Some(Aggregate::new("choo/t/combined"));
        let mut rep = RecordingReporter::new();
        assert_eq!(sync_train(&git, &mut rep, &train).unwrap(), None);
        assert!(rep.joined().contains("no branches"), "got: {}", rep.joined());
        assert!(!git.branch_exists("choo/t/combined").unwrap());
    }

    #[test]
    fn sync_train_errors_when_tip_branch_is_missing_locally() {
        let (_tmp, _store, git) = setup();
        let mut train = Train::new("t", "main");
        train.branches = vec!["ghost".into()];
        train.aggregate = Some(Aggregate::new("choo/t/combined"));
        assert!(matches!(
            sync_train(&git, &mut NullReporter, &train),
            Err(Error::UnknownBranch(b)) if b == "ghost"
        ));
    }

    #[test]
    fn sync_refuses_to_move_the_checked_out_aggregate_branch() {
        let (_tmp, store, mut git) = setup();
        enable(&store, &git, &mut NullReporter, None, None).unwrap();
        // Train tip advances while the user sits on the aggregate branch.
        git.tips.borrow_mut().insert("b".into(), "B2".into());
        git.current = "choo/t/combined".into();
        let err = run_sync(&store, &git, &mut NullReporter, None).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)), "got: {err}");
    }

    #[test]
    fn pr_title_names_the_train() {
        assert_eq!(pr_title(&Train::new("feat", "main")), "Combined: feat");
    }
}
