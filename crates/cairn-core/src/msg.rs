//! Messages, events, and effects — the three families of the TEA loop.

use crate::state::Side;
use cairn_ai::Plan;
use cairn_types::{ConnectionId, VfsPath};
use cairn_vfs::{ListPage, VfsError};

/// A high-level user action, resolved from input by the TUI keymap. Kept independent of any terminal
/// library so the core stays UI-agnostic and unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Move the cursor up one.
    CursorUp,
    /// Move the cursor down one.
    CursorDown,
    /// Move the cursor to the top.
    CursorTop,
    /// Move the cursor to the bottom.
    CursorBottom,
    /// Enter the directory under the cursor.
    Enter,
    /// Go to the parent directory.
    Leave,
    /// Switch focus to the other pane.
    SwitchPane,
    /// Toggle the mark on the entry under the cursor.
    ToggleMark,
    /// Copy the marked (or current) entries to the other pane.
    Copy,
    /// Move the marked (or current) entries to the other pane.
    Move,
    /// Delete the marked (or current) entries (asks for confirmation).
    Delete,
    /// Confirm a pending modal action.
    Confirm,
    /// Cancel a pending modal action / dismiss an overlay.
    Cancel,
    /// Reload the active pane.
    Refresh,
    /// Cycle the active pane's sort mode (name → size → modified). Re-orders in place, no re-list.
    CycleSort,
    /// Toggle whether the active pane lists hidden entries (re-lists with the new `all` flag).
    ToggleHidden,
    /// Begin filtering the active pane's listing (filter-as-you-type).
    Filter,
    /// Open the connection switcher (pick a backend to open in the active pane).
    OpenConnections,
    /// Open a prompt to create a new directory in the active pane.
    MakeDir,
    /// Open a prompt to rename the entry under the cursor.
    Rename,
    /// Ask the AI assistant to propose a plan (opens the plan → confirm overlay when it arrives).
    AiPropose,
    /// In the plan overlay: approve every step at once (only honored when no step is irreversible).
    ApproveAll,
    /// In the plan overlay: reject the step under the review cursor.
    Reject,
    /// Quit the application.
    Quit,
}

/// A message into the reducer.
#[non_exhaustive]
pub enum Msg {
    /// A resolved user action.
    Action(Action),
    /// A text-editing keystroke, routed to an open text prompt.
    Text(TextEdit),
    /// An asynchronous result coming back from the effect runner.
    Event(AppEvent),
    /// A periodic tick (animations, timeouts).
    Tick,
}

/// A single edit to a text field, kept terminal-agnostic so the core stays UI-independent (the TUI
/// layer maps key events to these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextEdit {
    /// Append a character.
    Insert(char),
    /// Delete the last character.
    Backspace,
    /// Accept the field (act on the entered text).
    Submit,
    /// Discard the prompt.
    Cancel,
}

/// Results flowing back from the async world.
#[non_exhaustive]
pub enum AppEvent {
    /// A directory listing page (or error) for a pane.
    Listed {
        /// Which pane requested it.
        pane: Side,
        /// The connection it was listed on (ignored if the pane has since switched connection).
        conn: ConnectionId,
        /// The directory that was listed (ignored if it no longer matches the pane's cwd).
        dir: VfsPath,
        /// The page result.
        result: Result<ListPage, VfsError>,
    },
    /// Incremental progress for an in-flight transfer: cumulative bytes written so far. Coalesced and
    /// delivered best-effort (may be dropped under load), so it is advisory display only.
    TransferProgress {
        /// Cumulative bytes transferred so far.
        bytes: u64,
    },
    /// A delete/mkdir/rename/plan operation finished; carries a status message and whether it failed.
    OpDone {
        /// Human-readable, secret-free status.
        status: String,
        /// Whether the operation failed.
        error: bool,
    },
    /// A transfer (copy/move) finished — distinct from [`AppEvent::OpDone`] so it (and only it) clears
    /// the transfer-progress indicator, immune to an unrelated op completing mid-transfer.
    TransferDone {
        /// Human-readable, secret-free status.
        status: String,
        /// Whether the transfer failed.
        error: bool,
    },
    /// The assistant proposed a plan, or failed to (carries a redacted message).
    AiPlanProposed(Result<Plan, String>),
}

/// Intents emitted by the reducer for the effect runner to execute. The reducer never performs I/O.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AppEffect {
    /// List a directory on a connection, delivering the result to `pane`.
    List {
        /// Pane to deliver the result to.
        pane: Side,
        /// Connection to list on.
        conn: ConnectionId,
        /// Directory to list.
        dir: VfsPath,
        /// Include hidden entries (maps to `ListOpts::all`).
        all: bool,
    },
    /// Copy or move entries from one connection to another.
    Transfer {
        /// Source connection.
        src_conn: ConnectionId,
        /// Destination connection.
        dst_conn: ConnectionId,
        /// `(source, destination)` path pairs.
        items: Vec<(VfsPath, VfsPath)>,
        /// Whether to move (delete source after copy) rather than copy.
        is_move: bool,
    },
    /// Cancel the in-flight transfer, if any.
    CancelTransfer,
    /// Delete entries on a connection.
    Delete {
        /// The connection.
        conn: ConnectionId,
        /// Paths to delete.
        paths: Vec<VfsPath>,
    },
    /// Create a directory on a connection.
    CreateDir {
        /// The connection.
        conn: ConnectionId,
        /// The directory path to create.
        path: VfsPath,
    },
    /// Rename an entry within a connection.
    Rename {
        /// The connection.
        conn: ConnectionId,
        /// The current path.
        from: VfsPath,
        /// The new path (same directory, new leaf name).
        to: VfsPath,
    },
    /// Ask the AI assistant to propose a plan for a natural-language request.
    RequestAiPlan {
        /// The user's request.
        prompt: String,
    },
    /// Execute an approved plan's steps.
    ExecutePlan {
        /// The fully-approved plan.
        plan: Plan,
    },
}
