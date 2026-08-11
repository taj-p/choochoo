//! Error types shared across the crate.

use std::io;
use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("io error: {0}")]
    BareIo(#[from] io::Error),

    #[error("failed to (de)serialize state: {0}")]
    Json(#[from] serde_json::Error),

    #[error("not inside a git repository (no .git directory found)")]
    NotInRepo,

    #[error("required tool `{0}` was not found on PATH")]
    MissingTool(&'static str),

    #[error("git failed ({code}): {stderr}")]
    Git { code: i32, stderr: String },

    #[error("gh failed ({code}): {stderr}")]
    Gh { code: i32, stderr: String },

    #[error("could not parse output of `{cmd}`: {reason}")]
    ParseOutput { cmd: &'static str, reason: String },

    #[error("train `{0}` does not exist")]
    UnknownTrain(String),

    #[error("train `{0}` already exists")]
    TrainExists(String),

    #[error("branch `{branch}` is not in train `{train}`")]
    BranchNotInTrain { train: String, branch: String },

    #[error("train `{train}` has no branch at position {position}; positions run 1..={len}")]
    NoBranchAtPosition {
        train: String,
        position: usize,
        len: usize,
    },

    #[error("branch `{branch}` is already in train `{train}`")]
    BranchAlreadyInTrain { train: String, branch: String },

    #[error("branch `{0}` does not exist locally")]
    UnknownBranch(String),

    #[error(
        "train `{train}` is missing branches here and on `{remote}`: {}",
        missing.join(", ")
    )]
    IncompleteTrain {
        train: String,
        remote: String,
        missing: Vec<String>,
    },

    #[error(
        "branch `{branch}` is in train `{train}` but exists neither locally \
         nor on `{remote}`; run `git fetch {remote}` if it was pushed from \
         another machine"
    )]
    BranchNotFetched {
        train: String,
        branch: String,
        remote: String,
    },

    #[error("no active train; pass --train or run `choo switch <name>`")]
    NoActiveTrain,

    #[error("rebase conflict on branch `{branch}`; resolve and run `choo rebase --continue`")]
    RebaseConflict { branch: String },

    #[error("could not start editor `{program}`: {source}; set $EDITOR to one that exists")]
    EditorLaunch {
        program: String,
        #[source]
        source: io::Error,
    },

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("state file is corrupted: {0}")]
    CorruptState(String),

    #[error("config at {path} is invalid: {reason}")]
    Config { path: PathBuf, reason: String },

    #[error("shared train state is unavailable: {0}")]
    StoreUnavailable(String),

    #[error("could not sync shared train state: {0}")]
    StoreSync(String),

    #[error(
        "another `choo` is using the shared train state (lock: {path}); \
         retry in a moment"
    )]
    StoreLocked { path: PathBuf },

    #[error(
        "this repository has no `{remote}` remote, so choochoo cannot tell \
         which repository's trains to load from shared state; add the remote, \
         or run with --no-sync"
    )]
    NoRepoIdentity { remote: String },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
