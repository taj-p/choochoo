//! Persistent state: the [`StateFile`] holds every [`Train`] in the repo,
//! plus pointers like the active train.
//!
//! All access goes through a [`Store`], which owns the question of *where*
//! state lives. [`Store::local`] is the original layout — one JSON file at
//! `.git/choochoo/state.json`, inside `.git` so it's excluded from the
//! worktree but travels with the repo it describes.
//!
//! Callers are handed a `Store` rather than resolving one themselves. That
//! keeps the choice explicit at the edges (`cli::run` builds it once) and
//! keeps tests hermetic: a test that constructs `Store::local(tmpdir)`
//! cannot accidentally reach a real remote or the developer's config.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Schema version. Bump when the on-disk format changes in an
/// incompatible way; older versions get rejected with a clear error.
///
/// v2 split the single `state.json` into a *shared* half (the trains) and a
/// *machine-local* half (the active-train pointer), so the trains can live
/// in a repo shared between devboxes while `active` stays per-machine.
/// v1 files are migrated on first load.
///
/// The bump is deliberate even though v2's shared half is nearly a subset
/// of v1: an older `choo` on another devbox then fails loudly with
/// "expected state version 1, found 2" rather than reading a file it only
/// half understands.
pub const STATE_VERSION: u32 = 2;

/// The complete in-memory view of choochoo's state: every train, plus the
/// pointers that say which one you're working on.
///
/// This is what all of `train::*`, `render`, and the TUI operate on. It is
/// assembled from two persisted halves — see [`StateFile::split`] and
/// [`StateFile::join`] — but nothing above the [`Store`] needs to know that.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateFile {
    pub version: u32,
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub trains: BTreeMap<String, Train>,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            active: None,
            trains: BTreeMap::new(),
        }
    }
}

/// The shared half: the trains themselves. This is the document that
/// travels between machines, whether it sits in `.git/choochoo/state.json`
/// (local mode) or in the store repo (shared mode).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedState {
    pub version: u32,
    #[serde(default)]
    pub trains: BTreeMap<String, Train>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            trains: BTreeMap::new(),
        }
    }
}

/// The machine-local half, always at `.git/choochoo/local.json`.
///
/// `active` lives here on purpose: which train *you* are working on right
/// now is a property of the machine you're sitting at. Sharing it would
/// mean two devboxes fighting over the pointer every time you switched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalState {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
}

impl Default for LocalState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            active: None,
        }
    }
}

/// A v1 `state.json`, where both halves lived in one document. Read only
/// during migration.
#[derive(Debug, Deserialize)]
struct LegacyStateFile {
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    trains: BTreeMap<String, Train>,
}

/// Just enough of any state document to find out which schema it is.
#[derive(Debug, Deserialize)]
struct VersionProbe {
    #[serde(default)]
    version: u32,
}

impl StateFile {
    /// Divide the in-memory state into the two documents that get persisted.
    pub fn split(&self) -> (SharedState, LocalState) {
        (
            SharedState {
                version: STATE_VERSION,
                trains: self.trains.clone(),
            },
            LocalState {
                version: STATE_VERSION,
                active: self.active.clone(),
            },
        )
    }

    /// Reassemble the halves.
    ///
    /// Returns the dropped active-train name when `local` points at a train
    /// that is no longer in `shared` — which happens legitimately once state
    /// is shared: another devbox can remove a train this box was sitting on.
    /// That has to self-heal rather than fail, because [`StateFile::validate`]
    /// (correctly) rejects a dangling `active`, and a remote action must not
    /// be able to brick a local checkout.
    pub fn join(shared: SharedState, local: LocalState) -> (Self, Option<String>) {
        let mut active = local.active;
        let mut dropped = None;
        if let Some(name) = &active {
            if !shared.trains.contains_key(name) {
                dropped = active.take();
            }
        }
        (
            Self {
                version: STATE_VERSION,
                active,
                trains: shared.trains,
            },
            dropped,
        )
    }
}

