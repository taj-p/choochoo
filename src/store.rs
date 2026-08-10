//! The shared state store: a git repo holding choochoo's train metadata.
//!
//! [`crate::state::Store`] is the façade the rest of the crate uses. This
//! module is the half that talks to a *second* git repository — the one the
//! user names in `[store] repo` — keyed per working repo:
//!
//! ```text
//! repos/github.com/owner/repo.json     # SharedState { version, trains }
//! repos/local/bare-1f2e3d4c.json
//! ```
//!
//! ## The store invariant
//!
//! > The clone's branch is **either exactly `origin/<branch>`, or exactly
//! > one commit ahead of it** — that one commit holding this machine's
//! > not-yet-published change.
//!
//! Every write re-establishes it: fetch, compute the merged content,
//! `reset --hard origin/<branch>`, write, commit, push. We never run
//! `git merge` or `git pull`, so **a git text conflict in the store is
//! structurally impossible**. The only conflict that can arise is the
//! semantic one — two machines editing the same train — and that is resolved
//! in Rust by [`merge3`], deterministically and with a warning.
//!
//! The invariant also means a crash between commit and push is harmless:
//! the clone is simply left one ahead, and the next command publishes it.
//! For *that* case the merge base is recoverable from git alone
//! (`merge-base HEAD origin/<branch>`), with nothing kept in a sidecar file.
//! Within a process, though, the base is the state we actually read — see the
//! [`GitStore::base`] field for why the git-derived one isn't safe there.
//!
//! ## Failure posture
//!
//! Committing locally is the durable step; pushing is best-effort. A failed
//! fetch degrades to the clone's contents with a warning, so `choo list`
//! works offline. A failed push keeps the commit and warns; the next command
//! drains it. Nothing is ever silently dropped — every degradation produces
//! a warning that [`GitStore::take_warnings`] hands to the UI.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use crate::config::StoreConfig;
use crate::error::{Error, Result};
use crate::git::{git_binary, git_command};
use crate::state::{SharedState, Train, write_json_atomic};

/// How long any single network git operation may take.
///
/// Without a cap, a fetch against an unreachable host sits in TCP connect
/// for the kernel timeout — over a minute — on *every* command. That is the
/// difference between "degraded" and "hung".
const NETWORK_TIMEOUT: Duration = Duration::from_secs(20);

/// Attempts to publish before giving up and leaving the commit pending.
const PUSH_ATTEMPTS: u32 = 3;

/// How long to wait for another `choo` process to release the store lock.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// A lock older than this is assumed to belong to a killed process.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(120);

/// Outcome of a push attempt. Rejection and failure need different
/// handling, so they are different values rather than one error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushResult {
    Pushed,
    /// The remote moved first. Merge and retry.
    Rejected,
}

/// A clone of the user's state repo, scoped to one working repository.
pub struct GitStore {
    dir: PathBuf,
    url: String,
    branch: String,
    /// This working repo's identity, e.g. `github.com/owner/repo`.
    key: String,
    git: PathBuf,
    /// `--no-sync`: use the clone exactly as it is, no network either way.
    offline: bool,
    /// Fetch at most once per process. `choo init --aggregate` loads twice,
    /// and the TUI reloads after every effect; neither should re-fetch.
    synced: Cell<bool>,
    /// The remote is known to be unreachable this run. Lets later messages
    /// stay short instead of repeating the same git error verbatim.
    unreachable: Cell<bool>,
    /// What [`GitStore::read_shared`] last handed out — the true base for a
    /// later three-way merge.
    ///
    /// It would be tempting to recover the base from git instead
    /// (`merge-base HEAD origin/<branch>`), and for a single process that
    /// agrees. But two `choo` processes share one clone: the second one's
    /// read-modify-write straddles the first one's commit, so by the time it
    /// writes, the clone has moved and `merge-base` describes a state it
    /// never read. Diffing against that makes the second process look like it
    /// *deleted* the first one's train. Remembering what we actually read is
    /// the only base that can't lie.
    base: RefCell<Option<SharedState>>,
    warnings: RefCell<Vec<String>>,
}

