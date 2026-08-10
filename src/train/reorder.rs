//! `choo move` — reposition a branch within a train.

use crate::error::{Error, Result};
use crate::state::{Store, Train};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Before,
    After,
}

pub fn run(
    store: &Store,
    train_name: Option<&str>,
    branch: &str,
    position: Position,
    relative_to: &str,
) -> Result<()> {
    let mut state = store.load()?;
    let name = state.resolve_train_name(train_name)?.to_string();
    apply(state.train_mut(&name)?, branch, position, relative_to)?;
    store.save(&state)
}

pub(crate) fn apply(
    train: &mut Train,
    branch: &str,
    position: Position,
    relative_to: &str,
) -> Result<()> {
    if branch == relative_to {
        return Err(Error::InvalidArgument(
            "cannot move a branch relative to itself".into(),
        ));
    }
    let from = train
        .position(branch)
        .ok_or_else(|| Error::BranchNotInTrain {
            train: train.name.clone(),
            branch: branch.to_string(),
        })?;
    train.branches.remove(from);
    let target = train
        .position(relative_to)
        .ok_or_else(|| Error::BranchNotInTrain {
            train: train.name.clone(),
            branch: relative_to.to_string(),
        })?;
    let insert_at = match position {
        Position::Before => target,
        Position::After => target + 1,
    };
    train.branches.insert(insert_at, branch.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn train(b: &[&str]) -> Train {
        let mut t = Train::new("t", "main");
        t.branches = b.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn move_before() {
        let mut t = train(&["a", "b", "c"]);
        apply(&mut t, "c", Position::Before, "b").unwrap();
        assert_eq!(t.branches, vec!["a", "c", "b"]);
    }

    #[test]
    fn move_after() {
        let mut t = train(&["a", "b", "c"]);
        apply(&mut t, "a", Position::After, "b").unwrap();
        assert_eq!(t.branches, vec!["b", "a", "c"]);
    }

    /// Reordering deliberately leaves recorded bases alone. A recorded base
    /// describes the commit a branch's *own* work sits on — a fact about that
    /// branch's history, not about which branch is currently its parent — so
    /// permuting the train keeps every entry both valid and correct.
    ///
    /// This is what makes `choo move` need no git access, and it's the reason
    /// `remove` is different: removal deletes a link out of the chain, whereas
    /// a move only permutes it.
    #[test]
    fn recorded_bases_survive_a_move_untouched() {
        let mut t = train(&["a", "b", "c"]);
        t.set_branch_base("a", "M");
        t.set_branch_base("b", "A");
        t.set_branch_base("c", "B");
        let before = t.branch_bases.clone();

        apply(&mut t, "c", Position::Before, "b").unwrap();

        assert_eq!(t.branches, vec!["a", "c", "b"]);
        assert_eq!(t.branch_bases, before);
    }

    #[test]
    fn move_to_same_logical_place_is_noop() {
        let mut t = train(&["a", "b", "c"]);
        apply(&mut t, "b", Position::After, "a").unwrap();
        assert_eq!(t.branches, vec!["a", "b", "c"]);
    }

    #[test]
    fn move_to_self_errors() {
        let mut t = train(&["a", "b"]);
        assert!(apply(&mut t, "a", Position::Before, "a").is_err());
    }

    #[test]
    fn move_unknown_branch_errors() {
        let mut t = train(&["a", "b"]);
        let err = apply(&mut t, "c", Position::Before, "a").unwrap_err();
        assert!(matches!(err, Error::BranchNotInTrain { .. }));
    }

    #[test]
    fn move_relative_to_unknown_errors() {
        let mut t = train(&["a", "b"]);
        let err = apply(&mut t, "a", Position::Before, "nope").unwrap_err();
        assert!(matches!(err, Error::BranchNotInTrain { .. }));
    }
}
