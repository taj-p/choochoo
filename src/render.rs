//! Pure rendering of PR descriptions and the train table.
//!
//! ## Marker convention
//!
//! choochoo owns one contiguous region of a PR description, delimited by
//! `<!-- choochoo:train:start ... -->` ... `<!-- choochoo:train:end -->`
//! markers. Everything outside that region is the user's — choochoo
//! preserves it verbatim across re-renders.
//!
//! On first sync of a PR that already has a description (or a PR created
//! outside choochoo), the managed block is appended to the **end** of the
//! existing body. On subsequent runs the block stays put.
//!
//! ## Migration from the old (pre-bugfix) marker scheme
//!
//! Earlier versions used a one-line header `<!-- choochoo:train ... -->`
//! followed by the table, with a separate `<!-- choochoo:body:start -->`
//! ... `<!-- choochoo:body:end -->` region for user content. That scheme
//! silently dropped anything *before* the header (a real footgun). When
//! [`rerender_pr_body`] sees an old-style body, it recovers content from
//! all three regions (prefix, body, suffix) and rewrites the description
//! using the new convention.

use crate::state::{PrInfo, Train};

/// Opening of the choochoo-managed region.
const TRAIN_START_PREFIX: &str = "<!-- choochoo:train:start";
const TRAIN_START_SUFFIX: &str = "-->";
/// Closing of the choochoo-managed region.
const TRAIN_END_MARKER: &str = "<!-- choochoo:train:end -->";

// Legacy markers retained for one-time migration only.
const LEGACY_HEADER_PREFIX: &str = "<!-- choochoo:train ";
const LEGACY_HEADER_SUFFIX: &str = "-->";
const LEGACY_BODY_START: &str = "<!-- choochoo:body:start -->";
const LEGACY_BODY_END: &str = "<!-- choochoo:body:end -->";

// Public for tests / external callers that want to assert on the marker
// constants without depending on the exact byte sequence.
pub const BODY_START: &str = LEGACY_BODY_START;
pub const BODY_END: &str = LEGACY_BODY_END;
pub const TRAIN_START: &str = TRAIN_START_PREFIX;
pub const TRAIN_END: &str = TRAIN_END_MARKER;

