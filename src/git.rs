//! Git operations behind a trait so they can be faked in tests.
//!
//! Production uses [`ProcessGitRunner`] which shells out to `git` (avoiding
//! a libgit2 dependency and matching user mental models). Tests can plug in
//! their own `impl GitRunner`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// Outcome of a rebase attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// Rebase finished successfully. The branch tip is now at `new_sha`.
    Ok { new_sha: String },
    /// Rebase stopped due to conflicts. Caller is now mid-rebase.
    Conflict {
        /// stderr from `git`, useful for displaying to the user.
        stderr: String,
    },
}

/// How aggressively to push: tri-state because each option has materially
/// different safety semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushMode {
    /// Default: `git push --force-with-lease`. Refuses the push if the
    /// remote has moved since the last fetch — preserves teammate work.
    ForceWithLease,
    /// `git push --force`. Overwrites the remote unconditionally. Use
    /// when the lease check is wrong (e.g. you fetched implicitly via a
    /// background tool and lost the lease).
    Force,
    /// Plain `git push`. Fails if the push wouldn't be fast-forward.
    Plain,
}

impl PushMode {
    /// Git CLI flag for this mode (or [`None`] for the plain push).
    pub fn git_flag(self) -> Option<&'static str> {
        match self {
            PushMode::ForceWithLease => Some("--force-with-lease"),
            PushMode::Force => Some("--force"),
            PushMode::Plain => None,
        }
    }
}

/// Abstraction over the git operations choochoo needs.
///
/// Methods are deliberately small and direct: each maps roughly 1:1 to a
/// `git` invocation. Higher-level concerns (stacked rebase, etc.) live in
/// the `train::*` modules.
pub trait GitRunner {
    fn current_branch(&self) -> Result<String>;
    fn branch_exists(&self, name: &str) -> Result<bool>;
    fn checkout(&self, branch: &str) -> Result<()>;
    fn rev_parse(&self, rev: &str) -> Result<String>;
    /// Rebase `branch` so commits in `upstream..branch` are replayed onto
    /// `onto`. Equivalent to `git rebase --onto <onto> <upstream> <branch>`.
    fn rebase_onto(
        &self,
        branch: &str,
        onto: &str,
        upstream: &str,
    ) -> Result<RebaseOutcome>;
    /// Abort an in-progress rebase. No-op if no rebase is in progress.
    fn rebase_abort(&self) -> Result<()>;
    fn push(&self, branch: &str, mode: PushMode, remote: &str) -> Result<()>;
    fn fetch(&self, remote: &str) -> Result<()>;
    /// Best-effort: returns Some((ahead, behind)) if both refs are valid.
    fn ahead_behind(&self, branch: &str, upstream: &str) -> Result<Option<(u32, u32)>>;
}

/// Production implementation of [`GitRunner`] that shells to `git`.
pub struct ProcessGitRunner {
    repo_root: PathBuf,
    git_bin: PathBuf,
}

