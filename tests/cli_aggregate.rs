//! End-to-end tests for the aggregate ("combined") branch: the extra
//! branch that holds every change in a train, plus its draft PR against
//! the train's base.

mod common;
use common::TestRepo;
use std::process::Command;

const COMBINED: &str = "choo/feat/combined";

fn three_branch_train(repo: &TestRepo) {
    repo.branch("a", "main");
    repo.commit("a.txt");
    repo.branch("b", "a");
    repo.commit("b.txt");
    repo.branch("c", "b");
    repo.commit("c.txt");
    repo.choo_ok(["add", "a"]);
    repo.choo_ok(["add", "b"]);
    repo.choo_ok(["add", "c"]);
    // Sit on `main` so nothing that has to move is checked out.
    repo.checkout("main");
}

fn git(repo: &TestRepo, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(repo.path())
        .output()
        .expect("run git")
}

fn branch_exists(repo: &TestRepo, branch: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "--quiet", branch])
        .status
        .success()
}

fn state_json(repo: &TestRepo) -> serde_json::Value {
    let raw =
        std::fs::read_to_string(repo.path().join(".git/choochoo/state.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

/// Files that differ between `main` and `branch`.
fn changed_files(repo: &TestRepo, branch: &str) -> Vec<String> {
    let out = git(repo, &["diff", "--name-only", "main", branch]);
    let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    files.sort();
    files
}

#[test]
fn init_with_aggregate_records_the_default_branch_name() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    let state = state_json(&repo);
    assert_eq!(state["trains"]["feat"]["aggregate"]["branch"], COMBINED);
    // An empty train has no tip to mirror yet, so no git branch is made.
    assert!(!branch_exists(&repo, COMBINED));
}

#[test]
fn init_with_explicit_aggregate_branch_name() {
    let repo = TestRepo::new();
    repo.choo_ok([
        "init",
        "feat",
        "--base",
        "main",
        "--aggregate-branch",
        "everything",
    ]);
    let state = state_json(&repo);
    assert_eq!(state["trains"]["feat"]["aggregate"]["branch"], "everything");
}

#[test]
fn combined_branch_holds_every_change_in_the_train() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);

    assert!(branch_exists(&repo, COMBINED));
    // Same commit as the tip, so the diff against `main` is the whole train.
    assert_eq!(repo.rev_parse(COMBINED), repo.rev_parse("c"));
    assert_eq!(
        changed_files(&repo, COMBINED),
        vec!["a.txt", "b.txt", "c.txt"]
    );
}

#[test]
fn enable_on_an_existing_train_syncs_immediately() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);
    three_branch_train(&repo);
    assert!(!branch_exists(&repo, COMBINED));

    repo.choo_ok(["aggregate", "enable"]);
    assert_eq!(repo.rev_parse(COMBINED), repo.rev_parse("c"));
}

#[test]
fn sync_follows_the_tip_when_a_branch_is_appended() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);

    repo.branch("d", "c");
    repo.commit("d.txt");
    repo.choo_ok(["add", "d"]);
    repo.checkout("main");
    let out = repo.choo_ok(["aggregate", "sync"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("tip of `d`"));
    assert_eq!(repo.rev_parse(COMBINED), repo.rev_parse("d"));
    assert_eq!(
        changed_files(&repo, COMBINED),
        vec!["a.txt", "b.txt", "c.txt", "d.txt"]
    );
}

#[test]
fn sync_reports_when_already_current() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);
    let out = repo.choo_ok(["aggregate", "sync"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("already current"));
}

#[test]
fn sync_refuses_while_the_combined_branch_is_checked_out_and_stale() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);

    // Tip advances while the user is sitting on the combined branch.
    repo.checkout("c");
    repo.commit("c2.txt");
    repo.checkout(COMBINED);
    let out = repo.choo().args(["aggregate", "sync"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("checked out"),
        "expected a clear diagnostic; stderr={stderr}"
    );
    // Working tree untouched: still on the combined branch at the old SHA.
    assert_eq!(repo.current_branch(), COMBINED);
}

#[test]
fn pr_opens_a_draft_combined_pr_against_the_base() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);
    let out = repo.choo_ok(["pr"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("created 4"), "got: {stdout}");
    assert!(stdout.contains("combined draft PR: #4"), "got: {stdout}");

    let gh = repo.fake_gh_state().unwrap();
    let combined = &gh["prs"][COMBINED];
    // Targets the base branch, not a train branch, and is a draft.
    assert_eq!(combined["base"], "main");
    assert_eq!(combined["draft"], true);
    assert_eq!(combined["number"], 4);
    assert_eq!(combined["title"], "Combined: feat");
    // The per-branch PRs still stack on each other.
    assert_eq!(gh["prs"]["a"]["base"], "main");
    assert_eq!(gh["prs"]["c"]["base"], "b");
}

