//! Command-line interface (clap derive).
//!
//! [`run`] is the entry point used by `main.rs`. Each subcommand maps onto
//! a function in the [`crate::train`] module; CLI-specific concerns
//! (argument parsing, formatting, exit codes) live here.

use std::io::{self, Write};
use std::path::Path;

use clap::{Parser, Subcommand};

use crate::error::Result;
use crate::git::{ProcessGitRunner, PushMode};
use crate::github;
use crate::report::StderrReporter;
use crate::state;
use crate::train;

#[derive(Debug, Parser)]
#[command(name = "choo", version, about = "Manage stacked PR trains on GitHub")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new (empty) train.
    Init {
        /// Name of the train.
        name: String,
        /// Base branch the train sits on top of.
        #[arg(long, default_value = "main")]
        base: String,
    },
    /// List every train in this repo.
    List,
    /// Show details of one train.
    Show {
        /// Train name. Defaults to the active train.
        name: Option<String>,
    },
    /// Set the active train.
    Switch {
        /// Train name to make active.
        name: String,
    },
    /// Append a branch (defaults to the current branch) to a train.
    Add {
        /// Branch name. Defaults to the current branch.
        branch: Option<String>,
        #[arg(short = 't', long = "train")]
        train: Option<String>,
    },
    /// Remove a branch from a train. Does not delete the underlying git branch.
    Remove {
        branch: String,
        #[arg(short = 't', long = "train")]
        train: Option<String>,
    },
    /// Reorder a branch within a train.
    Move {
        /// Branch to move.
        branch: String,
        /// Place it before this other branch.
        #[arg(long, conflicts_with = "after")]
        before: Option<String>,
        /// Place it after this other branch.
        #[arg(long, conflicts_with = "before")]
        after: Option<String>,
        #[arg(short = 't', long = "train")]
        train: Option<String>,
    },
    /// Check out a branch in a train.
    Checkout {
        branch: String,
        #[arg(short = 't', long = "train")]
        train: Option<String>,
    },
    /// Restack the whole train.
    Rebase {
        #[arg(short = 't', long = "train")]
        train: Option<String>,
        /// Continue a previously interrupted rebase.
        #[arg(long, conflicts_with_all = ["abort"])]
        r#continue: bool,
        /// Abort an in-progress rebase.
        #[arg(long)]
        abort: bool,
    },
    /// Push every branch in the train.
    ///
    /// By default uses `git push --force-with-lease`, which refuses if
    /// the remote moved since you last fetched. Pass `--without-lease`
    /// to drop the lease check (uses plain `--force`), or
    /// `--no-force-with-lease` to push without any force at all.
    Push {
        #[arg(short = 't', long = "train")]
        train: Option<String>,
        /// `git push --force` (no lease check). Use when the lease is
        /// stale (e.g. a background fetch invalidated it) and you've
        /// confirmed it's safe to overwrite the remote.
        #[arg(long = "without-lease", conflicts_with = "no_force_with_lease")]
        without_lease: bool,
        /// Plain `git push` — fails if the push wouldn't be fast-forward.
        #[arg(long = "no-force-with-lease")]
        no_force_with_lease: bool,
        /// Remote name.
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Create or update one PR per branch in the train and sync the table.
    Pr {
        #[arg(short = 't', long = "train")]
        train: Option<String>,
        /// Open PRs as drafts.
        #[arg(long)]
        draft: bool,
    },
    /// Launch the interactive TUI.
    Tui,
}

/// Entry point used by `main.rs`. Parses argv and dispatches.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let repo_root = state::find_repo_root(&std::env::current_dir()?)?;
    dispatch(cli, &repo_root)
}

