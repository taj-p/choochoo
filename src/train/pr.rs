//! `choo pr` — create or update a PR per branch and sync the train table.
//!
//! ## Algorithm
//!
//! 1. For each branch in train order, ensure a PR exists with `head=branch`
//!    and `base=parent`. If state has a PR number we trust it; otherwise we
//!    look it up via [`crate::github::GhRunner::find_pr_for_branch`]. If
//!    none exists we create one with a render-time-known body.
//! 2. If the train has an aggregate branch, ensure it too has a PR — always
//!    a **draft**, and always with `base` = the train's base rather than a
//!    train branch, so its diff is the whole train at once. It is created
//!    after the per-branch PRs so its table can reference their numbers.
//! 3. After steps 1-2 every branch has a PR number. Walk the train again
//!    and [`render::rerender_pr_body`] every PR body. The user-authored
//!    region outside choochoo's markers is preserved.
//!
//! Step 1 is idempotent: the second run finds the same PR, so no new PR is
//! created. Step 3 is also idempotent because [`render::rerender_pr_body`]
//! produces the same output for the same input.

use std::path::Path;

use crate::error::Result;
use crate::github::GhRunner;
use crate::render;
use crate::report::Reporter;
use crate::state::{self, PrInfo, Train};
use crate::train::aggregate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    pub train: String,
    /// Branches (including the aggregate branch, when enabled) whose PR was
    /// opened by this run.
    pub created: Vec<String>,
    /// Branches whose PR description this run rewrote.
    pub updated: Vec<String>,
    /// The train's aggregate PR, when the train has an aggregate branch.
    pub aggregate_pr: Option<PrInfo>,
}

/// PR metadata choochoo has recorded for `branch`, which may be a train
/// branch or the train's aggregate branch.
fn recorded_pr(train: &Train, branch: &str) -> Option<PrInfo> {
    if train.is_aggregate(branch) {
        train.aggregate.as_ref().and_then(|a| a.pr.clone())
    } else {
        train.prs.get(branch).cloned()
    }
}

fn record_pr(train: &mut Train, branch: &str, pr: PrInfo) {
    if train.is_aggregate(branch) {
        if let Some(agg) = train.aggregate.as_mut() {
            agg.pr = Some(pr);
        }
    } else {
        train.prs.insert(branch.to_string(), pr);
    }
}

