//! `choo remove` — drop a branch from a train. Does not delete the
//! underlying git branch.

use std::path::Path;

use crate::error::{Error, Result};
use crate::state::{self, Train};

pub fn run(repo_root: &Path, train_name: Option<&str>, branch: &str) -> Result<()> {
    let mut state = state::load(repo_root)?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    apply(state.train_mut(&train_name)?, branch)?;
    state::save(repo_root, &state)
}

pub(crate) fn apply(train: &mut Train, branch: &str) -> Result<()> {
    let pos = train
        .position(branch)
        .ok_or_else(|| Error::BranchNotInTrain {
            train: train.name.clone(),
            branch: branch.to_string(),
        })?;
    train.branches.remove(pos);
    train.prs.remove(branch);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PrInfo;

    fn train_with(branches: &[&str]) -> Train {
        let mut t = Train::new("t", "main");
        t.branches = branches.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn removes_existing_branch() {
        let mut t = train_with(&["a", "b", "c"]);
        apply(&mut t, "b").unwrap();
        assert_eq!(t.branches, vec!["a", "c"]);
    }

    #[test]
    fn removes_pr_metadata_too() {
        let mut t = train_with(&["a", "b"]);
        t.prs.insert(
            "b".into(),
            PrInfo {
                number: 1,
                url: "u".into(),
                title: None,
                last_pushed_sha: None,
            },
        );
        apply(&mut t, "b").unwrap();
        assert!(t.prs.is_empty());
    }

    #[test]
    fn missing_branch_errors() {
        let mut t = train_with(&["a"]);
        let err = apply(&mut t, "nope").unwrap_err();
        assert!(matches!(err, Error::BranchNotInTrain { .. }));
    }
}
