//! End-to-end tests for `choo init`, `choo add`, `choo remove`, `choo list`,
//! `choo show`, `choo switch`.

mod common;
use common::TestRepo;
use predicates::prelude::*;

#[test]
fn list_with_no_trains_says_so() {
    let repo = TestRepo::new();
    let out = repo.choo_ok(["list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("no trains"));
}

#[test]
fn init_creates_train_and_makes_active() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);
    let out = repo.choo_ok(["list"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("* feat"));
    assert!(s.contains("base=main"));
}

#[test]
fn init_duplicate_train_fails() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    let out = repo.choo().args(["init", "feat"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("already exists"));
}

#[test]
fn add_uses_current_branch_by_default() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    repo.branch("feature/a", "main");
    repo.commit("a.txt");
    repo.choo_ok(["add"]);
    let out = repo.choo_ok(["show"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("feature/a"));
}

#[test]
fn add_explicit_branch_appends() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    repo.branch("feature/a", "main");
    repo.commit("a.txt");
    repo.checkout("main");
    repo.choo_ok(["add", "feature/a"]);
    let out = repo.choo_ok(["show"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("feature/a"));
}

#[test]
fn add_unknown_branch_errors() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    let out = repo.choo().args(["add", "no-such-branch"]).output().unwrap();
    assert!(!out.status.success());
    assert!(predicate::str::contains("does not exist locally")
        .eval(&String::from_utf8_lossy(&out.stderr)));
}

#[test]
fn remove_takes_branch_out_of_train() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    repo.branch("feature/a", "main");
    repo.commit("a.txt");
    repo.choo_ok(["add"]);
    repo.choo_ok(["remove", "feature/a"]);
    let out = repo.choo_ok(["show"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("no branches yet"));
}

#[test]
fn move_reorders_branches_in_train() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    repo.branch("a", "main");
    repo.commit("a.txt");
    repo.branch("b", "a");
    repo.commit("b.txt");
    repo.branch("c", "b");
    repo.commit("c.txt");
    repo.choo_ok(["add", "a"]);
    repo.choo_ok(["add", "b"]);
    repo.choo_ok(["add", "c"]);

    repo.choo_ok(["move", "c", "--before", "b"]);
    let out = repo.choo_ok(["show"]);
    let s = String::from_utf8_lossy(&out.stdout);
    let pos_c = s.find("c").unwrap();
    let pos_b = s.find("b").unwrap();
    assert!(pos_c < pos_b, "expected c to come before b in:\n{s}");
}

#[test]
fn switch_changes_active_train() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "first"]);
    repo.choo_ok(["init", "second"]);
    repo.choo_ok(["switch", "second"]);
    let out = repo.choo_ok(["list"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("* second"));
    assert!(s.contains("  first"));
}

#[test]
fn switch_unknown_train_fails() {
    let repo = TestRepo::new();
    let out = repo.choo().args(["switch", "ghost"]).output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn show_explicit_train_overrides_active() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "first", "--base", "main"]);
    repo.choo_ok(["init", "second", "--base", "main"]);
    let out = repo.choo_ok(["show", "second"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Train: second"));
}
