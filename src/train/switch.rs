//! `choo switch` — change the active train.

use crate::error::{Error, Result};
use crate::state::Store;

pub fn run(store: &Store, name: &str) -> Result<()> {
    let mut state = store.load()?;
    if !state.trains.contains_key(name) {
        return Err(Error::UnknownTrain(name.to_string()));
    }
    state.active = Some(name.to_string());
    store.save(&state)
}
