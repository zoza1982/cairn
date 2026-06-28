//! The Cairn terminal UI.
//!
//! Provides the ratatui [`render`] of the [`cairn_core::AppState`] and the input [`keymap`]. It is
//! deliberately thin and side-effect-free: the application event loop, terminal lifecycle, and
//! effect execution live in the `cairn` binary. See `docs/LLD.md` §6.

pub mod keymap;
mod render;

pub use keymap::{action_for, Keymap};
pub use render::render;
