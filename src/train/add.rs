//! `choo add` — append a branch to a train.

use std::path::Path;

use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::state::{self, Train};

pub fn run(
    repo_root: &Path,
    git: &dyn GitRunner,
    train_name: Option<&str>,
    branch: Option<&str>,
) -> Result<()> {
    let mut state = state::load(repo_root)?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    let branch = match branch {
        Some(b) => b.to_string(),
        None => git.current_branch()?,
    };
    if !git.branch_exists(&branch)? {
        return Err(Error::UnknownBranch(branch));
    }
    apply(state.train_mut(&train_name)?, &branch)?;
    state::save(repo_root, &state)
}

pub(crate) fn apply(train: &mut Train, branch: &str) -> Result<()> {
    if branch == train.base {
        return Err(Error::InvalidArgument(format!(
            "cannot add base branch `{}` to its own train",
            train.base
        )));
    }
    if train.position(branch).is_some() {
        return Err(Error::BranchAlreadyInTrain {
            train: train.name.clone(),
            branch: branch.to_string(),
        });
    }
    train.branches.push(branch.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_to_end() {
        let mut t = Train::new("t", "main");
        apply(&mut t, "a").unwrap();
        apply(&mut t, "b").unwrap();
        assert_eq!(t.branches, vec!["a", "b"]);
    }

    #[test]
    fn rejects_duplicate() {
        let mut t = Train::new("t", "main");
        apply(&mut t, "a").unwrap();
        let err = apply(&mut t, "a").unwrap_err();
        assert!(matches!(err, Error::BranchAlreadyInTrain { .. }));
    }

    #[test]
    fn rejects_base_branch() {
        let mut t = Train::new("t", "main");
        let err = apply(&mut t, "main").unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
    }
}
