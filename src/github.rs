//! GitHub operations behind a trait so they can be faked in tests.
//!
//! Production uses [`ProcessGhRunner`] which shells out to `gh`. Tests
//! get a JSON-backed [`FakeGh`]; choosing between them at runtime is a
//! responsibility of the binary entry point (it inspects the
//! `CHOOCHOO_GH_FAKE` env var and selects accordingly).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::state::PrInfo;

/// Snapshot of a PR's mutable state on GitHub. Returned by
/// [`GhRunner::get_pr`]; [`PrInfo`] is the trimmed-down version persisted
/// in the choochoo state file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSnapshot {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub body: String,
}

/// Abstraction over the `gh` operations choochoo needs.
pub trait GhRunner {
    /// Look up an existing PR by its head branch. Returns `None` if no
    /// PR exists for the branch.
    fn find_pr_for_branch(&self, head: &str) -> Result<Option<PrInfo>>;

    /// Fetch the current state of a PR (number, url, title, body). Used
    /// by `choo pr` to re-render the train table with up-to-date titles
    /// and to preserve user-authored content around the managed block.
    fn get_pr(&self, number: u64) -> Result<PrSnapshot>;

    /// Create a PR. Returns the new PR's metadata.
    fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<PrInfo>;

    /// Replace a PR's body with new markdown.
    fn update_pr_body(&self, number: u64, body: &str) -> Result<()>;

    /// Update a PR's base branch (used after reordering trains).
    fn update_pr_base(&self, number: u64, base: &str) -> Result<()>;
}

/// Choose a runner based on the `CHOOCHOO_GH_FAKE` env var:
/// when present, the value is interpreted as a path to a JSON file used by
/// [`FakeGh`]. This is what the binary uses in tests; in production it's
/// always [`ProcessGhRunner`].
pub fn make_runner() -> Result<Box<dyn GhRunner>> {
    if let Ok(path) = std::env::var("CHOOCHOO_GH_FAKE") {
        Ok(Box::new(FakeGh::open(PathBuf::from(path))?))
    } else {
        Ok(Box::new(ProcessGhRunner::new()?))
    }
}

// ---------------------------------------------------------------------------
// Process implementation
// ---------------------------------------------------------------------------

pub struct ProcessGhRunner {
    gh_bin: PathBuf,
}

