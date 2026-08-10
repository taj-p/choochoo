//! End-to-end tests for `choo context`.
//!
//! These run the real [`choochoo::editor::ProcessEditor`] path — `$EDITOR`
//! is set to a command that edits the buffer non-interactively, exactly the
//! way a human's editor would. That's what makes them worth having on top
//! of the unit tests: they cover the shell invocation, the temp buffer, and
//! reading the file back after the editor has replaced it.

mod common;
use common::TestRepo;

use std::path::Path;
use std::process::{Command, Output};

/// Run `choo <args>` with `$EDITOR` set to a command that overwrites the
/// buffer with `text` — a stand-in for a human typing it and saving.
fn choo_editing(repo: &TestRepo, text: &str, args: &[&str]) -> Output {
    let source = repo.path().join("editor-input.md");
    std::fs::write(&source, text).unwrap();
    choo_with_editor(repo, &format!("cp {}", shell_quote(&source)), args)
}

/// Run `choo <args>` with `$EDITOR` set to `editor`.
fn choo_with_editor(repo: &TestRepo, editor: &str, args: &[&str]) -> Output {
    let mut cmd: Command = repo.choo();
    cmd.env("EDITOR", editor);
    cmd.env_remove("VISUAL");
    cmd.args(args).output().expect("run choo")
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "choo failed: stdout=`{}` stderr=`{}`",
        stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn two_branch_train(repo: &TestRepo) {
    repo.choo_ok(["init", "feat", "--base", "main"]);
    repo.branch("a", "main");
    repo.commit("a.txt");
    repo.branch("b", "a");
    repo.commit("b.txt");
    repo.choo_ok(["add", "a"]);
    repo.choo_ok(["add", "b"]);
}

#[test]
fn context_saves_what_the_editor_wrote() {
    let repo = TestRepo::new();
    two_branch_train(&repo);

    let out = choo_editing(&repo, "Split for reviewability.\n", &["context"]);
    assert_ok(&out);
    assert!(stdout(&out).contains("updated context for train `feat`"));

    let shown = choo_with_editor(&repo, "false", &["context", "--show"]);
    assert_ok(&shown);
    assert_eq!(stdout(&shown), "Split for reviewability.\n");
}

#[test]
fn context_lands_at_the_top_of_every_pr_description() {
    let repo = TestRepo::new();
    two_branch_train(&repo);
    assert_ok(&choo_editing(
        &repo,
        "Read this first.\n\nThen start at the bottom.\n",
        &["context"],
    ));
    repo.choo_ok(["pr"]);

    let state = repo.fake_gh_state().expect("gh state");
    for branch in ["a", "b"] {
        let body = state["prs"][branch]["body"].as_str().unwrap();
        assert!(
            body.starts_with("<!-- choochoo:context:start -->"),
            "`{branch}` doesn't lead with the context:\n{body}"
        );
        assert!(body.contains("## PR Train Context"));
        assert!(body.contains("Read this first."));
        assert!(body.contains("Then start at the bottom."));
        // The train table is still there, below it.
        assert!(body.contains("| Title | PR |"));
    }
}

/// The workflow the feature is for: change the text once, and `choo pr`
/// carries it to every description.
#[test]
fn editing_the_context_updates_every_pr_on_the_next_pr_run() {
    let repo = TestRepo::new();
    two_branch_train(&repo);
    assert_ok(&choo_editing(&repo, "Version one.\n", &["context"]));
    repo.choo_ok(["pr"]);

    assert_ok(&choo_editing(&repo, "Version two.\n", &["context"]));
    let out = repo.choo_ok(["pr"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("updated 2"));

    let state = repo.fake_gh_state().unwrap();
    for branch in ["a", "b"] {
        let body = state["prs"][branch]["body"].as_str().unwrap();
        assert!(body.contains("Version two."));
        assert!(!body.contains("Version one."));
        assert_eq!(body.matches("## PR Train Context").count(), 1);
    }

    // And a re-run is still a no-op.
    let again = repo.choo_ok(["pr"]);
    assert!(String::from_utf8_lossy(&again.stdout).contains("updated 0"));
}

#[test]
fn context_tells_you_how_many_prs_need_syncing() {
    let repo = TestRepo::new();
    two_branch_train(&repo);

    // Before any PRs exist, there's nothing to sync yet.
    let first = choo_editing(&repo, "Notes.\n", &["context"]);
    assert_ok(&first);
    assert!(
        stdout(&first).contains("run `choo pr` to open the train's PRs with it"),
        "got: {}",
        stdout(&first)
    );

    repo.choo_ok(["pr"]);
    let second = choo_editing(&repo, "Different notes.\n", &["context"]);
    assert_ok(&second);
    assert!(
        stdout(&second).contains("run `choo pr` to sync 2 PR description(s)"),
        "got: {}",
        stdout(&second)
    );
}

/// Quitting the editor non-zero (`:cq` in vim) abandons the edit.
#[test]
fn quitting_the_editor_without_saving_changes_nothing() {
    let repo = TestRepo::new();
    two_branch_train(&repo);
    assert_ok(&choo_editing(&repo, "Keep me.\n", &["context"]));

    let out = choo_with_editor(&repo, "false", &["context"]);
    assert_ok(&out);
    assert!(stdout(&out).contains("editor exited without saving"));

    let shown = choo_with_editor(&repo, "true", &["context", "--show"]);
    assert_eq!(stdout(&shown), "Keep me.\n");
}

/// Saving the buffer untouched is a no-op, not a spurious "updated".
#[test]
fn saving_without_editing_reports_unchanged() {
    let repo = TestRepo::new();
    two_branch_train(&repo);
    assert_ok(&choo_editing(&repo, "Same text.\n", &["context"]));

    // `true` leaves the buffer exactly as choochoo seeded it.
    let out = choo_with_editor(&repo, "true", &["context"]);
    assert_ok(&out);
    assert!(
        stdout(&out).contains("context for train `feat` unchanged"),
        "got: {}",
        stdout(&out)
    );
}

/// The editor opens on the stored text, so an edit starts from what's
/// already there rather than from a blank page.
#[test]
fn the_editor_opens_on_the_stored_text() {
    let repo = TestRepo::new();
    two_branch_train(&repo);
    assert_ok(&choo_editing(&repo, "First draft.\n", &["context"]));

    // `tee` copies the buffer somewhere we can inspect, then leaves the
    // file as it was.
    let captured = repo.path().join("seen.md");
    let out = choo_with_editor(
        &repo,
        &format!("tee {} <", shell_quote(&captured)),
        &["context"],
    );
    assert_ok(&out);
    assert_eq!(std::fs::read_to_string(&captured).unwrap(), "First draft.\n");
}

#[test]
fn emptying_the_buffer_clears_the_context_and_the_pr_sections() {
    let repo = TestRepo::new();
    two_branch_train(&repo);
    assert_ok(&choo_editing(&repo, "Temporary.\n", &["context"]));
    repo.choo_ok(["pr"]);

    let out = choo_editing(&repo, "\n", &["context"]);
    assert_ok(&out);
    assert!(stdout(&out).contains("cleared context for train `feat`"));

    repo.choo_ok(["pr"]);
    let state = repo.fake_gh_state().unwrap();
    for branch in ["a", "b"] {
        let body = state["prs"][branch]["body"].as_str().unwrap();
        assert!(!body.contains("## PR Train Context"), "got: {body}");
        assert!(!body.contains("Temporary."));
        assert!(body.contains("| Title | PR |"), "table lost: {body}");
    }
    assert_eq!(
        stdout(&choo_with_editor(&repo, "true", &["context", "--show"])),
        ""
    );
}

/// Contexts belong to a train, and `-t` picks which one.
#[test]
fn contexts_are_per_train() {
    let repo = TestRepo::new();
    two_branch_train(&repo);
    repo.choo_ok(["init", "other", "--base", "main"]);

    assert_ok(&choo_editing(&repo, "For feat.\n", &["context", "-t", "feat"]));
    assert_ok(&choo_editing(
        &repo,
        "For other.\n",
        &["context", "-t", "other"],
    ));

    for (train, expected) in [("feat", "For feat.\n"), ("other", "For other.\n")] {
        let shown = choo_with_editor(&repo, "true", &["context", "--show", "-t", train]);
        assert_eq!(stdout(&shown), expected);
    }
}

#[test]
fn show_displays_the_context_in_the_train_summary() {
    let repo = TestRepo::new();
    two_branch_train(&repo);
    assert_ok(&choo_editing(&repo, "Line one.\nLine two.\n", &["context"]));

    let out = repo.choo_ok(["show"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Context:"), "got: {text}");
    assert!(text.contains("  Line one."), "got: {text}");
    assert!(text.contains("  Line two."), "got: {text}");
}

#[test]
fn context_on_an_unknown_train_fails() {
    let repo = TestRepo::new();
    two_branch_train(&repo);
    let out = choo_editing(&repo, "x\n", &["context", "-t", "ghost"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ghost"));
}

/// The context is part of a train, so it travels with the train to another
/// machine rather than staying on the box that wrote it.
#[test]
fn context_is_shared_state() {
    use common::BareRepo;

    let code = BareRepo::new();
    let store = BareRepo::new();

    let mut one = TestRepo::new();
    one.with_origin(&code);
    one.share_state(&store);
    one.choo_ok(["init", "feat", "--base", "main"]);
    assert_ok(&choo_editing(&one, "Written on box one.\n", &["context"]));

    let mut two = TestRepo::clone_of(&code);
    two.share_state(&store);
    let shown = choo_with_editor(&two, "true", &["context", "--show", "-t", "feat"]);
    assert_ok(&shown);
    assert_eq!(stdout(&shown), "Written on box one.\n");
}
