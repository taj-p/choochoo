//! `choo pr` — create or update a PR per branch and sync the train table.
//!
//! ## Algorithm
//!
//! 1. For each branch in train order, ensure a PR exists with `head=branch`
//!    and `base=parent`. If state has a PR number we trust it; otherwise we
//!    look it up via [`crate::github::GhRunner::find_pr_for_branch`]. If
//!    none exists we create one with a render-time-known body.
//! 2. After step 1 every branch has a PR number. Walk the train again and
//!    [`render::rerender_pr_body`] every PR body. The user-authored region
//!    between `<!-- choochoo:body:start -->` markers is preserved.
//! 3. Adjust the `base` of any PR whose stored base differs from its
//!    train-parent (handles reorders).
//!
//! Step 1 is idempotent: the second run finds the same PR, so no new PR is
//! created. Step 2 is also idempotent because [`render::rerender_pr_body`]
//! produces the same output for the same input.

use std::path::Path;

use crate::error::Result;
use crate::github::GhRunner;
use crate::render;
use crate::report::Reporter;
use crate::state::{self, PrInfo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    pub train: String,
    pub created: Vec<String>,
    pub updated: Vec<String>,
}

pub fn run(
    repo_root: &Path,
    gh: &dyn GhRunner,
    reporter: &mut dyn Reporter,
    train_name: Option<&str>,
    draft: bool,
) -> Result<PrSummary> {
    let mut state = state::load(repo_root)?;
    let train_name = state.resolve_train_name(train_name)?.to_string();

    let pairs: Vec<(String, String)> = state
        .train(&train_name)?
        .pairs()
        .map(|(p, c)| (p.to_string(), c.to_string()))
        .collect();
    let total = pairs.len();

    let mut created = Vec::new();

    // Step 1: ensure every branch has a PR.
    for (i, (parent, child)) in pairs.iter().enumerate() {
        let train_snapshot = state.train(&train_name)?.clone();

        reporter.start(&format!(
            "ensuring PR for `{child}` ({n}/{total})",
            n = i + 1,
        ));

        let already = state
            .train(&train_name)?
            .prs
            .get(child)
            .cloned()
            .or(gh.find_pr_for_branch(child)?);

        match already {
            Some(pr) => {
                let detail = format!("#{} already exists", pr.number);
                state
                    .train_mut(&train_name)?
                    .prs
                    .insert(child.clone(), pr);
                reporter.ok(&detail);
            }
            None => {
                let body = render::render_pr_body(&train_snapshot, child, "");
                let title = child.clone();
                let pr = match gh.create_pr(child, parent, &title, &body, draft) {
                    Ok(pr) => pr,
                    Err(e) => {
                        reporter.fail(&e.to_string());
                        return Err(e);
                    }
                };
                let detail = format!("created #{}", pr.number);
                state
                    .train_mut(&train_name)?
                    .prs
                    .insert(child.clone(), pr);
                created.push(child.clone());
                reporter.ok(&detail);
            }
        }
    }

    // Step 2a: refresh title + body for every branch in one silent pass.
    // We have to gather everything before rendering anything, because
    // refreshing branch B's title affects how branch A's table renders.
    let mut snapshots: Vec<(String, u64, String)> = Vec::with_capacity(pairs.len());
    for (_parent, child) in &pairs {
        let info: PrInfo = state
            .train(&train_name)?
            .prs
            .get(child)
            .cloned()
            .expect("every branch should have a PR by step 2");
        let snap = gh.get_pr(info.number)?;
        if let Some(pr) = state.train_mut(&train_name)?.prs.get_mut(child) {
            pr.title = if snap.title.is_empty() {
                None
            } else {
                Some(snap.title.clone())
            };
        }
        snapshots.push((child.clone(), info.number, snap.body));
    }

    // Step 2b: re-render every PR's body now that all titles + numbers
    // are known. Idempotent: bodies that already match are skipped.
    let train_final = state.train(&train_name)?.clone();
    let mut updated = Vec::new();
    for (i, (child, number, existing_body)) in snapshots.iter().enumerate() {
        reporter.start(&format!(
            "syncing description for `{child}` ({n}/{total})",
            n = i + 1,
        ));
        let new_body = render::rerender_pr_body(&train_final, child, existing_body);
        if &new_body != existing_body {
            if let Err(e) = gh.update_pr_body(*number, &new_body) {
                reporter.fail(&e.to_string());
                return Err(e);
            }
            updated.push(child.clone());
            reporter.ok("updated");
        } else {
            reporter.ok("unchanged");
        }
    }

    state::save(repo_root, &state)?;
    Ok(PrSummary {
        train: train_name,
        created,
        updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::FakeGh;
    use crate::report::{NullReporter, RecordingReporter};
    use crate::state::{StateFile, Train};
    use tempfile::TempDir;

    fn setup() -> (TempDir, FakeGh) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/choochoo")).unwrap();
        let mut state = StateFile::default();
        let mut t = Train::new("t", "main");
        t.branches = vec!["a".into(), "b".into(), "c".into()];
        state.trains.insert("t".into(), t);
        state.active = Some("t".into());
        state::save(tmp.path(), &state).unwrap();

        let gh = FakeGh::open(tmp.path().join(".git/choochoo/gh.json")).unwrap();
        (tmp, gh)
    }

    #[test]
    fn first_run_creates_one_pr_per_branch() {
        let (tmp, gh) = setup();
        let summary = run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        assert_eq!(summary.created, vec!["a", "b", "c"]);
        assert_eq!(summary.updated.len(), 3);

        let state = state::load(tmp.path()).unwrap();
        let prs = &state.train("t").unwrap().prs;
        assert_eq!(prs.len(), 3);
    }

    #[test]
    fn second_run_is_a_true_noop() {
        // After the first run, every PR's body is already in sync with the
        // train state. The second run must therefore neither create nor
        // update anything: this is what makes `choo pr` safe to re-run.
        let (tmp, gh) = setup();
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        let summary = run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        assert!(summary.created.is_empty());
        assert!(
            summary.updated.is_empty(),
            "expected no body updates on a clean re-run, got: {:?}",
            summary.updated
        );
    }

    #[test]
    fn pr_for_first_branch_targets_base() {
        let (tmp, gh) = setup();
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        let s = gh.path();
        let raw = std::fs::read_to_string(s).unwrap();
        // PR for "a" should have base "main"; PR for "b" should have base "a".
        assert!(raw.contains("\"head\": \"a\""));
        assert!(raw.contains("\"base\": \"main\""));
        assert!(raw.contains("\"head\": \"b\""));
        // serde formats nested objects on separate lines, so this also passes:
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let prs = &parsed["prs"];
        assert_eq!(prs["a"]["base"], "main");
        assert_eq!(prs["b"]["base"], "a");
        assert_eq!(prs["c"]["base"], "b");
    }

    #[test]
    fn pr_body_contains_train_table_with_self_marker() {
        let (tmp, gh) = setup();
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        let raw = std::fs::read_to_string(gh.path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let body_b = parsed["prs"]["b"]["body"].as_str().unwrap().to_string();
        assert!(body_b.contains("**this PR**"));
        assert!(body_b.contains("| Title | PR |"));
        // `setup()` creates PRs with title=branch (the choo pr default), so
        // every row's title cell is the branch name as a plain title.
        assert!(body_b.contains("| a | #1 |"));
        assert!(body_b.contains("| b | #2 |"));
        assert!(body_b.contains("| c | #3 |"));
    }

    /// Regression: a second `choo pr` must NOT clobber whatever the user
    /// (or another tool) wrote into the PR body. The user-authored region
    /// between the `<!-- choochoo:body:* -->` markers must round-trip.
    #[test]
    fn second_run_preserves_user_body_between_markers() {
        let (tmp, gh) = setup();
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();

        // User edits PR `b`'s body via the GitHub UI: they write a
        // description between the markers and a stray comment outside.
        let state = state::load(tmp.path()).unwrap();
        let pr_b = state.train("t").unwrap().prs.get("b").cloned().unwrap();
        let edited = format!(
            "<!-- choochoo:train name=\"t\" -->\n\
             ## Train: `t`\n\n\
             | # | Branch | PR | |\n\
             |---|--------|----|---|\n\
             | 1 | `a` | #1 |  |\n\
             | 2 | `b` | #2 | **this PR** |\n\
             | 3 | `c` | #3 |  |\n\n\
             Base: `main`\n\n\
             {}\n\
             ## What this PR does\n\n\
             It refactors the widget store to use the new API.\n\n\
             - Bullet one\n\
             - Bullet two\n\
             {}\n",
            crate::render::BODY_START,
            crate::render::BODY_END,
        );
        gh.update_pr_body(pr_b.number, &edited).unwrap();

        // Re-run `choo pr`. The user content must survive verbatim.
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();

        let after = gh.get_pr(pr_b.number).unwrap().body;
        assert!(
            after.contains("It refactors the widget store"),
            "user body lost!\n--- after ---\n{after}"
        );
        assert!(after.contains("- Bullet one"));
        assert!(after.contains("- Bullet two"));
        // Train table still present too.
        assert!(after.contains("**this PR**"));
        assert!(after.contains("| Title | PR |"));
    }

    #[test]
    fn emits_two_progress_steps_per_branch() {
        // Once for "ensuring PR" (step 1) and once for "syncing description"
        // (step 2). For three branches that's six events.
        let (tmp, gh) = setup();
        let mut rep = RecordingReporter::new();
        run(tmp.path(), &gh, &mut rep, None, false).unwrap();
        assert_eq!(rep.events.len(), 6, "events: {:?}", rep.events);
        assert!(rep.events[0].starts_with("ensuring PR for `a`"));
        assert!(rep.events[0].contains("(1/3)"));
        assert!(rep.events[0].contains("created #1"));
        assert!(rep.events[3].starts_with("syncing description for `a`"));
        assert!(rep.events[3].contains("(1/3)"));
    }

    #[test]
    fn second_run_progress_says_unchanged() {
        let (tmp, gh) = setup();
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        let mut rep = RecordingReporter::new();
        run(tmp.path(), &gh, &mut rep, None, false).unwrap();
        // Ensuring step says "already exists" (#1, #2, #3); syncing says
        // "unchanged" because the body matches.
        let joined = rep.joined();
        assert!(joined.contains("already exists"), "got: {joined}");
        assert!(joined.contains("unchanged"), "got: {joined}");
    }

    /// Regression: a body that was authored outside choochoo (no markers
    /// at all) must survive verbatim. The managed block is *appended* to
    /// the end so the user's description stays prominent at the top.
    #[test]
    fn second_run_appends_block_to_foreign_body() {
        let (tmp, gh) = setup();
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        let pr_a = state::load(tmp.path())
            .unwrap()
            .train("t")
            .unwrap()
            .prs
            .get("a")
            .cloned()
            .unwrap();

        let foreign = "I wrote this manually before choochoo synced this PR.";
        gh.update_pr_body(pr_a.number, foreign).unwrap();

        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        let after = gh.get_pr(pr_a.number).unwrap().body;
        assert!(after.starts_with(foreign), "foreign body lost: {after}");
        assert!(after.contains(crate::render::TRAIN_START));
        assert!(after.contains(crate::render::TRAIN_END));
    }

    /// Renaming a PR title on GitHub propagates back into every PR's
    /// train table on the next `choo pr` run.
    #[test]
    fn renamed_title_propagates_into_train_tables() {
        let (tmp, gh) = setup();
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();

        // PR #1 (head=`a`) gets renamed by the user via the GitHub UI.
        gh.set_pr_title(1, "Refactor widget store").unwrap();

        // Next `choo pr` should pick that up and rewrite *every* PR's
        // body so the train table on PRs #2/#3 also shows the new title.
        let summary = run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        assert!(
            summary.updated.len() == 3,
            "all bodies should re-render to show new title; got: {:?}",
            summary.updated
        );
        for branch in ["a", "b", "c"] {
            let body = gh
                .get_pr(branch_pr_number(&gh, branch))
                .unwrap()
                .body;
            assert!(
                body.contains("Refactor widget store"),
                "PR for `{branch}` did not pick up the rename:\n{body}"
            );
            // Old title is gone from this PR's table cell.
            assert!(
                !body.contains("| a | #1 |"),
                "PR for `{branch}` still shows old title:\n{body}"
            );
        }

        // And the rename is now persisted in choochoo state too.
        let st = state::load(tmp.path()).unwrap();
        assert_eq!(
            st.train("t").unwrap().prs.get("a").unwrap().title.as_deref(),
            Some("Refactor widget store")
        );
    }

    fn branch_pr_number(gh: &FakeGh, branch: &str) -> u64 {
        gh.find_pr_for_branch(branch).unwrap().unwrap().number
    }

    /// Regression for the exact bug report: user wrote prose ABOVE the
    /// choochoo block (using the legacy markers). After re-render, the
    /// prose must survive AND the legacy markers must be migrated away.
    #[test]
    fn second_run_rescues_prose_above_legacy_block() {
        let (tmp, gh) = setup();
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        let pr_b = state::load(tmp.path())
            .unwrap()
            .train("t")
            .unwrap()
            .prs
            .get("b")
            .cloned()
            .unwrap();

        // Construct the same shape the user reported: a paragraph above
        // the legacy `<!-- choochoo:train ... -->` header, with empty
        // body markers below the table.
        let edited = format!(
            "Hello there\n\
             <!-- choochoo:train name=\"t\" -->\n\
             ## Train: `t`\n\n\
             | # | Branch | PR |   |\n\
             |---|--------|----|---|\n\
             | 1 | `a` | #1 |  |\n\
             | 2 | `b` | #2 | **this PR** |\n\
             | 3 | `c` | #3 |  |\n\n\
             Base: `main`\n\n\
             {}\n\n\
             {}\n",
            crate::render::BODY_START,
            crate::render::BODY_END,
        );
        gh.update_pr_body(pr_b.number, &edited).unwrap();

        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        let after = gh.get_pr(pr_b.number).unwrap().body;
        assert!(
            after.starts_with("Hello there"),
            "prefix prose lost!\n--- after ---\n{after}"
        );
        // Legacy markers fully migrated away.
        assert!(!after.contains("<!-- choochoo:train name=\""));
        assert!(!after.contains(crate::render::BODY_START));
        assert!(!after.contains(crate::render::BODY_END));
        // New markers in place.
        assert!(after.contains(crate::render::TRAIN_START));
        assert!(after.contains(crate::render::TRAIN_END));
    }
}
