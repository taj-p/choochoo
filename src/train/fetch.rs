//! `choo fetch` — materialise a train's branches on this machine.
//!
//! Shared state tells you a train *exists*; it doesn't put the branches in
//! your working copy. Walking onto a second devbox, `choo show` lists a
//! stack whose branches are only on `origin`, and `choo checkout` has
//! nothing local to switch to. This is the command that closes that gap.
//!
//! Two rules shape it:
//!
//! * **Never move a branch that already exists locally.** It may hold work
//!   you haven't pushed. Being behind `origin` is reported, not fixed —
//!   fast-forwarding someone's branch as a side effect of "fetch" would be
//!   the kind of surprise this tool exists to avoid.
//! * **Never move the working tree.** `git branch --track` creates every
//!   branch without touching HEAD, so a train of ten branches arrives
//!   without you leaving the one you're on.

use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::report::{Reporter, ReporterExt};
use crate::state::Store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchSummary {
    pub train: String,
    /// Local tracking branches this run created.
    pub created: Vec<String>,
    /// Already present locally, and left exactly as they were.
    pub existing: Vec<String>,
    /// Present locally but behind the remote, with how many commits.
    pub behind: Vec<(String, u32)>,
    /// In the train, but on neither this machine nor the remote — most
    /// likely never pushed from wherever the train was built.
    pub missing: Vec<String>,
}

impl FetchSummary {
    /// True when the train can't be used here yet.
    pub fn is_incomplete(&self) -> bool {
        !self.missing.is_empty()
    }
}

