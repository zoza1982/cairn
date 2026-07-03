//! Messages, events, and effects — the three families of the TEA loop.

use crate::state::{
    ContentHash, FileKind, LogViewerId, PagerId, RemoteEditId, RemoteVersion, Side, TransferId,
    WritebackConflictReason,
};
use bytes::Bytes;
use cairn_ai::Plan;
use cairn_secrets::SecretString;
use cairn_types::{ConnectionId, SessionId, UnixPerms, VfsPath};
use cairn_vfs::{ListPage, VfsError};
use std::path::PathBuf;

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
    /// Open a cooked exec session on the entry under the cursor. Available on backends that support
    /// the `"exec"` action (e.g. Kubernetes/Docker exec, SSH). The reducer uses `["sh"]` as the
    /// default argv. Wired to a configurable key once exec-capable backends land.
    OpenExecSession,
    /// Open the read-only pager on the entry under the cursor (`F3`). Unlike `Enter`, this skips
    /// the async text/binary sniff — F3 always opens the pager immediately in `Text` mode,
    /// flipping to `Hex` if the first streamed chunk contains a NUL byte.
    View,
    /// Edit the entry under the cursor in an external editor (`F4`, MC-faithful — no sniff, always
    /// opens the editor regardless of text/binary content). `Enter` on a `FileKind::Text` result
    /// (RFC-0012 P2) also routes here via [`AppEvent::FileSniffed`]. Local backends only in P2 —
    /// see [`AppEffect::SuspendAndEdit`].
    Edit,
    /// Scroll the active overlay up one page (log viewer, future overlays).
    PageUp,
    /// Scroll the active overlay down one page.
    PageDown,
    /// Quit the application.
    Quit,
    /// Open the add-connection form (scheme picker → fields).
    ///
    /// Bound globally to `Ctrl-N`. Inside the connection switcher it is also accessible via
    /// the `[Ctrl-N] New` hint line. Plain `n` inside the switcher maps to `Action::Cancel`
    /// (dismisses the switcher), consistent with the default `n`-closes-overlay convention.
    NewConnection,
    /// Open the edit-connection form for the selected profile. Only meaningful inside the
    /// connection switcher; ignored (no-op) elsewhere.
    EditConnection,
    /// Delete the selected connection profile. Only meaningful inside the connection switcher;
    /// ignored elsewhere. Requires confirmation via the connection switcher's selection.
    DeleteConnection,
    /// Toggle the "remember passphrase on this device" checkbox inside [`crate::Overlay::VaultCreate`].
    ///
    /// Emitted by the input router when `Ctrl-R` is pressed while the `VaultCreate` overlay is
    /// active (bypassing the normal text-routing path so the key is not inserted into the
    /// passphrase field). Silently ignored everywhere else.
    ToggleRemember,
    /// Probe the highlighted switcher entry's reachability without opening it into a pane
    /// (RFC-0011 P6 "test connection"). Only meaningful inside [`crate::Overlay::Connections`];
    /// a no-op elsewhere. See [`AppEffect::TestConnection`] for how the probe is dispatched and
    /// [`AppEvent::ConnectionTested`] for how the result comes back.
    TestConnection,
    /// Toggle whether the highlighted switcher entry is pinned to the top of the list (RFC-0011
    /// P6). Only meaningful inside [`crate::Overlay::Connections`]; a no-op elsewhere. Persisted
    /// to `[discovery].pinned` in `cairn.toml` — see [`AppEffect::SetConnectionPinned`].
    PinConnection,
    /// Toggle whether the highlighted switcher entry is hidden from the switcher's default view
    /// (RFC-0011 P6). Only meaningful inside [`crate::Overlay::Connections`]; a no-op elsewhere.
    /// Persisted to `[discovery].hidden` — see [`AppEffect::SetConnectionHidden`]. Toggling a
    /// currently-hidden entry (visible only because "show hidden" is on) un-hides it — hiding is
    /// never a one-way trap.
    HideConnection,
    /// Toggle whether hidden entries are revealed in the connection switcher (RFC-0011 P6).
    /// Ephemeral: resets to `false` every time [`crate::Overlay::Connections`] opens and is never
    /// persisted. Only meaningful inside that overlay; a no-op elsewhere.
    ToggleShowHidden,
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
    /// Close stdin on an active exec session pane (`Ctrl-D` inside `Overlay::ExecPane`).
    /// Handled by the text-routing path; a no-op outside an active session pane.
    CloseStdin,
    /// Move focus to the next field in the connection form (`Tab`). No-op outside the form.
    NextField,
    /// Move focus to the previous field in the connection form (`Shift-Tab`). No-op outside the form.
    PrevField,
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
    /// The vault-unlock effect finished. On success the vault is now unlocked and the reducer flips
    /// all [`NeedsVault`](crate::ChoiceStatus::NeedsVault) entries to
    /// [`NeedsOpen`](crate::ChoiceStatus::NeedsOpen) in the switcher; the connection that triggered
    /// the unlock (held in the `pending_conn` field of `Overlay::VaultUnlock`) is then auto-opened via
    /// [`AppEffect::OpenConnection`]. On failure: a secret-free, retryable message is shown in the
    /// overlay.
    ///
    /// **P2 behavioural change from P1:** previously this event carried `Ok(Vec<ConnectionChoice>)`
    /// and the reducer extended the switcher with the newly-opened connections. In P2, connections
    /// are already in the switcher as `NeedsVault` at enumeration time; the unlock simply makes
    /// them openable, so the payload is now `Ok(())`.
    VaultUnlocked {
        /// `Ok(())` on success, or `Err(message)` to keep the overlay open with a retryable error.
        result: Result<(), String>,
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
    /// A bounded prefix of a file was read and classified as text or binary (`Action::Enter` on a
    /// non-directory entry — see [`AppEffect::SniffFile`]). The reducer opens the read-only pager
    /// ([`crate::Overlay::Pager`]) seeded with `prefetch` for an instant first frame, then emits
    /// [`AppEffect::OpenPager`] to keep streaming past it.
    FileSniffed {
        /// The pane that issued the sniff (from [`AppEffect::SniffFile`]). The reducer opens the
        /// pager only if this pane is still positioned on `path`, so a result arriving after a pane
        /// switch is honored against the requesting pane rather than silently dropped.
        pane: Side,
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The file that was sniffed.
        path: VfsPath,
        /// Text vs binary, from the NUL-byte heuristic ([`crate::detect_file_kind`]).
        kind: FileKind,
        /// The bytes already read during classification — seeded into the pager so the first
        /// frame renders instantly instead of waiting for `AppEffect::OpenPager`'s stream to
        /// start. Never forwarded to the AI layer (raw file bytes).
        prefetch: Bytes,
    },
    /// The [`AppEffect::SniffFile`] read failed (e.g. permission denied, or the file vanished
    /// after the listing was taken). No overlay is opened; the reducer just shows the message.
    SniffFailed {
        /// Secret-free, redacted error message.
        message: String,
    },
    /// A decoded chunk of raw bytes from an open pager stream ([`AppEffect::OpenPager`]). Always
    /// the raw file bytes — text-vs-hex formatting happens in the reducer/render layer, never
    /// here, so a stream can flip mode (F3's Text → Hex on a NUL) after the fact.
    PagerChunk {
        /// Which pager session this belongs to.
        id: PagerId,
        /// The raw bytes read in this chunk.
        bytes: Bytes,
    },
    /// The pager's read stream ended: cleanly (`error: None, truncated: false`), because it hit
    /// [`crate::PAGER_MAX_BYTES`] (`truncated: true`), or with a redacted error.
    PagerDone {
        /// Which pager session ended.
        id: PagerId,
        /// `None` on clean EOF; `Some(redacted_message)` on error.
        error: Option<String>,
        /// Whether the byte cap was hit before EOF.
        truncated: bool,
    },
    /// A decoded chunk of output from an exec session (stdout/stderr combined).
    SessionOutput {
        /// Which session produced this output.
        id: SessionId,
        /// The UTF-8 decoded output text (cross-chunk multibyte sequences are correctly
        /// stitched in the effect runner; incomplete trailing bytes are carried over to
        /// the next chunk).
        text: String,
    },
    /// An exec or port-forward session has ended.
    SessionEnded {
        /// Which session ended.
        id: SessionId,
        /// Process exit code (exec), or `0` for clean port-forward teardown, or `None` if unknown.
        exit_code: Option<i32>,
        /// A redacted (secret-free) error message; `None` on clean exit.
        error: Option<String>,
    },
    /// The async connection-open attempt (emitted by [`AppEffect::OpenConnection`]) has completed.
    ///
    /// On success the backend is now in the [`VfsRegistry`](cairn_vfs::VfsRegistry) and the
    /// reducer flips the choice's status to [`Ready`](crate::ChoiceStatus::Ready) and navigates
    /// the requesting pane into it. On failure the status is set to
    /// [`Unreachable`](crate::ChoiceStatus::Unreachable) and a redacted error appears in the
    /// status line.
    ConnectionOpened {
        /// Which connection was opened (or attempted).
        conn: ConnectionId,
        /// `Ok(())` on success. `Err(message)` on failure — the message is already redacted and
        /// never carries host names, paths, or credential material.
        result: Result<(), String>,
    },
    /// [`AppEffect::TestConnection`] finished (RFC-0011 P6). Unlike [`AppEvent::ConnectionOpened`]
    /// this never touches `pending_conn_open` or navigates a pane — it only updates the choice's
    /// reachability badge and the status line with a result distinct from a real open ("reachable"
    /// / "unreachable: …" rather than "Opened …").
    ConnectionTested {
        /// Which connection was probed.
        conn: ConnectionId,
        /// The probe outcome; the error string is already redacted (never a host, path, or
        /// credential value — the effect runner reuses `OpenError`'s redaction contract).
        result: Result<(), String>,
    },
    /// [`AppEffect::SetConnectionPinned`] finished persisting to `cairn.toml` (RFC-0011 P6).
    ConnectionPinSet {
        /// Which connection's pin state was requested to change.
        conn: ConnectionId,
        /// The state that was requested. Applied to [`crate::ConnectionChoice::pinned`] only when
        /// `result` is `Ok`; on `Err` the display is left unchanged (the write never happened).
        pinned: bool,
        /// `Ok(())` on success; `Err(message)` if the config write failed.
        result: Result<(), String>,
    },
    /// [`AppEffect::SetConnectionHidden`] finished persisting to `cairn.toml` (RFC-0011 P6).
    ConnectionHideSet {
        /// Which connection's hidden state was requested to change.
        conn: ConnectionId,
        /// The state that was requested. Applied to [`crate::ConnectionChoice::hidden`] only when
        /// `result` is `Ok`; on `Err` the display is left unchanged.
        hidden: bool,
        /// `Ok(())` on success; `Err(message)` if the config write failed.
        result: Result<(), String>,
    },
    /// A port-forward session has bound its local TCP port and is ready to accept connections.
    ///
    /// Sent once, immediately after the listener is bound; may arrive before or after the overlay opens.
    PortForwardBound {
        /// Which session is now bound.
        id: SessionId,
        /// The local port that was bound (may differ from the requested port if `0` was used).
        local_port: u16,
    },
    /// A connection profile was successfully saved (created or updated). The reducer updates
    /// `saved_profiles`, patches the in-place switcher choice label (on edit) or notes that the
    /// new profile will appear after restart (on create), and shows a status message.
    ConnectionSaved {
        /// The UUID of the saved profile.
        id: uuid::Uuid,
        /// The `display_name` for the status message.
        display_name: String,
        /// The ready-to-use switcher label, computed by the effect runner using the same
        /// convention the provider uses: `"local: {path}"` for local, `display_name` for others.
        label: String,
        /// `true` when updating an existing profile; `false` when creating a new one.
        is_edit: bool,
        /// The saved profile data (so `saved_profiles` stays in sync).
        profile: crate::forms::ProfileData,
    },
    /// A connection profile was successfully deleted. The reducer removes it from `saved_profiles`
    /// and from the in-memory switcher list (without re-enumerating), then shows a status message.
    ConnectionDeleted {
        /// The UUID of the deleted profile.
        id: uuid::Uuid,
    },
    /// A connection save or delete operation failed. Clears the `connection_saving` flag so the
    /// user can retry (or the form closes). The message is already human-readable and secret-free.
    ConnectionOpFailed {
        /// Secret-free error message to display in the status line.
        message: String,
    },
    /// OS credential source detection (from [`AppEffect::DetectOsSources`]) completed.
    ///
    /// The reducer stores the result in [`AppState::os_sources`](crate::AppState::os_sources)
    /// and, if the connection form's credential method picker is open, updates the cursor to
    /// the recommended default for the current scheme.
    ///
    /// **Security invariant:** carries presence/name information only — never secret bytes.
    OsSourcesDetected {
        /// The detected OS credential source availability.
        os_sources: crate::forms::OsSources,
    },
    /// The [`AppEffect::CreateVault`] task completed.
    ///
    /// On success: the new vault is installed in the broker; the reducer closes the overlay, sets
    /// `vault_unlocked` and `vault_file_exists`, flips every
    /// [`NeedsVault`](crate::ChoiceStatus::NeedsVault) entry to
    /// [`NeedsOpen`](crate::ChoiceStatus::NeedsOpen), and auto-opens the connection that triggered
    /// the create (when `pending_conn` was set in the overlay).
    ///
    /// On failure: the overlay stays open with the inline error (the message is secret-free and
    /// value-free — only a category is shown, never a path or passphrase fragment).
    ///
    /// `already_exists` is `true` when the failure was specifically `VaultError::AlreadyExists`
    /// — the vault was created out-of-band (another instance or terminal) after this session
    /// started. The reducer uses this flag to set `vault_file_exists = true` so subsequent
    /// `Ctrl-U` presses open the unlock overlay instead of looping on "already exists".
    VaultCreated {
        /// `Ok(())` on success; `Err(message)` with a secret-free, value-free error.
        result: Result<(), String>,
        /// `true` when the error is specifically `VaultError::AlreadyExists`; `false` otherwise
        /// and when `result` is `Ok`. Only meaningful when `result` is `Err`.
        already_exists: bool,
    },
    /// The external editor launched by [`AppEffect::SuspendAndEdit`] has finished (RFC-0012 P2).
    ///
    /// The reducer always sets `status` to this message. On success (`error: false`) it also
    /// re-emits the active pane's `List` effect (via the same refresh path as `Action::Refresh`)
    /// since the file's size/mtime may have changed. `status` is already secret-free (a filename
    /// and an exit outcome only — never editor output, which is never captured in P2 since the
    /// editor inherits the real TTY).
    EditFinished {
        /// Human-readable, secret-free status (e.g. `"edited notes.txt"`, or a redacted failure).
        status: String,
        /// Whether the edit did not complete successfully (editor missing, non-zero exit, spawn
        /// failure, or the file being on a non-local backend).
        error: bool,
    },
    /// A [`AppEffect::SuspendAndEdit`] target has no local path (`Vfs::local_path` returned `None`)
    /// but is otherwise editable: the backend advertises `Caps::WRITE`, `stat` succeeded, and the
    /// reported size is within [`crate::REMOTE_EDIT_MAX_BYTES`] (RFC-0012 P3). The runtime has not
    /// touched the terminal yet. The reducer mints a fresh [`RemoteEditId`] and kicks off the
    /// download via [`AppEffect::DownloadForEdit`].
    RemoteEditNeedsDownload {
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The remote file's path.
        path: VfsPath,
        /// The version snapshot taken just before download — the write-back baseline.
        v0: RemoteVersion,
        /// The remote file's size at this snapshot (drives the later zero-length guard).
        size: u64,
        /// The remote file's Unix permissions at this snapshot, if the backend reports them —
        /// restored on the target after a staging-rename write-back (see
        /// [`AppEffect::WriteBack`]'s doc for why that path needs this).
        orig_perms: Option<UnixPerms>,
    },
    /// [`AppEffect::DownloadForEdit`] finished successfully: `temp_path` now holds a private local
    /// copy of the remote file, hashed as `download_hash`. The reducer emits
    /// [`AppEffect::EditRemoteTemp`] to run the editor on it.
    RemoteEditDownloaded {
        /// Correlates back to the runtime's held temp-file resources.
        id: RemoteEditId,
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The remote file's path (the eventual write-back target).
        path: VfsPath,
        /// The local temp file's real OS path.
        temp_path: PathBuf,
        /// The version snapshot taken before download.
        v0: RemoteVersion,
        /// The remote file's size at download time.
        orig_size: u64,
        /// The remote file's Unix permissions at download time, if reported.
        orig_perms: Option<UnixPerms>,
        /// The freshly downloaded temp file's content hash. **Stable for the whole remote-edit
        /// session** — this is the "is there anything at all to write back" baseline, and is
        /// carried forward unchanged through every subsequent event/effect (including any number
        /// of [`crate::WritebackChoice::KeepEditing`] loops); it is never replaced by a
        /// later/"current" hash. See [`AppEffect::EditRemoteTemp`]'s doc for why this distinction
        /// matters.
        download_hash: ContentHash,
    },
    /// The `$EDITOR` launched by [`AppEffect::EditRemoteTemp`] exited and the temp file's content is
    /// byte-identical to the **original download** (`download_hash`) — a true no-op across the
    /// whole session, not just this one editor invocation. Nothing is written back; the runtime
    /// cleans up the temp resources for `id` when this event arrives (see
    /// `crates/cairn/src/app.rs`).
    RemoteEditNoChange {
        /// Correlates back to the runtime's held temp-file resources (cleaned up on this event).
        id: RemoteEditId,
        /// The file's display name, for the status line.
        name: String,
    },
    /// The `$EDITOR` launched by [`AppEffect::EditRemoteTemp`] exited and the temp file's content
    /// no longer matches `download_hash` (the original download) — there is something to write
    /// back, whether or not it changed further *in this particular invocation* (see
    /// [`AppEffect::EditRemoteTemp`]'s doc). The reducer emits [`AppEffect::WriteBack`] in
    /// [`WriteBackMode::CheckThenWrite`](crate::WriteBackMode::CheckThenWrite) to re-`stat` the
    /// remote path and decide whether it is safe to overwrite silently.
    RemoteEditModified {
        /// Correlates back to the runtime's held temp-file resources.
        id: RemoteEditId,
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The remote file's path.
        path: VfsPath,
        /// The local temp file's real OS path.
        temp_path: PathBuf,
        /// The version snapshot taken before download — the write-back baseline.
        v0: RemoteVersion,
        /// The remote file's size at download time.
        orig_size: u64,
        /// The remote file's Unix permissions at download time, if reported.
        orig_perms: Option<UnixPerms>,
        /// The stable original-download hash, carried forward unchanged — see
        /// [`AppEvent::RemoteEditDownloaded`]'s doc.
        download_hash: ContentHash,
        /// The temp file's content hash observed right now, after this edit — becomes the
        /// "unchanged since reopen" reference if the user later chooses
        /// [`crate::WritebackChoice::KeepEditing`] (informational only; the terminal no-op
        /// decision always compares against `download_hash`, never against this value).
        hash: ContentHash,
    },
    /// A remote-edit session (identified by `id`) ended in a terminal failure: the download failed,
    /// the editor could not be launched or exited non-zero, or the write-back upload failed. The
    /// runtime cleans up the held temp resources for `id` when this event arrives. `status` is
    /// already redacted (secret-free).
    RemoteEditFailed {
        /// Correlates back to the runtime's held temp-file resources (cleaned up on this event).
        id: RemoteEditId,
        /// Human-readable, secret-free status.
        status: String,
    },
    /// [`AppEffect::WriteBack`] (in
    /// [`WriteBackMode::CheckThenWrite`](crate::WriteBackMode::CheckThenWrite)) found that it is not
    /// safe to overwrite silently — either the remote version no longer matches `v0` (including the
    /// remote file having been deleted), or the temp file is now zero bytes while the original was
    /// not. The reducer opens [`crate::Overlay::ConfirmWriteback`]; the temp file is **not** cleaned
    /// up (the runtime keeps holding it while the overlay is open).
    WriteBackConflict {
        /// Correlates back to the runtime's held temp-file resources.
        id: RemoteEditId,
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The remote file's path.
        path: VfsPath,
        /// The local temp file's real OS path.
        temp_path: PathBuf,
        /// The version snapshot taken before download — carried so `KeepEditing` can re-check
        /// against the same baseline after a further edit session.
        v0: RemoteVersion,
        /// The remote file's size at download time.
        orig_size: u64,
        /// The remote file's Unix permissions at download time, if reported.
        orig_perms: Option<UnixPerms>,
        /// The stable original-download hash, carried forward unchanged — see
        /// [`AppEvent::RemoteEditDownloaded`]'s doc. `KeepEditing` re-emits
        /// [`AppEffect::EditRemoteTemp`] with this same value, never a value updated at a
        /// conflict — see the "Keep editing" note on that effect.
        download_hash: ContentHash,
        /// The temp file's content hash at the point the conflict was detected.
        hash: ContentHash,
        /// Why this conflict was raised — display-only.
        reason: WritebackConflictReason,
    },
    /// [`AppEffect::WriteBack`] finished successfully (the original path in
    /// [`WriteBackMode::CheckThenWrite`](crate::WriteBackMode::CheckThenWrite)/
    /// [`WriteBackMode::ForceOverwrite`](crate::WriteBackMode::ForceOverwrite), or a fresh sibling
    /// path in [`WriteBackMode::SaveAsSibling`](crate::WriteBackMode::SaveAsSibling)). The runtime
    /// cleans up the held temp resources for `id` when this event arrives; the reducer refreshes
    /// the active pane's listing.
    WriteBackDone {
        /// Correlates back to the runtime's held temp-file resources (cleaned up on this event).
        id: RemoteEditId,
        /// The name written to, for the status line (may differ from the original on `SaveAs`).
        name: String,
    },
    /// [`AppEffect::MountArchive`] finished: the archive was indexed and mounted as a fresh,
    /// ephemeral connection in the registry (RFC-0013). The reducer pushes the pane's pre-mount
    /// `(conn, cwd)` onto its `mount_stack` and navigates it into `root` (mirroring the
    /// `ConnectionOpened` success path) — see [`crate::PaneState::mount_stack`].
    ArchiveMounted {
        /// The pane that requested the mount (from [`AppEffect::MountArchive`]).
        pane: Side,
        /// The freshly-minted connection id for the mounted `ArchiveVfs`.
        conn: ConnectionId,
        /// The directory to navigate the pane to (the archive root in v1).
        root: VfsPath,
    },
    /// [`AppEffect::MountArchive`] failed: the source entry has no local path (a `.zip` on a
    /// remote backend — copy it to a local pane first), the file was not a recognized tar/zip
    /// archive, indexing hit a security cap, or the binary was built without the `archive`
    /// feature. No connection is created; the reducer just shows `message`.
    ArchiveMountFailed {
        /// The pane that requested the mount.
        pane: Side,
        /// Secret-free, redacted error message.
        message: String,
    },
}

