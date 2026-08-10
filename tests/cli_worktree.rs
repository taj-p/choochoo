//! End-to-end tests for `choo` run from a linked git worktree.
//!
//! A worktree's `.git` is a *file* pointing into the main checkout's git
//! directory, so anything that builds a path under `<root>/.git` names a
//! location that cannot exist. Trains used to be stored that way, which
//! made `choo list` report "no trains" from every worktree — state that
//! looked lost but was only unreachable. State now hangs off the git
//! *common* directory, which every worktree of a repo shares.

mod common;
use common::TestRepo;

#[test]
fn worktree_sees_trains_from_the_main_checkout() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);
    repo.branch("a", "main");
    repo.commit("a.txt");
    repo.choo_ok(["add", "a"]);

    let wt = repo.worktree("wt-branch", "main");
    let out = repo
        .choo_from(wt.path())
        .arg("list")
        .output()
        .expect("run choo");
    assert!(
        out.status.success(),
        "choo list failed in worktree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("feat"),
        "worktree should see the train `feat`, got: {stdout}"
    );
}

#[test]
fn train_created_in_a_worktree_is_visible_from_the_main_checkout() {
    let repo = TestRepo::new();
    let wt = repo.worktree("wt-branch", "main");

    let out = repo
        .choo_from(wt.path())
        .args(["init", "from-wt", "--base", "main"])
        .output()
        .expect("run choo");
    assert!(
        out.status.success(),
        "choo init failed in worktree: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Written to the shared location, not to a phantom path under the
    // worktree's `.git` file.
    assert!(
        repo.path().join(".git/choochoo/state.json").exists(),
        "state should live in the main checkout's git dir"
    );
    assert!(
        !wt.path().join(".git/choochoo").exists(),
        "nothing should be written under the worktree's `.git` file"
    );

    let stdout = String::from_utf8_lossy(&repo.choo_ok(["list"]).stdout).to_string();
    assert!(
        stdout.contains("from-wt"),
        "main checkout should see the train, got: {stdout}"
    );
}

#[test]
fn rebase_conflict_in_a_worktree_is_reported_as_a_conflict() {
    let repo = TestRepo::new();
    let wt = repo.worktree("wt-branch", "main");

    // Build the conflict entirely inside the worktree: branch `a` writes X
    // to a file that `main` then advances to Y.
    let choo = |args: &[&str]| {
        let out = repo
            .choo_from(wt.path())
            .args(args)
            .output()
            .expect("run choo");
        (out.status.success(), out)
    };
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(wt.path())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    assert!(choo(&["init", "feat", "--base", "main"]).0);
    git(&["checkout", "-q", "-b", "a", "main"]);
    std::fs::write(wt.path().join("conflict.txt"), "X\n").unwrap();
    git(&["add", "conflict.txt"]);
    git(&["commit", "-q", "-m", "a-version"]);
    assert!(choo(&["add", "a"]).0);

    // Advance main in the main checkout with conflicting content.
    std::fs::write(repo.path().join("conflict.txt"), "Y\n").unwrap();
    repo.commit("conflict.txt");

    let (ok, out) = choo(&["rebase"]);
    assert!(!ok, "expected rebase to stop on the conflict");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("rebase conflict") && stderr.contains("`a`"),
        "a conflict in a worktree must be reported as one, got: {stderr}"
    );

    // And the progress file lands somewhere the next command can find it.
    assert!(
        repo.path()
            .join(".git/choochoo/rebase-progress.json")
            .exists(),
        "rebase progress should be written to the shared state dir"
    );
    assert!(choo(&["rebase", "--abort"]).0);
}