impl ProcessGhRunner {
    pub fn new() -> Result<Self> {
        let gh_bin = which::which("gh").map_err(|_| Error::MissingTool("gh"))?;
        Ok(Self { gh_bin })
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.gh_bin);
        c.env("LC_ALL", "C");
        c
    }

    fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.cmd().args(args).output().map_err(|e| Error::Io {
            path: self.gh_bin.clone(),
            source: e,
        })?;
        if !output.status.success() {
            return Err(Error::Gh {
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[derive(Deserialize)]
struct PrFullJson {
    number: u64,
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct PrListItem {
    number: u64,
    url: String,
    #[serde(default)]
    title: String,
}

impl GhRunner for ProcessGhRunner {
    fn get_pr(&self, number: u64) -> Result<PrSnapshot> {
        let stdout = self.run([
            "pr",
            "view",
            &number.to_string(),
            "--json",
            "number,url,title,body",
        ])?;
        let parsed: PrFullJson = serde_json::from_str(stdout.trim()).map_err(|e| {
            Error::ParseOutput {
                cmd: "gh pr view --json number,url,title,body",
                reason: format!("{e}; got `{}`", stdout.trim()),
            }
        })?;
        Ok(PrSnapshot {
            number: parsed.number,
            url: parsed.url,
            title: parsed.title,
            body: parsed.body,
        })
    }

    fn find_pr_for_branch(&self, head: &str) -> Result<Option<PrInfo>> {
        // `gh pr list -H <branch>` lists open PRs with that head. We pick
        // the first if any. Title is included so the train table can
        // render it without an extra `gh pr view` round-trip.
        let stdout = self.run([
            "pr",
            "list",
            "--head",
            head,
            "--state",
            "open",
            "--json",
            "number,url,title",
            "--limit",
            "1",
        ])?;
        let items: Vec<PrListItem> =
            serde_json::from_str(stdout.trim()).map_err(|e| Error::ParseOutput {
                cmd: "gh pr list",
                reason: format!("expected JSON list: {e}; got `{}`", stdout.trim()),
            })?;
        Ok(items.into_iter().next().map(|it| PrInfo {
            number: it.number,
            url: it.url,
            title: if it.title.is_empty() { None } else { Some(it.title) },
            last_pushed_sha: None,
        }))
    }

    fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<PrInfo> {
        let mut args: Vec<String> = vec![
            "pr".into(),
            "create".into(),
            "--head".into(),
            head.into(),
            "--base".into(),
            base.into(),
            "--title".into(),
            title.into(),
            "--body".into(),
            body.into(),
        ];
        if draft {
            args.push("--draft".into());
        }
        let stdout = self.run(args.iter().map(String::as_str))?;
        // `gh pr create` prints the PR URL on the last line of stdout.
        let url = stdout
            .lines()
            .rfind(|l| l.starts_with("http"))
            .ok_or_else(|| Error::ParseOutput {
                cmd: "gh pr create",
                reason: format!("no URL in output: `{}`", stdout.trim()),
            })?
            .trim()
            .to_string();
        // Now look up the number + title via `gh pr view`.
        let view = self.run([
            "pr",
            "view",
            &url,
            "--json",
            "number,url,title",
        ])?;
        let pr: PrFullJson = serde_json::from_str(view.trim()).map_err(|e| {
            Error::ParseOutput {
                cmd: "gh pr view",
                reason: format!("{e}; got `{}`", view.trim()),
            }
        })?;
        Ok(PrInfo {
            number: pr.number,
            url: pr.url,
            title: if pr.title.is_empty() { None } else { Some(pr.title) },
            last_pushed_sha: None,
        })
    }

    fn update_pr_body(&self, number: u64, body: &str) -> Result<()> {
        self.run([
            "pr",
            "edit",
            &number.to_string(),
            "--body",
            body,
        ])?;
        Ok(())
    }

    fn update_pr_base(&self, number: u64, base: &str) -> Result<()> {
        self.run([
            "pr",
            "edit",
            &number.to_string(),
            "--base",
            base,
        ])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fake implementation (used by integration tests)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct FakeGhState {
    #[serde(default)]
    pub next_number: u64,
    /// Map keyed by head branch name (most natural for our access pattern).
    #[serde(default)]
    pub prs: BTreeMap<String, FakePr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakePr {
    pub number: u64,
    pub url: String,
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
}

/// JSON-file-backed fake implementation of [`GhRunner`]. Multiple instances
/// can point at the same file (e.g., across `choo` invocations within a
/// single integration test) and they'll see each other's writes.
pub struct FakeGh {
    path: PathBuf,
    inner: Mutex<()>,
}

impl FakeGh {
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        if !path.exists() {
            std::fs::write(&path, "{}\n").map_err(|e| Error::Io {
                path: path.clone(),
                source: e,
            })?;
        }
        Ok(Self {
            path,
            inner: Mutex::new(()),
        })
    }

    fn load(&self) -> Result<FakeGhState> {
        let bytes = std::fs::read(&self.path).map_err(|e| Error::Io {
            path: self.path.clone(),
            source: e,
        })?;
        if bytes.is_empty() || bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(FakeGhState::default());
        }
        // empty `{}` parses fine into default-everything via serde defaults
        let s: FakeGhState = serde_json::from_slice(&bytes).map_err(|e| Error::ParseOutput {
            cmd: "fake-gh",
            reason: format!("{e}"),
        })?;
        Ok(s)
    }

    fn save(&self, s: &FakeGhState) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(s)?;
        bytes.push(b'\n');
        std::fs::write(&self.path, bytes).map_err(|e| Error::Io {
            path: self.path.clone(),
            source: e,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Test-only helper: simulate a user editing a PR's title in the
    /// GitHub UI. Not exposed on [`GhRunner`] because the production
    /// code path should never write titles (`gh pr edit --title ...`
    /// is something the user does interactively).
    pub fn set_pr_title(&self, number: u64, title: &str) -> Result<()> {
        let _g = self.inner.lock().unwrap();
        let mut s = self.load()?;
        let pr = s
            .prs
            .values_mut()
            .find(|p| p.number == number)
            .ok_or_else(|| {
                Error::InvalidArgument(format!("fake-gh: no PR with number {number}"))
            })?;
        pr.title = title.to_string();
        self.save(&s)
    }
}

impl GhRunner for FakeGh {
    fn get_pr(&self, number: u64) -> Result<PrSnapshot> {
        let _g = self.inner.lock().unwrap();
        let s = self.load()?;
        s.prs
            .values()
            .find(|p| p.number == number)
            .map(|p| PrSnapshot {
                number: p.number,
                url: p.url.clone(),
                title: p.title.clone(),
                body: p.body.clone(),
            })
            .ok_or_else(|| {
                Error::InvalidArgument(format!("fake-gh: no PR with number {number}"))
            })
    }

    fn find_pr_for_branch(&self, head: &str) -> Result<Option<PrInfo>> {
        let _g = self.inner.lock().unwrap();
        let s = self.load()?;
        Ok(s.prs.get(head).map(|p| PrInfo {
            number: p.number,
            url: p.url.clone(),
            title: if p.title.is_empty() {
                None
            } else {
                Some(p.title.clone())
            },
            last_pushed_sha: None,
        }))
    }

    fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<PrInfo> {
        let _g = self.inner.lock().unwrap();
        let mut s = self.load()?;
        if s.prs.contains_key(head) {
            return Err(Error::InvalidArgument(format!(
                "fake-gh: PR already exists for branch `{head}`"
            )));
        }
        if s.next_number == 0 {
            s.next_number = 1;
        }
        let number = s.next_number;
        s.next_number += 1;
        let url = format!("https://example.test/owner/repo/pull/{number}");
        s.prs.insert(
            head.to_string(),
            FakePr {
                number,
                url: url.clone(),
                head: head.to_string(),
                base: base.to_string(),
                title: title.to_string(),
                body: body.to_string(),
                draft,
            },
        );
        self.save(&s)?;
        Ok(PrInfo {
            number,
            url,
            title: if title.is_empty() {
                None
            } else {
                Some(title.to_string())
            },
            last_pushed_sha: None,
        })
    }

    fn update_pr_body(&self, number: u64, body: &str) -> Result<()> {
        let _g = self.inner.lock().unwrap();
        let mut s = self.load()?;
        let pr = s
            .prs
            .values_mut()
            .find(|p| p.number == number)
            .ok_or_else(|| {
                Error::InvalidArgument(format!("fake-gh: no PR with number {number}"))
            })?;
        pr.body = body.to_string();
        self.save(&s)
    }

    fn update_pr_base(&self, number: u64, base: &str) -> Result<()> {
        let _g = self.inner.lock().unwrap();
        let mut s = self.load()?;
        let pr = s
            .prs
            .values_mut()
            .find(|p| p.number == number)
            .ok_or_else(|| {
                Error::InvalidArgument(format!("fake-gh: no PR with number {number}"))
            })?;
        pr.base = base.to_string();
        self.save(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fake_gh_create_then_find() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gh.json");
        let gh = FakeGh::open(path).unwrap();
        assert!(gh.find_pr_for_branch("foo").unwrap().is_none());

        let pr = gh.create_pr("foo", "main", "T", "B", false).unwrap();
        assert_eq!(pr.number, 1);
        assert_eq!(pr.title.as_deref(), Some("T"));

        let again = gh.find_pr_for_branch("foo").unwrap().unwrap();
        assert_eq!(again.number, 1);
        assert_eq!(again.url, pr.url);
        assert_eq!(again.title.as_deref(), Some("T"));
    }

    #[test]
    fn fake_gh_get_pr_returns_full_snapshot() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gh.json");
        let gh = FakeGh::open(path).unwrap();
        let pr = gh.create_pr("foo", "main", "Initial", "body", false).unwrap();
        let snap = gh.get_pr(pr.number).unwrap();
        assert_eq!(snap.title, "Initial");
        assert_eq!(snap.body, "body");
    }

    #[test]
    fn fake_gh_set_pr_title_simulates_rename() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gh.json");
        let gh = FakeGh::open(path).unwrap();
        let pr = gh.create_pr("foo", "main", "Old", "B", false).unwrap();
        gh.set_pr_title(pr.number, "New").unwrap();
        assert_eq!(gh.get_pr(pr.number).unwrap().title, "New");
    }

    #[test]
    fn fake_gh_persists_across_instances() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gh.json");
        {
            let gh = FakeGh::open(path.clone()).unwrap();
            gh.create_pr("foo", "main", "T", "B", false).unwrap();
        }
        let gh = FakeGh::open(path).unwrap();
        assert!(gh.find_pr_for_branch("foo").unwrap().is_some());
    }

    #[test]
    fn fake_gh_update_body_changes_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gh.json");
        let gh = FakeGh::open(path).unwrap();
        let pr = gh.create_pr("foo", "main", "T", "old", false).unwrap();
        gh.update_pr_body(pr.number, "new").unwrap();
        let s = gh.load().unwrap();
        assert_eq!(s.prs.get("foo").unwrap().body, "new");
    }

    #[test]
    fn fake_gh_update_base_changes_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gh.json");
        let gh = FakeGh::open(path).unwrap();
        let pr = gh.create_pr("foo", "main", "T", "B", false).unwrap();
        gh.update_pr_base(pr.number, "develop").unwrap();
        let s = gh.load().unwrap();
        assert_eq!(s.prs.get("foo").unwrap().base, "develop");
    }

    #[test]
    fn fake_gh_create_duplicate_fails() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gh.json");
        let gh = FakeGh::open(path).unwrap();
        gh.create_pr("foo", "main", "T", "B", false).unwrap();
        assert!(gh.create_pr("foo", "main", "T", "B", false).is_err());
    }

    #[test]
    fn fake_gh_assigns_increasing_numbers() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gh.json");
        let gh = FakeGh::open(path).unwrap();
        let p1 = gh.create_pr("a", "main", "T", "B", false).unwrap();
        let p2 = gh.create_pr("b", "a", "T", "B", false).unwrap();
        assert_eq!((p1.number, p2.number), (1, 2));
    }
}
