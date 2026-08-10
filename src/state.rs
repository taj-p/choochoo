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
}

impl Train {
    pub fn new(name: impl Into<String>, base: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base: base.into(),
            branches: Vec::new(),
            prs: BTreeMap::new(),
        }
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