/// A train: an ordered list of branches stacked on top of `base`.
///
/// `branches[0]`'s parent is `base`; `branches[i]`'s parent is `branches[i - 1]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Train {
    pub name: String,
    pub base: String,
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub prs: BTreeMap<String, PrInfo>,
    /// Optional aggregate ("combined") branch: a branch choochoo keeps
    /// pointing at the train's tip, with its own draft PR against `base`,
    /// so reviewers can see every change in the train at once. Absent
    /// unless the user enabled it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<Aggregate>,
}

/// The train's aggregate branch and its draft PR.
///
/// The branch is *derived* state: choochoo force-updates it to the tip of
/// the train (the last branch) whenever the train is rebased, pushed, or
/// explicitly synced, so its diff against `base` is exactly the union of
/// every change in the train.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Aggregate {
    /// Branch name choochoo owns and keeps in sync with the train tip.
    pub branch: String,
    /// The aggregate branch's PR (always opened as a draft), once created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrInfo>,
}

impl Aggregate {
    pub fn new(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
            pr: None,
        }
    }
}

/// Default aggregate branch name for a train: `choo/<train>/combined`,
/// with characters git won't accept in a ref replaced by `-`.
pub fn default_aggregate_branch(train_name: &str) -> String {
    format!("choo/{}/combined", sanitize_ref_component(train_name))
}

/// Make `s` safe to embed in a git ref path component: git rejects
/// whitespace, `~^:?*[\`, and `..` sequences, and dislikes leading dots.
fn sanitize_ref_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let bad = ch.is_whitespace()
            || ch.is_control()
            || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | ']' | '\\' | '/');
        out.push(if bad { '-' } else { ch });
    }
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    let trimmed = out.trim_matches('.').trim_matches('-').to_string();
    if trimmed.is_empty() {
        "train".to_string()
    } else {
        trimmed
    }
}

impl Train {
    pub fn new(name: impl Into<String>, base: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base: base.into(),
            branches: Vec::new(),
            prs: BTreeMap::new(),
            aggregate: None,
        }
    }

    /// The aggregate branch name, if the aggregate branch is enabled.
    pub fn aggregate_branch(&self) -> Option<&str> {
        self.aggregate.as_ref().map(|a| a.branch.as_str())
    }

    /// The last branch in the train — the one whose content the aggregate
    /// branch mirrors. [`None`] for an empty train.
    pub fn tip(&self) -> Option<&str> {
        self.branches.last().map(String::as_str)
    }

    /// True when `branch` is this train's aggregate branch.
    pub fn is_aggregate(&self, branch: &str) -> bool {
        self.aggregate_branch() == Some(branch)
    }

    /// Returns `(parent, branch)` pairs for every branch in the train,
    /// where `parent` is the prior branch (or `base` for the first one).
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        let base = self.base.as_str();
        let branches = &self.branches;
        (0..branches.len()).map(move |i| {
            let parent = if i == 0 {
                base
            } else {
                branches[i - 1].as_str()
            };
            (parent, branches[i].as_str())
        })
    }

    /// Position of `branch` in the train, or [`None`] if absent.
    pub fn position(&self, branch: &str) -> Option<usize> {
        self.branches.iter().position(|b| b == branch)
    }
}

/// Stored PR metadata for a branch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
    /// PR title as last seen on GitHub. Used to render the train table's
    /// Title column without re-fetching for every render. May be absent on
    /// state files written by older choochoo versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Last SHA we successfully pushed for this branch, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pushed_sha: Option<String>,
}

impl StateFile {
    /// Validate the cross-cutting invariants. Called on load and before
    /// every save to catch programmer bugs early.
    pub fn validate(&self) -> Result<()> {
        if self.version != STATE_VERSION {
            return Err(Error::CorruptState(format!(
                "expected state version {STATE_VERSION}, found {}",
                self.version
            )));
        }
        if let Some(active) = &self.active {
            if !self.trains.contains_key(active) {
                return Err(Error::CorruptState(format!(
                    "active train `{active}` is not present in trains map"
                )));
            }
        }
        for (key, train) in &self.trains {
            if key != &train.name {
                return Err(Error::CorruptState(format!(
                    "train key `{key}` does not match train.name `{}`",
                    train.name
                )));
            }
            train.validate()?;
        }
        Ok(())
    }