impl ProcessGitRunner {
    pub fn new(repo_root: impl Into<PathBuf>) -> Result<Self> {
        let repo_root = repo_root.into();
        let git_bin = which::which("git").map_err(|_| Error::MissingTool("git"))?;
        Ok(Self { repo_root, git_bin })
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.git_bin);
        c.current_dir(&self.repo_root);
        // Make output stable and non-localized, ignore the user's pager.
        c.env("LC_ALL", "C");
        c.env("GIT_TERMINAL_PROMPT", "0");
        c.env("GIT_PAGER", "cat");
        c
    }

    fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.cmd().args(args).output().map_err(|e| Error::Io {
            path: self.git_bin.clone(),
            source: e,
        })?;
        if !output.status.success() {
            return Err(Error::Git {
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

impl GitRunner for ProcessGitRunner {
    fn current_branch(&self) -> Result<String> {
        let out = self.run(["rev-parse", "--abbrev-ref", "HEAD"])?;
        Ok(out)
    }

    fn branch_exists(&self, name: &str) -> Result<bool> {
        let output = self
            .cmd()
            .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}")])
            .output()
            .map_err(|e| Error::Io {
                path: self.git_bin.clone(),
                source: e,
            })?;
        Ok(output.status.success())
    }

    fn checkout(&self, branch: &str) -> Result<()> {
        self.run(["checkout", branch])?;
        Ok(())
    }

    fn rev_parse(&self, rev: &str) -> Result<String> {
        self.run(["rev-parse", rev])
    }

    fn rebase_onto(
        &self,
        branch: &str,
        onto: &str,
        upstream: &str,
    ) -> Result<RebaseOutcome> {
        let output = self
            .cmd()
            .args(["rebase", "--onto", onto, upstream, branch])
            .output()
            .map_err(|e| Error::Io {
                path: self.git_bin.clone(),
                source: e,
            })?;
        if output.status.success() {
            let new_sha = self.rev_parse(branch)?;
            Ok(RebaseOutcome::Ok { new_sha })
        } else {
            // rebase that's left in conflicted state: don't surface as Error::Git.
            // Heuristic: rebase exited non-zero but a rebase is in progress.
            let in_rebase = self.repo_root.join(".git/rebase-apply").exists()
                || self.repo_root.join(".git/rebase-merge").exists();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if in_rebase {
                Ok(RebaseOutcome::Conflict { stderr })
            } else {
                Err(Error::Git {
                    code: output.status.code().unwrap_or(-1),
                    stderr,
                })
            }
        }
    }

    fn rebase_abort(&self) -> Result<()> {
        let output = self
            .cmd()
            .args(["rebase", "--abort"])
            .output()
            .map_err(|e| Error::Io {
                path: self.git_bin.clone(),
                source: e,
            })?;
        // Ignore failure - it just means there's no rebase to abort.
        let _ = output;
        Ok(())
    }

    fn push(&self, branch: &str, mode: PushMode, remote: &str) -> Result<()> {
        // `--set-upstream` (a.k.a. `-u`) is included on every push: it's
        // idempotent if the upstream is already set, and it makes
        // subsequent `git status` / `git pull --rebase` / `git fetch`
        // work without arguments. It also tightens `--force-with-lease`,
        // which falls back to a more permissive comparison when there's
        // no remote-tracking ref to lease against.
        let mut args: Vec<String> = vec!["push".into(), "--set-upstream".into()];
        if let Some(flag) = mode.git_flag() {
            args.push(flag.into());
        }
        args.push(remote.to_string());
        args.push(branch.to_string());
        self.run(args.iter().map(String::as_str))?;
        Ok(())
    }

    fn fetch(&self, remote: &str) -> Result<()> {
        self.run(["fetch", remote])?;
        Ok(())
    }

    fn ahead_behind(&self, branch: &str, upstream: &str) -> Result<Option<(u32, u32)>> {
        // git rev-list --left-right --count upstream...branch
        let output = self
            .cmd()
            .args([
                "rev-list",
                "--left-right",
                "--count",
                &format!("{upstream}...{branch}"),
            ])
            .output()
            .map_err(|e| Error::Io {
                path: self.git_bin.clone(),
                source: e,
            })?;
        if !output.status.success() {
            return Ok(None);
        }
        let s = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() != 2 {
            return Err(Error::ParseOutput {
                cmd: "rev-list",
                reason: format!("expected `behind ahead`, got `{}`", s.trim()),
            });
        }
        let behind: u32 = parts[0].parse().map_err(|e| Error::ParseOutput {
            cmd: "rev-list",
            reason: format!("behind not a number: {e}"),
        })?;
        let ahead: u32 = parts[1].parse().map_err(|e| Error::ParseOutput {
            cmd: "rev-list",
            reason: format!("ahead not a number: {e}"),
        })?;
        Ok(Some((ahead, behind)))
    }
}
