//! `choo checkout` — switch the working tree to a branch in a train.

use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::report::{Reporter, ReporterExt};
use crate::state::{Store, Train};

pub fn run(
    store: &Store,
    git: &dyn GitRunner,
    reporter: &mut dyn Reporter,
    train_name: Option<&str>,
    branch: &str,
    remote: &str,
) -> Result<()> {
    let state = store.load()?;
    let name = state.resolve_train_name(train_name)?;
    let train = state.train(name)?;
    let target = resolve(train, branch)?;
    let branch = target.as_str();

    // With shared state, a train can arrive from another machine before its
    // branches do. Create the branch from the remote rather than handing
    // back a raw git error. No implicit `git fetch` — that keeps checkout
    // fast, and `choo fetch` is the command for pulling a whole train down.
    if !git.branch_exists(branch)? {
        if !git.remote_branch_exists(remote, branch)? {
            return Err(Error::BranchNotFetched {
                train: train.name.clone(),
                branch: branch.to_string(),
                remote: remote.to_string(),
            });
        }
        reporter.step_ok(
            &format!("creating `{branch}` from `{remote}/{branch}`"),
            "",
        );
        git.create_tracking_branch(branch, remote)?;
    }

    git.checkout(branch)
}

/// Resolve the `<branch>` argument to a branch name.
///
/// An exact match — a branch in the train, or its base — always wins, so a
/// branch literally named `2` stays reachable by name. Failing that, a bare
/// integer is read as a position in the train, numbered from 1 to match the
/// `#` column of the train table in a PR description.
fn resolve(train: &Train, arg: &str) -> Result<String> {
    if train.position(arg).is_some() || arg == train.base {
        return Ok(arg.to_string());
    }
    match arg.parse::<usize>() {
        Ok(n) if !train.branches.is_empty() => n
            .checked_sub(1)
            .and_then(|i| train.branches.get(i))
            .cloned()
            .ok_or_else(|| Error::NoBranchAtPosition {
                train: train.name.clone(),
                position: n,
                len: train.branches.len(),
            }),
        _ => Err(Error::BranchNotInTrain {
            train: train.name.clone(),
            branch: arg.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn train() -> Train {
        let mut t = Train::new("feat", "main");
        t.branches = vec!["a".into(), "b".into(), "c".into()];
        t
    }

    #[test]
    fn index_is_one_based() {
        assert_eq!(resolve(&train(), "1").unwrap(), "a");
        assert_eq!(resolve(&train(), "3").unwrap(), "c");
    }

    #[test]
    fn names_still_resolve() {
        assert_eq!(resolve(&train(), "b").unwrap(), "b");
        assert_eq!(resolve(&train(), "main").unwrap(), "main");
    }

    /// A branch named after a number is reachable by its own name; the
    /// index reading is only a fallback.
    #[test]
    fn exact_name_beats_index() {
        let mut t = train();
        t.branches.push("2".into());
        assert_eq!(resolve(&t, "2").unwrap(), "2");
    }

    #[test]
    fn out_of_range_index_is_rejected() {
        for arg in ["0", "4"] {
            assert!(matches!(
                resolve(&train(), arg),
                Err(Error::NoBranchAtPosition { .. })
            ));
        }
    }

    #[test]
    fn non_numeric_miss_is_still_branch_not_in_train() {
        assert!(matches!(
            resolve(&train(), "ghost"),
            Err(Error::BranchNotInTrain { .. })
        ));
    }

    /// Nothing to index into, so the number is just a missing branch name.
    #[test]
    fn index_into_empty_train_is_branch_not_in_train() {
        let t = Train::new("feat", "main");
        assert!(matches!(
            resolve(&t, "1"),
            Err(Error::BranchNotInTrain { .. })
        ));
    }
}
