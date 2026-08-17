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
    /// Whether `ancestor` is an ancestor of `descendant`. A commit counts as
    /// its own ancestor.
    ///
    /// False when either rev doesn't name a commit *in this repository*.
    /// "Not here" is an ordinary situation, not an error: a SHA choochoo
    /// recorded earlier can legitimately be absent — state synced from
    /// another machine naming commits this one has never fetched, or a
    /// commit that has since been garbage-collected. Callers treat that the
    /// same as "not an ancestor": don't trust it, fall back.
    ///
    /// Deliberately not built on [`GitRunner::rev_parse`] as an existence
    /// check: plain `git rev-parse` *succeeds* and echoes back any
    /// syntactically valid 40-hex string, whether or not the object exists.
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool>;
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
    /// Create or force-move `branch` so it points at `to_rev`. Equivalent
    /// to `git branch --force <branch> <to_rev>`; used to keep a train's
    /// aggregate branch pinned to its tip. Must not touch the working
    /// tree, so it fails when `branch` is the currently checked-out branch
    /// and would have to move.
    fn set_branch(&self, branch: &str, to_rev: &str) -> Result<()>;
    /// Fast-forward the *currently checked out* branch to `to_rev`, moving
    /// the working tree with it. Equivalent to `git merge --ff-only
    /// <to_rev>`.
    ///
    /// The companion to [`GitRunner::set_branch`], which is the right tool
    /// for every branch that *isn't* checked out and deliberately refuses
    /// the one that is. Git does the refusing here too: this fails rather
    /// than merging when `to_rev` isn't a descendant, and fails rather than
    /// overwriting when the working tree is dirty.
    fn fast_forward_current(&self, to_rev: &str) -> Result<()>;
    /// Move the *currently checked out* branch to `to_rev`, working tree and
    /// index with it. Equivalent to `git reset --hard <to_rev>`.
    ///
    /// Unlike [`GitRunner::fast_forward_current`] this discards whatever it
    /// has to — commits and uncommitted changes both — without asking, so
    /// callers own the decision that it's safe. Untracked files survive, as
    /// they do under plain `git reset --hard`.
    fn reset_hard_current(&self, to_rev: &str) -> Result<()>;
    /// Whether the working tree or index has changes to *tracked* files.
    ///
    /// Untracked files deliberately don't count: they're what
    /// [`GitRunner::reset_hard_current`] leaves alone, so treating a stray
    /// scratch file as "dirty" would block a reset that couldn't have
    /// harmed it.
    fn is_dirty(&self) -> Result<bool>;
    fn push(&self, branch: &str, mode: PushMode, remote: &str) -> Result<()>;
    /// Push several branches to `remote` in a *single* `git push`.
    ///
    /// A train is normally pushed as a whole, and one invocation means one
    /// connection and one ref advertisement instead of N — on a large repo
    /// that round trip dominates the cost of `choo push`.
    ///
    /// `atomic` asks the remote for all-or-nothing semantics, so a stack
    /// never lands half-pushed. Not every server implements it; callers
    /// pair this with [`is_atomic_unsupported`] to fall back.
    ///
    /// An empty `branches` is a no-op rather than a bare `git push`, which
    /// would consult `push.default` and send a ref nobody named.
    fn push_many(
        &self,
        branches: &[&str],
        mode: PushMode,
        remote: &str,
        atomic: bool,
    ) -> Result<()>;
    fn fetch(&self, remote: &str) -> Result<()>;
    /// Best-effort: returns Some((ahead, behind)) if both refs are valid.
    fn ahead_behind(&self, branch: &str, upstream: &str) -> Result<Option<(u32, u32)>>;
    /// URL configured for `remote`, or [`None`] when there is no such
    /// remote. A repo without an `origin` is an ordinary situation (a fresh
    /// `git init`), so it isn't an error.
    fn remote_url(&self, remote: &str) -> Result<Option<String>>;
    /// Whether `refs/remotes/<remote>/<branch>` exists locally. Reflects the
    /// last fetch, so callers fetch first.
    fn remote_branch_exists(&self, remote: &str, branch: &str) -> Result<bool>;
    /// Create local `branch` tracking `<remote>/<branch>`, equivalent to
    /// `git branch --track <branch> <remote>/<branch>`.
    ///
    /// Deliberately not `git checkout -b`: this runs over a whole train at
    /// once and must not move the user's working tree.
    fn create_tracking_branch(&self, branch: &str, remote: &str) -> Result<()>;
}

