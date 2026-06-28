//! The Cairn application core.
//!
//! Implements the Elm/TEA architecture: an [`AppState`] model, a set of [`Msg`]s, and a pure
//! [`update`] reducer that mutates the state and returns [`AppEffect`]s for the effect runner to
//! execute. The reducer performs **no I/O and never `.await`s** — that is what keeps the render path
//! non-blocking. Asynchronous results return as [`AppEvent`]s (wrapped in [`Msg::Event`]). See
//! `docs/LLD.md` §5.

mod msg;
mod state;
mod update;

pub use msg::{Action, AppEffect, AppEvent, Msg};
pub use state::{AppState, Listing, Overlay, PaneState, Side};
pub use update::{initial_effects, update};
