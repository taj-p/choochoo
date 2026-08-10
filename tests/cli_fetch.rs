//! `choo fetch` — getting a train's branches onto a second machine.
//!
//! The setup mirrors `cli_sync.rs`: two clones of one bare "GitHub" repo,
//! sharing one bare state repo. Machine A builds and pushes a train;
//! machine B has the metadata but none of the branches, which is exactly the
//! situation `choo fetch` exists for.

mod common;

use common::{BareRepo, TestRepo};

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git")
}

fn git_ok(dir: &std::path::Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn branches(repo: &std::path::Path) -> Vec<String> {
    git_ok(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// A pushed two-branch train on A, and a B that has the metadata only.
fn train_on_a_and_empty_b() -> (BareRepo, BareRepo, TestRepo, TestRepo) {
    let code = BareRepo::new();
    let store = BareRepo::new();

    let mut a = TestRepo::new();
    a.with_origin(&code);
    a.share_state(&store);

    a.choo_ok(["init", "my-feature"]);
    a.branch("feat/part-1", "main");
    a.commit("p1.txt");
    a.choo_ok(["add"]);
    a.branch("feat/part-2", "feat/part-1");
    a.commit("p2.txt");
    a.choo_ok(["add"]);
    a.choo_ok(["push"]);

    let mut b = TestRepo::clone_of(&code);
    git_ok(b.path(), &["fetch", "-q", "origin"]);
    git_ok(b.path(), &["checkout", "-q", "-b", "main", "origin/main"]);
    b.share_state(&store);

    (code, store, a, b)
}

#[test]
fn fetch_creates_every_branch_in_the_train() {
    let (_code, _store, _a, b) = train_on_a_and_empty_b();

    // B knows about the train but has none of its branches.
    assert!(stdout(&b.choo_ok(["list"])).contains("my-feature"));
    assert_eq!(branches(b.path()), vec!["main"]);

    let out = b.choo_ok(["fetch", "my-feature"]);
    assert!(
        stdout(&out).contains("created 2"),
        "got: {}",
        stdout(&out)
    );

    let mut got = branches(b.path());
    got.sort();
    assert_eq!(got, vec!["feat/part-1", "feat/part-2", "main"]);
}

/// The reason `choo fetch` uses `git branch --track` and not `checkout -b`:
/// pulling a ten-branch train must not move you off what you're working on.
#[test]
fn fetch_does_not_move_the_working_tree() {
    let (_code, _store, _a, b) = train_on_a_and_empty_b();
    let before = git_ok(b.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
    b.choo_ok(["fetch", "my-feature"]);
    let after = git_ok(b.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(before, after, "fetch must leave HEAD alone");
    assert_eq!(after, "main");
}

/// Created branches must track their remote counterpart, so plain `git pull`
/// and `git status` work afterwards without arguments.
#[test]
fn fetched_branches_track_the_remote() {
    let (_code, _store, _a, b) = train_on_a_and_empty_b();
    b.choo_ok(["fetch", "my-feature"]);

    for branch in ["feat/part-1", "feat/part-2"] {
        assert_eq!(
            git_ok(b.path(), &["config", "--get", &format!("branch.{branch}.remote")]),
            "origin"
        );
        assert_eq!(
            git_ok(b.path(), &["config", "--get", &format!("branch.{branch}.merge")]),
            format!("refs/heads/{branch}")
        );
    }
}

/// After fetching, the whole stack is actually usable: checking out the tip
/// gives you every change in the train.
#[test]
fn a_fetched_train_is_checkoutable_and_carries_the_changes() {
    let (_code, _store, _a, b) = train_on_a_and_empty_b();
    b.choo_ok(["fetch", "my-feature"]);
    b.choo_ok(["switch", "my-feature"]);
    b.choo_ok(["checkout", "feat/part-2"]);

    assert_eq!(b.current_branch(), "feat/part-2");
    assert!(b.path().join("p1.txt").exists(), "missing part-1's change");
    assert!(b.path().join("p2.txt").exists(), "missing part-2's change");
}

/// A branch that exists locally holds work we may not have pushed, so it is
/// reported, never moved.
#[test]
fn fetch_leaves_an_existing_local_branch_untouched() {
    let (_code, _store, _a, b) = train_on_a_and_empty_b();

    // B already has `feat/part-1`, sitting on its own unpushed commit.
    git_ok(b.path(), &["fetch", "-q", "origin"]);
    git_ok(b.path(), &["branch", "feat/part-1", "origin/feat/part-1"]);
    git_ok(b.path(), &["checkout", "-q", "feat/part-1"]);
    std::fs::write(b.path().join("mine.txt"), "mine\n").unwrap();
    git_ok(b.path(), &["add", "mine.txt"]);
    git_ok(b.path(), &["commit", "-q", "-m", "local work"]);
    let mine = git_ok(b.path(), &["rev-parse", "feat/part-1"]);
    git_ok(b.path(), &["checkout", "-q", "main"]);

    b.choo_ok(["fetch", "my-feature"]);

    assert_eq!(
        git_ok(b.path(), &["rev-parse", "feat/part-1"]),
        mine,
        "fetch must not move a branch that already exists locally"
    );
    // The other branch still arrived.
    assert!(branches(b.path()).contains(&"feat/part-2".to_string()));
}

/// A train whose branches were never pushed can't be used here. Say so with
/// a non-zero exit — but still create everything that *was* available.
#[test]
fn fetch_reports_branches_that_were_never_pushed() {
    let code = BareRepo::new();
    let store = BareRepo::new();

    let mut a = TestRepo::new();
    a.with_origin(&code);
    a.share_state(&store);
    a.choo_ok(["init", "half-pushed"]);
    a.branch("pushed", "main");
    a.commit("one.txt");
    a.choo_ok(["add"]);
    a.choo_ok(["push"]);
    // A second branch that only ever existed on A.
    a.branch("never-pushed", "pushed");
    a.commit("two.txt");
    a.choo_ok(["add"]);

    let mut b = TestRepo::clone_of(&code);
    git_ok(b.path(), &["fetch", "-q", "origin"]);
    git_ok(b.path(), &["checkout", "-q", "-b", "main", "origin/main"]);
    b.share_state(&store);

    let out = b.choo_try(["fetch", "half-pushed"]);
    assert!(!out.status.success(), "an unusable train should exit non-zero");
    let err = stderr(&out);
    assert!(err.contains("never-pushed"), "got: {err}");

    // But `pushed` was still created — partial progress isn't discarded.
    assert!(
        branches(b.path()).contains(&"pushed".to_string()),
        "got: {:?}",
        branches(b.path())
    );
}

/// `choo checkout` alone recovers a single branch, without a whole fetch.
#[test]
fn checkout_creates_a_missing_branch_from_the_remote() {
    let (_code, _store, _a, b) = train_on_a_and_empty_b();
    b.choo_ok(["switch", "my-feature"]);
    git_ok(b.path(), &["fetch", "-q", "origin"]);

    assert!(!branches(b.path()).contains(&"feat/part-1".to_string()));
    let out = b.choo_ok(["checkout", "feat/part-1"]);
    assert!(
        stderr(&out).contains("creating `feat/part-1`"),
        "should say what it did: {}",
        stderr(&out)
    );
    assert_eq!(b.current_branch(), "feat/part-1");
    assert!(b.path().join("p1.txt").exists());
}

/// A branch in the train that exists nowhere gets a clear explanation, not a
/// raw git error.
#[test]
fn checkout_of_a_branch_that_exists_nowhere_explains_itself() {
    let code = BareRepo::new();
    let store = BareRepo::new();

    let mut a = TestRepo::new();
    a.with_origin(&code);
    a.share_state(&store);
    a.choo_ok(["init", "t"]);
    a.branch("only-here", "main");
    a.commit("x.txt");
    a.choo_ok(["add"]);

    let mut b = TestRepo::clone_of(&code);
    git_ok(b.path(), &["fetch", "-q", "origin"]);
    git_ok(b.path(), &["checkout", "-q", "-b", "main", "origin/main"]);
    b.share_state(&store);
    b.choo_ok(["switch", "t"]);

    let out = b.choo_try(["checkout", "only-here"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("only-here"), "got: {err}");
    assert!(
        err.contains("neither locally nor on"),
        "should explain rather than leak a git error: {err}"
    );
}

/// The aggregate branch is part of the train's shape, so it comes too.
#[test]
fn fetch_brings_the_combined_branch_along() {
    let code = BareRepo::new();
    let store = BareRepo::new();

    let mut a = TestRepo::new();
    a.with_origin(&code);
    a.share_state(&store);
    a.choo_ok(["init", "combo", "--aggregate"]);
    a.branch("one", "main");
    a.commit("one.txt");
    a.choo_ok(["add"]);
    a.checkout("main");
    a.choo_ok(["push"]);

    let mut b = TestRepo::clone_of(&code);
    git_ok(b.path(), &["fetch", "-q", "origin"]);
    git_ok(b.path(), &["checkout", "-q", "-b", "main", "origin/main"]);
    b.share_state(&store);

    b.choo_ok(["fetch", "combo"]);
    assert!(
        branches(b.path()).contains(&"choo/combo/combined".to_string()),
        "got: {:?}",
        branches(b.path())
    );
}

/// `choo fetch` works without shared state configured at all — it's useful
/// any time a train's branches are on the remote but not here.
#[test]
fn fetch_works_without_shared_state() {
    let code = BareRepo::new();
    let a = TestRepo::new();
    a.with_origin(&code);

    a.choo_ok(["init", "t"]);
    a.branch("b1", "main");
    a.commit("b1.txt");
    a.choo_ok(["add"]);
    a.choo_ok(["push"]);

    // Drop the local branch, then get it back.
    a.checkout("main");
    git_ok(a.path(), &["branch", "-qD", "b1"]);
    let out = a.choo_ok(["fetch", "t"]);
    assert!(stdout(&out).contains("created 1"), "got: {}", stdout(&out));
    assert!(branches(a.path()).contains(&"b1".to_string()));
}