/// Render a complete PR body for a brand-new PR. The block is the entire
/// content; the user is expected to add their own description around it
/// later, and choochoo will preserve it.
pub fn render_pr_body(train: &Train, branch: &str, _: &str) -> String {
    let mut out = render_managed_block(train, branch);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Re-render a PR body, preserving any user content outside the
/// choochoo-managed region. Idempotent: a second call with the output of
/// the first returns the same string.
pub fn rerender_pr_body(train: &Train, branch: &str, existing: &str) -> String {
    let new_block = render_managed_block(train, branch);

    // (1) Already-converted body: replace just the new block.
    if let Some((prefix, suffix)) = split_around_new_block(existing) {
        return assemble(prefix, &new_block, suffix);
    }

    // (2) Legacy body: rescue prefix + (legacy inner body) + suffix.
    if let Some(parts) = split_around_legacy_block(existing) {
        let combined_suffix = combine_suffix(&parts.legacy_inner, parts.suffix);
        return assemble(parts.prefix, &new_block, &combined_suffix);
    }

    // (3) Foreign body (no markers at all): append the block to the end
    // so the existing description stays prominent.
    if existing.trim().is_empty() {
        return ensure_trailing_newline(new_block);
    }
    assemble(existing, &new_block, "")
}

/// Best-effort title from the most recent commit message subject. Falls
/// back to the branch name if the subject is empty.
pub fn pr_title_from_commit(commit_subject: &str, branch: &str) -> String {
    let subject = commit_subject.trim();
    if subject.is_empty() {
        branch.to_string()
    } else {
        subject.to_string()
    }
}

/// Render just the marker-wrapped managed block (no surrounding user
/// content). Public so callers / tests can assert on it directly.
pub fn render_managed_block(train: &Train, current: &str) -> String {
    let mut out = String::new();
    out.push_str(TRAIN_START_PREFIX);
    out.push_str(&format!(
        " name=\"{}\" {TRAIN_START_SUFFIX}\n",
        train.name.replace('\\', "\\\\").replace('"', "\\\"")
    ));
    out.push_str(&format!("## Train: `{}`\n\n", train.name));
    out.push_str(&render_table(train, Some(current)));
    out.push_str(&format!("\nBase: `{}`\n", train.base));
    if let Some(branch) = train.aggregate_branch() {
        out.push_str(&format!(
            "\n`{AGGREGATE_ROW_LABEL}` — combined branch `{branch}`: every change in the \
             train as one draft PR against `{base}`, for review and CI only. \
             Merge the PRs above, not this one.\n",
            base = train.base,
        ));
    }
    out.push_str(TRAIN_END_MARKER);
    out
}

/// Row label used in the `#` column for the aggregate ("combined") row.
pub const AGGREGATE_ROW_LABEL: &str = "Σ";

/// Render the markdown train table. If `highlight` matches a branch in the
/// train, mark it with "this PR" in the rightmost column.
///
/// The Title column shows the PR title (as last seen on GitHub); when no
/// PR exists yet or no title is recorded we fall back to the branch name
/// in backticks so the table is still useful mid-creation.
///
/// A train with an aggregate branch gets one extra row, numbered
/// [`AGGREGATE_ROW_LABEL`] rather than a position, because it sits beside
/// the stack rather than in it.
pub fn render_table(train: &Train, highlight: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("| # | Title | PR |   |\n");
    out.push_str("|---|-------|----|---|\n");
    for (i, branch) in train.branches.iter().enumerate() {
        let info = train.prs.get(branch);
        out.push_str(&render_row(
            &(i + 1).to_string(),
            branch,
            info,
            highlight,
        ));
    }
    if let Some(agg) = &train.aggregate {
        out.push_str(&render_row(
            AGGREGATE_ROW_LABEL,
            &agg.branch,
            agg.pr.as_ref(),
            highlight,
        ));
    }
    out
}

fn render_row(
    number_cell: &str,
    branch: &str,
    info: Option<&PrInfo>,
    highlight: Option<&str>,
) -> String {
    let title_cell = match info.and_then(|p| p.title.as_deref()) {
        Some(t) => escape_table_cell(t),
        None => format!("`{branch}`"),
    };
    let pr_cell = match info {
        Some(PrInfo { number, .. }) => format!("#{number}"),
        None => "—".to_string(),
    };
    let marker = match highlight {
        Some(h) if h == branch => "**this PR**",
        _ => "",
    };
    format!("| {number_cell} | {title_cell} | {pr_cell} | {marker} |\n")
}

/// Escape characters that would break a markdown table cell: pipe and
/// newline.
fn escape_table_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

// ---------------------------------------------------------------------------
// Splitting / assembly helpers
// ---------------------------------------------------------------------------

/// Locate the new-style managed block in `body` and return the surrounding
/// prefix and suffix slices (or [`None`] if the block isn't present).
fn split_around_new_block(body: &str) -> Option<(&str, &str)> {
    let start = body.find(TRAIN_START_PREFIX)?;
    let after_start = start + TRAIN_START_PREFIX.len();
    let end_in_body = body[after_start..].find(TRAIN_END_MARKER)?;
    let block_end = after_start + end_in_body + TRAIN_END_MARKER.len();
    Some((&body[..start], &body[block_end..]))
}

struct LegacyParts<'a> {
    prefix: &'a str,
    legacy_inner: String,
    suffix: &'a str,
}

