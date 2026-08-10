//! `choo list` and `choo show` — read-only views of trains.

use std::fmt::Write;

use crate::error::Result;
use crate::state::{StateFile, Store, Train};

/// Render a one-train summary that goes to stdout.
pub fn render_show(state: &StateFile, train_name: &str) -> Result<String> {
    let train = state.train(train_name)?;
    Ok(format_train(state, train))
}

/// Render the full list of trains.
pub fn render_list(state: &StateFile) -> String {
    if state.trains.is_empty() {
        return "(no trains; run `choo init <name>` to create one)\n".to_string();
    }
    let mut out = String::new();
    for (name, train) in &state.trains {
        let active = state.active.as_deref() == Some(name.as_str());
        let marker = if active { "*" } else { " " };
        let _ = writeln!(
            &mut out,
            "{marker} {name}  base={}  branches={}",
            train.base,
            train.branches.len()
        );
    }
    out
}

fn format_train(state: &StateFile, train: &Train) -> String {
    let mut out = String::new();
    let active = state.active.as_deref() == Some(train.name.as_str());
    let _ = writeln!(
        &mut out,
        "Train: {}{}",
        train.name,
        if active { " (active)" } else { "" }
    );
    let _ = writeln!(&mut out, "Base:  {}", train.base);
    append_context(&mut out, train);
    if train.branches.is_empty() {
        out.push_str("(no branches yet; `choo add <branch>` to add one)\n");
        append_aggregate(&mut out, train);
        return out;
    }
    out.push_str("Branches:\n");
    for (i, b) in train.branches.iter().enumerate() {
        let pr = train
            .prs
            .get(b)
            .map(|p| format!("#{} <{}>", p.number, p.url))
            .unwrap_or_else(|| "no PR".into());
        let _ = writeln!(&mut out, "  {}. {b}  [{pr}]", i + 1);
    }
    append_aggregate(&mut out, train);
    out
}

/// The PR Train Context, if the train has one, indented so a multi-line
/// context can't be mistaken for the rest of the summary.
fn append_context(out: &mut String, train: &Train) {
    let Some(text) = train.context() else {
        return;
    };
    out.push_str("Context:\n");
    for line in text.lines() {
        // Blank lines stay blank rather than becoming two spaces.
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            let _ = writeln!(out, "  {line}");
        }
    }
}

/// One extra line for the aggregate branch, if enabled. Mirrors the branch
/// lines but labelled rather than numbered, since it isn't in the stack.
fn append_aggregate(out: &mut String, train: &Train) {
    let Some(agg) = &train.aggregate else {
        return;
    };
    let pr = agg
        .pr
        .as_ref()
        .map(|p| format!("draft #{} <{}>", p.number, p.url))
        .unwrap_or_else(|| "no PR".into());
    let _ = writeln!(
        out,
        "Combined: {}  [{pr}]  (all changes, targets {})",
        agg.branch, train.base
    );
}

pub fn run_list(store: &Store) -> Result<String> {
    Ok(render_list(&store.load()?))
}

pub fn run_show(store: &Store, train_name: Option<&str>) -> Result<String> {
    let state = store.load()?;
    let name = state.resolve_train_name(train_name)?;
    render_show(&state, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PrInfo;

    fn sample_state() -> StateFile {
        let mut s = StateFile::default();
        let mut t = Train::new("feat", "main");
        t.branches = vec!["a".into(), "b".into()];
        t.prs.insert(
            "a".into(),
            PrInfo {
                number: 7,
                url: "https://example/pr/7".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        s.trains.insert("feat".into(), t);
        s.active = Some("feat".into());
        s
    }

    #[test]
    fn list_marks_active() {
        let s = sample_state();
        let out = render_list(&s);
        assert!(out.contains("* feat"));
        assert!(out.contains("base=main"));
        assert!(out.contains("branches=2"));
    }

    #[test]
    fn list_empty_state() {
        let s = StateFile::default();
        assert!(render_list(&s).contains("no trains"));
    }

    #[test]
    fn show_renders_branches_with_pr() {
        let s = sample_state();
        let out = render_show(&s, "feat").unwrap();
        assert!(out.contains("Train: feat (active)"));
        assert!(out.contains("a  [#7"));
        assert!(out.contains("b  [no PR]"));
    }

    #[test]
    fn show_renders_the_aggregate_branch() {
        let mut s = sample_state();
        s.train_mut("feat").unwrap().aggregate = Some(crate::state::Aggregate {
            branch: "choo/feat/combined".into(),
            pr: Some(PrInfo {
                number: 42,
                url: "https://example/pr/42".into(),
                title: None,
                last_pushed_sha: None,
            }),
        });
        let out = render_show(&s, "feat").unwrap();
        assert!(out.contains("Combined: choo/feat/combined  [draft #42"));
        assert!(out.contains("targets main"));
    }

    #[test]
    fn show_omits_the_aggregate_line_when_disabled() {
        let s = sample_state();
        assert!(!render_show(&s, "feat").unwrap().contains("Combined:"));
    }

    #[test]
    fn show_renders_the_aggregate_for_an_empty_train() {
        let mut s = StateFile::default();
        let mut t = Train::new("empty", "main");
        t.aggregate = Some(crate::state::Aggregate::new("choo/empty/combined"));
        s.trains.insert("empty".into(), t);
        let out = render_show(&s, "empty").unwrap();
        assert!(out.contains("no branches yet"));
        assert!(out.contains("Combined: choo/empty/combined  [no PR]"));
    }

    #[test]
    fn show_renders_the_context_indented() {
        let mut s = sample_state();
        s.train_mut("feat")
            .unwrap()
            .set_context("Why this stack exists.\n\nRead from the bottom.");
        let out = render_show(&s, "feat").unwrap();
        assert!(out.contains("Context:\n  Why this stack exists.\n"));
        // Blank lines inside the context carry no phantom indentation.
        assert!(out.contains("\n\n  Read from the bottom.\n"), "got: {out}");
    }

    #[test]
    fn show_omits_the_context_line_when_there_is_none() {
        let s = sample_state();
        assert!(!render_show(&s, "feat").unwrap().contains("Context:"));
    }

    #[test]
    fn show_unknown_train_errors() {
        let s = StateFile::default();
        assert!(render_show(&s, "ghost").is_err());
    }
}
