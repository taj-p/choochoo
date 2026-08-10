//! `choo list` and `choo show` — read-only views of trains.

use std::fmt::Write;
use std::path::Path;

use crate::error::Result;
use crate::state::{self, StateFile, Train};

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
    if train.branches.is_empty() {
        out.push_str("(no branches yet; `choo add <branch>` to add one)\n");
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
    out
}

pub fn run_list(repo_root: &Path) -> Result<String> {
    Ok(render_list(&state::load(repo_root)?))
}

pub fn run_show(repo_root: &Path, train_name: Option<&str>) -> Result<String> {
    let state = state::load(repo_root)?;
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
    fn show_unknown_train_errors() {
        let s = StateFile::default();
        assert!(render_show(&s, "ghost").is_err());
    }
}
