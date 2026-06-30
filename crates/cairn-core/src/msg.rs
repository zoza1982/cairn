//! Messages, events, and effects — the three families of the TEA loop.

use crate::state::{ConnectionChoice, LogViewerId, Side, TransferId};
use cairn_ai::Plan;
use cairn_secrets::SecretString;
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
    /// Open the transfer-queue overlay (view active + pending transfers).
    OpenQueue,
    /// In the queue view: move the selected pending transfer earlier (up).
    QueueMoveUp,
    /// In the queue view: move the selected pending transfer later (down).
    QueueMoveDown,
    /// Toggle pause/resume of the active transfer (no-op when none is running).
    TogglePause,
    /// Run the user-defined shell action at the given index (a config `[[shell_actions]]` entry, bound
    /// to a key). The index is into the validated action list shared by the keymap,
    /// `AppState::shell_actions`, and the runtime.
    RunShellAction(usize),
    /// Open the connection switcher (pick a backend to open in the active pane).
    OpenConnections,
    /// Open the vault-unlock overlay (enter the passphrase to unlock the secrets vault and retry the
    /// credential-bearing connections that were deferred while it was locked).
    VaultUnlock,
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
    /// Open the log viewer for the entry under the cursor.
    OpenLogViewer,
    /// Scroll the active overlay up one page (log viewer, future overlays).
    PageUp,
    /// Scroll the active overlay down one page.
    PageDown,
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
    /// Incremental progress for an in-flight transfer: cumulative bytes written so far, plus the
    /// average rate. Coalesced and delivered best-effort (may be dropped under load), so it is
    /// advisory display only.
    TransferProgress {
        /// Which transfer this update is for.
        id: TransferId,
        /// Cumulative bytes transferred so far.
        bytes: u64,
        /// Average throughput so far, in bytes per second.
        rate_bps: u64,
        /// Total bytes to transfer (from a pre-scan), if known — enables a percentage and ETA.
        total: Option<u64>,
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
        /// Which transfer finished.
        id: TransferId,
        /// Human-readable, secret-free status.
        status: String,
        /// Whether the transfer failed.
        error: bool,
    },
    /// A requested transfer would overwrite existing destinations; carries the parameters needed to
    /// re-issue it (with `overwrite: true`) once the user confirms.
    TransferConflict {
        /// The id of the transfer that bounced (so the runtime can release its slot).
        id: TransferId,
        /// Source connection.
        src_conn: ConnectionId,
        /// Destination connection.
        dst_conn: ConnectionId,
        /// The `(source, destination)` pairs of the original request.
        items: Vec<(VfsPath, VfsPath)>,
        /// Whether the original request was a move.
        is_move: bool,
        /// How many destinations already exist.
        conflicts: usize,
    },
    /// The assistant proposed a plan, or failed to (carries a redacted message).
    AiPlanProposed(Result<Plan, String>),
    /// An approved AI plan finished executing (completed, stopped on failure, or cancelled) — distinct
    /// from [`AppEvent::OpDone`] so it clears the `ai_executing` flag.
    AiPlanExecuted {
        /// Human-readable, secret-free status.
        status: String,
        /// Whether execution ended in failure.
        error: bool,
    },
    /// A user-defined shell action finished. Routes through the normal op-completion path (status +
    /// pane refresh, since the command may have changed the filesystem).
    ShellActionDone {
        /// Human-readable, redacted status (e.g. `"Checksum: exit 0"`).
        status: String,
        /// Whether it ended in failure (non-zero exit, timeout, or refusal).
        error: bool,
    },
    /// The vault-unlock effect finished. On success: the connections opened from the previously
    /// deferred credential-bearing profiles (to add to the switcher) — possibly empty. On failure: a
    /// secret-free, retryable message (wrong passphrase / missing vault) shown in the overlay.
    VaultUnlocked {
        /// `Ok(opened)` with the newly opened connections, or `Err(message)` to keep the overlay open.
        result: Result<Vec<ConnectionChoice>, String>,
    },
    /// A decoded chunk of log text from a streaming log-viewer session.
    LogChunk {
        /// Which session this belongs to.
        id: LogViewerId,
        /// The UTF-8 lossy-decoded text (may span multiple lines).
        text: String,
    },
    /// The log stream ended (cleanly or with an error).
    LogStreamEnded {
        /// Which session ended.
        id: LogViewerId,
        /// `None` on clean EOF; `Some(redacted_message)` on error.
        error: Option<String>,
    },
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
        /// Stable id minted by the reducer; addresses this transfer's progress/done events and its
        /// runtime control (cancel token + pause sender).
        id: TransferId,
        /// Source connection.
        src_conn: ConnectionId,
        /// Destination connection.
        dst_conn: ConnectionId,
        /// `(source, destination)` path pairs.
        items: Vec<(VfsPath, VfsPath)>,
        /// Whether to move (delete source after copy) rather than copy.
        is_move: bool,
        /// Overwrite existing destinations. `false` first checks for collisions and asks; `true`
        /// (after the user confirms) proceeds through them.
        overwrite: bool,
    },
    /// Cancel the in-flight transfer with this id, if it is still running.
    CancelTransfer {
        /// Which transfer to cancel.
        id: TransferId,
    },
    /// Set the paused state of the transfer with this id (`true` = pause, `false` = resume). No-op if
    /// it is no longer running.
    SetTransferPaused {
        /// Which transfer to pause/resume.
        id: TransferId,
        /// Target paused state.
        paused: bool,
    },
    /// Run a user-defined shell action (M8-7). The runtime resolves `index` to its definition, maps
    /// `target` to a real OS path via `Vfs::local_path` (refusing non-local backends), expands the
    /// argument templates, and spawns the program with no shell. Result returns as
    /// [`AppEvent::ShellActionDone`].
    RunShellAction {
        /// Index into the validated shell-action list.
        index: usize,
        /// Connection the target lives on (must be a local backend).
        conn: ConnectionId,
        /// The entry the action runs against.
        target: VfsPath,
    },
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
    /// Cancel the in-flight AI plan execution, if any.
    CancelAiPlan,
    /// Unlock the secrets vault with the entered passphrase, then re-open the credential-bearing
    /// connections that were deferred while it was locked. The runtime opens the vault off the render
    /// path (`Vault::open` via `spawn_blocking`), installs it into the broker, retries the deferred
    /// profiles, and reports back via [`AppEvent::VaultUnlocked`].
    ///
    /// The passphrase rides in a zeroizing [`SecretString`] — redacted in this effect's `Debug`,
    /// never logged, and wiped when the effect is dropped.
    UnlockVault {
        /// The passphrase to try (zeroized on drop).
        passphrase: SecretString,
    },
    /// Start streaming logs from a container/pod node. The runtime calls
    /// `Vfs::invoke(path, "logs", ActionCtx::Logs{follow:true,…})`, reads the
    /// `ActionOutcome::Stream`, and feeds each chunk back as `AppEvent::LogChunk`.
    OpenLogViewer {
        /// Session id minted by the reducer.
        id: LogViewerId,
        /// Connection.
        conn: ConnectionId,
        /// Path of the container/pod to stream.
        path: VfsPath,
        /// Title shown in the overlay border.
        title: String,
    },
    /// Cancel the log-viewer stream with this session id. Fired when Esc closes the overlay.
    CloseLogViewer {
        /// Session id to cancel.
        id: LogViewerId,
    },
}