/// Create a local tracking branch for every branch in the train that we
/// don't have yet.
///
/// Errors with [`Error::IncompleteTrain`] when some branch exists nowhere —
/// *after* creating every branch it could. Partial success with a non-zero
/// exit is the honest report: the train isn't usable, but there's no reason
/// to throw away the branches that did arrive.
pub fn run(
    store: &Store,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    train_name: Option<&str>,
    remote: &str,
) -> Result<FetchSummary> {
    let state = store.load()?;
    let name = state.resolve_train_name(train_name)?.to_string();
    let train = state.train(&name)?;

    reporter.start(&format!("fetching `{remote}`"));
    match git.fetch(remote) {
        Ok(()) => reporter.ok(""),
        Err(e) => {
            reporter.fail(&e.to_string());
            return Err(e);
        }
    }

    // The base matters too: a rebase needs it, and on a fresh clone of a
    // repo whose default branch isn't checked out it may be absent.
    let wanted: Vec<String> = std::iter::once(train.base.clone())
        .chain(train.branches.iter().cloned())
        .chain(train.aggregate_branch().map(str::to_string))
        .collect();

    let mut summary = FetchSummary {
        train: name.clone(),
        created: Vec::new(),
        existing: Vec::new(),
        behind: Vec::new(),
        missing: Vec::new(),
    };

    for branch in &wanted {
        if git.branch_exists(branch)? {
            // Leave it alone, but say so if it's stale — the user may want
            // to `git pull` before rebasing.
            let upstream = format!("{remote}/{branch}");
            if let Ok(Some((_ahead, behind))) = git.ahead_behind(branch, &upstream) {
                if behind > 0 {
                    reporter.step_ok(
                        &format!("`{branch}` already here"),
                        &format!("{behind} behind {upstream}"),
                    );
                    summary.behind.push((branch.clone(), behind));
                    continue;
                }
            }
            reporter.step_ok(&format!("`{branch}` already here"), "up to date");
            summary.existing.push(branch.clone());
            continue;
        }

        if !git.remote_branch_exists(remote, branch)? {
            reporter.step_ok(
                &format!("`{branch}`"),
                &format!("not on `{remote}` either — never pushed?"),
            );
            summary.missing.push(branch.clone());
            continue;
        }

        reporter.start(&format!("creating `{branch}` from `{remote}/{branch}`"));
        match git.create_tracking_branch(branch, remote) {
            Ok(()) => {
                reporter.ok("");
                summary.created.push(branch.clone());
            }
            Err(e) => {
                reporter.fail(&e.to_string());
                return Err(e);
            }
        }
    }

    if summary.is_incomplete() {
        return Err(Error::IncompleteTrain {
            train: name,
            remote: remote.to_string(),
            missing: summary.missing.clone(),
        });
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{PushMode, RebaseOutcome};
    use crate::report::{NullReporter, RecordingReporter};
    use crate::state::{Aggregate, StateFile, Train};
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    struct FakeGit {
        local: RefCell<BTreeSet<String>>,
        remote: BTreeSet<String>,
        /// Branches reported as behind their upstream, by how much.
        behind: u32,
        fetched: RefCell<Vec<String>>,
        /// Set if anything tries to move the working tree.
        checkouts: RefCell<Vec<String>>,
    }

    impl FakeGit {
        fn new(local: &[&str], remote: &[&str]) -> Self {
            Self {
                local: RefCell::new(local.iter().map(|s| s.to_string()).collect()),
                remote: remote.iter().map(|s| s.to_string()).collect(),
                behind: 0,
                fetched: RefCell::new(Vec::new()),
                checkouts: RefCell::new(Vec::new()),
            }
        }
    }

    impl GitRunner for FakeGit {
        fn current_branch(&self) -> Result<String> {
            Ok("main".into())
        }
        fn branch_exists(&self, name: &str) -> Result<bool> {
            Ok(self.local.borrow().contains(name))
        }
        fn checkout(&self, b: &str) -> Result<()> {
            self.checkouts.borrow_mut().push(b.to_string());
            Ok(())
        }
        fn rev_parse(&self, rev: &str) -> Result<String> {
            Ok(format!("sha-{rev}"))
        }
        fn is_ancestor(&self, _a: &str, _d: &str) -> Result<bool> {
            // `choo fetch` creates tracking branches; it never rebases, so it
            // never asks about ancestry.
            unreachable!()
        }
        fn rebase_onto(&self, _b: &str, _o: &str, _u: &str) -> Result<RebaseOutcome> {
            unreachable!()
        }
        fn rebase_abort(&self) -> Result<()> {
            Ok(())
        }
        fn set_branch(&self, _b: &str, _t: &str) -> Result<()> {
            Ok(())
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
        fn fetch(&self, remote: &str) -> Result<()> {
            self.fetched.borrow_mut().push(remote.to_string());
            Ok(())
        }
        fn ahead_behind(&self, _b: &str, _u: &str) -> Result<Option<(u32, u32)>> {
            Ok(Some((0, self.behind)))
        }
        fn remote_url(&self, _r: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn remote_branch_exists(&self, remote: &str, branch: &str) -> Result<bool> {
            Ok(self.remote.contains(&format!("{remote}/{branch}")))
        }
        fn create_tracking_branch(&self, branch: &str, remote: &str) -> Result<()> {
            assert!(
                self.remote.contains(&format!("{remote}/{branch}")),
                "tried to track a branch that isn't on the remote"
            );
            self.local.borrow_mut().insert(branch.to_string());
            Ok(())
        }
    }

    /// A store holding a two-branch train, as if synced from another box.
    fn setup(aggregate: Option<&str>) -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/choochoo")).unwrap();
        let store = Store::local(tmp.path());
        let mut state = StateFile::default();
        let mut t = Train::new("t", "main");
        t.branches = vec!["a".into(), "b".into()];
        t.aggregate = aggregate.map(Aggregate::new);
        state.trains.insert("t".into(), t);
        state.active = Some("t".into());
        store.save(&state).unwrap();
        (tmp, store)
    }

    #[test]
    fn creates_every_missing_branch_from_the_remote() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(
            &["main"],
            &["origin/main", "origin/a", "origin/b"],
        );
        let s = run(&store, &git, &mut NullReporter, None, "origin").unwrap();
        assert_eq!(s.created, vec!["a", "b"]);
        assert_eq!(s.existing, vec!["main"]);
        assert!(s.missing.is_empty());
    }

    /// The whole point of `git branch --track` over `checkout -b`.
    #[test]
    fn never_moves_the_working_tree() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&["main"], &["origin/main", "origin/a", "origin/b"]);
        run(&store, &git, &mut NullReporter, None, "origin").unwrap();
        assert!(
            git.checkouts.borrow().is_empty(),
            "fetch must not check anything out"
        );
    }

    #[test]
    fn fetches_the_remote_exactly_once() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&["main"], &["origin/main", "origin/a", "origin/b"]);
        run(&store, &git, &mut NullReporter, None, "origin").unwrap();
        assert_eq!(*git.fetched.borrow(), vec!["origin"]);
    }

    #[test]
    fn existing_branches_are_left_alone() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(
            &["main", "a", "b"],
            &["origin/main", "origin/a", "origin/b"],
        );
        let s = run(&store, &git, &mut NullReporter, None, "origin").unwrap();
        assert!(s.created.is_empty());
        assert_eq!(s.existing, vec!["main", "a", "b"]);
    }

    /// Being behind is reported, never silently fast-forwarded — the branch
    /// might hold unpushed work.
    #[test]
    fn a_stale_local_branch_is_reported_not_moved() {
        let (_tmp, store) = setup(None);
        let mut git = FakeGit::new(
            &["main", "a", "b"],
            &["origin/main", "origin/a", "origin/b"],
        );
        git.behind = 3;
        let mut rep = RecordingReporter::new();
        let s = run(&store, &git, &mut rep, None, "origin").unwrap();
        assert!(s.created.is_empty());
        assert_eq!(s.behind.len(), 3);
        assert_eq!(s.behind[0], ("main".to_string(), 3));
        assert!(rep.joined().contains("3 behind origin/main"), "{}", rep.joined());
    }

    #[test]
    fn the_aggregate_branch_comes_along_too() {
        let (_tmp, store) = setup(Some("choo/t/combined"));
        let git = FakeGit::new(
            &["main"],
            &[
                "origin/main",
                "origin/a",
                "origin/b",
                "origin/choo/t/combined",
            ],
        );
        let s = run(&store, &git, &mut NullReporter, None, "origin").unwrap();
        assert_eq!(s.created, vec!["a", "b", "choo/t/combined"]);
    }

    /// A train synced mid-work can legitimately name a branch the other box
    /// never pushed. Create what we can, then say the train is incomplete.
    #[test]
    fn missing_branches_error_but_only_after_creating_the_rest() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&["main"], &["origin/main", "origin/a"]);
        let err = run(&store, &git, &mut NullReporter, None, "origin").unwrap_err();
        match err {
            Error::IncompleteTrain { train, missing, .. } => {
                assert_eq!(train, "t");
                assert_eq!(missing, vec!["b"]);
            }
            other => panic!("expected IncompleteTrain, got {other}"),
        }
        // `a` still got created — partial progress isn't thrown away.
        assert!(git.branch_exists("a").unwrap());
    }

    #[test]
    fn the_base_branch_is_fetched_as_well() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&[], &["origin/main", "origin/a", "origin/b"]);
        let s = run(&store, &git, &mut NullReporter, None, "origin").unwrap();
        assert_eq!(s.created, vec!["main", "a", "b"]);
    }

    #[test]
    fn unknown_train_errors() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&["main"], &["origin/main"]);
        assert!(matches!(
            run(&store, &git, &mut NullReporter, Some("ghost"), "origin"),
            Err(Error::UnknownTrain(_))
        ));
    }

    #[test]
    fn honours_a_non_default_remote() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&["main"], &["upstream/main", "upstream/a", "upstream/b"]);
        let s = run(&store, &git, &mut NullReporter, None, "upstream").unwrap();
        assert_eq!(s.created, vec!["a", "b"]);
        assert_eq!(*git.fetched.borrow(), vec!["upstream"]);
    }
}
