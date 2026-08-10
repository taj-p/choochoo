//! Shared fixtures for end-to-end CLI integration tests.
//!
//! [`TestRepo`] creates a real git repository under a [`tempfile::TempDir`]
//! with stable identity so `git commit` doesn't fail on the test runner.
//! It exposes helpers for committing files, creating branches, and running
//! the `choo` binary inside the repo with `CHOOCHOO_GH_FAKE` set so the
//! `pr` subcommand records calls against a JSON file we can read back.
//!
//! ## Hermeticity
//!
//! These tests spawn the real `choo` binary, which resolves its config from
//! the environment. So [`TestRepo::choo`] pins every environment input to a
//! per-test tempdir: `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and
//! `CHOOCHOO_CONFIG=none`. Without that, a developer who has actually
//! configured shared state would have `cargo test` reading their real config
//! and pushing fixture trains into their real state repo.
//!
//! `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` are neutralised for the same
//! reason — that one closes a gap that predates shared state, where a
//! developer's global `core.hooksPath` or `include.path` could perturb the
//! suite.
//!
//! [`BareRepo`] stands in for a repo on GitHub. Pointing `origin` (or
//! `[store] repo`) at a bare repo in a tempdir exercises the real
//! clone/fetch/commit/push paths with no network and no auth, the same way
//! `FakeGh` stands in for the PR API.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

/// A bare git repo in a tempdir: our stand-in for a repo hosted on GitHub.
pub struct BareRepo {
    pub dir: TempDir,
}

impl BareRepo {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("create tempdir");
        run_git(
            dir.path(),
            &["init", "-q", "--bare", "--initial-branch=main"],
        );
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The URL to hand to git. A plain path, matching what the existing
    /// push tests already do successfully for `origin`.
    pub fn url(&self) -> String {
        self.path().display().to_string()
    }