/// Whether a failed [`GitRunner::push_many`] failed because the remote
/// doesn't implement the atomic-push capability — the one case where
/// retrying the same push one branch at a time is the right move.
///
/// Deliberately narrow. Git says `fatal: the receiving end does not
/// support --atomic push` for a missing capability, but words a *rejected
/// ref* under an atomic push differently ("atomic push failed for ref
/// ..."). Matching only the capability wording keeps a push that was
/// refused on its merits — a stale lease, a non-fast-forward — from being
/// quietly retried in a mode that might let half of it through.
pub fn is_atomic_unsupported(err: &Error) -> bool {
    matches!(err, Error::Git { stderr, .. } if stderr.contains("does not support --atomic"))
}

/// Build a `git` command with choochoo's standard environment.
///
/// Shared with the state-store plumbing in [`crate::store`], which runs git
/// against a different repository but wants the same guarantees: stable
/// non-localized output, no pager, and — importantly — no interactive
/// credential prompt, so a wrapper command can never hang waiting for a
/// password nobody is there to type.
pub(crate) fn git_command(git_bin: &Path, cwd: &Path) -> Command {
    let mut c = Command::new(git_bin);
    c.current_dir(cwd);
    c.env("LC_ALL", "C");
    c.env("GIT_TERMINAL_PROMPT", "0");
    c.env("GIT_PAGER", "cat");
    c
}

/// Locate `git`, or report it as a missing tool.
pub(crate) fn git_binary() -> Result<PathBuf> {
    which::which("git").map_err(|_| Error::MissingTool("git"))
}

/// The *common* git directory for the checkout at `repo_root`: the one
/// shared by the main checkout and all its linked worktrees.
///
/// [`None`] when git can't answer — no `git` on `PATH`, or a directory
/// that only looks like a repository — so callers can fall back to the
/// plain `<root>/.git` layout instead of failing.
pub(crate) fn common_dir(repo_root: &Path) -> Option<PathBuf> {
    resolve_dir(repo_root, "--git-common-dir")
}

/// This worktree's *own* git directory: `<main>/.git/worktrees/<name>` in
/// a linked worktree, and the same thing [`common_dir`] returns otherwise.
/// Per-worktree state — an in-progress rebase, say — lives here.
pub(crate) fn worktree_git_dir(repo_root: &Path) -> Option<PathBuf> {
    resolve_dir(repo_root, "--git-dir")
}

fn resolve_dir(repo_root: &Path, flag: &str) -> Option<PathBuf> {
    let git_bin = git_binary().ok()?;
    let out = git_command(&git_bin, repo_root)
        .args(["rev-parse", flag])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let answer = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if answer.is_empty() {
        return None;
    }
    // Git answers relative to the directory we ran it in (plain `.git`, in
    // the common case); joining makes that absolute and leaves an answer
    // that is already absolute alone.
    Some(repo_root.join(answer))
}

/// Production implementation of [`GitRunner`] that shells to `git`.
pub struct ProcessGitRunner {
    repo_root: PathBuf,
    git_bin: PathBuf,
}

