//! `choo pull` — bring a train's local branches up to date with the remote.
//!
//! `choo fetch` answers "I don't have these branches"; this answers "the
//! ones I have are stale". Both situations turn up in the same places: the
//! base branch advanced while you were working, a teammate pushed to a
//! branch in the train, or the other devbox pushed the train and this one
//! is a day behind. Doing it by hand is a `git checkout` and a `git pull`
//! per branch, which is exactly the sort of per-branch bookkeeping choochoo
//! exists to take over.
//!
//! It is `choo fetch` plus one extra liberty, and the boundary is the whole
//! design:
//!
//! * **Only fast-forwards.** A branch is moved only when the remote is
//!   strictly ahead of it, so nothing local is ever lost. A branch that has
//!   diverged — the normal state of a train you just rebased — is reported
//!   and left exactly where it is. `choo rebase` is the command that
//!   rewrites history; this one never does.
//! * **The working tree moves only for the branch you're on.** Everything
//!   else is updated through `git branch --force`, so pulling a ten-branch
//!   train doesn't take you off what you were doing. The checked-out branch
//!   is fast-forwarded with `git merge --ff-only`, which is the only way it
//!   can move without leaving the tree behind.
//!
//! ## `--reset`
//!
//! Those rules have one bad consequence, and it's the common case for this
//! tool rather than a corner: when another machine rebases the train and
//! force-pushes, *every* branch here looks diverged, and the default pull
//! declines all of them. The rewrite is exactly what you wanted, but pull
//! can't tell it apart from unpushed local work — the two are the same
//! shape.
//!
//! `--reset` is the answer, and it's a flag rather than a heuristic because
//! only the person typing it knows which side is the truth. It hard-resets
//! diverged branches onto the remote. Three things bound it:
//!
//! * **Diverged branches only.** A branch that is merely *ahead* has local
//!   commits and no rewrite behind it — nothing here says the remote is the
//!   better version — so `--reset` skips it and says so. A mistyped
//!   `--reset` can't quietly delete work that exists nowhere else.
//! * **Never over uncommitted changes.** `git reset --hard` discards them
//!   silently, so a dirty tree on a branch that would be reset stops the
//!   whole command *before* anything moves, rather than mid-train.
//! * **Nothing is unrecoverable.** Discarded commits stay in the reflog;
//!   the summary says so, with the branches to look in.

use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::report::{Reporter, ReporterExt};
use crate::state::Store;

/// What `choo pull` did to one branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Wasn't here; created as a local tracking branch of the remote.
    Created,
    /// Was behind the remote and nothing else; moved up to it.
    FastForwarded { commits: u32 },
    /// Already level with the remote.
    UpToDate,
    /// Has local commits the remote doesn't; nothing to pull.
    Ahead { commits: u32 },
    /// Both sides moved. Left alone — merging or resetting would be a
    /// judgement call, and `choo rebase` is where that happens.
    Diverged { ahead: u32, behind: u32 },
    /// Was diverged, and `--reset` said the remote wins. `discarded` local
    /// commits are no longer on the branch (but are still in its reflog).
    Reset { discarded: u32 },
    /// Here but not on the remote: never pushed, or already deleted there.
    NotOnRemote,
    /// Both sides exist but git wouldn't compare them. Left alone.
    Incomparable,
    /// On neither side — the train names a branch nobody ever pushed.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullSummary {
    pub train: String,
    /// Every branch considered — the base, the train, and the aggregate
    /// branch if enabled — in that order, with what happened to it.
    pub branches: Vec<(String, Outcome)>,
}

impl PullSummary {
    fn count(&self, pred: impl Fn(&Outcome) -> bool) -> usize {
        self.branches.iter().filter(|(_, o)| pred(o)).count()
    }