impl GitStore {
    /// Open (cloning on first use) the store described by `cfg`.
    ///
    /// `dir` is the clone location and `key` the working repo's identity.
    /// Fails only when there is no usable clone at all — if the directory is
    /// already there, an unreachable remote is a warning, not an error.
    pub fn open(
        cfg: &StoreConfig,
        dir: PathBuf,
        key: String,
        offline: bool,
    ) -> Result<Self> {
        if !crate::repoid::is_valid(&key) {
            return Err(Error::StoreUnavailable(format!(
                "refusing to use unsafe repository key `{key}`"
            )));
        }
        let store = Self {
            dir,
            url: cfg.repo.clone(),
            branch: cfg.branch.clone(),
            key,
            git: git_binary()?,
            offline,
            synced: Cell::new(false),
            unreachable: Cell::new(false),
            base: RefCell::new(None),
            warnings: RefCell::new(Vec::new()),
        };
        store.ensure_clone()?;
        Ok(store)
    }

    /// Where state lives, for `choo sync status` and error messages.
    pub fn describe(&self) -> String {
        format!("{} ({} @ {})", self.url, self.branch, self.dir.display())
    }

    /// Take the warnings accumulated so far. The CLI writes them to stderr;
    /// the TUI folds them into its status line (it owns the alternate
    /// screen, so it must not have anything printed underneath it).
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut self.warnings.borrow_mut())
    }

    fn warn(&self, msg: impl Into<String>) {
        self.warnings.borrow_mut().push(msg.into());
    }

    /// Record a warning raised outside this type (e.g. by [`open`]).
    pub(crate) fn push_warning(&self, msg: String) {
        self.warn(msg);
    }

    /// Tell the user about every train that had to be reconciled.
    ///
    /// Both the normal write path and the drain-a-pending-commit path go
    /// through here. Silently resolving a conflict in either would mean a
    /// user losing an edit with no idea it happened — and the recovery
    /// command is the whole reason keeping state in git pays off.
    fn report_conflicts(&self, conflicts: &[String]) {
        for train in conflicts {
            self.warn(format!(
                "train `{train}` was changed here and on another machine; kept \
                 this machine's version. The other one is still in the store's \
                 history: git -C {} log -p -- {}",
                self.dir.display(),
                self.entry_rel(),
            ));
        }
    }

    /// Path of this repo's entry within the clone.
    fn entry_path(&self) -> PathBuf {
        self.dir.join("repos").join(format!("{}.json", self.key))
    }

    /// Path of this repo's entry relative to the clone root, in the form git
    /// wants (forward slashes on every platform).
    fn entry_rel(&self) -> String {
        format!("repos/{}.json", self.key)
    }

    // -- reading -----------------------------------------------------------

    /// The shared half for this repo, syncing first (once per process).
    pub fn read_shared(&self) -> Result<SharedState> {
        let _lock = self.lock()?;
        self.sync_once();
        let shared = read_shared_file(&self.entry_path())?;
        // Remember it: this, not git's merge-base, is the base a later write
        // must diff against. See the `base` field.
        *self.base.borrow_mut() = Some(shared.clone());
        Ok(shared)
    }

    /// The state this process read, or — if it never read — the last commit
    /// this clone shares with the remote.
    fn merge_base_state(&self) -> Result<SharedState> {
        if let Some(base) = self.base.borrow().clone() {
            return Ok(base);
        }
        self.content_at(&self.merge_base())
    }

    // -- writing -----------------------------------------------------------

    /// Publish `shared` for this repo.
    ///
    /// Merges against whatever the remote holds now, commits, and pushes.
    /// Returns `Ok` even when the push fails: the commit is durable and the
    /// next command retries. A warning always says so.
    pub fn write_shared(&self, shared: &SharedState, what: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.sync_once();

        for attempt in 1..=PUSH_ATTEMPTS {
            let base = self.merge_base_state()?;
            let theirs = self.content_at(&self.remote_ref())?;
            let merged = merge3(&base, shared, &theirs);
            self.report_conflicts(&merged.conflicts);

            // Re-base onto the remote so our commit is exactly one ahead.
            self.reset_to_remote()?;
            write_json_atomic(&self.entry_path(), &merged.merged)?;

            if !self.commit_all(&format!("{}: {what}", self.key))? && attempt == 1 {
                // Nothing changed and nothing was pending: done.
                if !self.has_pending() {
                    return Ok(());
                }
            }

            if self.offline {
                self.warn(format!(
                    "--no-sync: change committed locally but not published to \
                     {}; it will publish on your next synced command",
                    self.url
                ));
                return Ok(());
            }

            match self.push() {
                Ok(PushResult::Pushed) => return Ok(()),
                Ok(PushResult::Rejected) => {
                    if attempt == PUSH_ATTEMPTS {
                        break;
                    }
                    if let Err(e) = self.fetch() {
                        self.warn(format!(
                            "could not re-fetch {} to retry the push: {e}",
                            self.url
                        ));
                        break;
                    }
                }
                Err(e) => {
                    // Don't restate the git error if we already reported the
                    // remote as unreachable a moment ago; one root cause
                    // deserves one explanation.
                    if self.unreachable.get() {
                        self.warn(
                            "your change is saved locally and will publish on \
                             your next command, once the store is reachable"
                                .to_string(),
                        );
                    } else {
                        self.warn(format!(
                            "your change is saved locally but was not published \
                             to {}: {e}. Run `choo sync` when you're back \
                             online.",
                            self.url
                        ));
                    }
                    return Ok(());
                }
            }
        }

        self.warn(format!(
            "another machine kept winning the race to {}; your change is saved \
             locally and will publish on your next command (or run `choo sync`)",
            self.url
        ));
        Ok(())
    }

    /// Force a sync now, publishing anything pending. Used by `choo sync`.
    pub fn sync_now(&self) -> Result<()> {
        let _lock = self.lock()?;
        self.synced.set(false);
        self.sync_once();
        Ok(())
    }

    /// True when the clone holds a commit that hasn't reached the remote.
    pub fn has_pending(&self) -> bool {
        let Some(head) = self.rev("HEAD") else {
            return false;
        };
        match self.rev(&self.remote_ref()) {
            None => true,
            Some(remote) => head != remote && !self.is_ancestor(&head, &remote),
        }
    }

    // -- sync --------------------------------------------------------------

    /// Bring the clone in line with the remote, publishing anything pending.
    /// Best-effort by design: a failure warns and leaves the cached clone in
    /// place, so read-only commands keep working offline.
    fn sync_once(&self) {
        if self.synced.replace(true) || self.offline {
            return;
        }
        if let Err(e) = self.fetch() {
            self.unreachable.set(true);
            self.warn(format!(
                "could not reach {}: {e}. Showing the last state synced to \
                 this machine.",
                self.url
            ));
            return;
        }

        let head = self.rev("HEAD");
        let remote = self.rev(&self.remote_ref());
        match (head, remote) {
            // No branch on the remote yet — a brand-new store repo. Keep
            // whatever we have; the first write creates it.
            (_, None) => {}
            // Nothing local yet: adopt the remote wholesale.
            (None, Some(_)) => {
                if let Err(e) = self.reset_to_remote() {
                    self.warn(format!("could not check out the store: {e}"));
                }
            }
            (Some(h), Some(r)) if h == r => {}
            // We are simply behind: fast-forward.
            (Some(h), Some(r)) if self.is_ancestor(&h, &r) => {
                if let Err(e) = self.reset_to_remote() {
                    self.warn(format!("could not update the store clone: {e}"));
                }
            }
            // We have a commit the remote hasn't seen. Drain it, so a read
            // command publishes work stranded by an earlier failure.
            (Some(_), Some(_)) => self.publish_pending(),
        }
    }

    /// Push a pending commit, merging if the remote moved on.
    fn publish_pending(&self) {
        for attempt in 1..=PUSH_ATTEMPTS {
            match self.push() {
                Ok(PushResult::Pushed) => return,
                Ok(PushResult::Rejected) => {
                    // Replay our entry on top of theirs and try again.
                    let replayed = (|| -> Result<()> {
                        let ours = read_shared_file(&self.entry_path())?;
                        let base = self.content_at(&self.merge_base())?;
                        let theirs = self.content_at(&self.remote_ref())?;
                        let merged = merge3(&base, &ours, &theirs);
                        self.report_conflicts(&merged.conflicts);
                        self.reset_to_remote()?;
                        write_json_atomic(&self.entry_path(), &merged.merged)?;
                        self.commit_all(&format!("{}: republish", self.key))?;
                        Ok(())
                    })();
                    if let Err(e) = replayed {
                        self.warn(format!("could not merge pending state: {e}"));
                        return;
                    }
                    if attempt < PUSH_ATTEMPTS {
                        if let Err(e) = self.fetch() {
                            self.warn(format!("could not re-fetch {}: {e}", self.url));
                            return;
                        }
                    }
                }
                Err(e) => {
                    self.warn(format!(
                        "state saved earlier is still not published to {}: {e}",
                        self.url
                    ));
                    return;
                }
            }
        }
        self.warn(format!(
            "could not publish pending state to {} after {PUSH_ATTEMPTS} \
             attempts; it is safe locally and will retry",
            self.url
        ));
    }

    // -- git plumbing ------------------------------------------------------

    fn remote_ref(&self) -> String {
        format!("refs/remotes/origin/{}", self.branch)
    }

    /// Clone the store repo if we don't have it yet.
    ///
    /// A brand-new empty repo has no branches at all, so this deliberately
    /// does *not* pass `-b <branch>` — `git clone -b main` fails outright
    /// against an empty repo, which is exactly the state a user is in right
    /// after creating one on GitHub.
    fn ensure_clone(&self) -> Result<()> {
        if self.dir.join(".git").exists() {
            return Ok(());
        }
        if self.offline {
            return Err(Error::StoreUnavailable(format!(
                "no local copy of {} yet, and --no-sync was requested",
                self.url
            )));
        }
        let parent = self.dir.parent().ok_or_else(|| {
            Error::StoreUnavailable(format!("invalid store path {}", self.dir.display()))
        })?;
        fs::create_dir_all(parent).map_err(|e| Error::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
        let dir = self.dir.to_string_lossy().to_string();
        let out = self.git_in(
            parent,
            &["clone", "--quiet", "--no-single-branch", &self.url, &dir],
            Some(NETWORK_TIMEOUT),
        )?;
        if !out.status.success() {
            // Nothing to fall back on: without a clone there is no state to
            // read, and quietly using a stale local file would show the
            // wrong trains. Fail, with the escape hatches named.
            return Err(Error::StoreUnavailable(format!(
                "could not clone {}: {}. Check `[store] repo` in your config, \
                 or run with --no-sync to work locally.",
                self.url,
                stderr_of(&out)
            )));
        }
        self.ensure_branch()?;
        Ok(())
    }

    /// Make sure HEAD names our branch, even in a repo with no commits.
    fn ensure_branch(&self) -> Result<()> {
        if self.rev(&self.remote_ref()).is_some() {
            self.reset_to_remote()?;
        } else {
            let head = format!("refs/heads/{}", self.branch);
            self.run(&["symbolic-ref", "HEAD", &head])?;
        }
        Ok(())
    }

    /// Fetch the store branch. Reports git's own words, not a wrapped error:
    /// every caller embeds this mid-sentence in a warning, where an extra
    /// "could not sync shared train state:" layer is just noise.
    fn fetch(&self) -> std::result::Result<(), String> {
        let out = self
            .git_in(
                &self.dir,
                &["fetch", "--quiet", "origin", &self.branch],
                Some(NETWORK_TIMEOUT),
            )
            .map_err(|e| one_line(&e.to_string()))?;
        if !out.status.success() {
            // A brand-new store repo has no such branch yet; not a failure.
            let err = stderr_of(&out);
            if err.contains("couldn't find remote ref") {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    fn push(&self) -> Result<PushResult> {
        let refspec = format!("HEAD:refs/heads/{}", self.branch);
        let out = self.git_in(
            &self.dir,
            &["push", "--quiet", "origin", &refspec],
            Some(NETWORK_TIMEOUT),
        )?;
        if out.status.success() {
            return Ok(PushResult::Pushed);
        }
        let err = stderr_of(&out);
        if is_rejection(&err) {
            return Ok(PushResult::Rejected);
        }
        Err(Error::StoreSync(err))
    }

    /// Discard local state and match the remote branch exactly. Safe because
    /// choochoo owns every file in this clone.
    fn reset_to_remote(&self) -> Result<()> {
        if self.rev(&self.remote_ref()).is_none() {
            return Ok(());
        }
        let remote = format!("origin/{}", self.branch);
        self.run(&["checkout", "--quiet", "-B", &self.branch, &remote])?;
        self.run(&["reset", "--quiet", "--hard", &remote])?;
        // Any leftover untracked file would end up in our next commit.
        self.run(&["clean", "--quiet", "-fd"])?;
        Ok(())
    }

    /// Stage everything and commit. `Ok(false)` when the tree was clean.
    fn commit_all(&self, message: &str) -> Result<bool> {
        self.run(&["add", "--all", "."])?;
        let out = self.git_in(
            &self.dir,
            &[
                // choochoo is the author here, not the user: this is a
                // machine-written commit, and a box with no `user.email`
                // configured would otherwise fail outright.
                "-c",
                "user.name=choochoo",
                "-c",
                "user.email=choochoo@localhost",
                // A global `commit.gpgsign = true` with no key present, or a
                // global `core.hooksPath` with a pre-commit hook, would
                // otherwise break auto-sync in a way nobody could diagnose.
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=",
                "commit",
                "--quiet",
                "--no-verify",
                "-m",
                message,
            ],
            None,
        )?;
        if out.status.success() {
            return Ok(true);
        }
        let combined = format!("{}{}", stderr_of(&out), stdout_of(&out));
        if combined.contains("nothing to commit")
            || combined.contains("nothing added to commit")
            || combined.contains("working tree clean")
        {
            return Ok(false);
        }
        Err(Error::StoreSync(combined))
    }

    /// The state this machine last read: the point where our pending commit
    /// (if any) diverged from the remote.
    fn merge_base(&self) -> String {
        let remote = self.remote_ref();
        if self.rev("HEAD").is_none() || self.rev(&remote).is_none() {
            return remote;
        }
        self.run(&["merge-base", "HEAD", &remote])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or(remote)
    }

    /// Our entry's content at `rev`. An absent rev or path means "empty",
    /// which is the correct base for a train that didn't exist yet.
    fn content_at(&self, rev: &str) -> Result<SharedState> {
        if self.rev(rev).is_none() {
            return Ok(SharedState::default());
        }
        let spec = format!("{rev}:{}", self.entry_rel());
        let out = self.git_in(&self.dir, &["show", &spec], None)?;
        if !out.status.success() {
            return Ok(SharedState::default());
        }
        parse_shared(&out.stdout, &spec)
    }

    fn rev(&self, rev: &str) -> Option<String> {
        let out = self
            .git_in(&self.dir, &["rev-parse", "--verify", "--quiet", rev], None)
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }

    fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.git_in(&self.dir, &["merge-base", "--is-ancestor", a, b], None)
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.git_in(&self.dir, args, None)?;
        if !out.status.success() {
            return Err(Error::StoreSync(format!(
                "git {}: {}",
                args.join(" "),
                stderr_of(&out)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Run git in `cwd`, optionally under a timeout.
    fn git_in(
        &self,
        cwd: &Path,
        args: &[&str],
        timeout: Option<Duration>,
    ) -> Result<Output> {
        let mut cmd = git_command(&self.git, cwd);
        cmd.args(args);
        let Some(timeout) = timeout else {
            return cmd.output().map_err(|e| Error::Io {
                path: self.git.clone(),
                source: e,
            });
        };

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| Error::Io {
            path: self.git.clone(),
            source: e,
        })?;

        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::StoreSync(format!(
                            "`git {}` timed out after {}s",
                            args.first().copied().unwrap_or("?"),
                            timeout.as_secs()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    return Err(Error::Io {
                        path: self.git.clone(),
                        source: e,
                    });
                }
            }
        }
        child.wait_with_output().map_err(|e| Error::Io {
            path: self.git.clone(),
            source: e,
        })
    }

    // -- locking -----------------------------------------------------------

    /// Serialize access to the clone across `choo` processes on this machine.
    ///
    /// Uses `mkdir`, which is atomic on every filesystem worth naming
    /// (including NFS), unlike create-a-file-and-check-the-pid dances. Also
    /// closes a pre-existing race: two concurrent `choo add` runs could
    /// previously lose one update.
    fn lock(&self) -> Result<StoreLock> {
        let path = self.dir.with_extension("lock");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        let start = Instant::now();
        loop {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(StoreLock { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        self.warn(format!(
                            "breaking a stale store lock at {} (left by a \
                             killed `choo`?)",
                            path.display()
                        ));
                        let _ = fs::remove_dir_all(&path);
                        continue;
                    }
                    if start.elapsed() >= LOCK_WAIT {
                        return Err(Error::StoreLocked { path });
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    return Err(Error::Io { path, source: e });
                }
            }
        }
    }
}

/// Releases the store lock on drop, including while unwinding.
struct StoreLock {
    path: PathBuf,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn lock_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e > LOCK_STALE_AFTER).unwrap_or(false))
        .unwrap_or(false)
}

/// Did the remote reject this push because it moved on, as opposed to the
/// push failing for an infrastructural reason?
///
/// A rejection means "merge and retry"; anything else (auth, DNS, a
/// protected branch) means "keep the commit and tell the user".
fn is_rejection(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("[rejected]")
        || s.contains("non-fast-forward")
        || s.contains("fetch first")
        || s.contains("stale info")
        // git's own wording in the hint block. Real output also carries
        // `[rejected]`, but matching the sentence too means a truncated or
        // reworded stderr still routes to the retry path rather than being
        // reported to the user as an infrastructure failure.
        || s.contains("updates were rejected")
}

fn stderr_of(out: &Output) -> String {
    one_line(&String::from_utf8_lossy(&out.stderr))
}

fn stdout_of(out: &Output) -> String {
    one_line(&String::from_utf8_lossy(&out.stdout))
}

/// Flatten git's output onto a single line.
///
/// git is chatty and multi-line on failure ("fatal: …", a blank line, then
/// three lines of advice). Dropped verbatim into a `warning:` that sprawls
/// across the terminal and buries the message that matters. Trailing
/// punctuation goes too, since callers embed this mid-sentence.
fn one_line(s: &str) -> String {
    let mut out = String::new();
    for part in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(part);
    }
    let out = out.trim_end_matches(['.', ' ']).to_string();
    if out.chars().count() > 300 {
        return out.chars().take(300).collect::<String>() + "…";
    }
    out
}

fn read_shared_file(path: &Path) -> Result<SharedState> {
    if !path.exists() {
        return Ok(SharedState::default());
    }
    let bytes = fs::read(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    parse_shared(&bytes, &path.display().to_string())
}

fn parse_shared(bytes: &[u8], what: &str) -> Result<SharedState> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(SharedState::default());
    }
    let shared: SharedState = serde_json::from_slice(bytes).map_err(|e| {
        Error::CorruptState(format!("failed to parse {what}: {e}"))
    })?;
    if shared.version > crate::state::STATE_VERSION {
        return Err(Error::CorruptState(format!(
            "{what} has state version {}, but this choochoo understands up to \
             {}; upgrade choochoo",
            shared.version,
            crate::state::STATE_VERSION
        )));
    }
    Ok(shared)
}

/// Decide where this repository's state lives, and open it.
///
/// With no `[store]` configured this is [`crate::state::Store::local`] and
/// nothing else happens — no config-dependent behaviour, no network, exactly
/// the historical layout. Otherwise the repo's identity is derived from its
/// `origin` URL and the shared store is opened (cloning on first use).
pub fn open(
    repo_root: &Path,
    config: &crate::config::Config,
    env: &crate::config::Env,
    git: &dyn crate::git::GitRunner,
) -> Result<crate::state::Store> {
    let Some(cfg) = config.store() else {
        return Ok(crate::state::Store::local(repo_root));
    };

    // Identity must be the same on every machine, so it comes from the
    // remote URL rather than the local path.
    let key = git
        .remote_url("origin")?
        .and_then(|url| crate::repoid::from_url(&url))
        .ok_or_else(|| Error::NoRepoIdentity {
            remote: "origin".to_string(),
        })?;

    let dir = crate::config::store_dir(env).ok_or_else(|| {
        Error::StoreUnavailable(
            "cannot locate a data directory; set $HOME, $XDG_DATA_HOME, or \
             $CHOOCHOO_STORE_DIR"
                .to_string(),
        )
    })?;

    let git_store = GitStore::open(cfg, dir, key, env.no_sync)?;
    let store = crate::state::Store::shared(repo_root, git_store);
    adopt_local_trains(repo_root, &store)?;
    Ok(store)
}

/// First run against a shared store: hand this repo's existing local trains
/// over to it.
///
/// Only ever *adds*: if the store already knows about this repo, the local
/// file is left alone and reported, rather than guessing which side should
/// win. The old file is renamed rather than deleted — it costs nothing to
/// keep and it's the obvious thing to want if this goes wrong.
fn adopt_local_trains(repo_root: &Path, store: &crate::state::Store) -> Result<()> {
    let legacy = crate::state::state_path(repo_root);
    if !legacy.exists() {
        return Ok(());
    }
    let local_only = crate::state::Store::local(repo_root);
    let existing = local_only.load()?;
    let adopted = legacy.with_extension("json.adopted");

    if existing.trains.is_empty() {
        fs::rename(&legacy, &adopted).map_err(|e| Error::Io {
            path: adopted,
            source: e,
        })?;
        return Ok(());
    }

    let shared = store.load()?;
    if !shared.trains.is_empty() {
        store.warn(format!(
            "{} still holds {} local train(s) but shared state already has \
             this repository; the local file is being ignored. Move any train \
             you still need across by hand, then delete it.",
            legacy.display(),
            existing.trains.len(),
        ));
        return Ok(());
    }

    store.save_described(&existing, "adopt local trains")?;
    fs::rename(&legacy, &adopted).map_err(|e| Error::Io {
        path: adopted,
        source: e,
    })?;
    store.warn(format!(
        "moved {} local train(s) into shared state; the old file is kept at {}",
        existing.trains.len(),
        legacy.with_extension("json.adopted").display(),
    ));
    Ok(())
}

/// Result of reconciling two machines' views of the same repo's trains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge3 {
    pub merged: SharedState,
    /// Trains changed on both sides, where this machine's version was kept.
    pub conflicts: Vec<String>,
}

