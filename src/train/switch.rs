//! `choo switch` — change the active train.

use std::path::Path;

use crate::error::{Error, Result};
use crate::state;

pub fn run(repo_root: &Path, name: &str) -> Result<()> {
    let mut state = state::load(repo_root)?;
    if !state.trains.contains_key(name) {
        return Err(Error::UnknownTrain(name.to_string()));
    }
    state.active = Some(name.to_string());
    state::save(repo_root, &state)
}
