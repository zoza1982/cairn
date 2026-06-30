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

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Canonical ids for the built-in backend actions — a single source of truth shared by the backends
/// that advertise them ([`crate::Vfs::actions_at`]) and the code that will dispatch them
/// ([`crate::Vfs::invoke`]), so advertisement and dispatch can never drift on a typo.
pub mod action_ids {
    /// Run a command in a container/pod.
    pub const EXEC: &str = "exec";
    /// Stream logs from a container/pod.
    pub const LOGS: &str = "logs";
    /// Forward a local port to a pod port.
    pub const PORT_FORWARD: &str = "port-forward";
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

/// A handle to a long-lived action session (port-forward or interactive exec), per RFC-0009.
///
/// The caller (the TUI) holds it for the session's lifetime: send `()` on `cancel` (or drop the
/// sender) to request termination; await `done` for the exit result. `local_port` is set for a
/// port-forward session; `stdin`/`stdout` carry the bidirectional I/O of an interactive exec;
/// `resize` forwards terminal window-resize events to a TTY exec session.
///
/// # Non-exhaustive note
///
/// This struct is `#[non_exhaustive]`: construct it via [`SessionHandle::new`] rather than with
/// a struct literal, which is forbidden outside the `cairn-vfs` crate. Field access is unrestricted.
#[non_exhaustive]
pub struct SessionHandle {
    /// Send `()` (or drop) to request cancellation of the session. The backend's relay task
    /// detects the signal and performs a best-effort teardown; `done` resolves shortly after.
    pub cancel: tokio::sync::oneshot::Sender<()>,
    /// Resolves once with the process exit code on success, or a [`VfsError`] on unexpected
    /// failure. Non-zero exit is `Ok(n)`, not an error — `Ok(0)` means clean exit or clean
    /// port-forward teardown; `Ok(-1)` is the sentinel used when the session is cancelled before
    /// the remote process exits. Backends must not panic if the consumer has dropped this receiver
    /// (e.g. a torn-down session pane); discard the send error silently.
    pub done: tokio::sync::oneshot::Receiver<Result<i32, VfsError>>,
    /// The local TCP port bound by a port-forward session; `None` for exec sessions.
    pub local_port: Option<u16>,
    /// Stdin pipe for an interactive exec. Absent for port-forward and non-interactive exec.
    /// The consumer owns it for the session's lifetime; dropping it closes stdin on the remote side.
    pub stdin: Option<tokio::sync::mpsc::Sender<Bytes>>,
    /// Combined stdout/stderr stream from an exec session. Absent for port-forward. The consumer
    /// owns and drains it to display output; dropping it is safe (the relay task exits when the
    /// sender detects a closed receiver).
    pub stdout: Option<tokio::sync::mpsc::Receiver<Bytes>>,
    /// TTY resize sink: send `(rows, cols)` to propagate a terminal window resize. Present only
    /// when the exec was started with `tty: true`; absent for non-TTY exec and port-forward.
    /// The backend forwards the value to the API-level resize mechanism (e.g. `TerminalSize` watch
    /// sender for Kubernetes, `resize_exec` for Docker). Errors on send are silently ignored
    /// (the session may have already ended).
    pub resize: Option<tokio::sync::mpsc::Sender<(u16, u16)>>,
}

impl SessionHandle {
    /// Construct a new [`SessionHandle`].
    ///
    /// Backend crates must use this constructor because `#[non_exhaustive]` forbids struct
    /// literal syntax outside `cairn-vfs`. Field access is unrestricted for callers (TUI, tests).
    #[must_use]
    pub fn new(
        cancel: tokio::sync::oneshot::Sender<()>,
        done: tokio::sync::oneshot::Receiver<Result<i32, VfsError>>,
        local_port: Option<u16>,
        stdin: Option<tokio::sync::mpsc::Sender<Bytes>>,
        stdout: Option<tokio::sync::mpsc::Receiver<Bytes>>,
        resize: Option<tokio::sync::mpsc::Sender<(u16, u16)>>,
    ) -> Self {
        Self {
            cancel,
            done,
            local_port,
            stdin,
            stdout,
            resize,
        }
    }
}

/// The result of invoking an action.
#[non_exhaustive]
pub enum ActionOutcome {
    /// Completed with no payload.
    Done,
    /// Completed with text output.
    Text(String),
    /// A live byte stream (e.g. follow-mode logs, exec output).
    Stream(BoxStream<'static, Result<Bytes, VfsError>>),
    /// A long-lived session (port-forward, interactive exec) — see [`SessionHandle`].
    Session(SessionHandle),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_id_roundtrip() {
        assert_eq!(ActionId::new("exec"), ActionId(SmolStr::new("exec")));
    }
}
