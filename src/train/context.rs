//! `choo context` — edit the train's shared "PR Train Context".
//!
//! The context is prose about the *train*: why the stack exists, how to
//! read it, what to look at first. [`crate::render`] puts it at the top of
//! every PR in the train, so it is written once here and pushed out to
//! every description by the next `choo pr`.
//!
//! This command deliberately doesn't touch GitHub. Editing text and
//! talking to the network are separate failures with separate remedies,
//! and `choo pr` is already the idempotent "make the PRs match the state"
//! command — so it stays the only thing that writes descriptions.

use std::fmt::Write;

use crate::editor::Editor;
use crate::error::Result;
use crate::state::{StateFile, Store};

/// Name the scratch buffer gets in the editor. `.md` so editors syntax
/// highlight it as what it is.
const BUFFER_NAME: &str = "PR_TRAIN_CONTEXT.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOutcome {
    pub train: String,
    /// Whether the saved text differed from what was already stored.
    pub changed: bool,
    /// Whether the change was "there is no longer any context".
    pub cleared: bool,
    /// How many PR descriptions the next `choo pr` has to rewrite — the
    /// per-branch PRs choochoo knows about, plus the combined one.
    pub prs: usize,
}

/// Open the train's context in `editor` and store what comes back.
///
/// Returns [`None`] when the user aborted (the editor exited non-zero);
/// nothing is written in that case.
pub fn run(
    store: &Store,
    editor: &dyn Editor,
    train_name: Option<&str>,
) -> Result<Option<ContextOutcome>> {
    let mut state = store.load()?;
    let train_name = state.resolve_train_name(train_name)?.to_string();

    let current = state.train(&train_name)?.context().unwrap_or("").to_string();
    let Some(edited) = editor.edit(&current, BUFFER_NAME)? else {
        return Ok(None);
    };

    let train = state.train_mut(&train_name)?;
    let changed = train.set_context(&edited);
    let cleared = train.context().is_none();
    let prs = train.prs.len()
        + train
            .aggregate
            .as_ref()
            .and_then(|a| a.pr.as_ref())
            .map_or(0, |_| 1);

    if changed {
        store.save_described(
            &state,
            &format!("update PR train context for `{train_name}`"),
        )?;
    }
    Ok(Some(ContextOutcome {
        train: train_name,
        changed,
        cleared,
        prs,
    }))
}

/// The stdout of `choo context --show`: the raw text, so it can be piped
/// somewhere useful. Empty (with a note on stderr's job left to the CLI)
/// when the train has no context.
pub fn render_show(state: &StateFile, train_name: &str) -> Result<String> {
    let mut out = String::new();
    if let Some(text) = state.train(train_name)?.context() {
        let _ = writeln!(&mut out, "{text}");
    }
    Ok(out)
}

