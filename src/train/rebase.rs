//! `choo rebase` — restack every branch in a train onto its parent.
//!
//! ## Algorithm
//!
//! For a train `base, b1, b2, ..., bn`:
//!
//! 1. **Snapshot** the tip SHA of every branch (the base + every train
//!    branch) into a [`RebaseProgress`] file at
//!    `.git/choochoo/rebase-progress.json`.
//! 2. For each pair `(parent, child)` in order, run
//!    `git rebase --onto <current parent tip> <boundary> <child>`. The
//!    current parent tip is read live, since the parent may have just been
//!    rebased by the previous iteration.
//! 3. If a rebase exits with conflicts, write the current pair index back
//!    to the progress file, return [`Error::RebaseConflict`], and let the
//!    user resolve the conflicts and run `choo rebase --continue` to pick
//!    up where we left off.
//! 4. On success of the final pair, delete the progress file.
//!
//! ## Picking the boundary
//!
//! `<boundary>` is the `--upstream` argument, and it decides which commits get
//! replayed. Getting it wrong is how a restack corrupts a stack, so there are
//! two sources, in order of preference:
//!
//! - [`BoundarySource::Recorded`] — the child's persisted true base
//!   ([`crate::state::Train::branch_bases`]), the commit its own commits sit
//!   directly on. Exact by construction, and the only option that survives a
//!   mid-stack history rewrite. Used only when [`trusted_bases`] confirms it's
//!   still an ancestor of the child.
//! - [`BoundarySource::Snapshot`] — the parent's tip from step 1. This is only
//!   a *proxy* for the boundary: it's the right commit as long as the child is
//!   still parented on it, which stops being true the moment someone amends or
//!   rebases the parent mid-stack. Kept as the fallback because it's what
//!   trains built by an older choochoo have, and it's correct in the common
//!   case.
//!
//! After each successful pair the `--onto` that was used is buffered into the
//! progress file and flushed to the train at the end, becoming the child's
//! recorded base for next time.
//!
//! Pure helpers (`build_plan`) are unit-tested in this module; orchestration is
//! exercised end-to-end by integration tests using a real temp git repo.

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
    /// True bases this run has established so far: child branch -> the
    /// `--onto` SHA it was replayed onto.
    ///
    /// Buffered here rather than written straight to the train because in
    /// shared mode every state save is a lock, fetch, merge, commit and
    /// network push — one per pair would turn a ten-branch restack into ten
    /// pushes. This file is machine-local and rewritten after every pair
    /// anyway, so buffering gives per-step durability for free, and makes
    /// `--abort` drop the lot by simply deleting the file.
    #[serde(default)]
    pub recorded_bases: BTreeMap<String, String>,
    /// The child branch that stopped on a conflict, and the `--onto` SHA it
    /// was being replayed onto. Promoted to a real recorded base by
    /// [`continue_run`], but only once the branch actually sits on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingBase>,
}

/// A recorded base we can't commit to yet: the rebase that would justify it
/// stopped on a conflict the user hasn't finished resolving.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingBase {
    pub branch: String,
    pub onto: String,
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

/// Where a [`PairStep::boundary`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundarySource {
    /// The branch's persisted true base, verified to still be an ancestor of
    /// it — so the replay range is exactly the branch's own commits.
    Recorded,
    /// No usable recorded base, so the parent's pre-rebase tip: what choochoo
    /// has always used. Correct unless history was rewritten mid-stack.
    Snapshot,
}

/// One unit of work in a rebase plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairStep {
    pub parent: String,
    pub child: String,
    /// The commit the replay range starts *after*: `<upstream>` in
    /// `git rebase --onto <live parent tip> <boundary> <child>`.
    pub boundary: String,
    pub boundary_source: BoundarySource,
}

