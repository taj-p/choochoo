//! `choo checkout` — switch the working tree to a branch in a train.

use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::report::{Reporter, ReporterExt};
use crate::state::Store;

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
    if train.position(branch).is_none() && branch != train.base {
        return Err(Error::BranchNotInTrain {
            train: train.name.clone(),
            branch: branch.to_string(),
        });
    }

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
