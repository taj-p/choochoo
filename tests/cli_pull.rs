//! `choo pull` — updating a train's local branches from the remote.
//!
//! Same two-machine setup as `cli_fetch.rs`: one bare "GitHub" repo, one
//! bare state repo, and two clones sharing both. A does the work and
//! pushes; B is the machine that has to catch up.

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

/// A pushed two-branch train, present on both machines.
fn two_machines() -> (BareRepo, BareRepo, TestRepo, TestRepo) {
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
    a.checkout("main");

    let mut b = TestRepo::clone_of(&code);
    git_ok(b.path(), &["fetch", "-q", "origin"]);
    git_ok(b.path(), &["checkout", "-q", "-b", "main", "origin/main"]);
    b.share_state(&store);
    b.choo_ok(["fetch", "my-feature"]);

    (code, store, a, b)
}

/// The headline case: A pushed more work, B picks it up without checking
/// out a single branch by hand.
#[test]
fn pull_fast_forwards_stale_branches() {
    let (_code, _store, a, b) = two_machines();

    a.checkout("feat/part-2");
    a.commit("p3.txt");
    a.choo_ok(["push"]);

    let before = b.rev_parse("feat/part-2");
    let out = b.choo_ok(["pull", "my-feature"]);
    assert!(stdout(&out).contains("updated 1"), "got: {}", stdout(&out));

    assert_ne!(b.rev_parse("feat/part-2"), before);
    assert_eq!(
        b.rev_parse("feat/part-2"),
        b.rev_parse("origin/feat/part-2"),
        "the branch should now be level with the remote"
    );
}

/// The base is the branch most likely to have moved, and the one
/// `choo rebase` needs current.
#[test]
fn pull_updates_the_base_branch() {
    let (_code, _store, a, b) = two_machines();

    a.checkout("main");
    a.commit("upstream.txt");
    git_ok(a.path(), &["push", "-q", "origin", "main"]);

    b.choo_ok(["pull", "my-feature"]);
    assert_eq!(b.rev_parse("main"), b.rev_parse("origin/main"));
    assert!(
        b.path().join("upstream.txt").exists(),
        "`main` is checked out, so the working tree should have moved too"
    );
}

/// Updating nine branches must not take you off the tenth.
#[test]
fn pull_leaves_you_on_the_branch_you_were_on() {
    let (_code, _store, a, b) = two_machines();

    a.checkout("feat/part-1");
    a.commit("more.txt");
    a.checkout("feat/part-2");
    git_ok(a.path(), &["rebase", "-q", "feat/part-1"]);
    a.choo_ok(["push"]);

    b.checkout("feat/part-2");
    let before = b.rev_parse("feat/part-2");
    b.choo_ok(["pull", "my-feature"]);

    assert_eq!(b.current_branch(), "feat/part-2");
    assert_eq!(b.rev_parse("feat/part-1"), b.rev_parse("origin/feat/part-1"));
    // `feat/part-2` was rebased on A, so from B's side it diverged and
    // stayed put.
    assert_eq!(b.rev_parse("feat/part-2"), before);
}

/// The safety property: a branch with local commits the remote hasn't seen
/// is never rolled back.
#[test]
fn pull_never_moves_a_diverged_branch() {
    let (_code, _store, a, b) = two_machines();

    // Both sides move `feat/part-1`.
    a.checkout("feat/part-1");
    a.commit("theirs.txt");
    git_ok(a.path(), &["push", "-qf", "origin", "feat/part-1"]);

    b.checkout("feat/part-1");
    std::fs::write(b.path().join("mine.txt"), "mine\n").unwrap();
    git_ok(b.path(), &["add", "mine.txt"]);
    git_ok(b.path(), &["commit", "-q", "-m", "local work"]);
    let mine = b.rev_parse("feat/part-1");
    b.checkout("main");

    let out = b.choo_ok(["pull", "my-feature"]);
    assert_eq!(
        b.rev_parse("feat/part-1"),
        mine,
        "a diverged branch must keep its local commits"
    );
    let said = format!("{}{}", stdout(&out), stderr(&out));
    assert!(said.contains("diverged"), "should say so: {said}");
    assert!(said.contains("feat/part-1"), "should name it: {said}");
}

/// Unpushed work is not stale work.
#[test]
fn pull_leaves_a_branch_that_is_only_ahead_alone() {
    let (_code, _store, _a, b) = two_machines();

    b.checkout("feat/part-2");
    b.commit("local-only.txt");
    let mine = b.rev_parse("feat/part-2");
    b.checkout("main");

    let out = b.choo_ok(["pull", "my-feature"]);
    assert_eq!(b.rev_parse("feat/part-2"), mine);
    assert!(
        !stdout(&out).contains("diverged"),
        "being ahead isn't a divergence: {}",
        stdout(&out)
    );
}

