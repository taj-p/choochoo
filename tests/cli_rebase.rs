//! End-to-end tests for `choo rebase`.
//!
//! These run real `git rebase` inside a tempdir-backed repo. They cover:
//! the happy path with a base advance, a conflict that pauses choo's loop,
//! `--abort`, and the recorded-base machinery that makes a mid-stack history
//! rewrite restack cleanly.

mod common;
use common::TestRepo;

/// Run a git command in the repo, asserting it succeeded.
fn git_ok(repo: &TestRepo, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run a git command, returning its trimmed stdout.
fn git_out(repo: &TestRepo, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo.path())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Commit `content` to `path`, replacing whatever was there.
fn write_commit(repo: &TestRepo, path: &str, content: &str, msg: &str) {
    std::fs::write(repo.path().join(path), content).unwrap();
    git_ok(repo, &["add", path]);
    git_ok(repo, &["commit", "-q", "-m", msg]);
}

/// The shared half of choochoo's state, as raw JSON.
fn state_json(repo: &TestRepo) -> serde_json::Value {
    let text =
        std::fs::read_to_string(repo.path().join(".git/choochoo/state.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// How many commits `child` has that `parent` doesn't.
fn commits_ahead(repo: &TestRepo, parent: &str, child: &str) -> usize {
    git_out(repo, &["rev-list", "--count", &format!("{parent}..{child}")])
        .parse()
        .unwrap()
}

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

/// The regression test this whole feature exists for.
///
/// Amending `a`'s commit leaves `b` parented on the orphaned pre-amend commit.
/// With the parent's tip as the boundary, `b`'s replay range widens to include
/// that orphan and the rebase dies on `CONFLICT (add/add)`; resolving it the
/// obvious way puts the pre-amend content back into every branch above.
#[test]
fn mid_stack_amend_does_not_resurrect_the_pre_amend_commit() {
    let repo = TestRepo::new();
    three_branch_train(&repo);
    // One clean restack first, so bases are recorded from the authoritative
    // source rather than only from `choo add`.
    repo.choo_ok(["rebase"]);

    // Amend `a`'s only commit, rewriting its content.
    repo.checkout("a");
    std::fs::write(repo.path().join("a.txt"), "a-amended\n").unwrap();
    git_ok(&repo, &["add", "a.txt"]);
    git_ok(&repo, &["commit", "-q", "--amend", "-m", "a-amended"]);
    repo.checkout("main");

    // Must succeed. Before recorded bases this failed on `b`.
    repo.choo_ok(["rebase"]);

    // Every branch descends from the amended `a`.
    let a_tip = repo.rev_parse("a");
    for branch in ["b", "c"] {
        let mb = git_out(&repo, &["merge-base", branch, &a_tip]);
        assert_eq!(mb, a_tip, "`{branch}` is not on top of the amended `a`");
    }

    // The amended content won, and the pre-amend content is gone.
    assert_eq!(git_out(&repo, &["show", "c:a.txt"]), "a-amended");

    // Each branch replayed exactly its own commit — no duplicates.
    assert_eq!(commits_ahead(&repo, "a", "b"), 1);
    assert_eq!(commits_ahead(&repo, "b", "c"), 1);

    // And the whole train still carries all three files.
    let mut files = git_out(&repo, &["diff", "--name-only", "main", "c"])
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(files, vec!["a.txt", "b.txt", "c.txt"]);
}

/// A train whose state predates recorded bases must behave exactly as before.
#[test]
fn rebase_falls_back_when_no_base_was_recorded() {
    let repo = TestRepo::new();
    three_branch_train(&repo);

    // Strip the recorded bases, as a state file written by an older choo
    // would have. `choo add` has already populated them by this point.
    let path = repo.path().join(".git/choochoo/state.json");
    let mut state = state_json(&repo);
    assert!(
        state["trains"]["feat"]["branch_bases"].is_object(),
        "expected `choo add` to have recorded bases: {state}"
    );
    state["trains"]["feat"]
        .as_object_mut()
        .unwrap()
        .remove("branch_bases");
    std::fs::write(&path, serde_json::to_string(&state).unwrap()).unwrap();

    repo.checkout("main");
    repo.commit("MAIN_NEW.txt");
    let new_main = repo.rev_parse("main");

    repo.choo_ok(["rebase"]);

    for branch in ["a", "b", "c"] {
        let mb = git_out(&repo, &["merge-base", branch, &new_main]);
        assert_eq!(mb, new_main, "`{branch}` is not on top of the new main");
    }
    assert_eq!(commits_ahead(&repo, "a", "b"), 1);
}

/// `choo add` records the tip it appended onto, so the mechanism works from
/// the first rebase rather than needing one to arm it.
#[test]
fn add_records_the_parent_tip_as_the_new_branchs_base() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);
    let main_tip = repo.rev_parse("main");
    repo.branch("a", "main");
    repo.commit("a.txt");
    repo.choo_ok(["add", "a"]);
    let a_tip = repo.rev_parse("a");
    repo.branch("b", "a");
    repo.commit("b.txt");
    repo.choo_ok(["add", "b"]);

    let bases = &state_json(&repo)["trains"]["feat"]["branch_bases"];
    assert_eq!(bases["a"], main_tip);
    assert_eq!(bases["b"], a_tip);
}

/// A branch cut from somewhere unrelated has no honest base to record, so
/// nothing is recorded and that pair falls back.
#[test]
fn add_of_a_branch_not_descending_from_the_tip_records_nothing() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);
    repo.branch("a", "main");
    repo.commit("a.txt");
    repo.choo_ok(["add", "a"]);

    // `b` is cut from main, not from the train tip `a`.
    repo.branch("b", "main");
    repo.commit("b.txt");
    repo.choo_ok(["add", "b"]);

    let bases = &state_json(&repo)["trains"]["feat"]["branch_bases"];
    assert!(bases.get("a").is_some(), "`a` does sit on main: {bases}");
    assert!(
        bases.get("b").is_none(),
        "`b` does not sit on `a`, so nothing should be recorded: {bases}"
    );
}