    /// Get a train by name, returning [`Error::UnknownTrain`] if missing.
    pub fn train(&self, name: &str) -> Result<&Train> {
        self.trains
            .get(name)
            .ok_or_else(|| Error::UnknownTrain(name.to_string()))
    }

    pub fn train_mut(&mut self, name: &str) -> Result<&mut Train> {
        self.trains
            .get_mut(name)
            .ok_or_else(|| Error::UnknownTrain(name.to_string()))
    }

    /// Resolve `--train` argument: explicit name wins, otherwise active.
    pub fn resolve_train_name<'a>(&'a self, requested: Option<&'a str>) -> Result<&'a str> {
        if let Some(name) = requested {
            if !self.trains.contains_key(name) {
                return Err(Error::UnknownTrain(name.to_string()));
            }
            Ok(name)
        } else {
            self.active.as_deref().ok_or(Error::NoActiveTrain)
        }
    }
}

impl Train {
    pub fn validate(&self) -> Result<()> {
        if self.base.trim().is_empty() {
            return Err(Error::CorruptState(format!(
                "train `{}` has empty base branch",
                self.name
            )));
        }
        let mut seen = std::collections::HashSet::with_capacity(self.branches.len());
        for branch in &self.branches {
            if branch == &self.base {
                return Err(Error::CorruptState(format!(
                    "train `{}` contains its own base `{}`",
                    self.name, self.base
                )));
            }
            if !seen.insert(branch.as_str()) {
                return Err(Error::CorruptState(format!(
                    "train `{}` contains duplicate branch `{}`",
                    self.name, branch
                )));
            }
        }
        for branch in self.prs.keys() {
            if !seen.contains(branch.as_str()) {
                return Err(Error::CorruptState(format!(
                    "train `{}` has PR metadata for unknown branch `{}`",
                    self.name, branch
                )));
            }
        }
        if let Some(agg) = &self.aggregate {
            if agg.branch.trim().is_empty() {
                return Err(Error::CorruptState(format!(
                    "train `{}` has an empty aggregate branch name",
                    self.name
                )));
            }
            if agg.branch == self.base {
                return Err(Error::CorruptState(format!(
                    "train `{}` uses its base `{}` as the aggregate branch",
                    self.name, self.base
                )));
            }
            if seen.contains(agg.branch.as_str()) {
                return Err(Error::CorruptState(format!(
                    "train `{}` aggregate branch `{}` is also a train branch",
                    self.name, agg.branch
                )));
            }
        }
        Ok(())
    }
}

/// Locate the repo root (the directory holding `.git`) starting from `start`.
///
/// Walks parents until either `.git` is found or we hit the filesystem root.
/// In a linked worktree `.git` is a *file* rather than a directory; that
/// still marks the root of a checkout, so it counts. Everything that needs
/// the git directory itself must go through [`state_dir`], which knows the
/// difference.
pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut cur = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()?.join(start)
    };
    loop {
        if cur.join(".git").exists() {
            return Ok(cur);
        }
        if !cur.pop() {
            return Err(Error::NotInRepo);
        }
    }
}

/// Standard layout: choochoo's files inside `<git-common-dir>/choochoo`.
///
/// That is `.git/choochoo` in an ordinary checkout. In a linked worktree
/// `<root>/.git` is a file pointing at `<main>/.git/worktrees/<name>`, so
/// hanging a directory off it would name a path that can never exist —
/// which is how `choo list` came to report "no trains" from every
/// worktree. Git's *common* directory is shared by every worktree of a
/// repo, so all of them see one set of trains, the same way they already
/// see one set of branches.
pub fn state_dir(repo_root: &Path) -> PathBuf {
    git_dir(repo_root).join("choochoo")
}