    /// Contents of `path` at `branch`, or `None` if either is absent.
    pub fn read(&self, branch: &str, path: &str) -> Option<String> {
        let out = Command::new("git")
            .args(["show", &format!("{branch}:{path}")])
            .current_dir(self.path())
            .output()
            .expect("git show");
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// The shared-state document choochoo wrote for `key`, parsed.
    pub fn read_state(&self, key: &str) -> Option<serde_json::Value> {
        let text = self.read("main", &format!("repos/{key}.json"))?;
        serde_json::from_str(&text).ok()
    }

    /// Every file tracked on `branch`.
    pub fn files(&self, branch: &str) -> Vec<String> {
        let out = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", branch])
            .current_dir(self.path())
            .output()
            .expect("git ls-tree");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn log(&self, branch: &str) -> Vec<String> {
        let out = Command::new("git")
            .args(["log", "--oneline", branch])
            .current_dir(self.path())
            .output()
            .expect("git log");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Make every push to this repo fail, server-side.
    ///
    /// A `pre-receive` hook is a deterministic way to force a push failure —
    /// far better than trying to race two processes — and being server-side
    /// it isn't affected by the client's neutralised git config.
    pub fn reject_pushes(&self) {
        let hooks = self.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-receive");
        std::fs::write(&hook, "#!/bin/sh\necho 'rejected by test' >&2\nexit 1\n")
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
    }

    pub fn accept_pushes(&self) {
        let hook = self.path().join("hooks/pre-receive");
        if hook.exists() {
            std::fs::remove_file(hook).unwrap();
        }
    }
}

/// A linked worktree of a [`TestRepo`], alive as long as this value is.
pub struct Worktree {
    /// Held only so the tempdir outlives the worktree.
    _dir: TempDir,
    path: PathBuf,
}

impl Worktree {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One self-contained git repo for a test, plus a path to the FakeGh JSON.
pub struct TestRepo {
    pub dir: TempDir,
    pub fake_gh: PathBuf,
    /// Sandboxed environment roots for this "machine".
    env: TempDir,
    /// `CHOOCHOO_CONFIG` value. `none` until [`TestRepo::share_state`].
    config: PathBuf,
}

impl TestRepo {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("create tempdir");
        let path = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "--initial-branch=main"]);
        Self::finish_setup(dir, true)
    }

    /// A second "machine" working on the same code repo: a clone sharing an
    /// `origin` with any other clone of `bare`, so both resolve to the same
    /// choochoo repository identity.
    pub fn clone_of(bare: &BareRepo) -> Self {
        let dir = TempDir::new().expect("create tempdir");
        let path = dir.path().to_path_buf();
        run_git(&path, &["init", "-q", "--initial-branch=main"]);
        run_git(&path, &["remote", "add", "origin", &bare.url()]);
        Self::finish_setup(dir, false)
    }

    fn finish_setup(dir: TempDir, seed_commit: bool) -> Self {
        let path = dir.path().to_path_buf();
        run_git(&path, &["config", "user.email", "test@example.invalid"]);
        run_git(&path, &["config", "user.name", "Test User"]);
        run_git(&path, &["config", "commit.gpgsign", "false"]);
        if seed_commit {
            // First commit on main so HEAD is valid.
            std::fs::write(path.join("README.md"), "# test repo\n").unwrap();
            run_git(&path, &["add", "README.md"]);
            run_git(&path, &["commit", "-q", "-m", "init"]);
        }

        let fake_gh = path.join(".git/choochoo/gh.json");
        let env = TempDir::new().expect("create env tempdir");
        for sub in ["home", "xdg-config", "xdg-data"] {
            std::fs::create_dir_all(env.path().join(sub)).unwrap();
        }
        Self {
            dir,
            fake_gh,
            env,
            config: PathBuf::from("none"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Point `origin` at `bare` and push `main` to it.
    pub fn with_origin(&self, bare: &BareRepo) -> &Self {
        run_git(self.path(), &["remote", "add", "origin", &bare.url()]);
        run_git(self.path(), &["push", "-q", "origin", "main"]);
        self
    }

    /// Turn on shared state for this machine, backed by `store`.
    pub fn share_state(&mut self, store: &BareRepo) -> &Self {
        let dir = self.env.path().join("xdg-config").join("choochoo");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!("[store]\nrepo = \"{}\"\n", store.url()),
        )
        .unwrap();
        self.config = config;
        self
    }

    /// Write `toml` as this machine's whole config file and use it.
    ///
    /// The counterpart to [`TestRepo::share_state`] for config that isn't
    /// about the store — it still lands inside this test's sandboxed
    /// `XDG_CONFIG_HOME`, never the developer's real one.
    pub fn set_config(&mut self, toml: &str) -> &Self {
        let dir = self.env.path().join("xdg-config").join("choochoo");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, toml).unwrap();
        self.config = config;
        self
    }

    /// Point shared state at a path that isn't a git repo.
    pub fn share_state_with_url(&mut self, url: &str) -> &Self {
        let dir = self.env.path().join("xdg-config").join("choochoo");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, format!("[store]\nrepo = \"{url}\"\n")).unwrap();
        self.config = config;
        self
    }

    /// This machine's store clone, once one exists.
    pub fn store_clone(&self) -> PathBuf {
        self.env.path().join("xdg-data/choochoo/store")
    }

    pub fn local_state(&self) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(
            self.path().join(".git/choochoo/local.json"),
        )
        .ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn active_train(&self) -> Option<String> {
        self.local_state()?
            .get("active")?
            .as_str()
            .map(str::to_string)
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

    /// Add a linked worktree checked out at a new branch `branch`, forked
    /// from `from`.
    ///
    /// It lives in its own tempdir rather than under the main working
    /// tree, matching how people actually lay worktrees out — and making
    /// sure nothing in the test passes by accident of the two checkouts
    /// sharing a parent directory.
    pub fn worktree(&self, branch: &str, from: &str) -> Worktree {
        let dir = TempDir::new().expect("create tempdir");
        let path = dir.path().join("wt");
        run_git(
            self.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().expect("utf-8 tempdir path"),
                from,
            ],
        );
        Worktree { _dir: dir, path }
    }

    /// Build a `choo` command pre-configured with the repo cwd, the FakeGh
    /// env var, and a fully sandboxed environment.
    ///
    /// Every environment input `choo` consults is pinned to this test's
    /// tempdirs — see the module docs for why that isn't optional.
    pub fn choo(&self) -> Command {
        let mut c = Command::cargo_bin("choo").expect("choo binary");
        c.current_dir(self.path());
        c.env("CHOOCHOO_GH_FAKE", &self.fake_gh);
        c.env("CHOOCHOO_LOG", "off");
        c.env("HOME", self.env.path().join("home"));
        c.env("XDG_CONFIG_HOME", self.env.path().join("xdg-config"));
        c.env("XDG_DATA_HOME", self.env.path().join("xdg-data"));
        c.env("CHOOCHOO_CONFIG", &self.config);
        c.env_remove("CHOOCHOO_NO_SYNC");
        c.env_remove("CHOOCHOO_STORE_DIR");
        // Ignore the developer's own git config entirely.
        c.env("GIT_CONFIG_GLOBAL", "/dev/null");
        c.env("GIT_CONFIG_SYSTEM", "/dev/null");
        c
    }

    /// Like [`TestRepo::choo`], but run from `dir` — a linked worktree of
    /// this repo, typically.
    pub fn choo_from(&self, dir: &Path) -> Command {
        let mut c = self.choo();
        c.current_dir(dir);
        c
    }

    /// Run `choo <args>`, returning the output without asserting on it.
    pub fn choo_try<I, S>(&self, args: I) -> std::process::Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.choo().args(args).output().expect("run choo")
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