/// Pure helper: build the ordered plan from a train, a snapshot map, and the
/// already-vetted recorded bases (child branch -> base SHA).
///
/// Prefers a recorded base, since it's an exact description of the child's own
/// commits; falls back to the parent's snapshot tip otherwise. Only the
/// fallback needs a snapshot entry, so a missing one stops being fatal for a
/// child whose base is recorded.
pub fn build_plan(
    train: &Train,
    snapshot: &BTreeMap<String, String>,
    trusted: &BTreeMap<String, String>,
) -> Result<Vec<PairStep>> {
    let mut plan = Vec::with_capacity(train.branches.len());
    for (parent, child) in train.pairs() {
        let (boundary, boundary_source) = match trusted.get(child) {
            Some(base) => (base.clone(), BoundarySource::Recorded),
            None => {
                let snap = snapshot.get(parent).ok_or_else(|| {
                    Error::CorruptState(format!(
                        "rebase snapshot missing tip for `{parent}`"
                    ))
                })?;
                (snap.clone(), BoundarySource::Snapshot)
            }
        };
        plan.push(PairStep {
            parent: parent.to_string(),
            child: child.to_string(),
            boundary,
            boundary_source,
        });
    }
    Ok(plan)
}

/// Filter the train's recorded bases down to the ones we're willing to act on:
/// still an ancestor of the branch they describe.
///
/// That single check covers both ways an entry goes bad. A base that's no
/// longer an ancestor means the branch was rewritten out from under it; a base
/// git can't resolve at all — garbage-collected, or synced from a machine whose
/// commits this one has never fetched — also answers "no". Either way the pair
/// falls back to snapshot semantics rather than failing.
fn trusted_bases(git: &dyn GitRunner, train: &Train) -> Result<BTreeMap<String, String>> {
    let mut trusted = BTreeMap::new();
    for branch in &train.branches {
        if let Some(base) = train.branch_base(branch) {
            if git.is_ancestor(base, branch)? {
                trusted.insert(branch.clone(), base.to_string());
            }
        }
    }
    Ok(trusted)
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
        recorded_bases: BTreeMap::new(),
        pending: None,
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
    let mut progress = load_progress(store.repo_root())?.ok_or_else(|| {
        Error::InvalidArgument("no rebase in progress".into())
    })?;

    // Promote the parked base — but only if the branch really does sit on the
    // commit we replayed it onto. It does after `git rebase --continue`, and
    // after `git rebase --skip` (the branch ends up *at* that commit, and
    // "zero own commits on top of it" is a true statement). It does not if the
    // user aborted the git rebase, or hasn't finished it, and recording it
    // then would be a lie that survives into future restacks.
    if let Some(p) = progress.pending.take() {
        if git.is_ancestor(&p.onto, &p.branch)? {
            progress.recorded_bases.insert(p.branch, p.onto);
        } else {
            reporter.info(&format!(
                "note: `{branch}` does not sit on the commit it was being \
                 replayed onto; not recording its base",
                branch = p.branch,
            ));
        }
    }

    // Advance past the conflicted pair (assumed resolved).
    progress.next_pair += 1;
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
    let trusted = trusted_bases(git, &train)?;
    let plan = build_plan(&train, &progress.snapshot, &trusted)?;
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

        // A recorded boundary that disagrees with the parent's pre-rebase tip
        // is precisely the signal that the parent was rewritten since the last
        // restack. Note this must be emitted *before* `reporter.start`:
        // `Reporter::info` flushes a pending start as "interrupted", so an
        // info line between `start` and `ok` mangles the step's output.
        if step.boundary_source == BoundarySource::Recorded
            && progress.snapshot.get(&step.parent) != Some(&step.boundary)
        {
            reporter.info(&format!(
                "note: `{parent}` was rewritten since the last restack; \
                 replaying only `{child}`'s own commits",
                parent = step.parent,
                child = step.child,
            ));
        }

        reporter.start(&format!(
            "rebasing `{child}` onto `{parent}` ({n}/{total})",
            child = step.child,
            parent = step.parent,
            n = progress.next_pair + 1,
        ));
        match git.rebase_onto(&step.child, &new_parent_tip, &step.boundary)? {
            RebaseOutcome::Ok { new_sha: _ } => {
                reporter.ok("");
                summary.rebased.push(step.child.clone());
                // `child`'s own commits now sit directly on the `--onto` we
                // just used: that is its true base, by definition of the
                // operation. Buffered, not saved — see `recorded_bases`.
                progress
                    .recorded_bases
                    .insert(step.child.clone(), new_parent_tip.clone());
                progress.next_pair += 1;
                save_progress(store.repo_root(), &progress)?;
            }
            RebaseOutcome::Conflict { stderr: _ } => {
                reporter.fail("conflict");
                // The child hasn't moved, so we can't claim this as its base
                // yet — park it for `continue_run` to verify and promote.
                progress.pending = Some(PendingBase {
                    branch: step.child.clone(),
                    onto: new_parent_tip.clone(),
                });
                save_progress(store.repo_root(), &progress)?;
                // Flush what *did* land. `git rebase --abort` only unwinds the
                // currently conflicted rebase, so every pair already completed
                // stays moved and its new base is a fact about the repo.
                flush_bases(store, state, reporter, &progress);
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

    flush_bases(store, state, reporter, &progress);
    clear_progress(store.repo_root())?;
    Ok(summary)
}

/// Copy the true bases this run established into the train and persist them.
///
/// Deliberately infallible from the caller's point of view: by the time this
/// runs the git work has already happened, so failing the whole command over a
/// state write would report failure for a restack that succeeded. A lost record
/// is self-correcting anyway — the stale entry is the child's *pre-rebase*
/// parent tip, which is no longer an ancestor of the rewritten child, so
/// [`trusted_bases`] rejects it and that pair falls back to snapshot
/// semantics: exactly the behaviour before this existed.
fn flush_bases(
    store: &Store,
    state: &mut StateFile,
    reporter: &mut dyn Reporter,
    progress: &RebaseProgress,
) {
    if progress.recorded_bases.is_empty() {
        return;
    }
    let result = state
        .train_mut(&progress.train)
        .map(|train| {
            for (branch, base) in &progress.recorded_bases {
                train.set_branch_base(branch, base.clone());
            }
        })
        .and_then(|()| store.save(state));
    if let Err(e) = result {
        reporter.info(&format!(
            "warning: restacked, but could not record branch bases: {e}"
        ));
    }
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

    fn snapshot_of(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn build_plan_walks_pairs_with_snapshot_tips() {
        let t = train(&["a", "b", "c"]);
        let snap = snapshot_of(&[("main", "M"), ("a", "A"), ("b", "B"), ("c", "C")]);
        let plan = build_plan(&t, &snap, &BTreeMap::new()).unwrap();
        assert_eq!(
            plan,
            vec![
                PairStep {
                    parent: "main".into(),
                    child: "a".into(),
                    boundary: "M".into(),
                    boundary_source: BoundarySource::Snapshot,
                },
                PairStep {
                    parent: "a".into(),
                    child: "b".into(),
                    boundary: "A".into(),
                    boundary_source: BoundarySource::Snapshot,
                },
                PairStep {
                    parent: "b".into(),
                    child: "c".into(),
                    boundary: "B".into(),
                    boundary_source: BoundarySource::Snapshot,
                },
            ]
        );
    }

    #[test]
    fn build_plan_prefers_a_trusted_recorded_base() {
        let t = train(&["a", "b"]);
        let snap = snapshot_of(&[("main", "M"), ("a", "A-REWRITTEN")]);
        let trusted = snapshot_of(&[("b", "A")]);
        let plan = build_plan(&t, &snap, &trusted).unwrap();
        // `a` has no recorded base, so it falls back; `b` uses its own.
        assert_eq!(plan[0].boundary, "M");
        assert_eq!(plan[0].boundary_source, BoundarySource::Snapshot);
        assert_eq!(plan[1].boundary, "A");
        assert_eq!(plan[1].boundary_source, BoundarySource::Recorded);
    }

    #[test]
    fn build_plan_errors_on_missing_snapshot() {
        let t = train(&["a"]);
        let snap = BTreeMap::new(); // no entries
        assert!(build_plan(&t, &snap, &BTreeMap::new()).is_err());
    }

    /// A recorded base makes the parent's snapshot entry irrelevant, so a gap
    /// in the snapshot stops being fatal for that pair.
    #[test]
    fn build_plan_tolerates_a_missing_snapshot_when_the_base_is_recorded() {
        let t = train(&["a"]);
        let trusted = snapshot_of(&[("a", "M")]);
        let plan = build_plan(&t, &BTreeMap::new(), &trusted).unwrap();
        assert_eq!(plan[0].boundary, "M");
    }

    /// In-memory fake GitRunner for orchestration tests.
    ///
    /// A commit is modelled as a `+`-joined **ordered component path** — `"M"`,
    /// `"M+a"`, `"M+a+b"` — so the fake has a real (linear) history rather than
    /// opaque tokens. That earns two things the old token model couldn't give:
    ///
    /// - `is_ancestor` is a genuine component-wise prefix test, so a rewritten
    ///   parent (`"M+a"` vs `"M+a2"`) is correctly *not* an ancestor. String
    ///   `starts_with` would get that wrong, and that case is the whole point.
    /// - `rebase_onto` honours `upstream` — it replays
    ///   `components(branch) \ components(upstream)` onto `onto` — so a wrong
    ///   boundary produces a visibly wrong *tip*, not just a different
    ///   argument. Tests can assert content, not only the call.
    ///
    /// `force_conflict_on`, if set, makes the next rebase of that branch
    /// conflict.
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

        /// Resolve a rev to its component path. A rev that isn't a branch is
        /// already a commit path, as a raw SHA would be with real git.
        fn components(&self, rev: &str) -> Vec<String> {
            let resolved = self
                .tips
                .borrow()
                .get(rev)
                .cloned()
                .unwrap_or_else(|| rev.to_string());
            resolved.split('+').map(str::to_string).collect()
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
        fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
            let a = self.components(ancestor);
            let d = self.components(descendant);
            // Component-wise prefix, deliberately not `str::starts_with`:
            // "M+a" is a string prefix of "M+a2" but must not be its ancestor.
            Ok(d.len() >= a.len() && d[..a.len()] == a[..])
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
            // `git rebase --onto <onto> <upstream> <branch>`: replay the
            // commits in `upstream..branch` on top of `onto`.
            let excluded = self.components(upstream);
            let replayed: Vec<String> = self
                .components(branch)
                .into_iter()
                .filter(|c| !excluded.contains(c))
                .collect();
            let mut path = self.components(onto);
            path.extend(replayed);
            let new_sha = path.join("+");
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
        fn push_many(
            &self,
            _b: &[&str],
            _m: crate::git::PushMode,
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

    /// Train `a -> b -> c` stacked on `main`, with a coherent history:
    /// `main` is at `M` and each branch adds one commit named after itself.
    /// No recorded bases, so this fixture exercises the snapshot fallback —
    /// i.e. exactly the behaviour that predates `branch_bases`.
    fn fake_repo() -> (TempDir, Store, FakeGit, StateFile) {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".git/choochoo")).unwrap();
        let store = Store::local(tmp.path());
        let git = FakeGit::new(&[
            ("main", "M"),
            ("a", "M+a"),
            ("b", "M+a+b"),
            ("c", "M+a+b+c"),
        ]);
        let mut state = StateFile::default();
        let mut t = Train::new("t", "main");
        t.branches = vec!["a".into(), "b".into(), "c".into()];
        state.trains.insert("t".into(), t);
        state.active = Some("t".into());
        store.save(&state).unwrap();
        (tmp, store, git, state)
    }

    /// Record true bases on the fixture train, as `choo add` would have.
    fn set_bases(store: &Store, bases: &[(&str, &str)]) {
        let mut state = store.load().unwrap();
        let t = state.train_mut("t").unwrap();
        for (branch, base) in bases {
            t.set_branch_base(branch, (*base).to_string());
        }
        store.save(&state).unwrap();
    }

    /// Advance `main` by one commit, so a restack has something to do.
    fn advance_main(git: &FakeGit) {
        git.tips.borrow_mut().insert("main".into(), "M+z".into());
    }

    #[test]
    fn happy_path_rebases_all_branches() {
        let (_tmp, store, git, _) = fake_repo();
        advance_main(&git);
        let summary = run(&store, &git, &mut NullReporter, None).unwrap();
        assert_eq!(summary.rebased, vec!["a", "b", "c"]);
        // With no recorded bases, every boundary is the parent's snapshot tip.
        let calls = git.rebase_calls.borrow().clone();
        assert_eq!(
            calls,
            vec![
                ("a".into(), "M+z".into(), "M+z".into()),
                // Parent `a` is now at M+z+a (post-rebase).
                ("b".into(), "M+z+a".into(), "M+a".into()),
                ("c".into(), "M+z+a+b".into(), "M+a+b".into()),
            ]
        );
        // Each branch kept exactly its own commit, on top of the new main.
        assert_eq!(git.rev_parse("c").unwrap(), "M+z+a+b+c");
        assert!(!progress_path(store.repo_root()).exists());
    }

    /// The regression test for the bug this whole mechanism exists to fix.
    ///
    /// `a`'s commit is amended, so `b` is still parented on the orphaned
    /// pre-amend commit. The snapshot boundary would be `a`'s *post*-amend tip
    /// — not an ancestor of `b` — which widens `b`'s replay range to include
    /// the orphaned commit. The recorded base is the pre-amend commit, which is
    /// still an ancestor, so the range stays exactly `b`'s own work.
    #[test]
    fn mid_stack_amend_uses_the_recorded_base_not_the_snapshot_tip() {
        let (_tmp, store, git, _) = fake_repo();
        set_bases(&store, &[("a", "M"), ("b", "M+a"), ("c", "M+a+b")]);
        // Amend `a`: its commit is replaced, not appended to.
        git.tips.borrow_mut().insert("a".into(), "M+a2".into());

        run(&store, &git, &mut NullReporter, None).unwrap();

        let calls = git.rebase_calls.borrow().clone();
        // The single assertion that separates fixed from broken: `b`'s upstream
        // is the recorded pre-amend base `M+a`, not the snapshot tip `M+a2`.
        assert_eq!(
            calls,
            vec![
                ("a".into(), "M".into(), "M".into()),
                ("b".into(), "M+a2".into(), "M+a".into()),
                ("c".into(), "M+a2+b".into(), "M+a+b".into()),
            ]
        );
        // And the tips prove the content is right rather than merely different:
        // the orphaned `a` component is gone, not duplicated as "M+a2+a+b".
        assert_eq!(git.rev_parse("b").unwrap(), "M+a2+b");
        assert_eq!(git.rev_parse("c").unwrap(), "M+a2+b+c");
    }

    #[test]
    fn mid_stack_amend_is_reported() {
        let (_tmp, store, git, _) = fake_repo();
        set_bases(&store, &[("a", "M"), ("b", "M+a"), ("c", "M+a+b")]);
        git.tips.borrow_mut().insert("a".into(), "M+a2".into());
        let mut rep = RecordingReporter::new();
        run(&store, &git, &mut rep, None).unwrap();
        let joined = rep.joined();
        assert!(
            joined.contains("`a` was rewritten"),
            "expected a rewrite note, got: {joined}"
        );
        // Only the rewritten parent is called out — `b` was not rewritten.
        assert!(
            !joined.contains("`b` was rewritten"),
            "unexpected note for `b`: {joined}"
        );
    }

    /// A recorded base that no longer describes the branch must be ignored.
    /// This is the cross-machine / garbage-collected case: `is_ancestor` says
    /// no, so the pair falls back instead of feeding git a bad boundary.
    #[test]
    fn recorded_base_that_is_not_an_ancestor_is_ignored() {
        let (_tmp, store, git, _) = fake_repo();
        advance_main(&git);
        set_bases(&store, &[("b", "X+y")]); // unrelated history
        run(&store, &git, &mut NullReporter, None).unwrap();
        let calls = git.rebase_calls.borrow().clone();
        // `b` used the snapshot tip, exactly as if nothing were recorded.
        assert_eq!(calls[1], ("b".into(), "M+z+a".into(), "M+a".into()));
        assert_eq!(git.rev_parse("c").unwrap(), "M+z+a+b+c");
    }

    #[test]
    fn successful_pairs_record_their_onto_as_the_new_base() {
        let (_tmp, store, git, _) = fake_repo();
        advance_main(&git);
        run(&store, &git, &mut NullReporter, None).unwrap();
        let state = store.load().unwrap();
        let t = state.train("t").unwrap();
        assert_eq!(t.branch_base("a"), Some("M+z"));
        assert_eq!(t.branch_base("b"), Some("M+z+a"));
        assert_eq!(t.branch_base("c"), Some("M+z+a+b"));
    }

    /// Recording is idempotent across runs: a second restack with nothing to
    /// do must leave every branch alone and every base unchanged.
    #[test]
    fn a_second_restack_is_a_no_op() {
        let (_tmp, store, git, _) = fake_repo();
        advance_main(&git);
        run(&store, &git, &mut NullReporter, None).unwrap();
        let tips_before = git.tips.borrow().clone();
        let bases_before = store.load().unwrap().train("t").unwrap().branch_bases.clone();

        run(&store, &git, &mut NullReporter, None).unwrap();

        assert_eq!(*git.tips.borrow(), tips_before);
        assert_eq!(
            store.load().unwrap().train("t").unwrap().branch_bases,
            bases_before
        );
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

    /// A conflict can't claim the child's new base yet — the child hasn't
    /// moved. It parks the hypothesis, and `--continue` promotes it once the
    /// branch really does sit there.
    #[test]
    fn conflict_parks_a_pending_base_and_continue_promotes_it() {
        let (_tmp, store, git, _) = fake_repo();
        advance_main(&git);
        *git.force_conflict_on.borrow_mut() = Some("b".into());
        let _ = run(&store, &git, &mut NullReporter, None).unwrap_err();

        let prog = load_progress(store.repo_root()).unwrap().unwrap();
        assert_eq!(
            prog.pending,
            Some(PendingBase {
                branch: "b".into(),
                onto: "M+z+a".into(),
            })
        );
        // Nothing recorded for `b` yet, but `a` landed and was recorded.
        let state = store.load().unwrap();
        assert_eq!(state.train("t").unwrap().branch_base("a"), Some("M+z"));
        assert_eq!(state.train("t").unwrap().branch_base("b"), None);

        // Model `git rebase --continue`: `b` now sits on the `--onto`.
        *git.force_conflict_on.borrow_mut() = None;
        git.tips.borrow_mut().insert("b".into(), "M+z+a+b".into());
        continue_run(&store, &git, &mut NullReporter).unwrap();

        let state = store.load().unwrap();
        assert_eq!(state.train("t").unwrap().branch_base("b"), Some("M+z+a"));
    }

    /// If the user aborts the git rebase instead of finishing it, the parked
    /// base is a lie. Recording it would poison every future restack, so the
    /// guard must reject it.
    #[test]
    fn continue_after_a_git_abort_does_not_record_a_false_base() {
        let (_tmp, store, git, _) = fake_repo();
        advance_main(&git);
        *git.force_conflict_on.borrow_mut() = Some("b".into());
        let _ = run(&store, &git, &mut NullReporter, None).unwrap_err();

        // Model `git rebase --abort`: `b` is left where it started, still
        // parented on `a`'s *pre*-restack tip.
        *git.force_conflict_on.borrow_mut() = None;
        assert_eq!(git.rev_parse("b").unwrap(), "M+a+b");

        let mut rep = RecordingReporter::new();
        continue_run(&store, &git, &mut rep).unwrap();

        assert_eq!(
            store.load().unwrap().train("t").unwrap().branch_base("b"),
            None,
            "a base must not be recorded for a branch that never moved"
        );
        assert!(
            rep.joined().contains("not recording its base"),
            "expected a warning, got: {}",
            rep.joined()
        );
    }

    /// `git rebase --abort` only unwinds the *conflicted* rebase; pairs that
    /// already completed stay moved. Their bases are facts, so they must be
    /// persisted even though the run as a whole failed.
    #[test]
    fn conflict_still_records_the_pairs_that_landed() {
        let (_tmp, store, git, _) = fake_repo();
        advance_main(&git);
        *git.force_conflict_on.borrow_mut() = Some("c".into());
        let _ = run(&store, &git, &mut NullReporter, None).unwrap_err();

        let state = store.load().unwrap();
        let t = state.train("t").unwrap();
        assert_eq!(t.branch_base("a"), Some("M+z"));
        assert_eq!(t.branch_base("b"), Some("M+z+a"));
        assert_eq!(t.branch_base("c"), None);
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
