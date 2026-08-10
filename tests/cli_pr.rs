//! End-to-end tests for `choo pr` using the FakeGh JSON-backed runner
//! selected via `CHOOCHOO_GH_FAKE`.

mod common;
use common::TestRepo;

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
fn pr_creates_one_per_branch_with_correct_bases() {
    let repo = TestRepo::new();
    three_branch_train(&repo);
    repo.choo_ok(["pr"]);

    let state = repo.fake_gh_state().expect("gh state file");
    let prs = &state["prs"];
    assert_eq!(prs["a"]["base"], "main");
    assert_eq!(prs["b"]["base"], "a");
    assert_eq!(prs["c"]["base"], "b");
    assert_eq!(prs["a"]["number"], 1);
    assert_eq!(prs["b"]["number"], 2);
    assert_eq!(prs["c"]["number"], 3);
}

#[test]
fn pr_body_contains_the_train_table() {
    let repo = TestRepo::new();
    three_branch_train(&repo);
    repo.choo_ok(["pr"]);

    let state = repo.fake_gh_state().unwrap();
    for branch in ["a", "b", "c"] {
        let body = state["prs"][branch]["body"].as_str().unwrap().to_string();
        // The Title column has each PR's title; with the default
        // `choo pr` title (= branch name), we get plain "a"/"b"/"c".
        assert!(body.contains("| Title | PR |"));
        assert!(body.contains("| a | #1 |"));
        assert!(body.contains("| b | #2 |"));
        assert!(body.contains("| c | #3 |"));
        assert!(body.contains("Base: `main`"));
    }
    let body_b = state["prs"]["b"]["body"].as_str().unwrap();
    assert!(body_b.contains("**this PR**"));
}

#[test]
fn pr_is_idempotent_on_rerun() {
    let repo = TestRepo::new();
    three_branch_train(&repo);

    let out1 = repo.choo_ok(["pr"]);
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(s1.contains("created 3"));
    let snap1 = repo.fake_gh_state().unwrap();

    let out2 = repo.choo_ok(["pr"]);
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(s2.contains("created 0"));
    let snap2 = repo.fake_gh_state().unwrap();

    // PR numbers and URLs unchanged across runs.
    for branch in ["a", "b", "c"] {
        assert_eq!(snap1["prs"][branch]["number"], snap2["prs"][branch]["number"]);
        assert_eq!(snap1["prs"][branch]["url"], snap2["prs"][branch]["url"]);
    }
}

#[test]
fn pr_state_persists_in_choo_state_file() {
    let repo = TestRepo::new();
    three_branch_train(&repo);
    repo.choo_ok(["pr"]);
    let raw = std::fs::read_to_string(repo.path().join(".git/choochoo/state.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let prs = &parsed["trains"]["feat"]["prs"];
    assert_eq!(prs["a"]["number"], 1);
    assert_eq!(prs["c"]["number"], 3);
}
