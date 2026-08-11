//! Command-line interface (clap derive).
//!
//! [`run`] is the entry point used by `main.rs`. Each subcommand maps onto
//! a function in the [`crate::train`] module; CLI-specific concerns
//! (argument parsing, formatting, exit codes) live here.

use std::io::{self, Write};
use std::path::Path;

use clap::{Parser, Subcommand};

use crate::error::Result;
use crate::git::{GitRunner, ProcessGitRunner, PushMode};
use crate::github;
use crate::report::StderrReporter;
use crate::state::{self, Store};
use crate::train;

#[derive(Debug, Parser)]
#[command(name = "choo", version, about = "Manage stacked PR trains on GitHub")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Don't touch the network for shared state: read the last-synced copy,
    /// and commit any change locally to publish on your next synced command.
    #[arg(long, global = true)]
    pub no_sync: bool,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Create a new (empty) train.
    Init {
        /// Name of the train.
        name: String,
        /// Base branch the train sits on top of. Defaults to this repo's
        /// `[repo."<url>"] base` in config.toml, else `main`.
        #[arg(long)]
        base: Option<String>,
        /// Also manage an aggregate ("combined") branch for this train: one
        /// extra branch holding every change in the train, with its own
        /// draft PR against the base branch.
        #[arg(long)]
        aggregate: bool,
        /// Name for the aggregate branch (implies `--aggregate`).
        /// Defaults to `choo/<train>/combined`.
        #[arg(long = "aggregate-branch", value_name = "BRANCH")]
        aggregate_branch: Option<String>,
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
    ///
    /// If the branch is in the train but not on this machine yet, it is
    /// created from `<remote>/<branch>`.
    Checkout {
        /// Branch name, the train's base, or a position in the train —
        /// `choo checkout 1` takes the first branch, matching the `#`
        /// column of the train table.
        branch: String,
        #[arg(short = 't', long = "train")]
        train: Option<String>,
        /// Remote to create the branch from when it's missing locally.
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Create local branches for every branch in a train, from the remote.
    ///
    /// Use this on a second machine: shared state tells you the train
    /// exists, this puts its branches in your working copy. Branches you
    /// already have are never moved, and your working tree stays put.
    Fetch {
        /// Train name. Defaults to the active train.
        train: Option<String>,
        #[arg(long, default_value = "origin")]
        remote: String,
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
    ///
    /// When the train has an aggregate branch, its draft PR (against the
    /// train's base) is created/updated too.
    Pr {
        #[arg(short = 't', long = "train")]
        train: Option<String>,
        /// Open PRs as drafts. The aggregate PR is always a draft.
        #[arg(long)]
        draft: bool,
    },
    /// Edit the train's "PR Train Context" — one block of prose that
    /// choochoo renders at the top of every PR in the train.
    ///
    /// Opens `$VISUAL`/`$EDITOR` (vim by default) on the current text;
    /// save and quit to store it, quit without saving (`:cq` in vim) to
    /// abandon the edit. Saving an empty buffer removes the section.
    ///
    /// Nothing is sent to GitHub here: run `choo pr` to push the new text
    /// into every PR description.
    Context {
        #[arg(short = 't', long = "train")]
        train: Option<String>,
        /// Print the stored context instead of opening an editor.
        #[arg(long)]
        show: bool,
    },
    /// Manage a train's aggregate ("combined") branch.
    ///
    /// The aggregate branch is kept pointing at the tip of the train, so
    /// its diff against the train's base is every change in the train. Its
    /// PR is always a draft: it exists for whole-change review and CI, and
    /// the per-branch PRs are what get merged.
    Aggregate {
        #[command(subcommand)]
        action: AggregateCommand,
    },
    /// Show where shared train state lives, and push anything pending.
    ///
    /// Every command already syncs; this is for checking the setup and for
    /// draining a change that couldn't be published earlier (say, you were
    /// offline when you ran `choo add`).
    Sync {
        /// Report only; don't contact the store.
        #[arg(long)]
        status: bool,
    },
    /// Launch the interactive TUI.
    Tui,
}

#[derive(Debug, Subcommand)]
pub enum AggregateCommand {
    /// Start managing an aggregate branch for the train.
    Enable {
        /// Branch name. Defaults to `choo/<train>/combined`.
        #[arg(long, value_name = "BRANCH")]
        branch: Option<String>,
        #[arg(short = 't', long = "train")]
        train: Option<String>,
    },
    /// Stop managing the aggregate branch. The git branch and its PR are
    /// left alone.
    Disable {
        #[arg(short = 't', long = "train")]
        train: Option<String>,
    },
    /// Re-point the aggregate branch at the current tip of the train.
    Sync {
        #[arg(short = 't', long = "train")]
        train: Option<String>,
    },
}

/// Entry point used by `main.rs`. Parses argv and dispatches.
pub fn run() -> Result<()> {
    let mut cli = Cli::parse();
    let repo_root = state::find_repo_root(&std::env::current_dir()?)?;

    let mut env = crate::config::Env::from_process();
    env.no_sync |= cli.no_sync;
    let config = crate::config::load(&env)?;

    // Only construct a git runner when the config actually needs one to
    // identify this repo; a purely local `choo` shouldn't require `git` on
    // PATH any earlier than it does today.
    let store = if config.store().is_some() {
        let git = ProcessGitRunner::new(repo_root.clone())?;
        crate::store::open(&repo_root, &config, &env, &git)?
    } else {
        Store::local(repo_root)
    };

    // Resolved here rather than in `dispatch` because it's the config layer's
    // answer, and `dispatch` deliberately takes only a store.
    if let Command::Init { base, .. } = &mut cli.command {
        if base.is_none() {
            *base = Some(default_base(&config, store.repo_root()));
        }
    }

    let result = dispatch(cli, &store);
    // Surface sync degradations even when the command itself failed — the
    // reason a command failed may well be in here.
    for warning in store.take_warnings() {
        eprintln!("warning: {warning}");
    }
    result
}

/// Base branch a new train should sit on when `--base` wasn't given.
///
/// `[repo."<origin url>"] base` from config, else [`config::DEFAULT_BASE`].
///
/// Identifying the repository means asking git for its `origin` URL, so this
/// stays entirely out of the way of anyone with no `[repo]` tables: no git
/// process, no failure mode. When there *are* tables but the repo can't be
/// identified — no `origin`, an unusable URL, no `git` on `PATH` — the answer
/// is the plain default. That's indistinguishable from having no entry for
/// this repo, which is what it means.
fn default_base(config: &crate::config::Config, repo_root: &Path) -> String {
    if config.repos.is_empty() {
        return crate::config::DEFAULT_BASE.to_string();
    }
    ProcessGitRunner::new(repo_root.to_path_buf())
        .ok()
        .and_then(|git| git.remote_url("origin").ok().flatten())
        .and_then(|url| crate::repoid::from_url(&url))
        .and_then(|key| config.base_for(&key).map(str::to_string))
        .unwrap_or_else(|| crate::config::DEFAULT_BASE.to_string())
}

pub fn dispatch(cli: Cli, store: &Store) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match cli.command {
        Command::Init {
            name,
            base,
            aggregate,
            aggregate_branch,
        } => {
            // `run` has already resolved this against the config; the
            // fallback is for anyone calling `dispatch` directly.
            let base = base
                .unwrap_or_else(|| crate::config::DEFAULT_BASE.to_string());
            train::init::run(store, &name, &base)?;
            writeln!(&mut out, "created train `{name}` (base `{base}`)").ok();
            if aggregate || aggregate_branch.is_some() {
                let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
                let mut reporter = StderrReporter::new();
                let branch = train::aggregate::enable(
                    store,
                    &git,
                    &mut reporter,
                    Some(&name),
                    aggregate_branch.as_deref(),
                )?;
                writeln!(&mut out, "combined branch: `{branch}` (targets `{base}`)").ok();
            }
        }
        Command::List => {
            let s = train::show::run_list(store)?;
            out.write_all(s.as_bytes()).ok();
        }
        Command::Show { name } => {
            let s = train::show::run_show(store, name.as_deref())?;
            out.write_all(s.as_bytes()).ok();
        }
        Command::Switch { name } => {
            train::switch::run(store, &name)?;
            writeln!(&mut out, "active train is now `{name}`").ok();
        }
        Command::Add { branch, train: t } => {
            let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
            train::add::run(store, &git, t.as_deref(), branch.as_deref())?;
            writeln!(&mut out, "ok").ok();
        }
        Command::Remove { branch, train: t } => {
            train::remove::run(store, t.as_deref(), &branch)?;
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
            train::reorder::run(store, t.as_deref(), &branch, position, &relative_to)?;
            writeln!(&mut out, "moved `{branch}`").ok();
        }
        Command::Checkout {
            branch,
            train: t,
            remote,
        } => {
            let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
            let mut reporter = StderrReporter::new();
            train::checkout::run(
                store,
                &git,
                &mut reporter,
                t.as_deref(),
                &branch,
                &remote,
            )?;
        }
        Command::Fetch { train: t, remote } => {
            let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
            let mut reporter = StderrReporter::new();
            let s =
                train::fetch::run(store, &git, &mut reporter, t.as_deref(), &remote)?;
            writeln!(
                &mut out,
                "train `{}`: created {}, already here {}{}",
                s.train,
                s.created.len(),
                s.existing.len() + s.behind.len(),
                if s.behind.is_empty() {
                    String::new()
                } else {
                    format!(" ({} behind `{remote}`)", s.behind.len())
                }
            )
            .ok();
        }
        Command::Rebase {
            train: t,
            r#continue,
            abort,
        } => {
            let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
            let mut reporter = StderrReporter::new();
            if abort {
                train::rebase::abort(store, &git)?;
                writeln!(&mut out, "rebase aborted").ok();
            } else if r#continue {
                let s = train::rebase::continue_run(store, &git, &mut reporter)?;
                writeln!(
                    &mut out,
                    "train `{}` rebased; continued for {} more branch(es)",
                    s.train,
                    s.rebased.len()
                )
                .ok();
            } else {
                let s = train::rebase::run(store, &git, &mut reporter, t.as_deref())?;
                writeln!(
                    &mut out,
                    "train `{}` rebased ({} branch(es))",
                    s.train,
                    s.rebased.len()
                )
                .ok();
                if let Some(branch) = &s.aggregate_synced {
                    writeln!(&mut out, "combined branch `{branch}` synced to tip").ok();
                }
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
            let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
            let mut reporter = StderrReporter::new();
            let s = train::push::run(
                store,
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
            if let Some(branch) = &s.aggregate_pushed {
                writeln!(&mut out, "pushed combined branch `{branch}`").ok();
            }
            if !s.atomic {
                writeln!(
                    &mut out,
                    "note: `{remote}` does not support atomic push, so branches \
                     were pushed one at a time"
                )
                .ok();
            }
        }
        Command::Pr { train: t, draft } => {
            let gh = github::make_runner()?;
            let mut reporter = StderrReporter::new();
            let s = train::pr::run(store, gh.as_ref(), &mut reporter, t.as_deref(), draft)?;
            writeln!(
                &mut out,
                "train `{}`: created {}, updated {}",
                s.train,
                s.created.len(),
                s.updated.len()
            )
            .ok();
            if let Some(pr) = &s.aggregate_pr {
                writeln!(&mut out, "combined draft PR: #{} <{}>", pr.number, pr.url).ok();
            }
        }
        Command::Context { train: t, show } => {
            if show {
                let s = train::context::run_show(store, t.as_deref())?;
                out.write_all(s.as_bytes()).ok();
            } else {
                let editor = crate::editor::ProcessEditor::from_env();
                match train::context::run(store, &editor, t.as_deref())? {
                    None => {
                        writeln!(&mut out, "editor exited without saving; context unchanged")
                            .ok();
                    }
                    Some(o) if !o.changed => {
                        writeln!(&mut out, "context for train `{}` unchanged", o.train).ok();
                    }
                    Some(o) => {
                        if o.cleared {
                            writeln!(&mut out, "cleared context for train `{}`", o.train).ok();
                        } else {
                            writeln!(&mut out, "updated context for train `{}`", o.train).ok();
                        }
                        if o.prs == 0 {
                            writeln!(
                                &mut out,
                                "run `choo pr` to open the train's PRs with it"
                            )
                            .ok();
                        } else {
                            writeln!(
                                &mut out,
                                "run `choo pr` to sync {} PR description(s)",
                                o.prs
                            )
                            .ok();
                        }
                    }
                }
            }
        }
        Command::Aggregate { action } => match action {
            AggregateCommand::Enable { branch, train: t } => {
                let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
                let mut reporter = StderrReporter::new();
                let branch = train::aggregate::enable(
                    store,
                    &git,
                    &mut reporter,
                    t.as_deref(),
                    branch.as_deref(),
                )?;
                writeln!(
                    &mut out,
                    "combined branch `{branch}` enabled; run `choo push` then \
                     `choo pr` to open its draft PR"
                )
                .ok();
            }
            AggregateCommand::Disable { train: t } => {
                let branch = train::aggregate::disable(store, t.as_deref())?;
                writeln!(
                    &mut out,
                    "combined branch `{branch}` no longer managed (branch and PR left as-is)"
                )
                .ok();
            }
            AggregateCommand::Sync { train: t } => {
                let git = ProcessGitRunner::new(store.repo_root().to_path_buf())?;
                let mut reporter = StderrReporter::new();
                match train::aggregate::run_sync(store, &git, &mut reporter, t.as_deref())? {
                    Some(o) => {
                        writeln!(
                            &mut out,
                            "combined branch `{}` -> tip of `{}` ({})",
                            o.branch,
                            o.tip,
                            if o.moved { "updated" } else { "already current" }
                        )
                        .ok();
                    }
                    None => {
                        writeln!(&mut out, "nothing to sync (train has no branches)").ok();
                    }
                }
            }
        },
        Command::Sync { status } => {
            if !store.is_shared() {
                writeln!(
                    &mut out,
                    "trains are local to this machine ({})\n\
                     configure `[store] repo` in your choochoo config to share them",
                    store.describe()
                )
                .ok();
            } else {
                if !status {
                    store.sync_now()?;
                }
                let state = store.load()?;
                writeln!(&mut out, "shared state: {}", store.describe()).ok();
                writeln!(&mut out, "trains:       {}", state.trains.len()).ok();
                writeln!(
                    &mut out,
                    "unpublished:  {}",
                    if store.has_unpublished() {
                        "yes — retried on your next command"
                    } else {
                        "no"
                    }
                )
                .ok();
            }
        }
        Command::Tui => {
            crate::tui::run(store)?;
        }
    }
    Ok(())
}
