//! Persistent state: the [`StateFile`] holds every [`Train`] in the repo,
//! plus pointers like the active train. Stored as JSON inside the repo's
//! `.git` directory so it's automatically excluded from the worktree but
//! lives alongside the repo it refers to.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Schema version. Bump when the on-disk format changes in an
/// incompatible way; older versions get rejected with a clear error.
pub const STATE_VERSION: u32 = 1;

/// Top-level on-disk structure stored at `.git/choochoo/state.json`.
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

/// Locate the repo root (directory containing `.git`) starting from `start`.
///
/// Walks parents until either `.git` is found or we hit the filesystem root.
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

/// Standard layout: state JSON inside `.git/choochoo/state.json`.
pub fn state_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".git").join("choochoo")
}

pub fn state_path(repo_root: &Path) -> PathBuf {
    state_dir(repo_root).join("state.json")
}

/// Load the state file from a repo root. If the file does not exist,
/// return a fresh empty state. Validation runs after deserialization.
pub fn load(repo_root: &Path) -> Result<StateFile> {
    let path = state_path(repo_root);
    if !path.exists() {
        return Ok(StateFile::default());
    }
    let bytes = fs::read(&path).map_err(|e| Error::Io {
        path: path.clone(),
        source: e,
    })?;
    let state: StateFile = serde_json::from_slice(&bytes).map_err(|e| {
        Error::CorruptState(format!("failed to parse {}: {e}", path.display()))
    })?;
    state.validate()?;
    Ok(state)
}

/// Persist the state file atomically (write to temp + rename).
pub fn save(repo_root: &Path, state: &StateFile) -> Result<()> {
    state.validate()?;
    let dir = state_dir(repo_root);
    fs::create_dir_all(&dir).map_err(|e| Error::Io {
        path: dir.clone(),
        source: e,
    })?;
    let final_path = state_path(repo_root);
    let tmp_path = final_path.with_extension("json.tmp");

    let mut json = serde_json::to_vec_pretty(state)?;
    json.push(b'\n');

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

    fs::rename(&tmp_path, &final_path).map_err(|e| Error::Io {
        path: final_path,
        source: e,
    })?;
    Ok(())
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
        let loaded = load(tmp.path()).unwrap();
        assert!(loaded.train("t").unwrap().aggregate.is_none());
        save(tmp.path(), &loaded).unwrap();
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
        save(tmp.path(), &state).unwrap();
        assert_eq!(load(tmp.path()).unwrap(), state);
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
        let s = load(tmp.path()).unwrap();
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

        save(tmp.path(), &state).unwrap();
        let loaded = load(tmp.path()).unwrap();
        assert_eq!(loaded, state);
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
        let err = load(tmp.path()).unwrap_err();
        assert!(matches!(err, Error::CorruptState(_)));
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
