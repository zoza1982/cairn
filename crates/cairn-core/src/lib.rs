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

pub use msg::{Action, AppEffect, AppEvent, Msg, TextEdit};
pub use state::{
    ActiveTransfer, AppState, ChoiceProvenance, ChoiceStatus, ConnectionChoice, ConnectionKind,
    DiscoverySource, Listing, LogViewerId, LogViewerStatus, MaskedInput, Overlay, PaneState,
    PromptKind, QueuedTransfer, SessionEnd, SessionRecord, ShellActionMeta, Side, SortMode,
    TransferId, LOG_VIEWER_MAX_BYTES, LOG_VIEWER_MAX_LINES, SESSION_OUTPUT_MAX_BYTES,
    SESSION_OUTPUT_MAX_LINES,
};
pub use update::{initial_effects, update};
