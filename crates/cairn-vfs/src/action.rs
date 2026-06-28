//! Backend-specific actions (exec, logs, port-forward, …) surfaced through a uniform
//! discover → describe → invoke interface, so the core `Vfs` trait stays small and object-safe.

use crate::error::VfsError;
use bytes::Bytes;
use futures::stream::BoxStream;
use smol_str::SmolStr;
use std::time::SystemTime;

/// A stable identifier for a backend action.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionId(pub SmolStr);

impl ActionId {
    /// Construct from a static string.
    #[must_use]
    pub fn new(s: &str) -> Self {
        Self(SmolStr::new(s))
    }
}

/// Describes an action available at a path, for rendering in the action menu.
#[derive(Debug, Clone)]
pub struct ActionDescriptor {
    /// The action's stable id.
    pub id: ActionId,
    /// A short human-readable label (e.g. "Stream logs").
    pub label: SmolStr,
    /// Hints the UI which handler/representation to use.
    pub kind: ActionKind,
    /// Whether the action mutates state destructively (drives confirm gating).
    pub destructive: bool,
}

/// The interaction shape of an action's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActionKind {
    /// Runs once and returns text or nothing.
    OneShot,
    /// Produces a continuous byte stream (e.g. follow-mode logs).
    Stream,
    /// Establishes a long-lived session (e.g. port-forward, interactive exec).
    Session,
    /// Interactive with bidirectional I/O (e.g. exec with a TTY).
    Interactive,
}

/// Input parameters for invoking an action.
#[non_exhaustive]
pub enum ActionCtx {
    /// No parameters.
    None,
    /// Execute a command, optionally with a TTY.
    Exec {
        /// The argument vector.
        argv: Vec<String>,
        /// Whether to allocate a TTY.
        tty: bool,
    },
    /// Stream logs.
    Logs {
        /// Follow (tail) the stream.
        follow: bool,
        /// Only return entries at or after this time.
        since: Option<SystemTime>,
        /// Restrict to a named container, where applicable.
        container: Option<SmolStr>,
    },
    /// Forward a local port to a remote port.
    PortForward {
        /// Local port.
        local: u16,
        /// Remote port.
        remote: u16,
    },
}

/// The result of invoking an action.
///
/// `#[non_exhaustive]`: a `Session` variant (for port-forward / interactive exec) is added with the
/// M6 container/cluster backends.
#[non_exhaustive]
pub enum ActionOutcome {
    /// Completed with no payload.
    Done,
    /// Completed with text output.
    Text(String),
    /// A live byte stream (e.g. follow-mode logs, exec output).
    Stream(BoxStream<'static, Result<Bytes, VfsError>>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_id_roundtrip() {
        assert_eq!(ActionId::new("exec"), ActionId(SmolStr::new("exec")));
    }
}
