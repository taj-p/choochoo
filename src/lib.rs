//! choochoo: manage stacked PR trains on GitHub.
//!
//! The library is organized into:
//! - [`state`]: persistent train metadata, reached through a
//!   [`state::Store`] that decides *where* it lives (today:
//!   `.git/choochoo/state.json`)
//! - [`git`]: a [`git::GitRunner`] trait + a process-shelling implementation
//! - [`github`]: a [`github::GhRunner`] trait + a process-shelling implementation
//! - [`editor`]: an [`editor::Editor`] trait + a `$EDITOR`-spawning
//!   implementation
//! - [`render`]: pure rendering of PR descriptions and the train table
//! - [`train`]: domain operations (add/remove/reorder/rebase/push/pr/...)
//! - [`tui`]: a small ratatui-based interactive UI
//! - [`cli`]: the clap command tree, exported as [`cli::run`]

pub mod cli;
pub mod config;
pub mod editor;
pub mod error;
pub mod git;
pub mod github;
pub mod render;
pub mod report;
pub mod repoid;
pub mod state;
pub mod store;
pub mod train;
pub mod tui;

pub use error::{Error, Result};