pub fn run_show(store: &Store, train_name: Option<&str>) -> Result<String> {
    let state = store.load()?;
    let name = state.resolve_train_name(train_name)?;
    render_show(&state, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::FakeEditor;
    use crate::state::{PrInfo, Train};
    use tempfile::TempDir;

    fn setup() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/choochoo")).unwrap();
        let store = Store::local(tmp.path());
        let mut state = StateFile::default();
        let mut t = Train::new("t", "main");
        t.branches = vec!["a".into(), "b".into()];
        t.prs.insert(
            "a".into(),
            PrInfo {
                number: 1,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        state.trains.insert("t".into(), t);
        state.active = Some("t".into());
        store.save(&state).unwrap();
        (tmp, store)
    }

    fn stored(store: &Store) -> Option<String> {
        store
            .load()
            .unwrap()
            .train("t")
            .unwrap()
            .context
            .clone()
    }

    #[test]
    fn saves_what_the_editor_returns() {
        let (_tmp, store) = setup();
        let ed = FakeEditor::saving("Split for review.");
        let out = run(&store, &ed, None).unwrap().unwrap();
        assert!(out.changed);
        assert!(!out.cleared);
        assert_eq!(stored(&store).as_deref(), Some("Split for review."));
    }

    #[test]
    fn opens_the_editor_on_the_stored_text() {
        let (_tmp, store) = setup();
        run(&store, &FakeEditor::saving("first"), None).unwrap();
        let ed = FakeEditor::saving("second");
        run(&store, &ed, None).unwrap();
        assert_eq!(ed.seen().as_deref(), Some("first"));
    }

    #[test]
    fn first_edit_starts_from_an_empty_buffer() {
        let (_tmp, store) = setup();
        let ed = FakeEditor::saving("x");
        run(&store, &ed, None).unwrap();
        assert_eq!(ed.seen().as_deref(), Some(""));
    }

    /// Quitting without saving must leave state exactly as it was — the
    /// escape hatch for "I opened this by accident".
    #[test]
    fn abort_writes_nothing() {
        let (_tmp, store) = setup();
        run(&store, &FakeEditor::saving("keep me"), None).unwrap();
        assert!(run(&store, &FakeEditor::aborting(), None).unwrap().is_none());
        assert_eq!(stored(&store).as_deref(), Some("keep me"));
    }

    #[test]
    fn saving_the_same_text_reports_unchanged() {
        let (_tmp, store) = setup();
        run(&store, &FakeEditor::saving("same"), None).unwrap();
        let out = run(&store, &FakeEditor::saving("same"), None).unwrap().unwrap();
        assert!(!out.changed);
    }

    /// Trailing newlines the editor adds are not a change, or every visit
    /// would look like an edit.
    #[test]
    fn trailing_whitespace_is_not_a_change() {
        let (_tmp, store) = setup();
        run(&store, &FakeEditor::saving("same"), None).unwrap();
        let out = run(&store, &FakeEditor::saving("same\n\n"), None)
            .unwrap()
            .unwrap();
        assert!(!out.changed);
        assert_eq!(stored(&store).as_deref(), Some("same"));
    }

    #[test]
    fn emptying_the_buffer_clears_the_context() {
        let (_tmp, store) = setup();
        run(&store, &FakeEditor::saving("something"), None).unwrap();
        let out = run(&store, &FakeEditor::saving("   \n\n"), None)
            .unwrap()
            .unwrap();
        assert!(out.changed);
        assert!(out.cleared);
        assert_eq!(stored(&store), None);
    }

    #[test]
    fn counts_the_prs_that_will_need_syncing() {
        let (_tmp, store) = setup();
        let out = run(&store, &FakeEditor::saving("x"), None).unwrap().unwrap();
        // Branch `a` has a PR, `b` doesn't yet.
        assert_eq!(out.prs, 1);
    }

    #[test]
    fn counts_the_combined_pr_too() {
        let (_tmp, store) = setup();
        let mut state = store.load().unwrap();
        let mut agg = crate::state::Aggregate::new("choo/t/combined");
        agg.pr = Some(PrInfo {
            number: 9,
            url: "u".into(),
            title: None,
            last_pushed_sha: None,
        });
        state.train_mut("t").unwrap().aggregate = Some(agg);
        store.save(&state).unwrap();

        let out = run(&store, &FakeEditor::saving("x"), None).unwrap().unwrap();
        assert_eq!(out.prs, 2);
    }

    #[test]
    fn show_prints_the_text_and_nothing_else() {
        let (_tmp, store) = setup();
        assert_eq!(run_show(&store, None).unwrap(), "");
        run(&store, &FakeEditor::saving("line one\nline two"), None).unwrap();
        assert_eq!(run_show(&store, None).unwrap(), "line one\nline two\n");
    }

    #[test]
    fn unknown_train_errors() {
        let (_tmp, store) = setup();
        assert!(run(&store, &FakeEditor::saving("x"), Some("ghost")).is_err());
    }

    /// Contexts are per-train, so editing one leaves the other alone.
    #[test]
    fn contexts_are_scoped_to_their_train() {
        let (_tmp, store) = setup();
        let mut state = store.load().unwrap();
        state
            .trains
            .insert("other".into(), Train::new("other", "main"));
        store.save(&state).unwrap();

        run(&store, &FakeEditor::saving("for t"), Some("t")).unwrap();
        run(&store, &FakeEditor::saving("for other"), Some("other")).unwrap();

        let state = store.load().unwrap();
        assert_eq!(state.train("t").unwrap().context(), Some("for t"));
        assert_eq!(state.train("other").unwrap().context(), Some("for other"));
    }
}
