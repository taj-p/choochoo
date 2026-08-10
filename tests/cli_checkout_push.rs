//! End-to-end tests for `choo checkout` and `choo push`.

mod common;
use common::TestRepo;
use std::process::Command;

fn three_branch_train(repo: &TestRepo) {
    repo.choo_ok(["init", "feat", "--base", "main"]);
    repo.branch("a", "main");
    repo.commit("a.txt");
    repo.branch("b", "a");
    repo.commit("b.txt");
    repo.choo_ok(["add", "a"]);
    repo.choo_ok(["add", "b"]);
}

#[test]
fn checkout_switches_branches() {
    let repo = TestRepo::new();
    three_branch_train(&repo);
    repo.choo_ok(["checkout", "a"]);
    assert_eq!(repo.current_branch(), "a");
    repo.choo_ok(["checkout", "b"]);
    assert_eq!(repo.current_branch(), "b");
}

#[test]
fn checkout_branch_not_in_train_fails() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    let out = repo.choo().args(["checkout", "ghost"]).output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn push_runs_against_local_bare_remote() {
    let repo = TestRepo::new();
    three_branch_train(&repo);

    // Set up a bare remote so `git push` can succeed.
    let remote_dir = tempfile::TempDir::new().unwrap();
    Command::new("git")
        .args(["init", "-q", "--bare", "--initial-branch=main"])
        .arg(remote_dir.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["remote", "add", "origin"])
        .arg(remote_dir.path())
        .current_dir(repo.path())
        .status()
        .unwrap();

    // First push needs to be non-force-with-lease since the remote has no
    // record of our branches yet.
    let out = repo
        .choo()
        .args(["push", "--no-force-with-lease"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "push failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // One `git push` carried the whole train: the progress log has a
    // single batched line, not one per branch. Real git against a real
    // (local) remote, so this also proves `--atomic` is accepted.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pushing 2 branches") && stderr.contains("atomic"),
        "expected one batched atomic push; stderr={stderr}"
    );
    assert!(
        !stderr.contains("does not support atomic push"),
        "did not expect the sequential fallback; stderr={stderr}"
    );

    // Branches present in the bare remote.
    for branch in ["a", "b"] {
        let s = Command::new("git")
            .args(["rev-parse", &format!("refs/heads/{branch}")])
            .current_dir(remote_dir.path())
            .output()
            .unwrap();
        assert!(s.status.success(), "branch {branch} not pushed");
    }

    // Upstream tracking is configured for every pushed branch.
    for branch in ["a", "b"] {
        let remote = Command::new("git")
            .args(["config", &format!("branch.{branch}.remote")])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&remote.stdout).trim(),
            "origin",
            "branch {branch} did not get its upstream remote set"
        );
        let merge = Command::new("git")
            .args(["config", &format!("branch.{branch}.merge")])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&merge.stdout).trim(),
            format!("refs/heads/{branch}"),
            "branch {branch} did not get its upstream ref set"
        );
    }
}

/// Set up a repo + bare remote, sync, then have *another writer* push
/// a divergent commit to the remote behind our back. A subsequent
/// default (`--force-with-lease`) push must refuse; `--without-lease`
/// must succeed.
#[test]
fn push_without_lease_overwrites_remote_when_lease_would_fail() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    repo.branch("a", "main");
    repo.commit("a.txt");
    repo.choo_ok(["add", "a"]);

    let remote_dir = tempfile::TempDir::new().unwrap();
    Command::new("git")
        .args(["init", "-q", "--bare", "--initial-branch=main"])
        .arg(remote_dir.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["remote", "add", "origin"])
        .arg(remote_dir.path())
        .current_dir(repo.path())
        .status()
        .unwrap();

    // Initial push so the remote tracks `a` and our local has a
    // remote-tracking ref pointing at it.
    let out = repo
        .choo()
        .args(["push", "--no-force-with-lease"])
        .output()
        .unwrap();
    assert!(out.status.success(), "initial push failed");

    // Simulate a teammate (= different clone) pushing a different
    // commit on top of `a`, behind our back.
    let other_dir = tempfile::TempDir::new().unwrap();
    Command::new("git")
        .args(["clone", "-q"])
        .arg(remote_dir.path())
        .arg(other_dir.path())
        .status()
        .unwrap();
    let teammate = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(other_dir.path())
            .status()
            .unwrap();
    };
    teammate(&["config", "user.email", "other@example.invalid"]);
    teammate(&["config", "user.name", "Other"]);
    teammate(&["config", "commit.gpgsign", "false"]);
    teammate(&["checkout", "-q", "-b", "a", "origin/a"]);
    std::fs::write(other_dir.path().join("teammate.txt"), "x\n").unwrap();
    teammate(&["add", "teammate.txt"]);
    teammate(&["commit", "-q", "-m", "teammate commit"]);
    teammate(&["push", "-q", "origin", "a"]);

    // Now amend our local commit so it conflicts with what's on the
    // remote — this is the classic "lease should refuse" scenario.
    Command::new("git")
        .args(["commit", "--amend", "-q", "--no-edit"])
        .current_dir(repo.path())
        .status()
        .unwrap();

    // Default (force-with-lease) push must refuse.
    let lease_attempt = repo.choo().args(["push"]).output().unwrap();
    assert!(
        !lease_attempt.status.success(),
        "expected default lease push to refuse; stdout={} stderr={}",
        String::from_utf8_lossy(&lease_attempt.stdout),
        String::from_utf8_lossy(&lease_attempt.stderr),
    );

    // --without-lease must succeed.
    let force_attempt = repo
        .choo()
        .args(["push", "--without-lease"])
        .output()
        .unwrap();
    assert!(
        force_attempt.status.success(),
        "expected --without-lease push to succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&force_attempt.stdout),
        String::from_utf8_lossy(&force_attempt.stderr),
    );

    // Stderr progress line should label the push mode.
    let stderr = String::from_utf8_lossy(&force_attempt.stderr);
    assert!(
        stderr.contains("force (no lease)"),
        "expected progress to label mode; stderr={stderr}",
    );

    // Remote now points at our amended commit (teammate's overwritten).
    let local_sha = Command::new("git")
        .args(["rev-parse", "a"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    let remote_sha = Command::new("git")
        .args(["rev-parse", "refs/heads/a"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&local_sha.stdout),
        String::from_utf8_lossy(&remote_sha.stdout),
        "remote should now match the amended local SHA"
    );
}

#[test]
fn push_without_lease_conflicts_with_no_force_with_lease() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    let out = repo
        .choo()
        .args(["push", "--without-lease", "--no-force-with-lease"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "expected clap conflict diagnostic; stderr={stderr}"
    );
}
