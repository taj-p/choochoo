//! Shared fixtures for end-to-end CLI integration tests.
//!
//! [`TestRepo`] creates a real git repository under a [`tempfile::TempDir`]
//! with stable identity so `git commit` doesn't fail on the test runner.
//! It exposes helpers for committing files, creating branches, and running
//! the `choo` binary inside the repo with `CHOOCHOO_GH_FAKE` set so the
//! `pr` subcommand records calls against a JSON file we can read back.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

/// One self-contained git repo for a test, plus a path to the FakeGh JSON.
pub struct TestRepo {
    pub dir: TempDir,
    pub fake_gh: PathBuf,
}

impl TestRepo {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("create tempdir");
        let path = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "--initial-branch=main"]);
        run_git(&path, &["config", "user.email", "test@example.invalid"]);
        run_git(&path, &["config", "user.name", "Test User"]);
        run_git(&path, &["config", "commit.gpgsign", "false"]);
        // First commit on main so HEAD is valid.
        std::fs::write(path.join("README.md"), "# test repo\n").unwrap();
        run_git(&path, &["add", "README.md"]);
        run_git(&path, &["commit", "-q", "-m", "init"]);

        let fake_gh = path.join(".git/choochoo/gh.json");
        Self { dir, fake_gh }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Add a commit on the current branch with `name` as both filename and
    /// commit subject.
    pub fn commit(&self, name: &str) {
        let p = self.path();
        std::fs::write(p.join(name), format!("{name}\n")).unwrap();
        run_git(p, &["add", name]);
        run_git(p, &["commit", "-q", "-m", &format!("add {name}")]);
    }

    /// Create branch `name` from `from` and switch to it.
    pub fn branch(&self, name: &str, from: &str) {
        run_git(self.path(), &["checkout", "-q", "-b", name, from]);
    }

    pub fn checkout(&self, name: &str) {
        run_git(self.path(), &["checkout", "-q", name]);
    }

    pub fn current_branch(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(self.path())
            .output()
            .expect("git rev-parse");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    pub fn rev_parse(&self, rev: &str) -> String {
        let out = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(self.path())
            .output()
            .expect("git rev-parse");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Build a `choo` command pre-configured with the repo cwd and the
    /// FakeGh env var.
    pub fn choo(&self) -> Command {
        let mut c = Command::cargo_bin("choo").expect("choo binary");
        c.current_dir(self.path());
        c.env("CHOOCHOO_GH_FAKE", &self.fake_gh);
        c.env("CHOOCHOO_LOG", "off");
        c
    }

    /// Convenience: run `choo <args>` and assert success.
    pub fn choo_ok<I, S>(&self, args: I) -> std::process::Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let out = self.choo().args(args).output().expect("run choo");
        if !out.status.success() {
            panic!(
                "choo failed: status={} stdout=`{}` stderr=`{}`",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        out
    }

    /// Read the FakeGh JSON file. Returns `None` if no PRs have been
    /// created yet.
    pub fn fake_gh_state(&self) -> Option<serde_json::Value> {
        if !self.fake_gh.exists() {
            return None;
        }
        let text = std::fs::read_to_string(&self.fake_gh).ok()?;
        if text.trim().is_empty() {
            return None;
        }
        serde_json::from_str(&text).ok()
    }
}

fn run_git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}
