//! End-to-end tests for `choo rebase`.
//!
//! These run real `git rebase` inside a tempdir-backed repo. They cover:
//! the happy path with a base advance, a conflict that pauses choo's loop,
//! and `--abort`.

mod common;
use common::TestRepo;

/// Build a 3-branch train with content that will *not* conflict.
fn three_branch_train(repo: &TestRepo) {
    repo.choo_ok(["init", "feat", "--base", "main"]);
    repo.branch("a", "main");
    repo.commit("a.txt");
    repo.branch("b", "a");
    repo.commit("b.txt");
    repo.branch("c", "b");
    repo.commit("c.txt");
    repo.choo_ok(["add", "a"]);
    repo.choo_ok(["add", "b"]);
    repo.choo_ok(["add", "c"]);
}

#[test]
fn rebase_when_base_advances_brings_branches_along() {
    let repo = TestRepo::new();
    three_branch_train(&repo);

    // Capture old tips.
    let old_a = repo.rev_parse("a");
    let _old_b = repo.rev_parse("b");
    let _old_c = repo.rev_parse("c");

    // Advance main with a non-conflicting commit.
    repo.checkout("main");
    repo.commit("MAIN_NEW.txt");
    let new_main = repo.rev_parse("main");

    repo.choo_ok(["rebase"]);

    // Each rebased branch must descend from the new main tip.
    for branch in ["a", "b", "c"] {
        let head = repo.rev_parse(branch);
        assert_ne!(
            head,
            old_a,
            "branch {branch} should have moved (old a was {old_a}, head now {head})"
        );
        let merge_base_with_main = std::process::Command::new("git")
            .args(["merge-base", branch, &new_main])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let mb = String::from_utf8_lossy(&merge_base_with_main.stdout)
            .trim()
            .to_string();
        assert_eq!(mb, new_main, "branch `{branch}` is not on top of new main");
    }
}

#[test]
fn rebase_with_conflict_reports_branch_and_writes_progress() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);

    // a touches conflict.txt with content X.
    repo.branch("a", "main");
    std::fs::write(repo.path().join("conflict.txt"), "X\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "conflict.txt"])
        .current_dir(repo.path())
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-q", "-m", "a-version"])
        .current_dir(repo.path())
        .status()
        .unwrap();
    repo.choo_ok(["add", "a"]);

    // Advance main with conflicting content Y on the same file.
    repo.checkout("main");
    std::fs::write(repo.path().join("conflict.txt"), "Y\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "conflict.txt"])
        .current_dir(repo.path())
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-q", "-m", "main-version"])
        .current_dir(repo.path())
        .status()
        .unwrap();

    // Rebase should fail with a conflict pointing at branch `a`.
    let out = repo.choo().arg("rebase").output().unwrap();
    assert!(!out.status.success(), "expected rebase to error on conflict");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("rebase conflict") && stderr.contains("`a`"),
        "stderr was: {stderr}"
    );

    // Progress file written.
    let progress = repo.path().join(".git/choochoo/rebase-progress.json");
    assert!(progress.exists());

    // Abort cleans up.
    repo.choo_ok(["rebase", "--abort"]);
    assert!(!progress.exists());
}

#[test]
fn rebase_abort_outside_rebase_is_noop() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    let out = repo.choo_ok(["rebase", "--abort"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("aborted"));
}