    pub fn created(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Created))
    }

    pub fn updated(&self) -> usize {
        self.count(|o| matches!(o, Outcome::FastForwarded { .. }))
    }

    pub fn up_to_date(&self) -> usize {
        self.count(|o| matches!(o, Outcome::UpToDate | Outcome::Ahead { .. }))
    }

    /// Branches left alone because moving them would need a decision.
    pub fn diverged(&self) -> Vec<&str> {
        self.named(|o| matches!(o, Outcome::Diverged { .. }))
    }

    /// Branches `--reset` moved onto the remote, with the number of local
    /// commits each one gave up.
    pub fn reset(&self) -> Vec<(&str, u32)> {
        self.branches
            .iter()
            .filter_map(|(b, o)| match o {
                Outcome::Reset { discarded } => Some((b.as_str(), *discarded)),
                _ => None,
            })
            .collect()
    }

    /// Branches `--reset` deliberately passed over: they hold local commits
    /// the remote has never seen, which is unpushed work, not a stale copy.
    pub fn kept_ahead(&self) -> Vec<&str> {
        self.named(|o| matches!(o, Outcome::Ahead { .. }))
    }

    fn named(&self, pred: impl Fn(&Outcome) -> bool) -> Vec<&str> {
        self.branches
            .iter()
            .filter(|(_, o)| pred(o))
            .map(|(b, _)| b.as_str())
            .collect()
    }

    pub fn missing(&self) -> Vec<String> {
        self.branches
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Missing))
            .map(|(b, _)| b.clone())
            .collect()
    }
}

/// Fetch `remote`, then bring every branch in the train up to it.
///
/// With `reset`, branches that have diverged are moved onto the remote
/// instead of being reported — see the module docs for what that does and
/// doesn't cover.
///
/// Errors with [`Error::IncompleteTrain`] when the train names a branch
/// that exists on neither side — after updating everything else, on the
/// same reasoning as [`crate::train::fetch::run`]: the train isn't usable
/// here, but the branches that *did* update stay updated.
pub fn run(
    store: &Store,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    train_name: Option<&str>,
    remote: &str,
    reset: bool,
) -> Result<PullSummary> {
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

    // The base first: it's the branch most likely to have moved, and the
    // one `choo rebase` restacks onto afterwards.
    let wanted: Vec<String> = std::iter::once(train.base.clone())
        .chain(train.branches.iter().cloned())
        .chain(train.aggregate_branch().map(str::to_string))
        .collect();

    // Asked once, not per branch: HEAD can't move under us here, and the
    // answer decides *how* a branch is fast-forwarded.
    let current = git.current_branch()?;

    // Judge everything before moving anything. `--reset` can refuse the
    // whole run (a dirty tree under the branch it would reset), and refusing
    // is only honest if it happens before the first branch has moved.
    let mut plan: Vec<(&String, Judgement)> = Vec::new();
    for branch in &wanted {
        plan.push((branch, judge(git, branch, remote)?));
    }

    if reset {
        let resetting_current = plan.iter().any(|(b, j)| {
            *b == &current && matches!(j, Judgement::Diverged { .. })
        });
        if resetting_current && git.is_dirty()? {
            return Err(Error::DirtyWorkingTree { branch: current });
        }
    }

    let mut summary = PullSummary {
        train: name.clone(),
        branches: Vec::new(),
    };

    for (branch, judgement) in plan {
        let outcome =
            act(git, reporter, branch, judgement, &current, remote, reset)?;
        summary.branches.push((branch.clone(), outcome));
    }

    let missing = summary.missing();
    if !missing.is_empty() {
        return Err(Error::IncompleteTrain {
            train: name,
            remote: remote.to_string(),
            missing,
        });
    }
    Ok(summary)
}

/// Where one branch stands against its upstream. Read-only: producing this
/// touches nothing, which is what lets the whole train be judged before the
/// first branch moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Judgement {
    /// Not here, but on the remote.
    Absent,
    /// On neither side.
    Missing,
    /// Here only.
    NotOnRemote,
    Level,
    Ahead(u32),
    Behind(u32),
    Diverged { ahead: u32, behind: u32 },
    Incomparable,
}

fn judge(git: &dyn GitRunner, branch: &str, remote: &str) -> Result<Judgement> {
    let on_remote = git.remote_branch_exists(remote, branch)?;
    if !git.branch_exists(branch)? {
        return Ok(if on_remote {
            Judgement::Absent
        } else {
            Judgement::Missing
        });
    }
    if !on_remote {
        return Ok(Judgement::NotOnRemote);
    }
    let upstream = format!("{remote}/{branch}");
    Ok(match git.ahead_behind(branch, &upstream)? {
        None => Judgement::Incomparable,
        Some((0, 0)) => Judgement::Level,
        Some((ahead, 0)) => Judgement::Ahead(ahead),
        Some((0, behind)) => Judgement::Behind(behind),
        Some((ahead, behind)) => Judgement::Diverged { ahead, behind },
    })
}

