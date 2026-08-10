//! `choo checkout` — switch the working tree to a branch in a train.

use std::path::Path;

use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::state;

pub fn run(
    repo_root: &Path,
    git: &dyn GitRunner,
    train_name: Option<&str>,
    branch: &str,
) -> Result<()> {
    let state = state::load(repo_root)?;
    let name = state.resolve_train_name(train_name)?;
    let train = state.train(name)?;
    if train.position(branch).is_none() && branch != train.base {
        return Err(Error::BranchNotInTrain {
            train: train.name.clone(),
            branch: branch.to_string(),
        });
    }
    git.checkout(branch)
}
