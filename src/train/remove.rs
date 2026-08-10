//! `choo remove` — drop a branch from a train. Does not delete the
//! underlying git branch.
//!
//! Purely a state edit, but not a semantically inert one: removing a branch
//! re-parents its successor, so the successor's recorded base
//! ([`Train::branch_bases`]) has to be spliced past the branch that's leaving.
//! Skipping that would make the next `choo rebase` narrow the successor's
//! replay range to exclude the removed branch's commits — silently dropping
//! that work out of the successor's content. See [`apply`].

use crate::error::{Error, Result};
use crate::state::{Store, Train};

pub fn run(store: &Store, train_name: Option<&str>, branch: &str) -> Result<()> {
    let mut state = store.load()?;
    let train_name = state.resolve_train_name(train_name)?.to_string();
    apply(state.train_mut(&train_name)?, branch)?;
    store.save(&state)
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

    // Splice the recorded-base chain rather than cutting it. `branch` is gone,
    // so its successor now hangs off whatever `branch` hung off. Handing the
    // successor `branch`'s *tip* instead would exclude `branch`'s commits from
    // the successor's replay range and silently drop that work; inheriting
    // `branch`'s own base keeps the range exactly as wide as it is today.
    //
    // With no base recorded for `branch` there's nothing to inherit, so drop
    // the successor's entry too and let that pair fall back to the snapshot
    // boundary — again, today's behaviour.
    let leaving = train.take_branch_base(branch);
    if let Some(successor) = train.branches.get(pos).cloned() {
        match leaving {
            Some(base) => train.set_branch_base(&successor, base),
            None => {
                train.take_branch_base(&successor);
            }
        }
    }
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

    /// The regression test for silent content loss. Handing `b`'s successor
    /// `b`'s *tip* would exclude `b`'s commits from the successor's replay
    /// range and drop that work; inheriting `b`'s own base keeps it.
    #[test]
    fn successor_inherits_the_removed_branchs_base() {
        let mut t = train_with(&["a", "b", "c"]);
        t.set_branch_base("a", "M");
        t.set_branch_base("b", "A");
        t.set_branch_base("c", "B");

        apply(&mut t, "b").unwrap();

        assert_eq!(t.branches, vec!["a", "c"]);
        assert_eq!(t.branch_base("a"), Some("M"));
        // `c` now hangs off whatever `b` hung off, not off `b`'s tip.
        assert_eq!(t.branch_base("c"), Some("A"));
        assert_eq!(t.branch_base("b"), None);
    }

    /// With nothing to inherit there is nothing correct to say, so the
    /// successor's entry goes too and that pair falls back to the snapshot
    /// boundary — the behaviour that predates recorded bases.
    #[test]
    fn successor_base_is_dropped_when_the_removed_branch_had_none() {
        let mut t = train_with(&["a", "b"]);
        t.set_branch_base("b", "A");

        apply(&mut t, "a").unwrap();

        assert_eq!(t.branch_base("b"), None);
        assert!(t.branch_bases.is_empty());
    }

    #[test]
    fn removing_the_last_branch_has_no_successor_to_splice() {
        let mut t = train_with(&["a", "b"]);
        t.set_branch_base("a", "M");
        t.set_branch_base("b", "A");

        apply(&mut t, "b").unwrap();

        assert_eq!(t.branch_base("a"), Some("M"));
        assert_eq!(t.branch_bases.len(), 1);
    }

    #[test]
    fn missing_branch_errors() {
        let mut t = train_with(&["a"]);
        let err = apply(&mut t, "nope").unwrap_err();
        assert!(matches!(err, Error::BranchNotInTrain { .. }));
    }
}
