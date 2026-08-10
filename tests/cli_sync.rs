//! Shared train state across machines, end to end, with no network.
//!
//! Two `TestRepo`s sharing one bare `origin` are two devboxes working on the
//! same repository: the shared origin means both derive the same choochoo
//! repository identity, and each gets its own `XDG_DATA_HOME` — hence its
//! own store clone — which is what actually makes them separate machines.
//! A second bare repo stands in for the private GitHub repo holding state.

mod common;

use common::{BareRepo, TestRepo};

/// Two devboxes on the same repo, sharing one state repo.
fn two_devboxes() -> (BareRepo, BareRepo, TestRepo, TestRepo) {
    let code = BareRepo::new();
    let store = BareRepo::new();

    let mut a = TestRepo::new();
    a.with_origin(&code);
    a.share_state(&store);

    let mut b = TestRepo::clone_of(&code);
    run(b.path(), &["fetch", "-q", "origin"]);
    run(b.path(), &["checkout", "-q", "-b", "main", "origin/main"]);
    b.share_state(&store);

    (code, store, a, b)
}

fn run(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// The headline behaviour: build a train on one devbox, see it on the other.
#[test]
fn a_train_created_on_one_machine_appears_on_the_other() {
    let (_code, store, a, b) = two_devboxes();

    a.choo_ok(["init", "my-feature"]);
    a.branch("feat/part-1", "main");
    a.commit("p1.txt");
    a.choo_ok(["add"]);

    let listed = stdout(&b.choo_ok(["list"]));
    assert!(listed.contains("my-feature"), "b did not see it: {listed}");
    assert!(listed.contains("branches=1"), "got: {listed}");

    let shown = stdout(&b.choo_ok(["show", "my-feature"]));
    assert!(shown.contains("feat/part-1"), "got: {shown}");

    // And it really is in the state repo, not just cached locally.
    assert!(
        store.files("main").iter().any(|f| f.starts_with("repos/")),
        "state repo has no entry: {:?}",
        store.files("main")
    );
}

/// `active` is deliberately per-machine, so switching on one devbox must not
/// move the other one's pointer.
#[test]
fn the_active_train_stays_per_machine() {
    let (_code, _store, a, b) = two_devboxes();

    a.choo_ok(["init", "train-a"]);
    a.choo_ok(["init", "train-b"]);

    // `init` made `train-a` active on A only; B has no opinion yet.
    assert_eq!(a.active_train().as_deref(), Some("train-a"));
    assert_eq!(b.active_train(), None);

    b.choo_ok(["switch", "train-b"]);
    assert_eq!(b.active_train().as_deref(), Some("train-b"));
    assert_eq!(
        a.active_train().as_deref(),
        Some("train-a"),
        "B's switch must not move A's active train"
    );

    // Each machine's own listing marks its own active train.
    assert!(stdout(&a.choo_ok(["list"])).contains("* train-a"));
    assert!(stdout(&b.choo_ok(["list"])).contains("* train-b"));
}

/// Both machines write from the same starting point. The second push is
/// rejected, merges, and retries — and neither train may be lost.
#[test]
fn concurrent_writes_from_two_machines_both_survive() {
    let (_code, store, a, b) = two_devboxes();

    // Align both clones on the same base first.
    a.choo_ok(["list"]);
    b.choo_ok(["list"]);

    a.choo_ok(["init", "from-a"]);
    b.choo_ok(["init", "from-b"]);

    for repo in [&a, &b] {
        let listed = stdout(&repo.choo_ok(["list"]));
        assert!(listed.contains("from-a"), "lost from-a: {listed}");
        assert!(listed.contains("from-b"), "lost from-b: {listed}");
    }

    // No history was discarded to get there.
    let log = store.log("main");
    assert!(log.len() >= 2, "expected real history, got {log:?}");
}

/// Because every command pulls before it writes, two machines editing the
/// same train in sequence *merge* rather than conflict — B sees A's branch
/// before adding its own, so both end up in the train.
#[test]
fn sequential_edits_to_one_train_merge_rather_than_conflict() {
    let (_code, _store, a, b) = two_devboxes();

    a.choo_ok(["init", "shared"]);
    b.choo_ok(["list"]);

    a.branch("from-a", "main");
    a.commit("a.txt");
    a.choo_ok(["add", "from-a", "-t", "shared"]);

    b.branch("from-b", "main");
    b.commit("b.txt");
    let out = b.choo_ok(["add", "from-b", "-t", "shared"]);
    assert!(
        !stderr(&out).contains("changed here and on another machine"),
        "no conflict was needed here: {}",
        stderr(&out)
    );

    let shown = stdout(&b.choo_ok(["show", "shared"]));
    assert!(shown.contains("from-a"), "lost A's branch: {shown}");
    assert!(shown.contains("from-b"), "lost B's branch: {shown}");
}

/// The genuine conflict: B edits a train while offline, so it never saw A's
/// concurrent edit to the same train. On publishing, B's version wins, B is
/// told, and A's version stays recoverable from the store's history.
#[test]
fn a_truly_concurrent_edit_keeps_this_machines_version_and_warns() {
    let (_code, store, a, b) = two_devboxes();

    a.choo_ok(["init", "shared"]);
    b.choo_ok(["list"]); // B's clone now matches A's

    // B goes offline and edits `shared`.
    let hidden = store.path().with_extension("gone");
    std::fs::rename(store.path(), &hidden).unwrap();
    b.branch("from-b", "main");
    b.commit("b.txt");
    b.choo_ok(["add", "from-b", "-t", "shared"]);
    std::fs::rename(&hidden, store.path()).unwrap();

    // Meanwhile A edited the same train and published.
    a.branch("from-a", "main");
    a.commit("a.txt");
    a.choo_ok(["add", "from-a", "-t", "shared"]);

    // B comes back online: its pending commit no longer applies cleanly.
    let out = b.choo_ok(["list"]);
    let err = stderr(&out);
    assert!(
        err.contains("changed here and on another machine") && err.contains("shared"),
        "the conflict must be reported, got: {err}"
    );
    assert!(
        err.contains("log -p"),
        "the warning should say how to recover the other version: {err}"
    );

    // B's version is what the store holds now...
    let shown = stdout(&b.choo_ok(["show", "shared"]));
    assert!(shown.contains("from-b"), "got: {shown}");

    // ...and A's is still in the store's history, as the warning promised.
    let history = store.log("main");
    assert!(history.len() >= 3, "expected full history, got {history:?}");
}

/// Reads must keep working with the store unreachable — that's the point of
/// keeping a clone rather than fetching state on demand.
#[test]
fn reads_degrade_to_the_cached_clone_when_the_store_is_unreachable() {
    let (_code, store, a, _b) = two_devboxes();
    a.choo_ok(["init", "cached"]);

    let hidden = store.path().with_extension("gone");
    std::fs::rename(store.path(), &hidden).unwrap();

    let out = a.choo_try(["list"]);
    assert!(out.status.success(), "list must not fail: {}", stderr(&out));
    assert!(stdout(&out).contains("cached"), "got: {}", stdout(&out));
    assert!(
        stderr(&out).contains("warning"),
        "degradation must be reported, got: {}",
        stderr(&out)
    );

    std::fs::rename(&hidden, store.path()).unwrap();
}

/// A write with the store unreachable is committed locally and published by
/// the next command that can reach it. Nothing is lost, and the user is told.
#[test]
fn a_write_made_offline_publishes_on_the_next_online_command() {
    let (_code, store, a, b) = two_devboxes();
    a.choo_ok(["init", "before"]);

    let hidden = store.path().with_extension("gone");
    std::fs::rename(store.path(), &hidden).unwrap();

    let out = a.choo_try(["init", "made-offline"]);
    assert!(
        out.status.success(),
        "an offline write should still succeed: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("saved locally"),
        "user must be told it isn't published: {}",
        stderr(&out)
    );

    let status = stdout(&a.choo_try(["sync", "--status"]));
    assert!(status.contains("unpublished:  yes"), "got: {status}");

    std::fs::rename(&hidden, store.path()).unwrap();

    // Any command drains the queue; a read is enough.
    a.choo_ok(["list"]);
    let listed = stdout(&b.choo_ok(["list"]));
    assert!(
        listed.contains("made-offline"),
        "the offline write never arrived: {listed}"
    );
    assert!(store.log("main").len() >= 2);
}

/// A push refused by the server (not a race) keeps the local commit and says
/// so, rather than failing the command whose real work already succeeded.
#[test]
fn a_rejected_push_keeps_the_change_locally_and_recovers_later() {
    let (_code, store, a, b) = two_devboxes();
    a.choo_ok(["init", "first"]);

    store.reject_pushes();
    let out = a.choo_try(["init", "blocked"]);
    assert!(
        out.status.success(),
        "the train was created; the command shouldn't fail: {}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("warning"), "got: {}", stderr(&out));
    // Locally it exists regardless.
    assert!(stdout(&a.choo_ok(["list"])).contains("blocked"));

    store.accept_pushes();
    a.choo_ok(["list"]);
    assert!(
        stdout(&b.choo_ok(["list"])).contains("blocked"),
        "should have published once the server accepted pushes again"
    );
}

/// `--no-sync` must not silently fall back to the old local file — that
/// would show an empty set of trains on a machine that has plenty.
#[test]
fn no_sync_reads_the_clone_rather_than_pretending_state_is_local() {
    let (_code, store, a, _b) = two_devboxes();
    a.choo_ok(["init", "present"]);

    let hidden = store.path().with_extension("gone");
    std::fs::rename(store.path(), &hidden).unwrap();

    let out = a.choo_try(["--no-sync", "list"]);
    assert!(out.status.success(), "got: {}", stderr(&out));
    assert!(
        stdout(&out).contains("present"),
        "--no-sync must still show the trains: {}",
        stdout(&out)
    );

    std::fs::rename(&hidden, store.path()).unwrap();
}

/// Trains that existed before sharing was configured are moved into the
/// store on first use, and the old file is kept rather than deleted.
#[test]
fn existing_local_trains_are_adopted_into_shared_state() {
    let code = BareRepo::new();
    let store = BareRepo::new();

    let mut a = TestRepo::new();
    a.with_origin(&code);

    // Local-only to begin with.
    a.choo_ok(["init", "legacy"]);
    a.branch("legacy-branch", "main");
    a.commit("l.txt");
    a.choo_ok(["add"]);
    assert!(a.path().join(".git/choochoo/state.json").exists());

    // Now turn on sharing.
    a.share_state(&store);
    let listed = stdout(&a.choo_ok(["list"]));
    assert!(listed.contains("legacy"), "adoption lost the train: {listed}");

    assert!(
        !a.path().join(".git/choochoo/state.json").exists(),
        "the old file should have been renamed, not left in place"
    );
    assert!(
        a.path().join(".git/choochoo/state.json.adopted").exists(),
        "the old file should be kept as a backup"
    );

    // And a second machine can see the adopted train.
    let mut b = TestRepo::clone_of(&code);
    b.share_state(&store);
    assert!(stdout(&b.choo_ok(["list"])).contains("legacy"));
}

/// With no config at all, nothing about the old behaviour changes.
#[test]
fn without_a_config_state_stays_in_the_repo() {
    let a = TestRepo::new();
    a.choo_ok(["init", "local-only"]);
    assert!(a.path().join(".git/choochoo/state.json").exists());
    assert!(!a.path().join(".git/choochoo/state.json.adopted").exists());

    let status = stdout(&a.choo_ok(["sync"]));
    assert!(
        status.contains("local to this machine"),
        "sync should explain there's nothing shared: {status}"
    );
}

/// Identity comes from `origin`. Without one there's no way to know whose
/// trains to load, and guessing would be worse than saying so.
#[test]
fn a_repo_with_no_origin_cannot_use_shared_state() {
    let store = BareRepo::new();
    let mut a = TestRepo::new(); // deliberately no origin
    a.share_state(&store);

    let out = a.choo_try(["list"]);
    assert!(!out.status.success(), "expected a refusal");
    assert!(
        stderr(&out).contains("origin"),
        "the error should name the missing remote: {}",
        stderr(&out)
    );
}

/// A store URL that isn't a repo can't be cloned, and there's no state to
/// fall back on — so this fails loudly with the escape hatch named.
#[test]
fn an_unclonable_store_fails_with_advice() {
    let mut a = TestRepo::new();
    let code = BareRepo::new();
    a.with_origin(&code);
    a.share_state_with_url("/definitely/not/a/repo/anywhere.git");

    let out = a.choo_try(["list"]);
    assert!(!out.status.success(), "expected a failure");
    let err = stderr(&out);
    assert!(err.contains("--no-sync"), "should name the escape hatch: {err}");
}

/// A typo in the config that quietly left sharing off would look exactly
/// like choochoo losing a train.
#[test]
fn a_malformed_config_is_reported_not_ignored() {
    let a = TestRepo::new();
    let dir = a.path().join("cfg");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "[store]\nrep0 = \"typo\"\n").unwrap();

    let out = a
        .choo()
        .env("CHOOCHOO_CONFIG", &cfg)
        .arg("list")
        .output()
        .unwrap();
    assert!(!out.status.success(), "a typo must not be ignored");
    assert!(
        stderr(&out).contains("config"),
        "should point at the config: {}",
        stderr(&out)
    );
}

/// `choo sync --status` is the one command to run when this misbehaves, so
/// it has to name the store, the branch, and whether anything is pending.
#[test]
fn sync_status_reports_where_state_lives() {
    let (_code, store, a, _b) = two_devboxes();
    a.choo_ok(["init", "t"]);

    let status = stdout(&a.choo_ok(["sync", "--status"]));
    assert!(status.contains(&store.url()), "got: {status}");
    assert!(status.contains("trains:       1"), "got: {status}");
    assert!(status.contains("unpublished:  no"), "got: {status}");
}

/// Two `choo` processes sharing one store clone must serialise, not lose an
/// update — the store lock also closes this race for the local backend.
#[test]
fn concurrent_choo_processes_on_one_machine_do_not_lose_updates() {
    let (_code, _store, a, _b) = two_devboxes();
    a.choo_ok(["init", "seed"]);

    let one = a.choo().args(["init", "race-one"]).spawn().unwrap();
    let two = a.choo().args(["init", "race-two"]).spawn().unwrap();
    let o1 = one.wait_with_output().unwrap();
    let o2 = two.wait_with_output().unwrap();
    assert!(o1.status.success(), "{}", stderr(&o1));
    assert!(o2.status.success(), "{}", stderr(&o2));

    let listed = stdout(&a.choo_ok(["list"]));
    for name in ["seed", "race-one", "race-two"] {
        assert!(listed.contains(name), "lost {name}: {listed}");
    }
}