/// Removing a mid-stack branch must not narrow its successor's replay range,
/// or the removed branch's commits vanish out of the successor's content.
#[test]
fn remove_then_rebase_keeps_the_removed_branchs_commits() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);
    repo.branch("b1", "main");
    repo.commit("b1.txt");
    repo.branch("b2", "b1");
    repo.commit("b2.txt");
    repo.choo_ok(["add", "b1"]);
    repo.choo_ok(["add", "b2"]);

    repo.choo_ok(["remove", "b1"]);
    repo.checkout("main");
    repo.commit("MAIN_NEW.txt");
    repo.choo_ok(["rebase"]);

    // b1's work is still part of b2's content. Narrowing b2's replay range to
    // exclude it — the bug this splice prevents — would drop the file.
    let mut files = git_out(&repo, &["diff", "--name-only", "main", "b2"])
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(
        files,
        vec!["b1.txt", "b2.txt"],
        "b1's work was dropped out of b2"
    );
}

/// Reordering permutes the stack; each branch must still replay only its own
/// commit. Before recorded bases, the moved branch dragged its old
/// predecessor's commit along with it.
#[test]
fn move_then_rebase_replays_only_the_moved_branchs_own_commit() {
    let repo = TestRepo::new();
    three_branch_train(&repo);
    repo.choo_ok(["rebase"]);

    repo.choo_ok(["move", "c", "--before", "b"]);
    repo.checkout("main");
    repo.choo_ok(["rebase"]);

    // New order is a -> c -> b.
    assert_eq!(
        git_out(&repo, &["diff", "--name-only", "a", "c"]),
        "c.txt",
        "`c` should add only its own file when stacked directly on `a`"
    );
    assert_eq!(commits_ahead(&repo, "a", "c"), 1);
    assert_eq!(commits_ahead(&repo, "c", "b"), 1);
}

