//! Core leaf types shared across every Cairn crate.
//!
//! This crate has no dependency on any other Cairn crate; everything else depends on it. It defines
//! the backend-agnostic vocabulary the rest of the system speaks: [`VfsPath`] (a normalized,
//! traversal-safe location), [`Entry`] (uniform directory-entry metadata), [`Caps`] (the capability
//! model that expresses what a backend can and cannot do), and identifiers like [`ConnectionId`].
//!
//! See `docs/LLD.md` §3 for the design rationale.

mod caps;
mod entry;
mod ids;
mod path;

pub use caps::Caps;
pub use entry::{ContainerState, Entry, EntryExt, EntryKind, PodPhase, UnixPerms};
pub use ids::{ConnectionId, CredentialId, Scheme};
pub use path::{PathError, VfsPath};