/// Pull subsumes fetch, so it works on a machine that has the train's
/// metadata and none of its branches.
#[test]
fn pull_creates_branches_that_are_not_here_yet() {
    let code = BareRepo::new();
    let store = BareRepo::new();

    let mut a = TestRepo::new();
    a.with_origin(&code);
    a.share_state(&store);
    a.choo_ok(["init", "t"]);
    a.branch("b1", "main");
    a.commit("b1.txt");
    a.choo_ok(["add"]);
    a.choo_ok(["push"]);

    let mut b = TestRepo::clone_of(&code);
    git_ok(b.path(), &["fetch", "-q", "origin"]);
    git_ok(b.path(), &["checkout", "-q", "-b", "main", "origin/main"]);
    b.share_state(&store);

    let out = b.choo_ok(["pull", "t"]);
    assert!(stdout(&out).contains("created 1"), "got: {}", stdout(&out));
    assert_eq!(b.rev_parse("b1"), b.rev_parse("origin/b1"));
}

/// Running it twice does nothing the second time.
#[test]
fn pull_is_idempotent() {
    let (_code, _store, a, b) = two_machines();

    a.checkout("feat/part-2");
    a.commit("p3.txt");
    a.choo_ok(["push"]);

    b.choo_ok(["pull", "my-feature"]);
    let out = b.choo_ok(["pull", "my-feature"]);
    assert!(
        stdout(&out).contains("updated 0"),
        "second run should have nothing to do: {}",
        stdout(&out)
    );
}

/// A train whose branches were never pushed can't be caught up, and says so.
#[test]
fn pull_reports_a_branch_that_exists_nowhere() {
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
    a.branch("never-pushed", "pushed");
    a.commit("two.txt");
    a.choo_ok(["add"]);

    let mut b = TestRepo::clone_of(&code);
    git_ok(b.path(), &["fetch", "-q", "origin"]);
    git_ok(b.path(), &["checkout", "-q", "-b", "main", "origin/main"]);
    b.share_state(&store);

    let out = b.choo_try(["pull", "half-pushed"]);
    assert!(!out.status.success(), "an unusable train should exit non-zero");
    assert!(stderr(&out).contains("never-pushed"), "got: {}", stderr(&out));
    // What could be pulled still was.
    assert_eq!(b.rev_parse("pushed"), b.rev_parse("origin/pushed"));
}

/// The combined branch is part of the train's shape, so it's kept current
/// too.
#[test]
fn pull_updates_the_combined_branch() {
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

    // A adds to the train, which moves the combined branch with it.
    a.checkout("one");
    a.commit("two.txt");
    a.checkout("main");
    a.choo_ok(["aggregate", "sync"]);
    a.choo_ok(["push"]);

    b.choo_ok(["pull", "combo"]);
    assert_eq!(
        b.rev_parse("choo/combo/combined"),
        b.rev_parse("origin/choo/combo/combined")
    );
}

// --- `--reset` -------------------------------------------------------------

/// The case `--reset` exists for: the other devbox rebased the whole train
/// and force-pushed it, so every branch here is diverged.
#[test]
fn reset_takes_the_remote_after_another_box_force_pushed() {
    let (_code, _store, a, b) = two_machines();

    // A rebases the train onto a moved `main` and force-pushes the lot.
    a.checkout("main");
    a.commit("upstream.txt");
    git_ok(a.path(), &["push", "-q", "origin", "main"]);
    a.choo_ok(["rebase"]);
    a.choo_ok(["push"]);

    // Without the flag, B can't do anything with them.
    let out = b.choo_ok(["pull", "my-feature"]);
    assert!(stdout(&out).contains("feat/part-1"), "got: {}", stdout(&out));
    assert!(stdout(&out).contains("diverged"), "got: {}", stdout(&out));
    assert_ne!(b.rev_parse("feat/part-1"), b.rev_parse("origin/feat/part-1"));

    let out = b.choo_ok(["pull", "my-feature", "--reset"]);
    assert!(stdout(&out).contains("reset 2"), "got: {}", stdout(&out));
    assert!(stdout(&out).contains("reflog"), "got: {}", stdout(&out));
    for branch in ["feat/part-1", "feat/part-2"] {
        assert_eq!(
            b.rev_parse(branch),
            b.rev_parse(&format!("origin/{branch}")),
            "`{branch}` should be on the remote's version"
        );
    }
    // And the rebased history really is here.
    b.choo_ok(["switch", "my-feature"]);
    b.choo_ok(["checkout", "feat/part-2"]);
    assert!(b.path().join("upstream.txt").exists());
}

/// The branch you're standing on gets reset too, working tree and all.
#[test]
fn reset_moves_the_checked_out_branch_and_its_working_tree() {
    let (_code, _store, a, b) = two_machines();

    a.checkout("feat/part-1");
    std::fs::write(a.path().join("one.txt"), "rewritten\n").unwrap();
    git_ok(a.path(), &["add", "one.txt"]);
    git_ok(a.path(), &["commit", "-q", "--amend", "-m", "one, revised"]);
    git_ok(a.path(), &["push", "-qf", "origin", "feat/part-1"]);

    b.checkout("feat/part-1");
    b.choo_ok(["pull", "my-feature", "--reset"]);

    assert_eq!(b.current_branch(), "feat/part-1");
    assert_eq!(b.rev_parse("feat/part-1"), b.rev_parse("origin/feat/part-1"));
    assert_eq!(
        std::fs::read_to_string(b.path().join("one.txt")).unwrap(),
        "rewritten\n",
        "the working tree should hold the remote's version"
    );
}

