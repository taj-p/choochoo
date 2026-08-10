//! `choo add` — append a branch to a train.

use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::state::{Store, Train};

pub fn run(
    store: &Store,
    git: &dyn GitRunner,
    train_name: Option<&str>,
    branch: Option<&str>,
) -> Result<()> {
    let mut state = store.load()?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    let branch = match branch {
        Some(b) => b.to_string(),
        None => git.current_branch()?,
    };
    if !git.branch_exists(&branch)? {
        return Err(Error::UnknownBranch(branch));
    }

    // Resolve the branch's true base *before* `apply` mutates `branches`,
    // otherwise the train tip would be the branch we're adding. The name has
    // to be owned so the immutable borrow is released before `train_mut`.
    let parent = {
        let train = state.train(&train_name)?;
        train.tip().unwrap_or(&train.base).to_string()
    };
    // Only record when the branch genuinely sits on top of the parent. It
    // won't when the parent has advanced past the fork point, or when the
    // branch was cut from somewhere else entirely — recording a base that
    // isn't an ancestor would be worse than recording nothing, since nothing
    // just means "fall back to the snapshot boundary".
    let base_sha = match git.rev_parse(&parent) {
        Ok(tip) if git.is_ancestor(&tip, &branch)? => Some(tip),
        _ => None,
    };

    apply(state.train_mut(&train_name)?, &branch, base_sha.as_deref())?;
    store.save(&state)
}

pub(crate) fn apply(train: &mut Train, branch: &str, base_sha: Option<&str>) -> Result<()> {
    if branch == train.base {
        return Err(Error::InvalidArgument(format!(
            "cannot add base branch `{}` to its own train",
            train.base
        )));
    }
    if train.is_aggregate(branch) {
        return Err(Error::InvalidArgument(format!(
            "`{branch}` is train `{}`'s combined branch; it mirrors the train \
             rather than being part of it",
            train.name
        )));
    }
    if train.position(branch).is_some() {
        return Err(Error::BranchAlreadyInTrain {
            train: train.name.clone(),
            branch: branch.to_string(),
        });
    }
    train.branches.push(branch.to_string());
    // After the push: `set_branch_base` ignores branches not in the train.
    if let Some(sha) = base_sha {
        train.set_branch_base(branch, sha);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_to_end() {
        let mut t = Train::new("t", "main");
        apply(&mut t, "a", None).unwrap();
        apply(&mut t, "b", None).unwrap();
        assert_eq!(t.branches, vec!["a", "b"]);
    }

    #[test]
    fn rejects_duplicate() {
        let mut t = Train::new("t", "main");
        apply(&mut t, "a", None).unwrap();
        let err = apply(&mut t, "a", None).unwrap_err();
        assert!(matches!(err, Error::BranchAlreadyInTrain { .. }));
    }

    #[test]
    fn rejects_base_branch() {
        let mut t = Train::new("t", "main");
        let err = apply(&mut t, "main", None).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
    }

    #[test]
    fn rejects_the_aggregate_branch() {
        let mut t = Train::new("t", "main");
        t.aggregate = Some(crate::state::Aggregate::new("choo/t/combined"));
        let err = apply(&mut t, "choo/t/combined", None).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
        assert!(t.branches.is_empty());
    }

    #[test]
    fn records_the_base_when_given_one() {
        let mut t = Train::new("t", "main");
        apply(&mut t, "a", Some("M")).unwrap();
        assert_eq!(t.branch_base("a"), Some("M"));
    }

    #[test]
    fn records_nothing_when_no_base_is_known() {
        let mut t = Train::new("t", "main");
        apply(&mut t, "a", None).unwrap();
        assert_eq!(t.branch_base("a"), None);
        assert!(t.branch_bases.is_empty());
    }

    /// A rejected add must not leave a recorded base behind.
    #[test]
    fn rejected_add_records_nothing() {
        let mut t = Train::new("t", "main");
        let _ = apply(&mut t, "main", Some("M"));
        assert!(t.branch_bases.is_empty());
    }
}
