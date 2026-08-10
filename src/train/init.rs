//! `choo init` — create a new (empty) train.

use std::path::Path;

use crate::error::{Error, Result};
use crate::state::{self, StateFile, Train};

/// Create a new train with `name` based off `base`. If no train was active,
/// the new train becomes active.
pub fn run(repo_root: &Path, name: &str, base: &str) -> Result<()> {
    let mut state = state::load(repo_root)?;
    apply(&mut state, name, base)?;
    state::save(repo_root, &state)
}

pub(crate) fn apply(state: &mut StateFile, name: &str, base: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::InvalidArgument("train name cannot be empty".into()));
    }
    if base.trim().is_empty() {
        return Err(Error::InvalidArgument("base branch cannot be empty".into()));
    }
    if state.trains.contains_key(name) {
        return Err(Error::TrainExists(name.to_string()));
    }
    state
        .trains
        .insert(name.to_string(), Train::new(name, base));
    if state.active.is_none() {
        state.active = Some(name.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_train_becomes_active() {
        let mut s = StateFile::default();
        apply(&mut s, "feat", "main").unwrap();
        assert_eq!(s.active.as_deref(), Some("feat"));
        assert!(s.trains.contains_key("feat"));
    }

    #[test]
    fn second_train_does_not_take_over_active() {
        let mut s = StateFile::default();
        apply(&mut s, "first", "main").unwrap();
        apply(&mut s, "second", "main").unwrap();
        assert_eq!(s.active.as_deref(), Some("first"));
    }

    #[test]
    fn duplicate_train_errors() {
        let mut s = StateFile::default();
        apply(&mut s, "feat", "main").unwrap();
        let err = apply(&mut s, "feat", "main").unwrap_err();
        assert!(matches!(err, Error::TrainExists(_)));
    }

    #[test]
    fn empty_name_errors() {
        let mut s = StateFile::default();
        assert!(apply(&mut s, "", "main").is_err());
    }

    #[test]
    fn empty_base_errors() {
        let mut s = StateFile::default();
        assert!(apply(&mut s, "x", "").is_err());
    }
}