/// Resolving a conflict with `git rebase --continue` must let choo finish the
/// rest of the train and record the resolved branch's base. This flow had no
/// end-to-end coverage before.
#[test]
fn conflict_then_git_continue_then_choo_continue() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);

    // `a` and main both touch shared.txt, so restacking `a` conflicts.
    repo.branch("a", "main");
    write_commit(&repo, "shared.txt", "a-version\n", "a-version");
    repo.choo_ok(["add", "a"]);
    repo.branch("b", "a");
    repo.commit("b.txt");
    repo.choo_ok(["add", "b"]);

    repo.checkout("main");
    write_commit(&repo, "shared.txt", "main-version\n", "main-version");

    let out = repo.choo().arg("rebase").output().unwrap();
    assert!(!out.status.success(), "expected a conflict on `a`");

    // Resolve it by hand and finish git's rebase.
    std::fs::write(repo.path().join("shared.txt"), "resolved\n").unwrap();
    git_ok(&repo, &["add", "shared.txt"]);
    git_ok(&repo, &["-c", "core.editor=true", "rebase", "--continue"]);

    repo.choo_ok(["rebase", "--continue"]);

    let a_tip = repo.rev_parse("a");
    assert_eq!(
        git_out(&repo, &["merge-base", "b", &a_tip]),
        a_tip,
        "`b` should sit on the resolved `a`"
    );
    assert_eq!(commits_ahead(&repo, "a", "b"), 1);
    assert!(!repo
        .path()
        .join(".git/choochoo/rebase-progress.json")
        .exists());

    // `a`'s resolved base was recorded, and `b`'s points at `a`'s new tip.
    let bases = &state_json(&repo)["trains"]["feat"]["branch_bases"];
    assert_eq!(bases["b"], a_tip);
}

/// If the user abandons the git rebase instead of finishing it, choo must not
/// record a base claiming the branch moved when it didn't.
#[test]
fn continue_after_git_abort_records_no_base_for_the_conflicted_branch() {
    let repo = TestRepo::new();
    repo.choo_ok(["init", "feat", "--base", "main"]);
    repo.branch("a", "main");
    write_commit(&repo, "shared.txt", "a-version\n", "a-version");
    repo.choo_ok(["add", "a"]);
    let base_before = state_json(&repo)["trains"]["feat"]["branch_bases"]["a"].clone();

    repo.checkout("main");
    write_commit(&repo, "shared.txt", "main-version\n", "main-version");

    let out = repo.choo().arg("rebase").output().unwrap();
    assert!(!out.status.success());

    // Walk away from the rebase entirely.
    git_ok(&repo, &["rebase", "--abort"]);
    let out = repo.choo_ok(["rebase", "--continue"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not recording its base"),
        "expected a warning, stderr was: {stderr}"
    );

    // The recorded base is untouched, not overwritten with a false one.
    assert_eq!(
        state_json(&repo)["trains"]["feat"]["branch_bases"]["a"],
        base_before
    );
}

/// The rewrite note must not eat the step's result line. `Reporter::info`
/// flushes a pending `start` as "interrupted", so ordering matters — and only
/// an integration test sees the real `StderrReporter`.
#[test]
fn rewrite_note_does_not_swallow_the_step_result() {
    let repo = TestRepo::new();
    three_branch_train(&repo);
    repo.choo_ok(["rebase"]);

    repo.checkout("a");
    std::fs::write(repo.path().join("a.txt"), "a-amended\n").unwrap();
    git_ok(&repo, &["add", "a.txt"]);
    git_ok(&repo, &["commit", "-q", "--amend", "-m", "a-amended"]);
    repo.checkout("main");

    let out = repo.choo_ok(["rebase"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("was rewritten"),
        "expected a rewrite note, stderr was: {stderr}"
    );
    assert!(
        !stderr.contains("interrupted"),
        "the note swallowed a step's result: {stderr}"
    );
    // The note precedes the step it describes.
    let note = stderr.find("was rewritten").unwrap();
    let step = stderr.find("rebasing `b`").unwrap();
    assert!(note < step, "note should come before the step: {stderr}");
    // And every step still reports a result.
    assert_eq!(stderr.matches("... ok").count(), 3, "stderr was: {stderr}");
}