fn act(
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    branch: &str,
    judgement: Judgement,
    current: &str,
    remote: &str,
    reset: bool,
) -> Result<Outcome> {
    let upstream = format!("{remote}/{branch}");
    let label = format!("`{branch}`");
    match judgement {
        Judgement::Missing => {
            reporter.step_ok(
                &label,
                &format!("not on `{remote}` either — never pushed?"),
            );
            Ok(Outcome::Missing)
        }
        Judgement::NotOnRemote => {
            reporter.step_ok(
                &label,
                &format!("not on `{remote}` — nothing to pull"),
            );
            Ok(Outcome::NotOnRemote)
        }
        Judgement::Incomparable => {
            reporter.step_ok(
                &label,
                &format!("could not compare with `{upstream}` — left alone"),
            );
            Ok(Outcome::Incomparable)
        }
        Judgement::Level => {
            reporter.step_ok(&label, "up to date");
            Ok(Outcome::UpToDate)
        }
        Judgement::Ahead(ahead) => {
            // Local commits with no rewrite behind them. `--reset` stays
            // away from these on purpose: there is nothing here saying the
            // remote is the better version, and they may exist nowhere else.
            let detail = if reset {
                format!(
                    "{ahead} ahead of `{upstream}` — unpushed work, \
                     not reset"
                )
            } else {
                format!("{ahead} ahead of `{upstream}` — nothing to pull")
            };
            reporter.step_ok(&label, &detail);
            Ok(Outcome::Ahead { commits: ahead })
        }
        Judgement::Absent => {
            reporter.start(&format!("creating `{branch}` from `{upstream}`"));
            move_it(reporter, || git.create_tracking_branch(branch, remote), "")?;
            Ok(Outcome::Created)
        }
        Judgement::Behind(behind) => {
            reporter.start(&format!(
                "updating `{branch}` to `{upstream}` ({behind} commit(s))"
            ));
            // `git branch --force` can't move the branch you're standing
            // on; `git merge --ff-only` can, and only that one.
            let on_it = branch == current;
            move_it(
                reporter,
                || {
                    if on_it {
                        git.fast_forward_current(&upstream)
                    } else {
                        git.set_branch(branch, &upstream)
                    }
                },
                if on_it { "working tree updated" } else { "" },
            )?;
            Ok(Outcome::FastForwarded { commits: behind })
        }
        Judgement::Diverged { ahead, behind } => {
            if !reset {
                // The state of every branch in a train that was rebased on
                // either side. Fast-forwarding isn't possible and anything
                // else would throw away one side of the history.
                reporter.step_ok(
                    &label,
                    &format!(
                        "diverged from `{upstream}` ({ahead} ahead, \
                         {behind} behind) — left alone"
                    ),
                );
                return Ok(Outcome::Diverged { ahead, behind });
            }
            reporter.start(&format!(
                "resetting `{branch}` to `{upstream}` \
                 (discarding {ahead} local commit(s))"
            ));
            // The dirty-tree guard in `run` has already cleared this when
            // `branch` is the one checked out.
            let on_it = branch == current;
            move_it(
                reporter,
                || {
                    if on_it {
                        git.reset_hard_current(&upstream)
                    } else {
                        git.set_branch(branch, &upstream)
                    }
                },
                if on_it { "working tree updated" } else { "" },
            )?;
            Ok(Outcome::Reset { discarded: ahead })
        }
    }
}

