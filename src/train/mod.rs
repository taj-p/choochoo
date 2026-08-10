//! Domain operations on trains.
//!
//! Each submodule covers one operation. Operations come in two flavours:
//!
//! * Pure helpers on [`crate::state::Train`] (e.g. inserting/reordering
//!   branches in the in-memory model) — these are unit-tested directly.
//! * Action functions like [`rebase::run`], [`push::run`], [`pr::run`]
//!   that orchestrate the trait runners + state. These are tested via
//!   the integration tests against fake runners.

pub mod add;
pub mod checkout;
pub mod init;
pub mod pr;
pub mod push;
pub mod rebase;
pub mod remove;
pub mod reorder;
pub mod show;
pub mod switch;