impl ProcessGitRunner {
    pub fn new(repo_root: impl Into<PathBuf>) -> Result<Self> {
        let repo_root = repo_root.into();
        let git_bin = git_binary()?;
        Ok(Self { repo_root, git_bin })
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    fn cmd(&self) -> Command {
        git_command(&self.git_bin, &self.repo_root)
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

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let output = self
            .cmd()
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .output()
            .map_err(|e| Error::Io {
                path: self.git_bin.clone(),
                source: e,
            })?;
        // 0 = yes. 1 = no. 128 = one of the revs doesn't name a commit here,
        // which is a "don't know" and must answer `false` rather than error —
        // see the trait doc.
        Ok(output.status.success())
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
            // A rebase is recorded in *this worktree's* git dir, which is
            // only `<root>/.git` when the checkout isn't a linked worktree.
            let git_dir = worktree_git_dir(&self.repo_root)
                .unwrap_or_else(|| self.repo_root.join(".git"));
            let in_rebase = git_dir.join("rebase-apply").exists()
                || git_dir.join("rebase-merge").exists();
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

    fn set_branch(&self, branch: &str, to_rev: &str) -> Result<()> {
        let target = self.rev_parse(to_rev)?;
        if self.branch_exists(branch)? && self.rev_parse(branch)? == target {
            return Ok(()); // already there; nothing to move.
        }
        // `git branch --force` refuses to move the checked-out branch, and
        // we deliberately don't reach for `git reset --hard` (it would
        // discard the user's working tree). Say so plainly instead.
        if self.current_branch()? == branch {
            return Err(Error::InvalidArgument(format!(
                "branch `{branch}` is checked out and would have to move; \
                 check out another branch first"
            )));
        }
        self.run(["branch", "--force", branch, &target])?;
        Ok(())
    }

    fn fast_forward_current(&self, to_rev: &str) -> Result<()> {
        self.run(["merge", "--ff-only", to_rev])?;
        Ok(())
    }

    fn reset_hard_current(&self, to_rev: &str) -> Result<()> {
        self.run(["reset", "--hard", to_rev])?;
        Ok(())
    }

    fn is_dirty(&self) -> Result<bool> {
        let out = self.run(["status", "--porcelain", "--untracked-files=no"])?;
        Ok(!out.trim().is_empty())
    }

    fn push(&self, branch: &str, mode: PushMode, remote: &str) -> Result<()> {
        // Single-branch push is the batched one with a one-element batch;
        // `--atomic` buys nothing for one ref, so it's left off.
        self.push_many(&[branch], mode, remote, false)
    }

    fn push_many(
        &self,
        branches: &[&str],
        mode: PushMode,
        remote: &str,
        atomic: bool,
    ) -> Result<()> {
        if branches.is_empty() {
            return Ok(());
        }
        // `--set-upstream` (a.k.a. `-u`) is included on every push: it's
        // idempotent if the upstream is already set, and it makes
        // subsequent `git status` / `git pull --rebase` / `git fetch`
        // work without arguments. It also tightens `--force-with-lease`,
        // which falls back to a more permissive comparison when there's
        // no remote-tracking ref to lease against.
        let mut args: Vec<String> = vec!["push".into(), "--set-upstream".into()];
        if atomic {
            args.push("--atomic".into());
        }
        // Both force flags apply per-ref: `--force-with-lease` with no
        // explicit expected value leases each branch against its own
        // remote-tracking ref, so batching doesn't weaken the check.
        if let Some(flag) = mode.git_flag() {
            args.push(flag.into());
        }
        args.push(remote.to_string());
        args.extend(branches.iter().map(|b| (*b).to_string()));
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

    fn remote_url(&self, remote: &str) -> Result<Option<String>> {
        // `git remote get-url` exits 2 for an unconfigured remote, so this
        // deliberately checks the status itself rather than going through
        // `run`, which would turn that into an `Error::Git`.
        let output = self
            .cmd()
            .args(["remote", "get-url", remote])
            .output()
            .map_err(|e| Error::Io {
                path: self.git_bin.clone(),
                source: e,
            })?;
        if !output.status.success() {
            return Ok(None);
        }
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(if url.is_empty() { None } else { Some(url) })
    }

    fn remote_branch_exists(&self, remote: &str, branch: &str) -> Result<bool> {
        let refname = format!("refs/remotes/{remote}/{branch}");
        let output = self
            .cmd()
            .args(["show-ref", "--verify", "--quiet", &refname])
            .output()
            .map_err(|e| Error::Io {
                path: self.git_bin.clone(),
                source: e,
            })?;
        Ok(output.status.success())
    }

    fn create_tracking_branch(&self, branch: &str, remote: &str) -> Result<()> {
        let start = format!("{remote}/{branch}");
        self.run(["branch", "--track", branch, &start])?;
        Ok(())
    }
}