#[test]
fn every_pr_table_lists_the_combined_row() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);
    repo.choo_ok(["pr"]);

    let gh = repo.fake_gh_state().unwrap();
    for branch in ["a", "b", "c", COMBINED] {
        let body = gh["prs"][branch]["body"].as_str().unwrap();
        assert!(
            body.contains("| Σ | Combined: feat | #4 |"),
            "PR for `{branch}` is missing the combined row:\n{body}"
        );
        assert!(
            body.contains(&format!("combined branch `{COMBINED}`")),
            "PR for `{branch}` is missing the legend:\n{body}"
        );
    }
    // Only the combined PR marks the combined row as "this PR".
    let combined_body = gh["prs"][COMBINED]["body"].as_str().unwrap();
    assert!(combined_body.contains("| Σ | Combined: feat | #4 | **this PR** |"));
    let body_a = gh["prs"]["a"]["body"].as_str().unwrap();
    assert!(body_a.contains("| 1 | a | #1 | **this PR** |"), "got: {body_a}");
}

#[test]
fn pr_stays_idempotent_with_a_combined_pr() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);
    repo.choo_ok(["pr"]);

    let before = repo.fake_gh_state().unwrap();
    let out = repo.choo_ok(["pr"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("created 0, updated 0"), "got: {stdout}");
    assert_eq!(before, repo.fake_gh_state().unwrap());
}

#[test]
fn enabling_the_aggregate_later_backfills_existing_pr_descriptions() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);
    three_branch_train(&repo);
    repo.choo_ok(["pr"]);
    let gh = repo.fake_gh_state().unwrap();
    assert!(!gh["prs"]["a"]["body"].as_str().unwrap().contains("Σ"));

    repo.choo_ok(["aggregate", "enable"]);
    let out = repo.choo_ok(["pr"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("created 1"), "got: {stdout}");
    // The three existing descriptions get the new row, and the brand-new
    // combined PR's own body is re-rendered once its number is known.
    assert!(stdout.contains("updated 4"), "got: {stdout}");

    let gh = repo.fake_gh_state().unwrap();
    for branch in ["a", "b", "c"] {
        assert!(gh["prs"][branch]["body"].as_str().unwrap().contains("| Σ |"));
    }
}

#[test]
fn disable_leaves_the_branch_and_pr_but_drops_them_from_the_tables() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);
    repo.choo_ok(["pr"]);

    let out = repo.choo_ok(["aggregate", "disable"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("no longer managed"));
    assert!(state_json(&repo)["trains"]["feat"]["aggregate"].is_null());
    // Branch survives, like `choo remove` never deleting git branches.
    assert!(branch_exists(&repo, COMBINED));

    repo.choo_ok(["pr"]);
    let gh = repo.fake_gh_state().unwrap();
    // The combined PR still exists on "GitHub" but is no longer referenced.
    assert!(gh["prs"][COMBINED]["number"].is_u64());
    for branch in ["a", "b", "c"] {
        assert!(!gh["prs"][branch]["body"].as_str().unwrap().contains("| Σ |"));
    }
}

#[test]
fn sync_without_enabling_first_errors() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);
    three_branch_train(&repo);
    let out = repo.choo().args(["aggregate", "sync"]).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("aggregate enable"),
        "expected a pointer to `choo aggregate enable`"
    );
}

#[test]
fn the_combined_branch_cannot_be_added_to_the_train() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);

    let out = repo.choo().args(["add", COMBINED]).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("combined branch"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn show_lists_the_combined_branch() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);
    repo.choo_ok(["pr"]);

    let out = repo.choo_ok(["show"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("Combined: {COMBINED}  [draft #4")),
        "got: {stdout}"
    );
    assert!(stdout.contains("targets main"), "got: {stdout}");
}

#[test]
fn rebase_carries_the_combined_branch_to_the_new_tip() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);
    repo.choo_ok(["aggregate", "sync"]);
    let before = repo.rev_parse(COMBINED);

    // `main` advances underneath the train.
    repo.checkout("main");
    repo.commit("upstream.txt");
    let out = repo.choo_ok(["rebase"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("synced to tip"), "got: {stdout}");

    assert_ne!(repo.rev_parse(COMBINED), before, "combined branch is stale");
    assert_eq!(repo.rev_parse(COMBINED), repo.rev_parse("c"));
    // Still the whole train, now on top of the new `main`.
    assert_eq!(
        changed_files(&repo, COMBINED),
        vec!["a.txt", "b.txt", "c.txt"]
    );
}

#[test]
fn push_syncs_and_pushes_the_combined_branch_last() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main", "--aggregate"]);
    three_branch_train(&repo);

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

    // Note: never synced explicitly — `choo push` does it.
    let out = repo.choo_ok(["push", "--no-force-with-lease"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("pushed combined branch `{COMBINED}`")),
        "got: {stdout}"
    );

    let remote_sha = Command::new("git")
        .args(["rev-parse", &format!("refs/heads/{COMBINED}")])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();
    assert!(remote_sha.status.success(), "combined branch not pushed");
    assert_eq!(
        String::from_utf8_lossy(&remote_sha.stdout).trim(),
        repo.rev_parse("c"),
        "remote combined branch should match the train tip"
    );
}
