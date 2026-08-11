//! End-to-end tests for `choo init`, `choo add`, `choo remove`, `choo list`,
//! `choo show`, `choo switch`.

mod common;
use common::{BareRepo, TestRepo};
use predicates::prelude::*;

#[test]
fn list_with_no_trains_says_so() {
    let repo = TestRepo::new();
    let out = repo.choo_ok(["list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("no trains"));
}

#[test]
fn init_defaults_to_main_with_no_config() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat"]);
    let out = repo.choo_ok(["list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("base=main"));
}

/// The per-repo setting someone configures for a repo whose trunk isn't
/// `main`: `choo init` with no `--base` has to pick it up.
#[test]
fn init_uses_the_base_configured_for_this_repo() {
    let origin = BareRepo::new();
    let mut repo = TestRepo::new();
    repo.with_origin(&origin);
    repo.set_config(&format!(
        "[repo.\"{}\"]\nbase = \"master\"\n",
        origin.url()
    ));

    repo.choo_ok(["init", "feat"]);
    let out = repo.choo_ok(["list"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("base=master"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn explicit_base_flag_beats_the_configured_one() {
    let origin = BareRepo::new();
    let mut repo = TestRepo::new();
    repo.with_origin(&origin);
    repo.set_config(&format!(
        "[repo.\"{}\"]\nbase = \"master\"\n",
        origin.url()
    ));

    repo.choo_ok(["init", "feat", "--base", "develop"]);
    let out = repo.choo_ok(["list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("base=develop"));
}

/// An entry for someone else's repo must not leak into this one.
#[test]
fn config_for_another_repo_does_not_apply() {
    let origin = BareRepo::new();
    let mut repo = TestRepo::new();
    repo.with_origin(&origin);
    repo.set_config(
        "[repo.\"https://github.com/someone/else\"]\nbase = \"master\"\n",
    );

    repo.choo_ok(["init", "feat"]);
    let out = repo.choo_ok(["list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("base=main"));
}

/// A configured repo you happen to be working in without an `origin` is not
/// an error — it just can't be identified, so the default stands.
#[test]
fn a_repo_with_no_origin_still_inits() {
    let mut repo = TestRepo::new();
    repo.set_config(
        "[repo.\"https://github.com/Canva/canva\"]\nbase = \"master\"\n",
    );

    repo.choo_ok(["init", "feat"]);
    let out = repo.choo_ok(["list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("base=main"));
}

/// Two spellings of one repo would leave one silently ignored, so the file
/// is rejected outright rather than half-applied.
#[test]
fn duplicate_repo_entries_are_reported() {
    let mut repo = TestRepo::new();
    repo.set_config(
        "[repo.\"https://github.com/Canva/canva\"]\nbase = \"master\"\n\
         [repo.\"git@github.com:canva/canva.git\"]\nbase = \"main\"\n",
    );

    let out = repo.choo_try(["list"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("same repository"), "got: {stderr}");
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
fn show_json_chains_each_branch_to_its_parent() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    repo.branch("feature/a", "main");
    repo.commit("a.txt");
    repo.choo_ok(["add"]);
    repo.branch("feature/b", "feature/a");
    repo.commit("b.txt");
    repo.choo_ok(["add"]);

    let out = repo.choo_ok(["show", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["base"], "main");
    assert_eq!(v["branches"][0]["branch"], "feature/a");
    assert_eq!(v["branches"][0]["parent"], "main");
    assert_eq!(v["branches"][1]["branch"], "feature/b");
    assert_eq!(v["branches"][1]["parent"], "feature/a");
    // The aggregate holds the whole train, so it diffs against the base.
    assert_eq!(v["aggregate"]["parent"], "main");
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