/// The guard: uncommitted changes on a branch that would be reset stop the
/// command, and stop it before anything has moved.
#[test]
fn reset_refuses_over_uncommitted_changes() {
    let (_code, _store, a, b) = two_machines();

    a.checkout("main");
    a.commit("upstream.txt");
    git_ok(a.path(), &["push", "-q", "origin", "main"]);
    a.choo_ok(["rebase"]);
    a.choo_ok(["push"]);

    b.checkout("feat/part-1");
    std::fs::write(b.path().join("p1.txt"), "work in progress\n").unwrap();
    let before_main = b.rev_parse("main");
    let before_two = b.rev_parse("feat/part-2");

    let out = b.choo_try(["pull", "my-feature", "--reset"]);
    assert!(!out.status.success(), "should refuse");
    let err = stderr(&out);
    assert!(err.contains("uncommitted changes"), "got: {err}");
    assert!(err.contains("stash"), "should say how to proceed: {err}");

    assert_eq!(
        std::fs::read_to_string(b.path().join("p1.txt")).unwrap(),
        "work in progress\n",
        "the edit must survive"
    );
    assert_eq!(b.rev_parse("main"), before_main, "nothing may have moved");
    assert_eq!(b.rev_parse("feat/part-2"), before_two);
}

/// A dirty tree only blocks the branch being reset. Editing on a branch the
/// reset doesn't touch is fine — those moves never go near the tree.
#[test]
fn a_dirty_tree_on_an_unaffected_branch_does_not_block_reset() {
    let (_code, _store, a, b) = two_machines();

    a.checkout("feat/part-1");
    a.commit("theirs.txt");
    git_ok(a.path(), &["push", "-qf", "origin", "feat/part-1"]);
    // B has its own commit on feat/part-1, so it diverged.
    b.checkout("feat/part-1");
    b.commit("mine.txt");

    // ...but B is sitting on `main` with an uncommitted edit.
    b.checkout("main");
    std::fs::write(b.path().join("r.md"), "scratch\n").unwrap();

    b.choo_ok(["pull", "my-feature", "--reset"]);
    assert_eq!(b.rev_parse("feat/part-1"), b.rev_parse("origin/feat/part-1"));
    assert_eq!(
        std::fs::read_to_string(b.path().join("r.md")).unwrap(),
        "scratch\n",
        "the untouched branch's edit must survive"
    );
}

/// The other safety half: `--reset` is not a licence to delete work that
/// exists nowhere else.
#[test]
fn reset_leaves_unpushed_local_commits_alone() {
    let (_code, _store, _a, b) = two_machines();

    b.checkout("feat/part-2");
    b.commit("only-here.txt");
    let mine = b.rev_parse("feat/part-2");
    b.checkout("main");

    let out = b.choo_ok(["pull", "my-feature", "--reset"]);
    assert_eq!(
        b.rev_parse("feat/part-2"),
        mine,
        "a branch that is only ahead must survive --reset"
    );
    assert!(
        stdout(&out).contains("not reset"),
        "should say it skipped it: {}",
        stdout(&out)
    );
    // The commit is still on the branch (we're on `main`, so not in the tree).
    assert!(
        git_ok(b.path(), &["log", "--oneline", "feat/part-2"])
            .contains("only-here.txt"),
        "the unpushed commit should still be there"
    );
}

/// Discarded commits are recoverable, which is what makes the flag safe to
/// hand out.
#[test]
fn commits_dropped_by_reset_are_still_in_the_reflog() {
    let (_code, _store, a, b) = two_machines();

    a.checkout("feat/part-1");
    a.commit("theirs.txt");
    git_ok(a.path(), &["push", "-qf", "origin", "feat/part-1"]);

    b.checkout("feat/part-1");
    b.commit("mine.txt");
    let mine = b.rev_parse("feat/part-1");
    b.checkout("main");

    b.choo_ok(["pull", "my-feature", "--reset"]);
    assert_ne!(b.rev_parse("feat/part-1"), mine);
    let reflog = git_ok(b.path(), &["reflog", "show", "feat/part-1"]);
    assert!(
        reflog.contains(&mine[..7]),
        "the dropped commit should be recoverable: {reflog}"
    );
}

/// Without shared state it's still the quickest way to catch a stack up.
#[test]
fn pull_works_without_shared_state() {
    let code = BareRepo::new();
    let a = TestRepo::new();
    a.with_origin(&code);

    a.choo_ok(["init", "t"]);
    a.branch("b1", "main");
    a.commit("b1.txt");
    a.choo_ok(["add"]);
    a.choo_ok(["push"]);

    // Rewind the local branch, as if it were a day behind.
    a.checkout("main");
    git_ok(a.path(), &["branch", "-f", "b1", "main"]);

    let out = a.choo_ok(["pull", "t"]);
    assert!(stdout(&out).contains("updated 1"), "got: {}", stdout(&out));
    assert_eq!(a.rev_parse("b1"), a.rev_parse("origin/b1"));
}