fn record_title(train: &mut Train, branch: &str, title: Option<String>) {
    let slot = if train.is_aggregate(branch) {
        train.aggregate.as_mut().and_then(|a| a.pr.as_mut())
    } else {
        train.prs.get_mut(branch)
    };
    if let Some(pr) = slot {
        pr.title = title;
    }
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

        let already = recorded_pr(state.train(&train_name)?, child)
            .or(gh.find_pr_for_branch(child)?);

        match already {
            Some(pr) => {
                let detail = format!("#{} already exists", pr.number);
                record_pr(state.train_mut(&train_name)?, child, pr);
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
                record_pr(state.train_mut(&train_name)?, child, pr);
                created.push(child.clone());
                reporter.ok(&detail);
            }
        }
    }

    // Step 2: the aggregate branch's draft PR, if the train has one. It
    // targets the train's *base*, not a train branch, so its diff is every
    // change in the train; and it's a draft regardless of `--draft`,
    // because it exists to be read, not merged.
    // An empty train has no combined branch yet (nothing to mirror), so
    // there's nothing to open a PR for either.
    let aggregate_branch = state
        .train(&train_name)?
        .aggregate_branch()
        .filter(|_| total > 0)
        .map(str::to_string);
    if let Some(branch) = &aggregate_branch {
        let train_snapshot = state.train(&train_name)?.clone();
        reporter.start(&format!("ensuring combined PR for `{branch}`"));
        let already =
            recorded_pr(&train_snapshot, branch).or(gh.find_pr_for_branch(branch)?);
        match already {
            Some(pr) => {
                let detail = format!("#{} already exists", pr.number);
                record_pr(state.train_mut(&train_name)?, branch, pr);
                reporter.ok(&detail);
            }
            None => {
                let body = render::render_pr_body(&train_snapshot, branch, "");
                let title = aggregate::pr_title(&train_snapshot);
                let pr = match gh.create_pr(
                    branch,
                    &train_snapshot.base,
                    &title,
                    &body,
                    /* draft */ true,
                ) {
                    Ok(pr) => pr,
                    Err(e) => {
                        reporter.fail(&e.to_string());
                        return Err(e);
                    }
                };
                let detail = format!("created draft #{}", pr.number);
                record_pr(state.train_mut(&train_name)?, branch, pr);
                created.push(branch.clone());
                reporter.ok(&detail);
            }
        }
    }

    // Step 3a: refresh title + body for every PR in one silent pass. We
    // have to gather everything before rendering anything, because
    // refreshing branch B's title affects how branch A's table renders.
    let all_branches: Vec<String> = pairs
        .iter()
        .map(|(_, child)| child.clone())
        .chain(aggregate_branch.clone())
        .collect();
    let mut snapshots: Vec<(String, u64, String)> = Vec::with_capacity(all_branches.len());
    for child in &all_branches {
        let info: PrInfo = recorded_pr(state.train(&train_name)?, child)
            .expect("every branch should have a PR by step 3");
        let snap = gh.get_pr(info.number)?;
        let title = if snap.title.is_empty() {
            None
        } else {
            Some(snap.title.clone())
        };
        record_title(state.train_mut(&train_name)?, child, title);
        snapshots.push((child.clone(), info.number, snap.body));
    }

    // Step 3b: re-render every PR's body now that all titles + numbers
    // are known. Idempotent: bodies that already match are skipped.
    let train_final = state.train(&train_name)?.clone();
    let sync_total = snapshots.len();
    let mut updated = Vec::new();
    for (i, (child, number, existing_body)) in snapshots.iter().enumerate() {
        reporter.start(&format!(
            "syncing description for `{child}` ({n}/{sync_total})",
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

    let aggregate_pr = state
        .train(&train_name)?
        .aggregate
        .as_ref()
        .and_then(|a| a.pr.clone());
    state::save(repo_root, &state)?;
    Ok(PrSummary {
        train: train_name,
        created,
        updated,
        aggregate_pr,
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

    /// Enable the aggregate branch on the fixture train.
    fn enable_aggregate(tmp: &TempDir, branch: &str) {
        let mut state = state::load(tmp.path()).unwrap();
        state.train_mut("t").unwrap().aggregate =
            Some(crate::state::Aggregate::new(branch));
        state::save(tmp.path(), &state).unwrap();
    }

    #[test]
    fn aggregate_gets_a_draft_pr_against_the_base() {
        let (tmp, gh) = setup();
        enable_aggregate(&tmp, "choo/t/combined");
        let summary = run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        assert_eq!(summary.created, vec!["a", "b", "c", "choo/t/combined"]);

        let raw = std::fs::read_to_string(gh.path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let combined = &parsed["prs"]["choo/t/combined"];
        // Base is the train's base, not the tip: the diff is the whole train.
        assert_eq!(combined["base"], "main");
        assert_eq!(combined["draft"], true);
        assert_eq!(combined["title"], "Combined: t");
        assert_eq!(
            summary.aggregate_pr.map(|p| p.number),
            combined["number"].as_u64()
        );
    }

    /// `--draft` is about the per-branch PRs; the combined PR is a draft
    /// either way, because it isn't the thing being merged.
    #[test]
    fn aggregate_pr_is_a_draft_even_when_branch_prs_are_not() {
        let (tmp, gh) = setup();
        enable_aggregate(&tmp, "choo/t/combined");
        run(tmp.path(), &gh, &mut NullReporter, None, /* draft */ false).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(gh.path()).unwrap()).unwrap();
        assert_eq!(parsed["prs"]["a"]["draft"], false);
        assert_eq!(parsed["prs"]["choo/t/combined"]["draft"], true);
    }

    #[test]
    fn aggregate_run_is_also_idempotent() {
        let (tmp, gh) = setup();
        enable_aggregate(&tmp, "choo/t/combined");
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
    fn aggregate_row_appears_in_every_pr_body() {
        let (tmp, gh) = setup();
        enable_aggregate(&tmp, "choo/t/combined");
        run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(gh.path()).unwrap()).unwrap();
        for branch in ["a", "b", "c", "choo/t/combined"] {
            let body = parsed["prs"][branch]["body"].as_str().unwrap();
            assert!(
                body.contains("| Σ | Combined: t | #4 |"),
                "`{branch}` body missing the combined row:\n{body}"
            );
        }
        // The combined PR is the one marked "this PR" on that row.
        let combined_body = parsed["prs"]["choo/t/combined"]["body"]
            .as_str()
            .unwrap();
        assert!(combined_body.contains("| Σ | Combined: t | #4 | **this PR** |"));
    }

    /// An empty train has nothing to combine, so no combined PR is opened
    /// (its branch doesn't exist yet either).
    #[test]
    fn empty_train_with_aggregate_creates_no_prs() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/choochoo")).unwrap();
        let mut state = StateFile::default();
        let mut t = Train::new("t", "main");
        t.aggregate = Some(crate::state::Aggregate::new("choo/t/combined"));
        state.trains.insert("t".into(), t);
        state.active = Some("t".into());
        state::save(tmp.path(), &state).unwrap();
        let gh = FakeGh::open(tmp.path().join(".git/choochoo/gh.json")).unwrap();

        let summary = run(tmp.path(), &gh, &mut NullReporter, None, false).unwrap();
        assert!(summary.created.is_empty());
        assert!(summary.aggregate_pr.is_none());
        assert!(gh.find_pr_for_branch("choo/t/combined").unwrap().is_none());
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