pub fn dispatch(cli: Cli, repo_root: &Path) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match cli.command {
        Command::Init { name, base } => {
            train::init::run(repo_root, &name, &base)?;
            writeln!(&mut out, "created train `{name}` (base `{base}`)").ok();
        }
        Command::List => {
            let s = train::show::run_list(repo_root)?;
            out.write_all(s.as_bytes()).ok();
        }
        Command::Show { name } => {
            let s = train::show::run_show(repo_root, name.as_deref())?;
            out.write_all(s.as_bytes()).ok();
        }
        Command::Switch { name } => {
            train::switch::run(repo_root, &name)?;
            writeln!(&mut out, "active train is now `{name}`").ok();
        }
        Command::Add { branch, train: t } => {
            let git = ProcessGitRunner::new(repo_root.to_path_buf())?;
            train::add::run(repo_root, &git, t.as_deref(), branch.as_deref())?;
            writeln!(&mut out, "ok").ok();
        }
        Command::Remove { branch, train: t } => {
            train::remove::run(repo_root, t.as_deref(), &branch)?;
            writeln!(&mut out, "removed `{branch}`").ok();
        }
        Command::Move {
            branch,
            before,
            after,
            train: t,
        } => {
            let (position, relative_to) = match (before, after) {
                (Some(b), None) => (train::reorder::Position::Before, b),
                (None, Some(a)) => (train::reorder::Position::After, a),
                (Some(_), Some(_)) => unreachable!("clap conflicts_with"),
                (None, None) => {
                    return Err(crate::Error::InvalidArgument(
                        "pass either --before <branch> or --after <branch>".into(),
                    ));
                }
            };
            train::reorder::run(repo_root, t.as_deref(), &branch, position, &relative_to)?;
            writeln!(&mut out, "moved `{branch}`").ok();
        }
        Command::Checkout { branch, train: t } => {
            let git = ProcessGitRunner::new(repo_root.to_path_buf())?;
            train::checkout::run(repo_root, &git, t.as_deref(), &branch)?;
        }
        Command::Rebase {
            train: t,
            r#continue,
            abort,
        } => {
            let git = ProcessGitRunner::new(repo_root.to_path_buf())?;
            let mut reporter = StderrReporter::new();
            if abort {
                train::rebase::abort(repo_root, &git)?;
                writeln!(&mut out, "rebase aborted").ok();
            } else if r#continue {
                let s = train::rebase::continue_run(repo_root, &git, &mut reporter)?;
                writeln!(
                    &mut out,
                    "train `{}` rebased; continued for {} more branch(es)",
                    s.train,
                    s.rebased.len()
                )
                .ok();
            } else {
                let s = train::rebase::run(repo_root, &git, &mut reporter, t.as_deref())?;
                writeln!(
                    &mut out,
                    "train `{}` rebased ({} branch(es))",
                    s.train,
                    s.rebased.len()
                )
                .ok();
            }
        }
        Command::Push {
            train: t,
            without_lease,
            no_force_with_lease,
            remote,
        } => {
            let mode = match (without_lease, no_force_with_lease) {
                (false, false) => PushMode::ForceWithLease,
                (true, false) => PushMode::Force,
                (false, true) => PushMode::Plain,
                (true, true) => unreachable!("clap conflicts_with"),
            };
            let git = ProcessGitRunner::new(repo_root.to_path_buf())?;
            let mut reporter = StderrReporter::new();
            let s = train::push::run(
                repo_root,
                &git,
                &mut reporter,
                t.as_deref(),
                mode,
                &remote,
            )?;
            writeln!(
                &mut out,
                "train `{}`: pushed {} branch(es)",
                s.train,
                s.pushed.len()
            )
            .ok();
        }
        Command::Pr { train: t, draft } => {
            let gh = github::make_runner()?;
            let mut reporter = StderrReporter::new();
            let s = train::pr::run(repo_root, gh.as_ref(), &mut reporter, t.as_deref(), draft)?;
            writeln!(
                &mut out,
                "train `{}`: created {}, updated {}",
                s.train,
                s.created.len(),
                s.updated.len()
            )
            .ok();
        }
        Command::Tui => {
            crate::tui::run(repo_root)?;
        }
    }
    Ok(())
}