/// The common git directory for `repo_root`.
///
/// A `.git` directory answers this by itself, and that is the case for
/// nearly every invocation — worth not spawning a process for. Only the
/// `.git`-as-a-file case (linked worktree, submodule) needs git to resolve
/// the indirection, and if git can't answer we fall back to the plain
/// layout rather than failing the command.
fn git_dir(repo_root: &Path) -> PathBuf {
    let plain = repo_root.join(".git");
    if plain.is_dir() {
        return plain;
    }
    crate::git::common_dir(repo_root).unwrap_or(plain)
}

/// The shared half in local mode, and the v1 file we migrate from.
pub fn state_path(repo_root: &Path) -> PathBuf {
    state_dir(repo_root).join("state.json")
}

/// The machine-local half. Never shared, never leaves this checkout.
pub fn local_path(repo_root: &Path) -> PathBuf {
    state_dir(repo_root).join("local.json")
}

/// Write `value` as pretty JSON to `path`, atomically: serialize to a
/// sibling `.tmp` file, `fsync`, then rename over the target. A reader
/// therefore only ever sees a complete document, and a crash mid-write
/// leaves the previous version intact.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| Error::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
    }

    let mut json = serde_json::to_vec_pretty(value)?;
    json.push(b'\n');

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = path.with_file_name(tmp_name);

    {
        let mut f = fs::File::create(&tmp_path).map_err(|e| Error::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
        f.write_all(&json).map_err(|e| Error::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
        f.sync_all().map_err(|e| Error::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
    }

    fs::rename(&tmp_path, path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Read a JSON document, mapping a parse failure to [`Error::CorruptState`]
/// so the user gets the offending path rather than a bare serde message.
/// Returns [`None`] when the file doesn't exist.
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let value = serde_json::from_slice(&bytes).map_err(|e| {
        Error::CorruptState(format!("failed to parse {}: {e}", path.display()))
    })?;
    Ok(Some(value))
}

/// Where a repo's choochoo state is read from and written to.
///
/// Construct one per process at the edge (`cli::run`, `tui::run`) and pass
/// it down; the `train::*` operations take `&Store` in place of a repo root,
/// since a store knows its own repo root.
///
/// The machine-local half always lives in `.git/choochoo/local.json`. Only
/// the *shared* half moves: local mode keeps it in
/// `.git/choochoo/state.json`, shared mode keeps it in a git repo — see
/// [`crate::store`].
pub struct Store {
    repo_root: PathBuf,
    backend: Backend,
}

enum Backend {
    /// Shared half in `.git/choochoo/state.json`. The original layout, and
    /// what you get when no config names somewhere else.
    Local,
    /// Shared half in a clone of the user's state repo. Boxed because it is
    /// much larger than `Local`, and there is exactly one `Store` per
    /// process, so the indirection costs nothing.
    Shared(Box<crate::store::GitStore>),
}

impl Store {
    /// State stays inside this repo. Touches nothing outside `repo_root`,
    /// reads no config, and makes no network access — which is what makes
    /// it the right constructor for tests.
    pub fn local(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            backend: Backend::Local,
        }
    }

    /// The shared half lives in `git_store`.
    pub fn shared(
        repo_root: impl Into<PathBuf>,
        git_store: crate::store::GitStore,
    ) -> Self {
        Self {
            repo_root: repo_root.into(),
            backend: Backend::Shared(Box::new(git_store)),
        }
    }

    /// The working repo this store describes. Callers need it for git
    /// operations and for machine-local scratch files such as
    /// `rebase-progress.json`.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// True when the trains are shared with other machines.
    pub fn is_shared(&self) -> bool {
        matches!(self.backend, Backend::Shared(_))
    }

    /// Take the degradations accumulated while syncing — an unreachable
    /// store, an unpublished commit, a merged conflict.
    ///
    /// These are surfaced by the caller rather than printed here, because
    /// the TUI owns the alternate screen: anything written straight to
    /// stderr would corrupt its display.
    pub fn take_warnings(&self) -> Vec<String> {
        match &self.backend {
            Backend::Local => Vec::new(),
            Backend::Shared(gs) => gs.take_warnings(),
        }
    }

    /// Record a warning to be surfaced with the rest.
    pub(crate) fn warn(&self, msg: impl Into<String>) {
        if let Backend::Shared(gs) = &self.backend {
            gs.push_warning(msg.into());
        }
    }

    /// Publish anything pending and pull the latest. Used by `choo sync`.
    pub fn sync_now(&self) -> Result<()> {
        match &self.backend {
            Backend::Local => Ok(()),
            Backend::Shared(gs) => gs.sync_now(),
        }
    }

    /// True when a change is saved locally but hasn't reached the remote.
    pub fn has_unpublished(&self) -> bool {
        match &self.backend {
            Backend::Local => false,
            Backend::Shared(gs) => gs.has_pending(),
        }
    }

    /// A short human-readable description of where state lives, for
    /// `choo sync status` and for error messages.
    pub fn describe(&self) -> String {
        match &self.backend {
            Backend::Local => state_path(&self.repo_root).display().to_string(),
            Backend::Shared(gs) => gs.describe(),
        }
    }

    /// Read the state. A store that has never been written to yields a
    /// fresh empty [`StateFile`] rather than an error. Validation runs
    /// after the two halves are joined.
    pub fn load(&self) -> Result<StateFile> {
        let (state, _dropped) = self.load_reporting_drift()?;
        Ok(state)
    }

    /// [`Store::load`], additionally reporting an `active` pointer that had
    /// to be dropped because the train is gone from the shared half.
    pub fn load_reporting_drift(&self) -> Result<(StateFile, Option<String>)> {
        let shared = match &self.backend {
            Backend::Local => read_shared(&state_path(&self.repo_root))?,
            Backend::Shared(gs) => gs.read_shared()?,
        };
        let local = self.read_local()?;
        let (state, dropped) = StateFile::join(shared, local);
        state.validate()?;
        Ok((state, dropped))
    }

    /// Persist the state. Validated first, so a programmer bug can't write
    /// state that would fail to load.
    ///
    /// The machine-local half is written **first**: if publishing the shared
    /// half then fails, `active` has still been recorded, and a stale
    /// pointer self-heals on the next load. The reverse order could lose it.
    ///
    /// `what` describes the change for the store repo's commit message.
    pub fn save_described(&self, state: &StateFile, what: &str) -> Result<()> {
        state.validate()?;
        let (shared, local) = state.split();
        write_json_atomic(&local_path(&self.repo_root), &local)?;
        match &self.backend {
            Backend::Local => {
                write_json_atomic(&state_path(&self.repo_root), &shared)
            }
            Backend::Shared(gs) => gs.write_shared(&shared, what),
        }
    }

    /// [`Store::save_described`] with a generic commit message.
    pub fn save(&self, state: &StateFile) -> Result<()> {
        self.save_described(state, "update trains")
    }

    fn read_local(&self) -> Result<LocalState> {
        let path = local_path(&self.repo_root);
        if let Some(local) = read_versioned::<LocalState>(&path)? {
            return Ok(local);
        }
        // No `local.json` yet. Either this is a fresh repo, or a v1
        // `state.json` still holds the `active` pointer we need to lift out.
        let legacy = state_path(&self.repo_root);
        if let Some(v) = read_json::<VersionProbe>(&legacy)? {
            if v.version < STATE_VERSION {
                let old: LegacyStateFile = read_json(&legacy)?.ok_or_else(|| {
                    Error::CorruptState(format!(
                        "{} vanished while being read",
                        legacy.display()
                    ))
                })?;
                return Ok(LocalState {
                    version: STATE_VERSION,
                    active: old.active,
                });
            }
        }
        Ok(LocalState::default())
    }
}

/// Read a shared-state document, migrating a v1 `state.json` in place (in
/// memory — the file is only rewritten on the next save).
fn read_shared(path: &Path) -> Result<SharedState> {
    let Some(probe) = read_json::<VersionProbe>(path)? else {
        return Ok(SharedState::default());
    };
    if probe.version > STATE_VERSION {
        return Err(Error::CorruptState(format!(
            "{} has state version {}, but this choochoo understands up to \
             {STATE_VERSION}; upgrade choochoo",
            path.display(),
            probe.version
        )));
    }
    if probe.version < STATE_VERSION {
        let old: LegacyStateFile = read_json(path)?.ok_or_else(|| {
            Error::CorruptState(format!("{} vanished while being read", path.display()))
        })?;
        return Ok(SharedState {
            version: STATE_VERSION,
            trains: old.trains,
        });
    }
    read_versioned(path).map(Option::unwrap_or_default)
}

/// Read a current-version document, rejecting anything from the future.
fn read_versioned<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    let Some(probe) = read_json::<VersionProbe>(path)? else {
        return Ok(None);
    };
    if probe.version != STATE_VERSION {
        return Err(Error::CorruptState(format!(
            "expected state version {STATE_VERSION} in {}, found {}",
            path.display(),
            probe.version
        )));
    }
    read_json(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_train(name: &str, base: &str, branches: &[&str]) -> Train {
        let mut t = Train::new(name, base);
        t.branches = branches.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn default_state_validates() {
        StateFile::default().validate().unwrap();
    }

    #[test]
    fn pairs_walks_base_then_each_predecessor() {
        let train = make_train("t", "main", &["a", "b", "c"]);
        let collected: Vec<(&str, &str)> = train.pairs().collect();
        assert_eq!(
            collected,
            vec![("main", "a"), ("a", "b"), ("b", "c")]
        );
    }

    #[test]
    fn duplicate_branch_fails_validation() {
        let train = make_train("t", "main", &["a", "a"]);
        assert!(train.validate().is_err());
    }

    #[test]
    fn base_in_branches_fails_validation() {
        let train = make_train("t", "main", &["main"]);
        assert!(train.validate().is_err());
    }

    #[test]
    fn pr_for_unknown_branch_fails_validation() {
        let mut train = make_train("t", "main", &["a"]);
        train.prs.insert(
            "ghost".into(),
            PrInfo {
                number: 1,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        assert!(train.validate().is_err());
    }

    #[test]
    fn aggregate_branch_equal_to_base_fails_validation() {
        let mut train = make_train("t", "main", &["a"]);
        train.aggregate = Some(Aggregate::new("main"));
        assert!(train.validate().is_err());
    }

    #[test]
    fn aggregate_branch_also_in_train_fails_validation() {
        let mut train = make_train("t", "main", &["a", "b"]);
        train.aggregate = Some(Aggregate::new("b"));
        assert!(train.validate().is_err());
    }

    #[test]
    fn aggregate_branch_alongside_train_validates() {
        let mut train = make_train("t", "main", &["a", "b"]);
        train.aggregate = Some(Aggregate::new("choo/t/combined"));
        train.validate().unwrap();
        assert_eq!(train.aggregate_branch(), Some("choo/t/combined"));
        assert_eq!(train.tip(), Some("b"));
        assert!(train.is_aggregate("choo/t/combined"));
        assert!(!train.is_aggregate("b"));
    }

    #[test]
    fn default_aggregate_branch_is_ref_safe() {
        assert_eq!(default_aggregate_branch("feat"), "choo/feat/combined");
        // Whitespace and ref-hostile characters are replaced.
        assert_eq!(
            default_aggregate_branch("my feat~1"),
            "choo/my-feat-1/combined"
        );
        assert_eq!(default_aggregate_branch("a..b"), "choo/a.b/combined");
        assert_eq!(default_aggregate_branch("  "), "choo/train/combined");
    }

    /// State files written before the aggregate feature existed must still
    /// load (the field is absent), and must not gain the key on rewrite.
    #[test]
    fn state_without_aggregate_field_round_trips() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".git/choochoo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("state.json"),
            r#"{"version":1,"active":"t","trains":{"t":{"name":"t","base":"main","branches":["a"],"prs":{}}}}"#,
        )
        .unwrap();
        let store = Store::local(tmp.path());
        let loaded = store.load().unwrap();
        assert!(loaded.train("t").unwrap().aggregate.is_none());
        store.save(&loaded).unwrap();
        let text = fs::read_to_string(dir.join("state.json")).unwrap();
        assert!(!text.contains("aggregate"), "got: {text}");
    }

    #[test]
    fn aggregate_survives_save_load_with_pr() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let mut state = StateFile::default();
        let mut train = make_train("feat", "main", &["a"]);
        train.aggregate = Some(Aggregate {
            branch: "choo/feat/combined".into(),
            pr: Some(PrInfo {
                number: 99,
                url: "https://example/pr/99".into(),
                title: Some("Combined: feat".into()),
                last_pushed_sha: None,
            }),
        });
        state.trains.insert("feat".into(), train);
        state.active = Some("feat".into());
        let store = Store::local(tmp.path());
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
    }

    #[test]
    fn active_must_be_a_known_train() {
        let s = StateFile {
            active: Some("nope".into()),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn key_must_match_train_name() {
        let mut s = StateFile::default();
        s.trains
            .insert("mismatch".into(), make_train("real", "main", &[]));
        assert!(s.validate().is_err());
    }

    #[test]
    fn resolve_train_name_prefers_explicit() {
        let mut s = StateFile::default();
        s.trains.insert("foo".into(), make_train("foo", "main", &[]));
        s.trains.insert("bar".into(), make_train("bar", "main", &[]));
        s.active = Some("foo".into());
        assert_eq!(s.resolve_train_name(Some("bar")).unwrap(), "bar");
        assert_eq!(s.resolve_train_name(None).unwrap(), "foo");
    }

    #[test]
    fn resolve_train_name_errors_when_no_active() {
        let s = StateFile::default();
        assert!(matches!(
            s.resolve_train_name(None),
            Err(Error::NoActiveTrain)
        ));
    }

    #[test]
    fn resolve_train_name_unknown() {
        let s = StateFile::default();
        assert!(matches!(
            s.resolve_train_name(Some("nope")),
            Err(Error::UnknownTrain(_))
        ));
    }

    #[test]
    fn load_returns_default_when_file_absent() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let s = Store::local(tmp.path()).load().unwrap();
        assert_eq!(s, StateFile::default());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();

        let mut state = StateFile::default();
        let mut train = make_train("feat", "main", &["a", "b"]);
        train.prs.insert(
            "a".into(),
            PrInfo {
                number: 42,
                url: "https://example/pr/42".into(),
                title: None,
                last_pushed_sha: Some("abc".into()),
            },
        );
        state.trains.insert("feat".into(), train);
        state.active = Some("feat".into());

        let store = Store::local(tmp.path());
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
    }

    /// A half-written state file must never be observable: `save` writes to
    /// a sibling temp file and renames, so the temp name is gone afterwards
    /// and the real file parses.
    #[test]
    fn save_leaves_no_temp_file_behind() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let store = Store::local(tmp.path());
        store.save(&StateFile::default()).unwrap();
        let dir = tmp.path().join(".git/choochoo");
        assert!(dir.join("state.json").exists());
        assert!(!dir.join("state.json.tmp").exists());
    }

    #[test]
    fn load_rejects_wrong_version() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".git/choochoo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("state.json"),
            r#"{"version": 999, "active": null, "trains": {}}"#,
        )
        .unwrap();
        let err = Store::local(tmp.path()).load().unwrap_err();
        assert!(matches!(err, Error::CorruptState(_)));
    }

    #[test]
    fn load_reports_the_path_of_unparseable_state() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".git/choochoo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("state.json"), "{ not json").unwrap();
        let err = Store::local(tmp.path()).load().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("state.json"), "got: {msg}");
    }

    #[test]
    fn find_repo_root_walks_up() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let nested = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let root = find_repo_root(&nested).unwrap();
        assert_eq!(
            fs::canonicalize(root).unwrap(),
            fs::canonicalize(tmp.path()).unwrap()
        );
    }

    #[test]
    fn find_repo_root_errors_outside_repo() {
        let tmp = TempDir::new().unwrap();
        let err = find_repo_root(tmp.path()).unwrap_err();
        assert!(matches!(err, Error::NotInRepo));
    }
}