/// Three-way merge at *train* granularity.
///
/// Per train name: if we changed it relative to `base`, ours wins;
/// otherwise theirs does. A train only one side added comes through
/// untouched, which is the property that matters — a train created on
/// another devbox is never lost just because this one pushed second.
///
/// **Modification beats deletion, deliberately.** If one machine removed a
/// train and the other edited it, the edit survives. The costs are
/// asymmetric: a resurrected train is a nuisance you undo with one
/// `choo remove` (which never touches git branches anyway), whereas a lost
/// train means not being able to find your work — the exact problem shared
/// state exists to solve.
pub fn merge3(base: &SharedState, ours: &SharedState, theirs: &SharedState) -> Merge3 {
    let mut merged = SharedState::default();
    let mut conflicts = Vec::new();

    let names: std::collections::BTreeSet<&String> = base
        .trains
        .keys()
        .chain(ours.trains.keys())
        .chain(theirs.trains.keys())
        .collect();

    for name in names {
        let b = base.trains.get(name);
        let o = ours.trains.get(name);
        let t = theirs.trains.get(name);

        let we_changed = o != b;
        let they_changed = t != b;

        let pick: Option<&Train> = match (we_changed, they_changed) {
            (false, _) => t,
            (true, false) => o,
            (true, true) => {
                if o == t {
                    o
                } else {
                    conflicts.push(name.clone());
                    // Deletion loses to modification, whichever side deleted.
                    match (o, t) {
                        (None, Some(t)) => Some(t),
                        (some_o, _) => some_o,
                    }
                }
            }
        };

        if let Some(train) = pick {
            merged.trains.insert(name.clone(), train.clone());
        }
    }

    Merge3 { merged, conflicts }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Train;

    fn shared(trains: &[(&str, &[&str])]) -> SharedState {
        let mut s = SharedState::default();
        for (name, branches) in trains {
            let mut t = Train::new(*name, "main");
            t.branches = branches.iter().map(|b| b.to_string()).collect();
            s.trains.insert((*name).to_string(), t);
        }
        s
    }

    fn names(s: &SharedState) -> Vec<&str> {
        s.trains.keys().map(String::as_str).collect()
    }

    #[test]
    fn untouched_train_takes_theirs() {
        let base = shared(&[("t", &["a"])]);
        let theirs = shared(&[("t", &["a", "b"])]);
        let m = merge3(&base, &base, &theirs);
        assert_eq!(m.merged, theirs);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn our_change_wins_when_they_did_not_touch_it() {
        let base = shared(&[("t", &["a"])]);
        let ours = shared(&[("t", &["a", "b"])]);
        let m = merge3(&base, &ours, &base);
        assert_eq!(m.merged, ours);
        assert!(m.conflicts.is_empty());
    }

    /// The headline case: two devboxes each create a train. Neither may be
    /// lost, and it isn't a conflict — they're different trains.
    #[test]
    fn trains_added_on_both_sides_all_survive() {
        let base = SharedState::default();
        let ours = shared(&[("mine", &["a"])]);
        let theirs = shared(&[("yours", &["b"])]);
        let m = merge3(&base, &ours, &theirs);
        assert_eq!(names(&m.merged), vec!["mine", "yours"]);
        assert!(m.conflicts.is_empty(), "got {:?}", m.conflicts);
    }

    #[test]
    fn identical_concurrent_edits_are_not_a_conflict() {
        let base = shared(&[("t", &["a"])]);
        let both = shared(&[("t", &["a", "b"])]);
        let m = merge3(&base, &both, &both);
        assert_eq!(m.merged, both);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn divergent_edits_to_one_train_keep_ours_and_report_it() {
        let base = shared(&[("t", &["a"])]);
        let ours = shared(&[("t", &["a", "mine"])]);
        let theirs = shared(&[("t", &["a", "yours"])]);
        let m = merge3(&base, &ours, &theirs);
        assert_eq!(m.merged, ours);
        assert_eq!(m.conflicts, vec!["t"]);
    }

    #[test]
    fn our_deletion_sticks_when_they_did_not_touch_it() {
        let base = shared(&[("t", &["a"]), ("u", &["b"])]);
        let ours = shared(&[("u", &["b"])]);
        let m = merge3(&base, &ours, &base);
        assert_eq!(names(&m.merged), vec!["u"]);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn their_deletion_sticks_when_we_did_not_touch_it() {
        let base = shared(&[("t", &["a"]), ("u", &["b"])]);
        let theirs = shared(&[("u", &["b"])]);
        let m = merge3(&base, &base, &theirs);
        assert_eq!(names(&m.merged), vec!["u"]);
        assert!(m.conflicts.is_empty());
    }

    /// Losing a train is worse than resurrecting one, so an edit beats a
    /// concurrent delete from either direction.
    #[test]
    fn modification_beats_deletion_both_ways() {
        let base = shared(&[("t", &["a"])]);
        let edited = shared(&[("t", &["a", "b"])]);
        let deleted = SharedState::default();

        let m = merge3(&base, &edited, &deleted);
        assert_eq!(m.merged, edited, "our edit must survive their delete");
        assert_eq!(m.conflicts, vec!["t"]);

        let m = merge3(&base, &deleted, &edited);
        assert_eq!(m.merged, edited, "their edit must survive our delete");
        assert_eq!(m.conflicts, vec!["t"]);
    }

    #[test]
    fn same_name_added_on_both_sides_conflicts_and_keeps_ours() {
        let base = SharedState::default();
        let ours = shared(&[("t", &["mine"])]);
        let theirs = shared(&[("t", &["yours"])]);
        let m = merge3(&base, &ours, &theirs);
        assert_eq!(m.merged, ours);
        assert_eq!(m.conflicts, vec!["t"]);
    }

    #[test]
    fn merging_identical_states_is_a_noop() {
        let s = shared(&[("t", &["a", "b"]), ("u", &["c"])]);
        let m = merge3(&s, &s, &s);
        assert_eq!(m.merged, s);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn merge_is_idempotent() {
        let base = shared(&[("t", &["a"])]);
        let ours = shared(&[("t", &["a", "b"])]);
        let theirs = shared(&[("u", &["c"])]);
        let once = merge3(&base, &ours, &theirs).merged;
        let twice = merge3(&once, &once, &once).merged;
        assert_eq!(once, twice);
    }

    #[test]
    fn everything_empty_merges_to_empty() {
        let e = SharedState::default();
        assert_eq!(merge3(&e, &e, &e).merged, e);
    }

    #[test]
    fn recognizes_the_shapes_of_a_push_rejection() {
        for s in [
            " ! [rejected]        main -> main (fetch first)",
            "error: failed to push some refs\nhint: Updates were rejected \
             because the remote contains work",
            "! [rejected] main -> main (non-fast-forward)",
            "stale info",
        ] {
            assert!(is_rejection(s), "should be a rejection: {s}");
        }
    }

    /// Auth and network failures must *not* look like a rejection, or we'd
    /// spin the retry loop instead of telling the user.
    #[test]
    fn auth_and_network_failures_are_not_rejections() {
        for s in [
            "Permission denied (publickey).",
            "fatal: could not read Username for 'https://github.com'",
            "ssh: Could not resolve hostname github.com",
            "fatal: repository not found",
        ] {
            assert!(!is_rejection(s), "should not be a rejection: {s}");
        }
    }

    #[test]
    fn blank_and_absent_entries_read_as_empty() {
        assert_eq!(parse_shared(b"", "x").unwrap(), SharedState::default());
        assert_eq!(parse_shared(b"  \n", "x").unwrap(), SharedState::default());
        assert_eq!(
            read_shared_file(Path::new("/definitely/not/here.json")).unwrap(),
            SharedState::default()
        );
    }

    #[test]
    fn entry_from_the_future_is_rejected_with_advice() {
        let err = parse_shared(br#"{"version":99,"trains":{}}"#, "x").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("upgrade choochoo"), "got: {msg}");
    }
}