/// Run a branch-moving step, reporting either side of it.
fn move_it(
    reporter: &mut dyn Reporter,
    op: impl FnOnce() -> Result<()>,
    detail: &str,
) -> Result<()> {
    match op() {
        Ok(()) => {
            reporter.ok(detail);
            Ok(())
        }
        Err(e) => {
            reporter.fail(&e.to_string());
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{PushMode, RebaseOutcome};
    use crate::report::{NullReporter, RecordingReporter};
    use crate::state::{Aggregate, StateFile, Train};
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use tempfile::TempDir;

    /// Branches are modelled as (ahead, behind) against their upstream, which
    /// is all `pull` ever asks about.
    struct FakeGit {
        local: RefCell<BTreeSet<String>>,
        remote: BTreeSet<String>,
        divergence: BTreeMap<String, (u32, u32)>,
        /// Branches whose comparison git refuses to answer.
        incomparable: BTreeSet<String>,
        current: String,
        fetched: RefCell<Vec<String>>,
        /// Branch moves, as (branch, target) — `set_branch` calls.
        moved: RefCell<Vec<(String, String)>>,
        /// `merge --ff-only` targets: moves that took the working tree along.
        merged: RefCell<Vec<String>>,
        /// `reset --hard` targets.
        reset_to: RefCell<Vec<String>>,
        checkouts: RefCell<Vec<String>>,
        /// Branches whose fast-forward fails (a dirty working tree, say).
        wedged: BTreeSet<String>,
        /// Tracked changes in the working tree.
        dirty: bool,
    }

    impl FakeGit {
        fn new(local: &[&str], remote: &[&str]) -> Self {
            Self {
                local: RefCell::new(local.iter().map(|s| s.to_string()).collect()),
                remote: remote.iter().map(|s| s.to_string()).collect(),
                divergence: BTreeMap::new(),
                incomparable: BTreeSet::new(),
                current: "main".into(),
                fetched: RefCell::new(Vec::new()),
                moved: RefCell::new(Vec::new()),
                merged: RefCell::new(Vec::new()),
                reset_to: RefCell::new(Vec::new()),
                checkouts: RefCell::new(Vec::new()),
                wedged: BTreeSet::new(),
                dirty: false,
            }
        }

        fn behind(mut self, branch: &str, n: u32) -> Self {
            self.divergence.insert(branch.into(), (0, n));
            self
        }

        fn ahead(mut self, branch: &str, n: u32) -> Self {
            self.divergence.insert(branch.into(), (n, 0));
            self
        }

        fn diverged(mut self, branch: &str, ahead: u32, behind: u32) -> Self {
            self.divergence.insert(branch.into(), (ahead, behind));
            self
        }

        fn on(mut self, branch: &str) -> Self {
            self.current = branch.into();
            self
        }
    }

    impl GitRunner for FakeGit {
        fn current_branch(&self) -> Result<String> {
            Ok(self.current.clone())
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
            unreachable!()
        }
        fn rebase_onto(
            &self,
            _b: &str,
            _o: &str,
            _u: &str,
        ) -> Result<RebaseOutcome> {
            unreachable!("pull never rewrites history")
        }
        fn rebase_abort(&self) -> Result<()> {
            Ok(())
        }
        fn set_branch(&self, b: &str, to: &str) -> Result<()> {
            assert_ne!(
                b, self.current,
                "must not `git branch -f` the checked-out branch"
            );
            if self.wedged.contains(b) {
                return Err(Error::Git {
                    code: 1,
                    stderr: "would clobber".into(),
                });
            }
            self.moved.borrow_mut().push((b.to_string(), to.to_string()));
            Ok(())
        }
        fn fast_forward_current(&self, to: &str) -> Result<()> {
            if self.wedged.contains(&self.current) {
                return Err(Error::Git {
                    code: 1,
                    stderr: "local changes would be overwritten by merge".into(),
                });
            }
            self.merged.borrow_mut().push(to.to_string());
            Ok(())
        }
        fn reset_hard_current(&self, to: &str) -> Result<()> {
            self.reset_to.borrow_mut().push(to.to_string());
            Ok(())
        }
        fn is_dirty(&self) -> Result<bool> {
            Ok(self.dirty)
        }
        fn push(&self, _b: &str, _m: PushMode, _r: &str) -> Result<()> {
            unreachable!("pull never pushes")
        }
        fn push_many(
            &self,
            _b: &[&str],
            _m: PushMode,
            _r: &str,
            _atomic: bool,
        ) -> Result<()> {
            unreachable!("pull never pushes")
        }
        fn fetch(&self, remote: &str) -> Result<()> {
            self.fetched.borrow_mut().push(remote.to_string());
            Ok(())
        }
        fn ahead_behind(&self, b: &str, _u: &str) -> Result<Option<(u32, u32)>> {
            if self.incomparable.contains(b) {
                return Ok(None);
            }
            Ok(Some(self.divergence.get(b).copied().unwrap_or((0, 0))))
        }
        fn remote_url(&self, _r: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn remote_branch_exists(&self, remote: &str, branch: &str) -> Result<bool> {
            Ok(self.remote.contains(&format!("{remote}/{branch}")))
        }
        fn create_tracking_branch(&self, branch: &str, remote: &str) -> Result<()> {
            assert!(self.remote.contains(&format!("{remote}/{branch}")));
            self.local.borrow_mut().insert(branch.to_string());
            Ok(())
        }
    }

    /// A two-branch train on `main`, as `choo fetch`'s tests set up.
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

    fn all_local() -> FakeGit {
        FakeGit::new(
            &["main", "a", "b"],
            &["origin/main", "origin/a", "origin/b"],
        )
    }

    fn outcome<'a>(s: &'a PullSummary, branch: &str) -> &'a Outcome {
        &s.branches
            .iter()
            .find(|(b, _)| b == branch)
            .unwrap_or_else(|| panic!("no outcome for `{branch}`"))
            .1
    }

    #[test]
    fn fetches_the_remote_once_before_looking_at_anything() {
        let (_tmp, store) = setup(None);
        let git = all_local();
        run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(*git.fetched.borrow(), vec!["origin"]);
    }

    #[test]
    fn a_branch_behind_the_remote_is_fast_forwarded() {
        let (_tmp, store) = setup(None);
        let git = all_local().behind("a", 3).on("b");
        let s = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(outcome(&s, "a"), &Outcome::FastForwarded { commits: 3 });
        assert_eq!(
            *git.moved.borrow(),
            vec![("a".to_string(), "origin/a".to_string())]
        );
        assert_eq!(s.updated(), 1);
    }

    /// The point of the command: `main` moved, so the train has something to
    /// be rebased onto.
    #[test]
    fn the_base_branch_is_updated_too() {
        let (_tmp, store) = setup(None);
        let git = all_local().behind("main", 7).on("a");
        let s = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(outcome(&s, "main"), &Outcome::FastForwarded { commits: 7 });
    }

    /// A stale branch you happen to be standing on has to move the working
    /// tree with it — the one case `git branch --force` can't handle.
    #[test]
    fn the_checked_out_branch_is_fast_forwarded_by_merge() {
        let (_tmp, store) = setup(None);
        let git = all_local().behind("main", 2).on("main");
        let s = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(outcome(&s, "main"), &Outcome::FastForwarded { commits: 2 });
        assert_eq!(*git.merged.borrow(), vec!["origin/main"]);
        assert!(git.moved.borrow().is_empty());
    }

    /// Every other branch is moved without leaving the branch you're on.
    #[test]
    fn never_checks_anything_out() {
        let (_tmp, store) = setup(None);
        let git = all_local().behind("a", 1).behind("b", 1).on("main");
        run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert!(git.checkouts.borrow().is_empty(), "pull must not move HEAD");
    }

    /// The safety property. A rebased train is ahead *and* behind on every
    /// branch, and pulling must not undo the rebase.
    #[test]
    fn a_diverged_branch_is_reported_never_moved() {
        let (_tmp, store) = setup(None);
        let git = all_local().diverged("a", 2, 5).on("main");
        let mut rep = RecordingReporter::new();
        let s = run(&store, &git, &mut rep, None, "origin", false).unwrap();
        assert_eq!(
            outcome(&s, "a"),
            &Outcome::Diverged {
                ahead: 2,
                behind: 5
            }
        );
        assert!(git.moved.borrow().is_empty(), "diverged branches must not move");
        assert!(git.merged.borrow().is_empty());
        assert_eq!(s.diverged(), vec!["a"]);
        assert!(rep.joined().contains("2 ahead, 5 behind"), "{}", rep.joined());
    }

    /// Unpushed work is not something to pull, and not something to warn
    /// about either.
    #[test]
    fn a_branch_ahead_of_the_remote_is_left_alone() {
        let (_tmp, store) = setup(None);
        let git = all_local().ahead("b", 4).on("main");
        let s = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(outcome(&s, "b"), &Outcome::Ahead { commits: 4 });
        assert!(git.moved.borrow().is_empty());
        assert!(s.diverged().is_empty());
    }

    #[test]
    fn an_up_to_date_train_moves_nothing() {
        let (_tmp, store) = setup(None);
        let git = all_local();
        let s = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(s.updated(), 0);
        assert_eq!(s.up_to_date(), 3);
        assert!(git.moved.borrow().is_empty());
    }

    /// Pull subsumes fetch: a branch that isn't here yet arrives.
    #[test]
    fn a_missing_branch_is_created_from_the_remote() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&["main"], &["origin/main", "origin/a", "origin/b"]);
        let s = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(outcome(&s, "a"), &Outcome::Created);
        assert_eq!(s.created(), 2);
    }

    #[test]
    fn a_branch_only_on_this_machine_is_reported_not_touched() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&["main", "a", "b"], &["origin/main", "origin/a"]);
        let s = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(outcome(&s, "b"), &Outcome::NotOnRemote);
        assert!(git.moved.borrow().is_empty());
    }

    #[test]
    fn a_branch_that_exists_nowhere_fails_the_train() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&["main"], &["origin/main", "origin/a"]);
        let err = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap_err();
        match err {
            Error::IncompleteTrain { train, missing, .. } => {
                assert_eq!(train, "t");
                assert_eq!(missing, vec!["b"]);
            }
            other => panic!("expected IncompleteTrain, got {other}"),
        }
        // ...but `a` still arrived.
        assert!(git.branch_exists("a").unwrap());
    }

    #[test]
    fn the_aggregate_branch_is_pulled_too() {
        let (_tmp, store) = setup(Some("choo/t/combined"));
        let git = FakeGit::new(
            &["main", "a", "b", "choo/t/combined"],
            &["origin/main", "origin/a", "origin/b", "origin/choo/t/combined"],
        )
        .behind("choo/t/combined", 1);
        let s = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(
            outcome(&s, "choo/t/combined"),
            &Outcome::FastForwarded { commits: 1 }
        );
    }

    /// A dirty working tree stops the fast-forward. Say so with git's own
    /// words rather than pressing on and losing the changes.
    #[test]
    fn a_fast_forward_that_git_refuses_surfaces_the_error() {
        let (_tmp, store) = setup(None);
        let mut git = all_local().behind("main", 1).on("main");
        git.wedged.insert("main".into());
        let err = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap_err();
        assert!(
            matches!(&err, Error::Git { stderr, .. } if stderr.contains("overwritten")),
            "got: {err}"
        );
    }

    /// Comparison failing is a "don't know", and a "don't know" never moves
    /// a branch.
    #[test]
    fn an_incomparable_branch_is_left_alone() {
        let (_tmp, store) = setup(None);
        let mut git = all_local();
        git.incomparable.insert("a".into());
        let s = run(&store, &git, &mut NullReporter, None, "origin", false).unwrap();
        assert_eq!(outcome(&s, "a"), &Outcome::Incomparable);
        assert!(git.moved.borrow().is_empty());
    }

    #[test]
    fn honours_a_non_default_remote() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(
            &["main", "a", "b"],
            &["upstream/main", "upstream/a", "upstream/b"],
        )
        .behind("a", 1)
        .on("main");
        let s = run(&store, &git, &mut NullReporter, None, "upstream", false).unwrap();
        assert_eq!(*git.fetched.borrow(), vec!["upstream"]);
        assert_eq!(
            *git.moved.borrow(),
            vec![("a".to_string(), "upstream/a".to_string())]
        );
        assert_eq!(s.updated(), 1);
    }

    // --- `--reset` --------------------------------------------------------

    /// The case the flag exists for: the other devbox rebased and
    /// force-pushed, so every branch here is diverged and every one of them
    /// should end up on the remote's version.
    #[test]
    fn reset_moves_diverged_branches_onto_the_remote() {
        let (_tmp, store) = setup(None);
        let git = all_local().diverged("a", 1, 2).diverged("b", 1, 2).on("main");
        let s = run(&store, &git, &mut NullReporter, None, "origin", true).unwrap();
        assert_eq!(outcome(&s, "a"), &Outcome::Reset { discarded: 1 });
        assert_eq!(outcome(&s, "b"), &Outcome::Reset { discarded: 1 });
        assert_eq!(
            *git.moved.borrow(),
            vec![
                ("a".to_string(), "origin/a".to_string()),
                ("b".to_string(), "origin/b".to_string()),
            ]
        );
        assert_eq!(s.reset(), vec![("a", 1), ("b", 1)]);
    }

    /// The branch you're standing on needs `reset --hard`; `git branch -f`
    /// can't touch it.
    #[test]
    fn the_checked_out_branch_is_reset_hard() {
        let (_tmp, store) = setup(None);
        let git = all_local().diverged("a", 3, 1).on("a");
        let s = run(&store, &git, &mut NullReporter, None, "origin", true).unwrap();
        assert_eq!(outcome(&s, "a"), &Outcome::Reset { discarded: 3 });
        assert_eq!(*git.reset_to.borrow(), vec!["origin/a"]);
        assert!(git.moved.borrow().is_empty());
    }

    /// The guard that keeps `--reset` from silently eating uncommitted work.
    #[test]
    fn reset_refuses_a_dirty_working_tree_on_the_branch_it_would_reset() {
        let (_tmp, store) = setup(None);
        let mut git = all_local().diverged("a", 1, 1).on("a");
        git.dirty = true;
        let err = run(&store, &git, &mut NullReporter, None, "origin", true)
            .unwrap_err();
        assert!(
            matches!(&err, Error::DirtyWorkingTree { branch } if branch == "a"),
            "got: {err}"
        );
    }

    /// ...and it refuses *before* moving anything, so a refusal never leaves
    /// the train half-reset.
    #[test]
    fn the_dirty_refusal_happens_before_any_branch_moves() {
        let (_tmp, store) = setup(None);
        // `main` is behind (would be fast-forwarded first) and the dirty
        // checked-out branch `b` is diverged, later in the train's order.
        let mut git = all_local().behind("main", 1).diverged("b", 1, 1).on("b");
        git.dirty = true;
        assert!(run(&store, &git, &mut NullReporter, None, "origin", true).is_err());
        assert!(
            git.moved.borrow().is_empty() && git.merged.borrow().is_empty(),
            "nothing may move before the refusal"
        );
    }

    /// A dirty tree is only a problem for the branch being reset — other
    /// branches move without going near the working tree.
    #[test]
    fn a_dirty_tree_elsewhere_does_not_block_the_reset() {
        let (_tmp, store) = setup(None);
        let mut git = all_local().diverged("a", 1, 1).on("main");
        git.dirty = true;
        let s = run(&store, &git, &mut NullReporter, None, "origin", true).unwrap();
        assert_eq!(outcome(&s, "a"), &Outcome::Reset { discarded: 1 });
    }

    /// The other half of the safety story: unpushed work has no rewrite
    /// behind it, so `--reset` leaves it be even though it's asked to be
    /// forceful.
    #[test]
    fn reset_does_not_touch_a_branch_that_is_only_ahead() {
        let (_tmp, store) = setup(None);
        let git = all_local().ahead("b", 2).on("main");
        let mut rep = RecordingReporter::new();
        let s = run(&store, &git, &mut rep, None, "origin", true).unwrap();
        assert_eq!(outcome(&s, "b"), &Outcome::Ahead { commits: 2 });
        assert!(git.moved.borrow().is_empty());
        assert!(git.reset_to.borrow().is_empty());
        assert_eq!(s.kept_ahead(), vec!["b"]);
        assert!(
            rep.joined().contains("unpushed work, not reset"),
            "should say why it skipped it: {}",
            rep.joined()
        );
    }

    /// `--reset` changes what happens to diverged branches and nothing else.
    #[test]
    fn reset_still_fast_forwards_and_creates_as_usual() {
        let (_tmp, store) = setup(None);
        let git = FakeGit::new(&["main", "a"], &["origin/main", "origin/a", "origin/b"])
            .behind("main", 1)
            .on("main");
        let s = run(&store, &git, &mut NullReporter, None, "origin", true).unwrap();
        assert_eq!(outcome(&s, "main"), &Outcome::FastForwarded { commits: 1 });
        assert_eq!(outcome(&s, "b"), &Outcome::Created);
        assert_eq!(s.reset(), vec![]);
    }

    #[test]
    fn unknown_train_errors() {
        let (_tmp, store) = setup(None);
        let git = all_local();
        assert!(matches!(
            run(&store, &git, &mut NullReporter, Some("ghost"), "origin", false),
            Err(Error::UnknownTrain(_))
        ));
    }
}