/// Locate the old-style managed block in `body`. The block runs from the
/// legacy header marker to the legacy `body:end` marker (or to a sensible
/// fallback if `body:end` isn't present).
fn split_around_legacy_block(body: &str) -> Option<LegacyParts<'_>> {
    let header_at = body.find(LEGACY_HEADER_PREFIX)?;
    // Confirm it really is a header line by finding the closing `-->`
    // on the same line.
    let after_header_prefix = header_at + LEGACY_HEADER_PREFIX.len();
    let header_close_rel = body[after_header_prefix..].find(LEGACY_HEADER_SUFFIX)?;
    let header_end = after_header_prefix + header_close_rel + LEGACY_HEADER_SUFFIX.len();

    let prefix = &body[..header_at];

    // Block ends at body:end if present; otherwise the rest of the body
    // belongs to the legacy block (no suffix).
    let inner_search = &body[header_end..];
    let (block_end_in_body, body_end_marker_present) =
        if let Some(rel) = inner_search.find(LEGACY_BODY_END) {
            (header_end + rel + LEGACY_BODY_END.len(), true)
        } else {
            (body.len(), false)
        };

    let suffix = if block_end_in_body < body.len() {
        &body[block_end_in_body..]
    } else {
        ""
    };

    // What lived between body:start and body:end? If the user wrote
    // anything there, we want to preserve it.
    let legacy_inner = if body_end_marker_present {
        let between = &body[header_end..block_end_in_body - LEGACY_BODY_END.len()];
        if let Some(bs) = between.find(LEGACY_BODY_START) {
            let after_bs = bs + LEGACY_BODY_START.len();
            between[after_bs..].trim_matches('\n').to_string()
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    Some(LegacyParts {
        prefix,
        legacy_inner,
        suffix,
    })
}

fn combine_suffix(legacy_inner: &str, suffix: &str) -> String {
    let inner = legacy_inner.trim();
    let suffix = suffix.trim();
    match (inner.is_empty(), suffix.is_empty()) {
        (true, _) => suffix.to_string(),
        (false, true) => inner.to_string(),
        (false, false) => format!("{inner}\n\n{suffix}"),
    }
}

fn assemble(prefix: &str, block: &str, suffix: &str) -> String {
    let prefix = prefix.trim_end_matches('\n').trim_end();
    let suffix = suffix.trim_start_matches('\n').trim_start();
    let block = block.trim_matches('\n');

    let mut out = String::new();
    if !prefix.is_empty() {
        out.push_str(prefix);
        out.push_str("\n\n");
    }
    out.push_str(block);
    out.push('\n');
    if !suffix.is_empty() {
        out.push('\n');
        out.push_str(suffix);
        if !suffix.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn ensure_trailing_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PrInfo;

    fn sample_train() -> Train {
        let mut t = Train::new("feat", "main");
        t.branches = vec!["a".into(), "b".into(), "c".into()];
        t.prs.insert(
            "a".into(),
            PrInfo {
                number: 10,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        t.prs.insert(
            "b".into(),
            PrInfo {
                number: 11,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        t
    }

    #[test]
    fn table_marks_current() {
        let t = sample_train();
        insta::assert_snapshot!("table_marks_current", render_table(&t, Some("b")));
    }

    #[test]
    fn table_uses_title_when_known_and_falls_back_to_branch() {
        let mut t = Train::new("feat", "main");
        t.branches = vec!["a".into(), "b".into()];
        t.prs.insert(
            "a".into(),
            PrInfo {
                number: 10,
                url: "u".into(),
                title: Some("Refactor widget store".into()),
                last_pushed_sha: None,
            },
        );
        // `b` has a PR but no recorded title -> fall back to branch name.
        t.prs.insert(
            "b".into(),
            PrInfo {
                number: 11,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        insta::assert_snapshot!(
            "table_with_titles",
            render_table(&t, Some("a"))
        );
    }

    #[test]
    fn table_escapes_pipe_characters_in_titles() {
        let mut t = Train::new("feat", "main");
        t.branches = vec!["x".into()];
        t.prs.insert(
            "x".into(),
            PrInfo {
                number: 1,
                url: "u".into(),
                title: Some("Add foo|bar option".into()),
                last_pushed_sha: None,
            },
        );
        let out = render_table(&t, None);
        assert!(out.contains("Add foo\\|bar option"), "got: {out}");
    }

    #[test]
    fn render_pr_body_for_new_pr_is_just_the_block() {
        let t = sample_train();
        let body = render_pr_body(&t, "b", "");
        insta::assert_snapshot!("body_new_pr", body);
    }

    #[test]
    fn table_appends_aggregate_row_after_the_branches() {
        let mut t = sample_train();
        t.aggregate = Some(crate::state::Aggregate {
            branch: "choo/feat/combined".into(),
            pr: Some(PrInfo {
                number: 12,
                url: "u".into(),
                title: Some("Combined: feat".into()),
                last_pushed_sha: None,
            }),
        });
        insta::assert_snapshot!("table_with_aggregate", render_table(&t, Some("a")));
    }

    #[test]
    fn aggregate_row_can_be_the_highlighted_one() {
        let mut t = sample_train();
        t.aggregate = Some(crate::state::Aggregate::new("choo/feat/combined"));
        let out = render_table(&t, Some("choo/feat/combined"));
        // No PR yet -> branch name fallback and an em-dash PR cell.
        assert!(
            out.contains("| Σ | `choo/feat/combined` | — | **this PR** |"),
            "got: {out}"
        );
        // ...and none of the stack rows are marked.
        assert_eq!(out.matches("**this PR**").count(), 1);
    }

    #[test]
    fn table_has_no_aggregate_row_when_disabled() {
        let t = sample_train();
        assert!(!render_table(&t, None).contains(AGGREGATE_ROW_LABEL));
    }

    #[test]
    fn managed_block_explains_the_aggregate_row() {
        let mut t = sample_train();
        t.aggregate = Some(crate::state::Aggregate::new("choo/feat/combined"));
        let block = render_managed_block(&t, "a");
        assert!(block.contains("combined branch `choo/feat/combined`"));
        assert!(block.contains("draft PR against `main`"));
        assert!(block.contains("Merge the PRs above, not this one."));
    }

    #[test]
    fn managed_block_omits_the_legend_when_disabled() {
        let t = sample_train();
        assert!(!render_managed_block(&t, "a").contains("combined branch"));
    }

    #[test]
    fn rerender_stays_idempotent_with_an_aggregate_row() {
        let mut t = sample_train();
        t.aggregate = Some(crate::state::Aggregate::new("choo/feat/combined"));
        let first = rerender_pr_body(&t, "choo/feat/combined", "Notes above.");
        let second = rerender_pr_body(&t, "choo/feat/combined", &first);
        assert_eq!(first, second);
        assert!(second.starts_with("Notes above."));
    }

    /// Enabling the aggregate on an existing train must rewrite bodies
    /// that were rendered before it existed, without eating user content.
    #[test]
    fn rerender_adds_the_aggregate_row_to_an_older_body() {
        let mut t = sample_train();
        let before = rerender_pr_body(&t, "a", "My description.");
        assert!(!before.contains(AGGREGATE_ROW_LABEL));
        t.aggregate = Some(crate::state::Aggregate::new("choo/feat/combined"));
        let after = rerender_pr_body(&t, "a", &before);
        assert!(after.starts_with("My description."));
        assert!(after.contains("| Σ | `choo/feat/combined` |"));
    }

    #[test]
    fn rerender_is_idempotent() {
        let t = sample_train();
        let first = rerender_pr_body(&t, "a", "User text\n\nlives here.");
        let second = rerender_pr_body(&t, "a", &first);
        assert_eq!(first, second, "rerender must be idempotent");
    }

    #[test]
    fn rerender_preserves_prefix_above_block() {
        // The bug-report case: user content above the (new) managed block
        // must round-trip.
        let t = sample_train();
        let pre_existing = rerender_pr_body(&t, "a", "Hello there");
        let updated = rerender_pr_body(&t, "a", &pre_existing);
        assert!(updated.starts_with("Hello there"), "got: {updated}");
        assert!(updated.contains(TRAIN_START_PREFIX));
        assert!(updated.contains(TRAIN_END_MARKER));
    }

    #[test]
    fn rerender_preserves_suffix_below_block() {
        let t = sample_train();
        let after_text = "## Notes\n\nI added a footnote.";
        let block = render_managed_block(&t, "a");
        let body = format!("{block}\n\n{after_text}");
        let updated = rerender_pr_body(&t, "a", &body);
        assert!(updated.contains(after_text), "got: {updated}");
    }

    #[test]
    fn rerender_preserves_prefix_and_suffix_simultaneously() {
        let t = sample_train();
        let block = render_managed_block(&t, "a");
        let body = format!("Above\n\n{block}\n\nBelow");
        let updated = rerender_pr_body(&t, "a", &body);
        assert!(updated.starts_with("Above"));
        assert!(updated.trim_end().ends_with("Below"), "got: {updated}");
    }

    #[test]
    fn rerender_appends_block_to_legacy_body_without_choochoo_markers() {
        let t = sample_train();
        let legacy = "I wrote this manually before choochoo existed.";
        let updated = rerender_pr_body(&t, "a", legacy);
        assert!(updated.starts_with(legacy));
        assert!(updated.contains(TRAIN_START_PREFIX));
    }

    /// Regression: the actual user-reported failure mode. Body had "Hello
    /// there" above the *old* `choochoo:train ` header, with empty body
    /// markers. After re-render, "Hello there" must survive.
    #[test]
    fn rerender_migrates_old_markers_and_rescues_prefix() {
        let mut t = Train::new("local_lsr_export", "master");
        t.branches = vec![
            "tajp-lsr-rasteriser-no-throw-on-unbounded".into(),
            "tajp-export-recorders".into(),
        ];
        t.prs.insert(
            "tajp-lsr-rasteriser-no-throw-on-unbounded".into(),
            PrInfo {
                number: 900490,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        t.prs.insert(
            "tajp-export-recorders".into(),
            PrInfo {
                number: 900495,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        let legacy = "Hello there\n\
                      <!-- choochoo:train name=\"local_lsr_export\" -->\n\
                      ## Train: `local_lsr_export`\n\n\
                      | # | Branch | PR |   |\n\
                      |---|--------|----|---|\n\
                      | 1 | `tajp-lsr-rasteriser-no-throw-on-unbounded` | #900490 | **this PR** |\n\
                      | 2 | `tajp-export-recorders` | #900495 |  |\n\n\
                      Base: `master`\n\n\n\
                      <!-- choochoo:body:start -->\n\n\n\
                      <!-- choochoo:body:end -->\n";
        let updated = rerender_pr_body(&t, "tajp-lsr-rasteriser-no-throw-on-unbounded", legacy);
        assert!(updated.starts_with("Hello there"), "got: {updated}");
        assert!(updated.contains(TRAIN_START_PREFIX));
        assert!(updated.contains(TRAIN_END_MARKER));
        // Old markers fully gone.
        assert!(!updated.contains(LEGACY_HEADER_PREFIX));
        assert!(!updated.contains(LEGACY_BODY_START));
        assert!(!updated.contains(LEGACY_BODY_END));

        // Migrated body must itself be idempotent under further re-renders.
        let again = rerender_pr_body(&t, "tajp-lsr-rasteriser-no-throw-on-unbounded", &updated);
        assert_eq!(updated, again);
    }

    #[test]
    fn rerender_migrates_old_markers_and_rescues_inner_body_too() {
        let t = sample_train();
        let legacy = format!(
            "Above\n\
             <!-- choochoo:train name=\"feat\" -->\n\
             ## Train: `feat`\n\n\
             (table goes here)\n\
             Base: `main`\n\n\
             {LEGACY_BODY_START}\n\
             What this PR does: refactors the widget store.\n\
             {LEGACY_BODY_END}\n\
             Below"
        );
        let updated = rerender_pr_body(&t, "a", &legacy);
        assert!(updated.contains("Above"));
        assert!(updated.contains("What this PR does"));
        assert!(updated.contains("Below"));
        assert!(!updated.contains(LEGACY_BODY_START));
    }

    #[test]
    fn rerender_updates_when_branch_added() {
        let mut t = sample_train();
        let body = render_pr_body(&t, "a", "");
        t.branches.push("d".into());
        let updated = rerender_pr_body(&t, "a", &body);
        assert!(updated.contains("`d`"));
    }

    #[test]
    fn pr_title_uses_subject_when_present() {
        assert_eq!(pr_title_from_commit("Add feature", "branch"), "Add feature");
        assert_eq!(pr_title_from_commit("  ", "branch"), "branch");
    }

    #[test]
    fn empty_existing_body_produces_just_the_block() {
        let t = sample_train();
        let updated = rerender_pr_body(&t, "a", "");
        assert!(updated.starts_with(TRAIN_START_PREFIX));
        assert!(updated.trim_end().ends_with(TRAIN_END_MARKER));
    }
}