/// How [`AppEffect::WriteBack`] should resolve a potential conflict before writing (RFC-0012 P3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteBackMode {
    /// Re-`stat` the remote path, compare against `v0`, and guard against a zero-length write over
    /// a previously non-empty file; only proceed to write if both checks pass. This is always the
    /// *first* attempt for a modified edit.
    CheckThenWrite,
    /// Skip both checks and overwrite the original path directly — the user already confirmed via
    /// [`crate::WritebackChoice::Overwrite`] in [`crate::Overlay::ConfirmWriteback`].
    ForceOverwrite,
    /// Skip both checks and write to a freshly chosen sibling path instead of the original — the
    /// user chose [`crate::WritebackChoice::SaveAs`].
    SaveAsSibling,
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
    /// Read a bounded prefix of a file (~8 KiB) and classify it as text or binary. Emitted by the
    /// reducer on `Action::Enter` over a non-directory entry; the result arrives as
    /// [`AppEvent::FileSniffed`] (or [`AppEvent::SniffFailed`] on a read error).
    SniffFile {
        /// The pane that issued the sniff — carried back on [`AppEvent::FileSniffed`] so a result
        /// that resolves after the user has switched panes is matched against the pane that asked,
        /// not whichever pane happens to be focused when it arrives (mirrors [`AppEvent::Listed`]).
        pane: Side,
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The file to sniff.
        path: VfsPath,
    },
    /// Stream a file's contents into the pager overlay `id`. The runtime calls
    /// `Vfs::open_read(path, None)` and reads in fixed-size chunks, forwarding each as
    /// [`AppEvent::PagerChunk`] and a final [`AppEvent::PagerDone`] on completion or error.
    ///
    /// `skip` is the number of bytes at the start of the file already shown from a sniff's
    /// `prefetch` ([`AppEvent::FileSniffed`]) — `0` when opened directly via `Action::View`,
    /// which has no prefetch. The runner discards `skip` bytes from the fresh stream before
    /// forwarding any `PagerChunk`, so the prefetched window is never shown twice.
    OpenPager {
        /// Session id minted by the reducer.
        id: PagerId,
        /// Connection to read from.
        conn: ConnectionId,
        /// Path of the file to stream.
        path: VfsPath,
        /// Bytes to discard from the start of the stream (already shown via `prefetch`).
        skip: u64,
    },
    /// Cancel the in-flight pager stream with this id. Fired when the pager overlay closes
    /// (`Esc`/`q`), or by the reducer itself when [`crate::PAGER_MAX_BYTES`] is reached and no
    /// more bytes are needed.
    ClosePager {
        /// Session id to cancel.
        id: PagerId,
    },
    /// Start an interactive cooked exec session and open an `Overlay::ExecPane` for it.
    ///
    /// The effect runner calls `Vfs::invoke(path, "exec", ActionCtx::Exec { argv, tty })`, receives
    /// the `ActionOutcome::Session(SessionHandle)`, and spawns relay tasks that feed `SessionOutput`
    /// and `SessionEnded` events into the loop. The `id` is minted by the reducer.
    OpenExecSession {
        /// Session id minted by the reducer.
        id: SessionId,
        /// Connection to exec on.
        conn: ConnectionId,
        /// Path of the container/pod (or workspace node) to exec in.
        path: VfsPath,
        /// Argument vector.
        argv: Vec<String>,
        /// Whether to allocate a PTY. `false` for v1 (cooked mode); `true` is reserved for v2.
        tty: bool,
        /// Display title shown in the overlay border.
        title: String,
    },
    /// Start a port-forward session and open an `Overlay::PortForwardStatus` for it.
    OpenPortForward {
        /// Session id minted by the reducer.
        id: SessionId,
        /// Connection to port-forward on.
        conn: ConnectionId,
        /// Path of the pod/service to forward to.
        path: VfsPath,
        /// Local port (`0` = OS-assigned ephemeral).
        local_port: u16,
        /// Remote port on the pod/service.
        remote_port: u16,
        /// Display title.
        title: String,
    },
    /// Cancel and tear down the session with this id (closes stdin + fires the cancel sender).
    CloseSession {
        /// The session to close.
        id: SessionId,
    },
    /// Send bytes into an exec session's stdin.
    SendSessionInput {
        /// Target session.
        id: SessionId,
        /// The bytes to send (e.g. a line plus `\n`, or `\x04` for Ctrl-D).
        bytes: Vec<u8>,
    },
    /// Open a connection that is not yet mounted in the [`VfsRegistry`](cairn_vfs::VfsRegistry).
    ///
    /// Emitted by the reducer when the user selects a
    /// [`NeedsOpen`](crate::ChoiceStatus::NeedsOpen) entry from the connection switcher, or
    /// automatically after a successful vault unlock for the connection that triggered it. The
    /// runtime looks up the connection descriptor by id in the descriptor side-map, calls the
    /// opener, registers the result, and reports back via
    /// [`AppEvent::ConnectionOpened`].
    ///
    /// The runtime guards against double-open: if the connection is already in the registry the
    /// effect is a no-op that immediately sends `ConnectionOpened { Ok(()) }`.
    OpenConnection {
        /// The connection to open.
        conn: ConnectionId,
    },
    /// Probe a connection's reachability without mounting it into any pane (RFC-0011 P6, "test
    /// connection"). The runner reuses the exact same per-scheme construction
    /// [`AppEffect::OpenConnection`] uses (no new credential handling), but the resulting backend
    /// handle is dropped immediately rather than inserted into the [`VfsRegistry`](cairn_vfs::VfsRegistry)
    /// — a probe never leaves a live connection running. Reports back via
    /// [`AppEvent::ConnectionTested`]. Never emitted for a [`crate::ChoiceStatus::NeedsVault`]
    /// entry — the reducer detects that purely from state and reports "needs unlock" without any
    /// I/O (see `apply_connections_action`'s `TestConnection` arm).
    TestConnection {
        /// The connection to probe.
        conn: ConnectionId,
    },
    /// Persist whether a switcher entry is pinned, to `[discovery].pinned` in `cairn.toml`
    /// (RFC-0011 P6). Reports back via [`AppEvent::ConnectionPinSet`]; the reducer only updates
    /// [`crate::ConnectionChoice::pinned`] once that event confirms the write succeeded.
    SetConnectionPinned {
        /// The connection to (un)pin.
        conn: ConnectionId,
        /// The new pinned state to persist.
        pinned: bool,
    },
    /// Persist whether a switcher entry is hidden, to `[discovery].hidden` in `cairn.toml`
    /// (RFC-0011 P6). Reports back via [`AppEvent::ConnectionHideSet`]; the reducer only updates
    /// [`crate::ConnectionChoice::hidden`] once that event confirms the write succeeded.
    SetConnectionHidden {
        /// The connection to (un)hide.
        conn: ConnectionId,
        /// The new hidden state to persist.
        hidden: bool,
    },
    /// Drop the stdin sender for an exec session, signalling EOF to the remote process without
    /// cancelling the session. The overlay stays open to show remaining output; `SessionEnded`
    /// arrives when the process exits. Unlike `CloseSession`, this does NOT fire the cancel token.
    CloseStdin {
        /// Target session.
        id: SessionId,
    },
    /// Propagate a terminal-window resize to a TTY exec session. No-op in v1 (always `tty: false`);
    /// the variant is present for forward-compatibility so v2 can wire this without an API break.
    ResizeSession {
        /// Target session.
        id: SessionId,
        /// New terminal rows.
        rows: u16,
        /// New terminal columns.
        cols: u16,
    },
    /// Save a connection profile (new or edited). The runtime writes the profile to `cairn.toml`
    /// and calls `register_connections` to rebuild the switcher list, then reports back via
    /// [`AppEvent::ConnectionSaved`].
    SaveConnection {
        /// The profile data to persist.
        profile: crate::forms::ProfileData,
        /// `true` when updating an existing profile; `false` for a new one. Used for the status message.
        is_edit: bool,
    },
    /// Delete a connection profile by its stable UUID. The runtime removes it from `cairn.toml`
    /// and rebuilds the switcher list, then reports back via [`AppEvent::ConnectionDeleted`].
    /// If the profile had a vault credential, `secret_ref` carries the id so the effect runner
    /// can call the broker's `remove` to avoid orphaning vault entries.
    DeleteConnection {
        /// The profile to remove.
        id: uuid::Uuid,
        /// The vault credential id to remove, if any.
        secret_ref: Option<uuid::Uuid>,
    },
    /// Provision a credential in the vault and save the connection profile.
    ///
    /// The effect runner (binary edge) assembles a typed `CredentialSecret` from the
    /// [`CredentialDraft`](crate::forms::CredentialDraft), calls `Broker::store` to add it to the
    /// vault and persist to disk, sets `profile.secret_ref` to the new id, and then saves the
    /// config (same as `SaveConnection`). The `ConnectionSaved` event is returned on success.
    ///
    /// When `draft` is [`KeepExisting`](crate::forms::CredentialDraft::KeepExisting) (edit mode),
    /// no vault operation is performed and the profile is saved with its existing `secret_ref`.
    ///
    /// When `draft` is a deferred-P5 method (e.g. `GcpServiceAccountJson`), the profile is saved
    /// without a vault credential (`secret_ref = None`) and a status note is shown.
    ///
    /// ## Security invariant
    ///
    /// The `CredentialSecret` assembly lives exclusively in `crates/cairn/src/app.rs`
    /// (the binary edge). `cairn-core` emits only the `CredentialDraft` in this effect;
    /// `cairn-vault` is never in `cairn-core`'s dependency graph. The `Debug` of this variant
    /// is safe to log: `SecretString`'s `Debug` always prints `SecretBox<str>([REDACTED])`.
    ProvisionAndSaveConnection {
        /// Profile data (endpoint fields + display name). `secret_ref` will be set by the runner.
        profile: crate::forms::ProfileData,
        /// The credential intent; assembled into a `CredentialSecret` at the binary edge.
        draft: crate::forms::CredentialDraft,
        /// `true` when updating an existing profile, `false` when creating a new one.
        is_edit: bool,
    },
    /// Detect which OS credential sources are present (names / existence only — no secret bytes).
    ///
    /// Emitted once at startup by [`initial_effects`](crate::initial_effects). The effect runner
    /// checks environment variables and file existence (not content) for:
    ///
    /// - SSH: `SSH_AUTH_SOCK` presence.
    /// - AWS: profile section names in `~/.aws/credentials`.
    /// - GCP: `GOOGLE_APPLICATION_CREDENTIALS` or the ADC JSON file existence.
    /// - Azure: `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, or `AZURE_CLIENT_SECRET` presence.
    ///
    /// The result arrives as [`AppEvent::OsSourcesDetected`] and is stored in
    /// [`AppState::os_sources`](crate::AppState::os_sources) for use by the credential picker.
    DetectOsSources,
    /// Create a new vault at the configured path (first-run setup). Argon2id key derivation is
    /// CPU-bound; the effect runner executes this under `spawn_blocking` to keep the render path
    /// responsive. The passphrase is consumed within the blocking task and zeroized on drop; it
    /// is never logged, never printed via `Debug`, and never passed to `cairn-ai`/`cairn-plugin`.
    ///
    /// On success: the new vault is installed into the broker and
    /// [`AppEvent::VaultCreated { Ok(()) }`](AppEvent::VaultCreated) is returned.
    /// On failure (I/O error, already-exists, etc.): a redacted, value-free
    /// [`AppEvent::VaultCreated { Err(…) }`](AppEvent::VaultCreated) keeps the overlay open.
    CreateVault {
        /// The new vault passphrase (zeroized on drop; Debug is redacted via [`SecretString`]).
        passphrase: SecretString,
        /// Whether to persist the passphrase in the OS keychain after successful creation.
        /// Honoured only when the `keychain` feature is enabled; otherwise silently ignored.
        remember: bool,
    },
    /// Suspend the TUI and launch `$VISUAL`/`$EDITOR`/`vi` on `path`, in place, on the real TTY
    /// (RFC-0012 P2 — `Action::Edit` (`F4`), or `Enter` on a `FileKind::Text` sniff result).
    ///
    /// **Not run through the normal effect runner** (`dispatch`, which has no `&mut Terminal` and
    /// runs effects concurrently): this variant is special-cased inline in the `event_loop`'s
    /// effect loop, which owns the terminal and can pause/resume the blocking input reader around
    /// the editor's exclusive TTY ownership. See `docs/adr/0011-terminal-suspend-and-editor-launch.md`.
    ///
    /// **Local backends only in P2** — the runtime resolves `Vfs::local_path(&path)` *before*
    /// touching the terminal; a `None` (remote backend) result in [`AppEvent::EditFinished`] with
    /// a P3 pointer, leaving the TUI completely undisturbed.
    SuspendAndEdit {
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The file to edit.
        path: VfsPath,
    },
    /// Download `path` to a private local temp file so it can be edited via `$EDITOR` (RFC-0012
    /// P3 — the runtime's `Vfs::local_path(&path)` resolved to `None` inside
    /// [`AppEffect::SuspendAndEdit`]'s handler, and the backend/size checks passed).
    ///
    /// Dispatched through the normal effect runner (spawned, never blocking the render loop) —
    /// unlike `SuspendAndEdit` itself, this does not need the terminal. **Not currently
    /// cancellable**: no `CancellationToken` is wired to this effect yet (wiring UI cancellation
    /// for the download/write-back phases is a tracked follow-up — see RFC-0012's deferred list).
    /// The runtime creates the private temp file (0600, `O_EXCL`, non-predictable directory,
    /// preferring `$XDG_RUNTIME_DIR`) synchronously before spawning the download, and keeps it
    /// (keyed by `id`) until the whole remote-edit session reaches a terminal outcome. On success
    /// this reports [`AppEvent::RemoteEditDownloaded`]; on failure,
    /// [`AppEvent::RemoteEditFailed`] (and the temp resources are cleaned up immediately).
    DownloadForEdit {
        /// Minted by the reducer; addresses this session's held temp-file resources and every
        /// subsequent effect/event in the flow.
        id: RemoteEditId,
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The remote file's path.
        path: VfsPath,
        /// The version snapshot taken just before this effect was emitted.
        v0: RemoteVersion,
        /// The remote file's size at this snapshot (threaded through so `RemoteEditDownloaded`'s
        /// `orig_size` doesn't need a redundant re-`stat`; also drives the zero-length guard later).
        size: u64,
        /// The remote file's Unix permissions at this snapshot, if reported — threaded through so
        /// a later staging-rename write-back can restore them (see `WriteBack`'s doc).
        orig_perms: Option<UnixPerms>,
    },
    /// Suspend the TUI and launch `$VISUAL`/`$EDITOR`/`vi` directly on `temp_path` — the RFC-0012 P3
    /// analogue of [`AppEffect::SuspendAndEdit`] once a remote file has already been downloaded to a
    /// local temp copy (no `Vfs::local_path` resolution needed; `temp_path` is already local by
    /// construction). Also re-entered when the user picks
    /// [`crate::WritebackChoice::KeepEditing`] in [`crate::Overlay::ConfirmWriteback`] — note that
    /// doing so does **not** clear or resolve the conflict that led here, it only re-opens the same
    /// temp file; the next exit re-runs the same conflict check from scratch.
    ///
    /// Special-cased inline in the runtime's `event_loop`, exactly like `SuspendAndEdit` (same
    /// exclusive terminal/stdin ownership requirement — see
    /// `docs/adr/0011-terminal-suspend-and-editor-launch.md`). After the editor exits the runtime
    /// re-hashes `temp_path` and compares against **`download_hash`** (the stable, original-download
    /// baseline — see its doc below): a match reports [`AppEvent::RemoteEditNoChange`]; any
    /// difference reports [`AppEvent::RemoteEditModified`] (which the reducer routes into
    /// [`AppEffect::WriteBack`]) — regardless of whether this particular invocation changed
    /// anything further, so a `KeepEditing` pass that exits with the *same* (already-differing-from-
    /// `download_hash`) content still gets written back rather than silently discarded. A launch
    /// failure or non-zero exit reports [`AppEvent::RemoteEditFailed`].
    EditRemoteTemp {
        /// Correlates to the runtime's held temp-file resources.
        id: RemoteEditId,
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The remote file's path (the eventual write-back target).
        path: VfsPath,
        /// The local temp file's real OS path.
        temp_path: PathBuf,
        /// The version snapshot taken before download — the write-back baseline.
        v0: RemoteVersion,
        /// The remote file's size at download time.
        orig_size: u64,
        /// The remote file's Unix permissions at download time, if reported.
        orig_perms: Option<UnixPerms>,
        /// The content hash captured right after the original download. **Stable for the whole
        /// remote-edit session** — never replaced by a later/"current" hash, including across any
        /// number of `KeepEditing` loops. This is the *only* value the post-edit no-op decision
        /// compares against (see the effect's doc above) — comparing against a per-round hash
        /// instead was a real bug (a `KeepEditing` pass that changed nothing *in that round* could
        /// silently discard an earlier, still-unwritten edit; fixed by keeping this baseline fixed).
        download_hash: ContentHash,
        /// The temp file's content hash immediately before *this* edit invocation starts — i.e.
        /// after any prior edit round in this session, not necessarily the original download's.
        /// Informational/for a future "no further changes since reopen" optimization; the
        /// authoritative no-op decision always uses `download_hash`, never this field.
        hash: ContentHash,
    },
    /// Write `temp_path`'s content back to the remote backend (RFC-0012 P3). Dispatched through the
    /// normal effect runner (spawned, never blocking the render loop). **Not currently
    /// cancellable** (see `DownloadForEdit`'s doc) and not subject to the `[transfers] concurrency`
    /// cap — a remote edit's download/write-back always runs as its own single transfer, outside
    /// the bulk-copy queue.
    ///
    /// [`WriteBackMode::CheckThenWrite`] first re-`stat`s `path` and compares against `v0`
    /// (`RemoteVersion::confirmed_equal`) and guards against writing zero bytes over a previously
    /// non-empty file; either guard tripping reports [`AppEvent::WriteBackConflict`] instead of
    /// writing. `ForceOverwrite`/`SaveAsSibling` skip both guards (the user already confirmed via
    /// [`crate::Overlay::ConfirmWriteback`]).
    ///
    /// The actual write uses a staging-then-rename sequence when the backend advertises
    /// `Caps::RENAME` at the target (write to a sibling name, then rename over it — atomic-ish);
    /// otherwise a direct overwrite (documented non-atomic window — Docker/K8s have no rename).
    /// **The staging-rename path creates a brand-new inode**, so afterward it best-effort restores
    /// `orig_perms` onto the target (guarded by `Caps::CHMOD`; a failure here does not fail the
    /// overall write) — without this, a `0600` file would come back as the staged copy's
    /// umask-default mode after one remote edit. The direct-overwrite path needs no such fix-up: it
    /// truncates the existing inode in place, so its mode is untouched.
    WriteBack {
        /// Correlates to the runtime's held temp-file resources; cleaned up on the terminal event
        /// this produces ([`AppEvent::WriteBackDone`]/[`AppEvent::RemoteEditFailed`]) but **not**
        /// on [`AppEvent::WriteBackConflict`] (the flow continues).
        id: RemoteEditId,
        /// The connection the file lives on.
        conn: ConnectionId,
        /// The remote file's path (the write-back target for `CheckThenWrite`/`ForceOverwrite`).
        path: VfsPath,
        /// The local temp file's real OS path (the content being written back).
        temp_path: PathBuf,
        /// The version snapshot taken before download — the write-back baseline.
        v0: RemoteVersion,
        /// The remote file's size at download time (drives the zero-length guard).
        orig_size: u64,
        /// The remote file's Unix permissions at download time, if reported — restored on the
        /// target after a staging-rename write (see this effect's doc).
        orig_perms: Option<UnixPerms>,
        /// The stable original-download hash, forwarded purely so a resulting
        /// [`AppEvent::WriteBackConflict`] can carry it into
        /// [`crate::Overlay::ConfirmWriteback`] for a subsequent `KeepEditing` pass — not consulted
        /// by this effect's own conflict/zero-length guards.
        download_hash: ContentHash,
        /// How to resolve a potential conflict before writing.
        mode: WriteBackMode,
    },
    /// Abandon a remote-edit session: drop the runtime's held temp-file resources for `id` (deleting
    /// the temp file) without writing anything back. Emitted when the user picks
    /// [`crate::WritebackChoice::Discard`] in [`crate::Overlay::ConfirmWriteback`]. Handled
    /// synchronously in the effect runner (no spawn needed — it only drops a value), mirroring
    /// [`AppEffect::CancelTransfer`].
    CancelRemoteEdit {
        /// Which remote-edit session to abandon.
        id: RemoteEditId,
    },
    /// Mount `path` (an entry classified [`crate::FileKind::Archive`] by
    /// [`AppEffect::SniffFile`]) as a read-only archive backend and open it in `pane`
    /// (RFC-0013, `docs/adr/0012-archive-mount-model.md`).
    ///
    /// The runtime resolves `Vfs::local_path(&path)` first (via `spawn_blocking`); `None` (the
    /// source entry is not on a local backend) fails cleanly with
    /// [`AppEvent::ArchiveMountFailed`] and touches nothing else. On `Some(local_path)` it builds
    /// `ArchiveVfs::open`, mints a fresh [`ConnectionId`], registers the result in the
    /// [`VfsRegistry`](cairn_vfs::VfsRegistry), and reports back via
    /// [`AppEvent::ArchiveMounted`]. Gated on the binary's `archive` feature; without it, the
    /// runner reports a clean "archive support not built in" failure rather than failing to
    /// compile.
    MountArchive {
        /// The pane that will be navigated into the mounted archive.
        pane: Side,
        /// The connection the archive file itself lives on.
        conn: ConnectionId,
        /// The archive file's path on `conn`.
        path: VfsPath,
    },
}
