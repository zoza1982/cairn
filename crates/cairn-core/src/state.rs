//! The application state model.

use cairn_ai::Plan;
use cairn_secrets::SecretString;
use cairn_types::SessionId;
use cairn_types::{ConnectionId, Entry, UnixPerms, VfsPath};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use zeroize::Zeroize;

/// A text field whose contents are a **secret** (the vault passphrase).
///
/// It behaves like a tiny line editor (push / backspace), but unlike a plain `String` it never
/// reveals what was typed:
/// - its [`Debug`] impl prints a fixed placeholder, so the passphrase can never leak through a
///   `{:?}` of [`Overlay`]/[`AppState`] (or any `tracing` field that formats them);
/// - rendering must use [`len`](MaskedInput::len) to draw a mask (e.g. `•`) — the characters are
///   never exposed;
/// - the buffer is zeroized when cleared, when the secret is taken, and on drop.
///
/// The only way to read the value out is [`take_secret`](MaskedInput::take_secret), which yields a
/// zeroizing [`SecretString`] and wipes the field.
///
/// Defense-in-depth caveat: the backing `String` can, if a long passphrase outgrows the capacity
/// reserved at construction, reallocate while typing — leaving a freed (un-zeroized) copy of the
/// partial value in the heap until it is overwritten. We reserve capacity up front so realistic
/// passphrases never trigger a growth realloc, and [`take_secret`](MaskedInput::take_secret) clones
/// into an exact-capacity buffer so the `SecretString` conversion never reallocates either. The
/// remaining (very-long-passphrase) realloc window is a known, accepted gap, not an exposed leak.
///
/// [`Clone`] is implemented to return an **empty** field rather than copying the passphrase: `Overlay`
/// and `AppState` derive `Clone`, and a silent second heap copy of a live secret would defeat the
/// zeroize-on-drop design. Cloning a field mid-entry is never intended, so dropping its contents is
/// the safe choice (mirrors why `secrecy`'s `SecretString` does not implement `Clone`).
pub struct MaskedInput {
    buf: String,
}

impl Clone for MaskedInput {
    fn clone(&self) -> Self {
        Self::new()
    }
}

/// Capacity reserved for the passphrase buffer, chosen so realistic passphrases never trigger a
/// growth reallocation (which would leave an un-zeroized partial copy in freed heap).
const MASKED_INPUT_CAPACITY: usize = 128;

impl MaskedInput {
    /// An empty field.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: String::with_capacity(MASKED_INPUT_CAPACITY),
        }
    }

    /// Append a typed character.
    pub fn push(&mut self, c: char) {
        self.buf.push(c);
    }

    /// Remove the last typed character, if any.
    pub fn backspace(&mut self) {
        self.buf.pop();
    }

    /// The number of characters entered — for drawing a mask of that width, never the content.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.chars().count()
    }

    /// Whether nothing has been entered yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Take the entered passphrase as a zeroizing [`SecretString`], leaving the field empty and
    /// wiping the internal buffer.
    #[must_use]
    pub fn take_secret(&mut self) -> SecretString {
        // Clone into an exact-capacity buffer first: `SecretString::from(String)` may `shrink_to_fit`,
        // which reallocates (freeing an un-zeroized copy) when `capacity > len`. A fresh clone has
        // `capacity == len`, so the conversion does not reallocate. We then wipe our own buffer.
        let secret = SecretString::from(self.buf.clone());
        self.buf.zeroize();
        secret
    }

    /// Clear and zeroize the field without producing a secret (e.g. on a failed attempt).
    pub fn clear(&mut self) {
        // `zeroize` on a `String` wipes the bytes and sets the length to 0.
        self.buf.zeroize();
    }
}

impl Default for MaskedInput {
    fn default() -> Self {
        Self::new()
    }
}

// Never reveal the passphrase — not the characters, and not even the length (which a `{:?}` in a log
// has no business knowing). The on-screen mask width comes from `len()`, used only by the renderer.
impl fmt::Debug for MaskedInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MaskedInput(<redacted>)")
    }
}

impl Drop for MaskedInput {
    fn drop(&mut self) {
        self.buf.zeroize();
    }
}

/// A transfer waiting in the queue behind the active one (transfers run one at a time).
///
/// SYNC: mirrors the fields of `AppEffect::Transfer` minus `overwrite` (a dequeued transfer always
/// starts a fresh attempt with `overwrite: false`). Keep in step if that effect gains a field.
#[derive(Debug, Clone)]
pub struct QueuedTransfer {
    /// Source connection.
    pub src_conn: ConnectionId,
    /// Destination connection.
    pub dst_conn: ConnectionId,
    /// The `(source, destination)` pairs to transfer.
    pub items: Vec<(VfsPath, VfsPath)>,
    /// Whether the transfer is a move.
    pub is_move: bool,
}

/// Which pane is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The left pane.
    Left,
    /// The right pane.
    Right,
}

impl Side {
    /// The other side.
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }

    /// Index into the `[PaneState; 2]` array.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }
}

/// How a pane orders its entries. Directories always sort before files regardless of mode; the mode
/// only decides the ordering *within* each group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// Case-insensitive by name, ascending (the default, MC-like).
    #[default]
    Name,
    /// By size, largest first. Entries without a known size (e.g. directories) sort after sized ones.
    Size,
    /// By modification time, newest first. Entries without a known time sort last.
    Modified,
    /// By file type (extension), grouping like with like; entries with no extension sort first.
    Type,
}

impl SortMode {
    /// The next mode in the cycle (`Name → Size → Modified → Type → Name`).
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Name => Self::Size,
            Self::Size => Self::Modified,
            Self::Modified => Self::Type,
            Self::Type => Self::Name,
        }
    }

    /// A short label for the status/header (e.g. `"name"`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
            Self::Modified => "modified",
            Self::Type => "type",
        }
    }
}

/// The state of a directory listing in a pane.
#[derive(Debug, Clone)]
pub enum Listing {
    /// A listing is in flight.
    Loading,
    /// Entries are loaded (shared `Arc` so rendering borrows cheaply).
    Ready(Arc<Vec<Entry>>),
    /// The listing failed; carries a redacted message.
    Error(String),
}

impl Listing {
    /// The loaded entries, or an empty slice if not ready.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        match self {
            Self::Ready(v) => v,
            _ => &[],
        }
    }
}

/// One pane: a location within a backend, plus view state.
#[derive(Debug, Clone)]
pub struct PaneState {
    /// The backend connection this pane browses.
    pub conn: ConnectionId,
    /// The current directory.
    pub cwd: VfsPath,
    /// The current listing.
    pub listing: Listing,
    /// The cursor index into the **visible** (filtered) entries — see [`PaneState::visible`].
    pub cursor: usize,
    /// Marked (multi-selected) entry indices, into the **visible** entries. Cleared whenever the
    /// filter changes, since filtering re-indexes the view.
    pub marked: BTreeSet<usize>,
    /// How entries are ordered within the pane.
    pub sort: SortMode,
    /// Whether hidden entries (e.g. dotfiles) are listed. Passed to the backend as `ListOpts::all`.
    pub show_hidden: bool,
    /// The active name filter (case-insensitive substring), or `None` when not filtering.
    pub filter: Option<String>,
    /// Whether keystrokes are currently editing the filter live (filter-as-you-type). Editing is
    /// per-pane and persists across a pane switch: switching away pauses capture (the other pane is
    /// active), switching back resumes it. Cleared on directory change and when an overlay takes over.
    pub filter_editing: bool,
    /// The stack of connections/directories this pane descended *from* to reach an archive mount
    /// (RFC-0013). A successful [`crate::AppEvent::ArchiveMounted`] pushes the pane's
    /// pre-mount `(conn, cwd)` here before switching into the archive; the reducer's `leave_dir`
    /// pops it when the pane is at the archive's root and the user presses `..`/Leave again, restoring the
    /// origin connection and directory. A `Vec` (not a single slot) so mounting an archive found
    /// *inside* another mounted archive nests correctly. Empty for a pane that has never mounted
    /// an archive.
    pub mount_stack: Vec<MountFrame>,
}

/// One entry in a pane's archive [`PaneState::mount_stack`]: where to return to when the user
/// leaves the mounted archive from its root.
#[derive(Debug, Clone)]
pub struct MountFrame {
    /// The connection the pane was browsing before the mount.
    pub conn: ConnectionId,
    /// The directory within that connection the pane was at before the mount.
    pub cwd: VfsPath,
}

impl PaneState {
    /// Create a pane at `cwd` on `conn`, initially loading.
    #[must_use]
    pub fn new(conn: ConnectionId, cwd: VfsPath) -> Self {
        Self {
            conn,
            cwd,
            listing: Listing::Loading,
            cursor: 0,
            marked: BTreeSet::new(),
            sort: SortMode::default(),
            show_hidden: false,
            filter: None,
            filter_editing: false,
            mount_stack: Vec::new(),
        }
    }

    /// The entries currently visible after applying the active filter (a case-insensitive substring
    /// match on the name). With no filter this is every loaded entry, in order.
    ///
    /// The cursor and marks index into *this* view. Returns borrowed refs into the listing, so it
    /// allocates only the `Vec` of pointers (cheap); large-list virtualization is deferred (M1-9).
    ///
    /// The synthetic `..` entry (when present at position 0 in the listing) is **always** included
    /// at position 0 regardless of any active filter — it must never be hidden by a name search,
    /// matching MC behaviour.
    #[must_use]
    pub fn visible(&self) -> Vec<&Entry> {
        let entries = self.listing.entries();
        // Peel off the synthetic `..` sentinel (position 0 when not at the VFS root).
        let (dot_dot, real) = match entries.first() {
            Some(e) if e.is_dotdot_sentinel() => (Some(e), &entries[1..]),
            _ => (None, entries),
        };
        let filtered: Vec<&Entry> = match &self.filter {
            None => real.iter().collect(),
            Some(f) => {
                let needle = f.to_lowercase();
                real.iter()
                    .filter(|e| e.name.to_lowercase().contains(&needle))
                    .collect()
            }
        };
        if let Some(dd) = dot_dot {
            let mut result = Vec::with_capacity(filtered.len() + 1);
            result.push(dd);
            result.extend(filtered);
            result
        } else {
            filtered
        }
    }

    /// The number of visible (filtered) entries. O(n) and allocates while a filter is active (it goes
    /// through [`PaneState::visible`]); hoist the value if you need it more than once. Optimizing the
    /// filtered scan is folded into the deferred large-list virtualization work (M1-9).
    #[must_use]
    pub fn len(&self) -> usize {
        self.visible().len()
    }

    /// Whether the visible (filtered) listing is empty. Avoids the `visible()` allocation.
    ///
    /// When the synthetic `..` entry is present at position 0 the pane is never empty,
    /// consistent with [`PaneState::visible`] always including `..` at the top.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let entries = self.listing.entries();
        // If `..` is present, there is always at least one visible entry.
        if entries.first().is_some_and(|e| e.is_dotdot_sentinel()) {
            return false;
        }
        match &self.filter {
            None => entries.is_empty(),
            Some(f) => {
                let needle = f.to_lowercase();
                !entries
                    .iter()
                    .any(|e| e.name.to_lowercase().contains(&needle))
            }
        }
    }

    /// The entry under the cursor (within the visible view), if any.
    #[must_use]
    pub fn current(&self) -> Option<&Entry> {
        self.visible().get(self.cursor).copied()
    }

    /// Clamp the cursor into the valid range after the listing changes.
    pub(crate) fn clamp_cursor(&mut self) {
        let n = self.len();
        if n == 0 {
            self.cursor = 0;
        } else if self.cursor >= n {
            self.cursor = n - 1;
        }
    }
}

/// A connection-form field value: either a plain (non-secret) string or a secret held in a
/// [`MaskedInput`].
///
/// Endpoint fields (host, port, bucket, …) are always [`Plain`](FieldValue::Plain) — they are
/// stored in `ProfileData::endpoint` and written to the config file in clear text. Credential
/// fields whose `FieldSpec.secret` flag is `true` are [`Secret`](FieldValue::Secret) — they
/// exist only in this buffer until the form submits, at which point they are moved into a
/// [`CredentialDraft`] and then zeroized by the [`MaskedInput::take_secret`] call.
///
/// ## Clone contract
///
/// Cloning a [`Secret`](FieldValue::Secret) variant returns an **empty** [`MaskedInput`] to
/// prevent silent duplication of live key material (mirrors [`MaskedInput`]'s own clone contract
/// and `secrecy`'s design rationale for not implementing `Clone` on `Secret<S>` in older versions).
/// Cloning a [`Plain`](FieldValue::Plain) variant clones the string normally.
///
/// ## Debug contract
///
/// [`Secret`](FieldValue::Secret) variants always print `Secret(<redacted>)`.
/// [`Plain`](FieldValue::Plain) variants print the value (endpoint data is not secret).
///
/// [`CredentialDraft`]: crate::forms::CredentialDraft
pub enum FieldValue {
    /// A plain text value (non-secret endpoint fields).
    Plain(String),
    /// A secret value held in a masked, zeroizing buffer.
    Secret(MaskedInput),
}

impl Default for FieldValue {
    fn default() -> Self {
        Self::Plain(String::new())
    }
}

impl Clone for FieldValue {
    fn clone(&self) -> Self {
        match self {
            Self::Plain(s) => Self::Plain(s.clone()),
            // Never silently copy a live secret — return an empty field instead.
            // This mirrors MaskedInput's own Clone contract.
            Self::Secret(_) => Self::Secret(MaskedInput::new()),
        }
    }
}

impl fmt::Debug for FieldValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(s) => write!(f, "Plain({s:?})"),
            Self::Secret(_) => f.write_str("Secret(<redacted>)"),
        }
    }
}

impl FieldValue {
    /// The plain text value for non-secret fields, or `""` for secret fields (use the
    /// [`Secret`](FieldValue::Secret) variant's [`MaskedInput`] directly when rendering).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Plain(s) => s.as_str(),
            Self::Secret(_) => "",
        }
    }

    /// Append a character to the field value.
    pub fn push_char(&mut self, c: char) {
        match self {
            Self::Plain(s) => s.push(c),
            Self::Secret(m) => m.push(c),
        }
    }

    /// Remove the last character from the field value.
    pub fn backspace(&mut self) {
        match self {
            Self::Plain(s) => {
                s.pop();
            }
            Self::Secret(m) => m.backspace(),
        }
    }

    /// Whether the field is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Plain(s) => s.is_empty(),
            Self::Secret(m) => m.is_empty(),
        }
    }

    /// The trimmed plain text, or `""` for secret fields.
    #[must_use]
    pub fn as_str_trimmed(&self) -> &str {
        match self {
            Self::Plain(s) => s.trim(),
            Self::Secret(_) => "",
        }
    }

    /// Length of the value (character count), for masking secret fields in the renderer.
    ///
    /// For [`Secret`](FieldValue::Secret) this is the only safe way to know how many bullets
    /// to draw. For [`Plain`](FieldValue::Plain) it returns the character count of the string.
    #[must_use]
    pub fn display_len(&self) -> usize {
        match self {
            Self::Plain(s) => s.chars().count(),
            Self::Secret(m) => m.len(),
        }
    }
}

/// The stages of the connection form overlay (P5 adds the credential-picker stages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionFormStage {
    /// Step 1: choose a backend scheme from the list (cursor-based, not capturing text).
    SchemePicker,
    /// Step 2: fill in the endpoint fields for the chosen scheme (captures text as `TextEdit`).
    Fields,
    /// Step 3 (P5): choose an authentication method from the list (cursor-based, not capturing
    /// text). Skipped for schemes that need no credentials (e.g. `"local"`).
    CredentialMethodPicker,
    /// Step 4 (P5): fill in credential fields for the chosen method (captures text as `TextEdit`).
    /// Skipped for delegation methods (no fields required) and deferred methods.
    CredentialFields,
}

/// A credential-provisioning task deferred until the vault is unlocked or created.
///
/// Carried in `pending_save` by [`Overlay::VaultUnlock`] and [`Overlay::VaultCreate`]. After a
/// successful vault unlock or creation, the reducer emits
/// [`AppEffect::ProvisionAndSaveConnection`](crate::AppEffect::ProvisionAndSaveConnection) using
/// the data here — so the user does not need to re-fill the connection form.
///
/// `Clone` is implemented because vault overlays derive `Clone` (as part of `Overlay`), and the
/// nested `CredentialDraft` is cloneable (its `SecretString` fields are cloned, not moved).
#[derive(Debug, Clone)]
pub struct PendingSave {
    /// The profile to save once the vault is available.
    pub profile: crate::forms::ProfileData,
    /// The credential to provision into the vault.
    pub draft: crate::forms::CredentialDraft,
    /// Whether this is an update to an existing profile.
    pub is_edit: bool,
}

/// A modal overlay awaiting user input.
#[derive(Debug, Clone)]
pub enum Overlay {
    /// Confirm deletion of the listed paths on a connection.
    ConfirmDelete {
        /// The connection the paths live on.
        conn: ConnectionId,
        /// The paths to delete.
        paths: Vec<VfsPath>,
    },
    /// Review an AI-proposed plan before executing it (plan → confirm).
    AiPlan {
        /// The proposed plan; its steps carry per-step approval state.
        plan: Plan,
        /// The step the review cursor is on (for step-through approval).
        cursor: usize,
    },
    /// Pick a connection to open in the active pane (the choices live in [`AppState::connections`]).
    Connections {
        /// The selection cursor — an index into the **currently visible** subset of
        /// [`AppState::connections`] (see [`visible_connection_indices`]), not into the raw list
        /// directly. A hidden entry only participates in this indexing while `show_hidden` is
        /// `true`.
        cursor: usize,
        /// Whether hidden entries (RFC-0011 P6) are currently revealed in this switcher session.
        /// Always starts `false` when the overlay opens; never persisted — this is purely a
        /// this-session view toggle, distinct from the persisted `hidden` flag on each choice.
        show_hidden: bool,
    },
    /// A single-line text entry (new-directory name, rename, …).
    Prompt {
        /// What submitting the text will do.
        kind: PromptKind,
        /// The text entered so far.
        input: String,
    },
    /// View the transfer queue (the active transfer plus pending ones); navigate with the cursor,
    /// drop the selected pending transfer, or clear them all.
    TransferQueue {
        /// Selection cursor into the pending queue.
        cursor: usize,
    },
    /// Confirm overwriting existing destinations before a copy/move proceeds. Holds the parameters to
    /// re-issue the transfer with overwrite enabled.
    ConfirmOverwrite {
        /// Source connection.
        src_conn: ConnectionId,
        /// Destination connection.
        dst_conn: ConnectionId,
        /// The `(source, destination)` pairs to transfer.
        items: Vec<(VfsPath, VfsPath)>,
        /// Whether the transfer is a move.
        is_move: bool,
        /// How many destinations already exist.
        conflicts: usize,
    },
    /// Unlock the secrets vault by entering its passphrase (M3-7). The typed passphrase is held in a
    /// [`MaskedInput`] (no-echo, never logged, wiped on drop); submitting it emits
    /// [`AppEffect::UnlockVault`](crate::AppEffect::UnlockVault). A failed attempt (wrong passphrase /
    /// missing vault) keeps the overlay open with [`error`](Overlay::VaultUnlock::error) set.
    ///
    /// `pending_conn` (added in Phase P2) records which connection triggered this overlay: when the
    /// user selects a [`NeedsVault`](crate::ChoiceStatus::NeedsVault) entry, that connection's id is
    /// stored here so the runtime can auto-open it immediately after a successful unlock. `None`
    /// when the user explicitly pressed `Ctrl-U` rather than selecting a locked connection.
    ///
    /// `pending_save` (added in Phase P5) carries a credential + profile that need to be stored
    /// and saved once the vault is unlocked. Set when the user submits the credential form while
    /// the vault is locked; the reducer emits `ProvisionAndSaveConnection` after a successful unlock.
    VaultUnlock {
        /// The passphrase entered so far — masked on screen, redacted in `Debug`, zeroized on drop.
        input: MaskedInput,
        /// A retryable error from the last attempt, shown in the overlay; `None` before the first try.
        error: Option<String>,
        /// The connection that triggered this unlock, if any (Phase P2+).
        pending_conn: Option<ConnectionId>,
        /// A deferred credential-provisioning task to complete after vault unlock (Phase P5).
        /// Boxed to keep the `Overlay` enum variant size reasonable (CredentialDraft is larger).
        pending_save: Option<Box<PendingSave>>,
    },
    /// Confirm running a user-defined shell action before it executes (security gate — shell actions
    /// run a local program). Holds what's needed to dispatch on confirm.
    ConfirmShellAction {
        /// Index into the validated shell-action list.
        index: usize,
        /// The action's name, for display.
        name: String,
        /// Connection the target lives on.
        conn: ConnectionId,
        /// The entry the action will run against.
        target: VfsPath,
    },
    /// Live log stream from a container/pod node (M6-7 Stream half).
    ///
    /// `lines` is a ring buffer of decoded UTF-8 lines; `partial` holds a line not yet terminated by
    /// `'\n'`. `byte_size` tracks total retained bytes so the cap check avoids summing `lines` each
    /// chunk. `follow` auto-scrolls to the most-recent line; any scroll-up disables it. `scroll` is
    /// the index of the topmost visible line when `!follow`. `status` drives the "following…" /
    /// "paused" / "ended" / "error" indicator.
    LogViewer {
        /// Session id — matches `AppEvent::LogChunk` and `AppEvent::LogStreamEnded`.
        id: LogViewerId,
        /// Display title (e.g. `"nginx — logs"`).
        title: String,
        /// Accumulated complete lines, oldest-first.
        lines: VecDeque<String>,
        /// Incomplete last line (no trailing `'\n'` yet).
        partial: String,
        /// Total byte size of `lines` + `partial` (for the byte-cap check).
        byte_size: usize,
        /// When `true`, the render always scrolls to the most recent line.
        follow: bool,
        /// Index of the first visible line when `!follow`.
        scroll: usize,
        /// Current stream status.
        status: LogViewerStatus,
    },
    /// A read-only pager over a file's contents (`F3` / `Enter` on a non-directory entry —
    /// RFC-0012 P1). Modeled closely on [`Overlay::LogViewer`]: a capped, `byte_size`-tracked
    /// buffer fed by a streaming effect, except the pager never evicts from the front — a static
    /// file shows its *first* N bytes up to the cap, not a live tail's most recent N (see
    /// [`PagerStatus::Truncated`]).
    ///
    /// `lines` holds one stored entry per display row regardless of mode: complete
    /// `\n`-terminated text lines in [`PagerMode::Text`] (decoded lossily, like the log viewer),
    /// or raw [`PAGER_HEX_ROW_BYTES`]-byte rows (preserved exactly via a lossless byte↔char
    /// mapping, so a stray byte can never corrupt the hex dump the way lossy UTF-8 decoding
    /// would) in [`PagerMode::Hex`]. `partial` holds the not-yet-flushed tail of the current
    /// line/row; it is flushed into `lines` when the stream ends so the last (possibly
    /// incomplete) line/row is always visible without special-casing it in the renderer.
    ///
    /// `total_size` is the entry's size from the directory listing, if known — used only to
    /// render a percentage position; the pager works fine without it (backends that don't report
    /// size, or a size unknown at listing time).
    Pager {
        /// Session id — matches `AppEvent::PagerChunk`/`AppEvent::PagerDone`.
        id: PagerId,
        /// Display title (filename + a short suffix, e.g. `"README.md — view"`).
        title: String,
        /// Text or hex display mode.
        mode: PagerMode,
        /// Accumulated complete lines/rows, oldest-first (see the mode-dependent contract above).
        lines: VecDeque<String>,
        /// Incomplete last line/row (no trailing `'\n'` yet in `Text` mode; fewer than
        /// [`PAGER_HEX_ROW_BYTES`] bytes in `Hex` mode). Flushed into `lines` on stream end.
        partial: String,
        /// Total bytes represented by `lines` + `partial` (for the [`PAGER_MAX_BYTES`] cap check).
        byte_size: usize,
        /// The entry's size from the directory listing, if the backend reports one.
        total_size: Option<u64>,
        /// Index of the first visible line/row.
        scroll: usize,
        /// Current stream status.
        status: PagerStatus,
        /// Whether `Text`-mode content soft-wraps to the pane width (`Hex` rows never wrap).
        /// No keybinding toggles this in P1 (`// v2`: a toggle action is future work).
        wrap: bool,
    },
    /// An interactive cooked-mode exec session (v1: line-oriented, no TTY emulation, `tty: false`).
    ///
    /// Renders `SessionRecord.output_lines` as scrollable text (like [`Overlay::LogViewer`]) plus a
    /// single-line input field at the bottom. Key input is routed here while this overlay is active,
    /// bypassing the file-manager keymap. `Enter` submits the line. `Ctrl-D` (`TextEdit::CloseStdin`)
    /// closes stdin. `Ctrl-]` (mapped to `Action::Cancel` before the text-capture path) detaches
    /// without killing the remote process.
    ExecPane {
        /// Session id — output and state live in [`AppState::sessions`].
        id: SessionId,
        /// The text the user is currently typing; submitted on `Enter`.
        input: String,
        /// Index of the topmost visible output line.
        scroll: usize,
        /// When `true`, auto-scrolls to the most recent output line.
        follow: bool,
    },
    /// A read-only status overlay for a port-forward session.
    ///
    /// Shows the title, local address (`127.0.0.1:<port>` once bound, otherwise "binding…"), and the
    /// ended state. `Esc` or `q` (`Action::Cancel`) closes the overlay AND fires `CloseSession` to
    /// tear down the forward.
    PortForwardStatus {
        /// Session id — state lives in [`AppState::sessions`].
        id: SessionId,
    },
    /// Create a new vault (first-run setup).
    ///
    /// Presents two masked passphrase fields (`passphrase` + `confirm`), an OS-keychain
    /// "remember" toggle, and an inline error. The raw passphrase bytes live only in the two
    /// [`MaskedInput`] buffers — redacted in `Debug`, zeroized on drop — until
    /// [`AppEffect::CreateVault`](crate::AppEffect::CreateVault) carries the taken secret to
    /// the effect runner. The confirm field is zeroized immediately after the comparison.
    ///
    /// `capturing_text()` returns `true` while this overlay is open, routing keystrokes to
    /// whichever field has `focus`. `creating` suppresses duplicate submissions while the
    /// Argon2id `spawn_blocking` task is running.
    ///
    /// `pending_conn` carries the connection that triggered this overlay via a
    /// [`NeedsVault`](ChoiceStatus::NeedsVault) selection, so it can be auto-opened after
    /// the vault is created and unlocked. `None` when the user pressed `Ctrl-U` explicitly.
    ///
    /// `pending_save` (added in Phase P5) carries a deferred credential-provisioning task,
    /// parallel to `Overlay::VaultUnlock::pending_save`.
    VaultCreate {
        /// The new passphrase being typed (focus 0).
        passphrase: MaskedInput,
        /// The passphrase confirmation to match (focus 1). Zeroized after the comparison.
        confirm: MaskedInput,
        /// Which field has input focus: 0 = passphrase field, 1 = confirm field.
        focus: u8,
        /// Whether to store the passphrase in the OS keychain after successful vault creation.
        remember: bool,
        /// In-place validation error (mismatch, too-short, etc.). Cleared on each new `Insert`.
        error: Option<String>,
        /// Set while the Argon2id `spawn_blocking` task is running; suppresses double-submit.
        ///
        /// Unlike `AppState::vault_unlocking` (not yet tracked), this lives inside the overlay
        /// because it is only meaningful while VaultCreate is open — there is no global
        /// "vault-creation in progress" state that the rest of the reducer needs to observe.
        creating: bool,
        /// The connection that triggered this overlay (auto-opened after vault creation).
        pending_conn: Option<ConnectionId>,
        /// A deferred credential-provisioning task to complete after vault creation (Phase P5).
        pending_save: Option<Box<PendingSave>>,
    },
    /// Confirm deletion of a saved connection profile. The user sees the profile name and must
    /// press `[Enter]` to confirm or `[Esc]`/`[q]` to cancel. Destructive: removes the entry from
    /// the config file.
    ConfirmDeleteConnection {
        /// The stable UUID of the profile to delete.
        id: uuid::Uuid,
        /// Display name shown in the confirmation prompt.
        display_name: String,
    },
    /// The add-/edit-connection form (Phases P4 + P5 of RFC-0011).
    ///
    /// The form progresses through up to four stages:
    ///
    /// 1. [`SchemePicker`](ConnectionFormStage::SchemePicker) — choose a backend type.
    /// 2. [`Fields`](ConnectionFormStage::Fields) — fill in endpoint parameters.
    /// 3. [`CredentialMethodPicker`](ConnectionFormStage::CredentialMethodPicker) — choose an
    ///    authentication method (P5; skipped for `"local"` and other no-credential schemes).
    /// 4. [`CredentialFields`](ConnectionFormStage::CredentialFields) — fill in credential
    ///    fields for the chosen method (P5; skipped for delegation and deferred methods).
    ///
    /// `editing_id` is `None` for a new connection and `Some(id)` when editing an existing profile.
    /// `existing_secret_ref` carries the `secret_ref` from the live profile so it is not silently
    /// dropped when the user edits and re-saves a connection that already has credentials.
    ConnectionForm {
        /// Current stage.
        stage: ConnectionFormStage,
        /// The chosen scheme id (e.g. `"ssh"`), populated when advancing past `SchemePicker`.
        scheme: String,
        /// Live endpoint field values keyed by [`crate::forms::FieldSpec::key`] (plain strings).
        values: HashMap<String, String>,
        /// Which endpoint field (index into the scheme's field list) currently has focus.
        focus: usize,
        /// Per-endpoint-field validation errors, shown inline. Cleared when the field is edited.
        field_errors: HashMap<String, String>,
        /// `None` = new connection; `Some(id)` = editing an existing profile.
        editing_id: Option<uuid::Uuid>,
        /// The existing `secret_ref` from the profile being edited, preserved on save. `None`
        /// for new connections.
        existing_secret_ref: Option<uuid::Uuid>,

        // ── P5 credential stage ──────────────────────────────────────────────────────────
        /// Cursor position in the credential method picker (index into
        /// `credential_methods(scheme, editing_id.is_some())`).
        cred_method_cursor: usize,
        /// The chosen credential method; `None` until the user selects one in the picker.
        cred_method: Option<crate::forms::CredentialMethod>,
        /// Live credential field values, keyed by [`crate::forms::FieldSpec::key`].
        /// Plain fields are [`FieldValue::Plain`]; masked fields are [`FieldValue::Secret`].
        cred_fields: HashMap<String, FieldValue>,
        /// Which credential field currently has focus (index into the method's field list).
        cred_focus: usize,
    },
    /// Confirm what to do with a remote edit whose write-back cannot proceed silently (RFC-0012
    /// P3): either the remote file changed underneath the editor (re-`stat` disagrees with the
    /// snapshot taken before download — [`WritebackConflictReason::RemoteChanged`]), or the edited
    /// temp file is now zero bytes while the original was not
    /// ([`WritebackConflictReason::ZeroLengthGuard`], a crashed-editor/truncated-save guard).
    ///
    /// Offers four choices (see [`WritebackChoice`]), cursor-selected like [`Overlay::Connections`]:
    /// overwrite the original anyway, save to a fresh sibling path instead, resume editing the same
    /// temp file, or discard the edit entirely. The temp file is **not** deleted while this overlay
    /// is open — see `crates/cairn/src/app.rs`'s `remote_edit_temps` map, which owns the RAII
    /// cleanup and is only dropped when the flow reaches a terminal outcome.
    ConfirmWriteback {
        /// Correlates this overlay's choice back to the runtime's held temp-file resources.
        id: RemoteEditId,
        /// The connection the remote file lives on.
        conn: ConnectionId,
        /// The remote file's path (the original write-back target).
        path: VfsPath,
        /// The local temp file's real OS path (still present on disk).
        temp_path: PathBuf,
        /// The remote version observed just before the file was downloaded for editing — the
        /// baseline `confirmed_equal` compares against on every write-back attempt, including a
        /// subsequent one after `KeepEditing` is chosen here.
        v0: RemoteVersion,
        /// The remote file's size at download time (drives the zero-length guard).
        orig_size: u64,
        /// The remote file's Unix permissions at download time, if reported — restored on the
        /// target after a staging-rename write-back (see `crate::AppEffect::WriteBack`'s doc).
        orig_perms: Option<UnixPerms>,
        /// The content hash captured right after the original download. **Stable for the whole
        /// session** — carried forward unchanged into a subsequent `EditRemoteTemp` if
        /// `KeepEditing` is chosen; this is the only value the no-op-edit decision ever compares
        /// against (see `crate::AppEffect::EditRemoteTemp`'s doc for why that distinction matters).
        download_hash: ContentHash,
        /// The most recently observed content hash of the temp file (the pre-round baseline for a
        /// further edit session if `KeepEditing` is chosen — informational only, not used for the
        /// no-op decision).
        hash: ContentHash,
        /// Why this overlay opened — display-only, doesn't change the four choices offered.
        reason: WritebackConflictReason,
        /// Selection cursor into [`WritebackChoice::ALL`].
        cursor: usize,
    },
    /// The `F1` keybinding reference: a scrollable, read-only list of sections (`crate::HELP_SECTIONS`).
    /// Modeled on [`Overlay::Pager`]'s scroll semantics, but over static content rather than a
    /// streamed file, so there is no session id and no effect fires when it closes.
    Help {
        /// Index of the topmost visible display row (a row is either a section header or one
        /// keybinding entry — see [`crate::help_line_count`]).
        scroll: usize,
    },
    /// The `F9` action menu: a categorized, cursor-selectable list of actions (`crate::MENU_SECTIONS`).
    /// `Enter` closes the overlay and dispatches the selected entry's [`crate::Action`] through the
    /// normal action-handling path (`apply_menu_action` in `update.rs`) rather than duplicating each
    /// action's logic here. `Esc` closes without acting.
    Menu {
        /// Index into the flattened selectable entries (see [`crate::menu_entries`]), skipping
        /// the category headers, which are not selectable.
        cursor: usize,
    },
}

/// A stable identifier for an in-flight remote-edit session (RFC-0012 P3): download → edit →
/// conflict-check → write-back. Minted monotonically by the reducer (like [`TransferId`]/
/// [`PagerId`]) the moment a remote edit is confirmed underway (`AppEvent::RemoteEditNeedsDownload`
/// handler), and threaded through every subsequent effect/event so the runtime's held temp-file
/// resources (never visible to the reducer) can be found and cleaned up by id.
pub type RemoteEditId = u64;

/// The largest remote file RFC-0012 P3 will download-and-edit. Editing a multi-GB file by
/// round-tripping it through `$EDITOR` is the wrong workflow regardless of available disk/memory —
/// this is a deliberate UX guardrail, not a technical ceiling. Enforced by the runtime (which holds
/// the `Entry.size` needed to check it) before any download begins.
pub const REMOTE_EDIT_MAX_BYTES: u64 = 100 * 1024 * 1024; // 100 MiB

/// A SHA-256 digest of a temp file's content, used purely as a local before/after diff to detect a
/// no-op edit (the editor was opened and closed without changing the bytes) — never a cross-system
/// integrity check, so the choice of hash function has no interop constraint.
pub type ContentHash = [u8; 32];

/// A backend's reported "version" of a remote object at a point in time, used to detect whether it
/// changed underneath an in-progress edit (RFC-0012 P3).
///
/// Built once from the [`Entry`] returned by `stat` just before download (`v0`, the baseline) and
/// again immediately before write-back (`v1`); [`confirmed_equal`](Self::confirmed_equal) decides
/// whether it is safe to overwrite silently. Per-backend signal availability (documented in
/// `docs/rfcs/0012-file-open-view-edit.md`): S3/GCS/Azure report an `etag`; SFTP/local report
/// `modified`+`size` but no `etag`; **Docker/K8s currently report neither** (their `Entry` never
/// populates `modified`), so they degrade to `Unknown` today — moot in practice since neither
/// advertises `Caps::WRITE` yet, so remote-edit write-back is refused before `RemoteVersion` is
/// ever consulted for them; if/when they gain write support, they will always prompt on conflict
/// until `modified` is plumbed through. Anything reporting neither `etag` nor `modified`+`size`
/// degrades to `Unknown`, which is deliberately **never** treated as a match — an unverifiable
/// version must always prompt rather than risk a silent clobber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteVersion {
    /// The backend reports a content/version tag (object stores). Compared for exact equality.
    ETag(String),
    /// The backend reports modification time and size but no tag (SFTP, local, container/pod
    /// filesystems). Compared as a pair — either differing is a change.
    MTimeSize {
        /// Last-modified time reported by the backend.
        modified: SystemTime,
        /// Size in bytes reported by the backend.
        size: u64,
    },
    /// Neither an `etag` nor both `modified`+`size` were available (or the entry vanished — see
    /// callers that map a `NotFound` re-`stat` to this variant). Never `confirmed_equal` to
    /// anything, including another `Unknown` — an unverifiable version can never be "confirmed
    /// unchanged".
    Unknown,
}

impl RemoteVersion {
    /// Build a version snapshot from a `stat` result: prefers `etag` (exact, cheap to compare),
    /// falls back to `modified`+`size` when both are present, else [`RemoteVersion::Unknown`].
    #[must_use]
    pub fn from_entry(entry: &Entry) -> Self {
        if let Some(etag) = &entry.etag {
            return Self::ETag(etag.to_string());
        }
        if let (Some(modified), Some(size)) = (entry.modified, entry.size) {
            return Self::MTimeSize { modified, size };
        }
        Self::Unknown
    }

    /// Whether `self` and `other` are confirmed to describe the same object state.
    ///
    /// `false` whenever either side is [`RemoteVersion::Unknown`] (including two `Unknown`s
    /// compared against each other) or the two sides are different variants (a backend should never
    /// switch signal kind for the same path mid-session, but a mismatch is treated the same as "not
    /// confirmed" rather than assumed benign) — an unconfirmed version must always prompt rather
    /// than risk silently overwriting a concurrent change.
    #[must_use]
    pub fn confirmed_equal(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ETag(a), Self::ETag(b)) => a == b,
            (
                Self::MTimeSize {
                    modified: m1,
                    size: s1,
                },
                Self::MTimeSize {
                    modified: m2,
                    size: s2,
                },
            ) => m1 == m2 && s1 == s2,
            _ => false,
        }
    }
}

/// The four resolutions offered by [`Overlay::ConfirmWriteback`] when a remote edit's write-back
/// cannot proceed silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritebackChoice {
    /// Write the edited content over the original path anyway (accepting the loss of whatever
    /// changed remotely, or the truncation, since the last known-good snapshot).
    Overwrite,
    /// Write the edited content to a fresh sibling path instead, leaving the original untouched.
    SaveAs,
    /// Dismiss the overlay and resume editing the same temp file (e.g. to reconcile changes by
    /// hand before deciding) — re-runs `$EDITOR` on the same local copy.
    KeepEditing,
    /// Abandon the edit: delete the temp file, write nothing back.
    Discard,
}

impl WritebackChoice {
    /// Every choice, in the order the overlay lists and cursor-selects them.
    pub const ALL: [Self; 4] = [
        Self::Overwrite,
        Self::SaveAs,
        Self::KeepEditing,
        Self::Discard,
    ];

    /// A short label for the overlay's choice list.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Overwrite => "Overwrite",
            Self::SaveAs => "Save as a new file",
            Self::KeepEditing => "Keep editing",
            Self::Discard => "Discard my changes",
        }
    }
}

/// Why [`Overlay::ConfirmWriteback`] opened — display-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritebackConflictReason {
    /// The remote file's version at write-back time no longer [`RemoteVersion::confirmed_equal`]s
    /// the baseline observed before download (including the remote path having been deleted).
    RemoteChanged,
    /// The temp file is now zero bytes but the original remote file was not — most likely a crashed
    /// or misbehaving editor, not a deliberate "empty this file" edit.
    ZeroLengthGuard,
}

impl WritebackConflictReason {
    /// A short, user-facing explanation shown in the overlay title/status.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::RemoteChanged => "the remote file changed since you started editing",
            Self::ZeroLengthGuard => "the edited file is now empty, but the original was not",
        }
    }
}

/// What submitting a [`Overlay::Prompt`] text field will do.
#[derive(Debug, Clone)]
pub enum PromptKind {
    /// Create a new directory (the entered name) in the active pane's current directory.
    MakeDir,
    /// Rename an existing entry (at `from`) to the entered name, within the same directory.
    Rename {
        /// The path being renamed.
        from: VfsPath,
    },
    /// Enter a freeform natural-language request for the AI assistant.
    AiPrompt,
}

impl PromptKind {
    /// A short title for the prompt box.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::MakeDir => "New directory",
            Self::Rename { .. } => "Rename",
            Self::AiPrompt => "Ask the assistant",
        }
    }

    /// Whether the entered text is a single filename component (so `/` and control chars are rejected
    /// as you type). Freeform prompts accept arbitrary text.
    ///
    /// Maintainer note: keep this in sync with [`PromptKind`] — a new filename-style variant must be
    /// added here explicitly (the `matches!` will not warn if the list is incomplete).
    #[must_use]
    pub fn is_filename(&self) -> bool {
        matches!(self, Self::MakeDir | Self::Rename { .. })
    }
}

/// The auto-discovery mechanism that surfaced a connection in the switcher.
///
/// Used in [`ChoiceProvenance::Discovered`] to distinguish Docker sockets from kubeconfig
/// contexts. Additional sources (in-cluster Kubernetes, Podman, …) will extend this enum
/// in Phase P3 of RFC-0011.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverySource {
    /// Docker daemon socket or compatible Podman socket.
    Docker,
    /// A Kubernetes kubeconfig context.
    Kubeconfig,
}

/// How a [`ConnectionChoice`] entered the switcher.
///
/// Populated by the `ConnectionCoordinator` at enumeration time. The renderer uses this
/// for icons or section headers (Phase P3+). The default is `Builtin` so the built-in
/// local roots require no extra annotation at their construction sites.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ChoiceProvenance {
    /// A built-in local root (`/`, `$HOME`). Not user-configurable.
    #[default]
    Builtin,
    /// A user-saved connection profile from `cairn.toml`.
    Saved,
    /// Auto-discovered from the environment (Docker socket, kubeconfig context, …).
    Discovered {
        /// The mechanism that found this entry.
        source: DiscoverySource,
    },
}

/// Reachability status of a switcher entry.
///
/// `Ready` is the default (eagerly-mounted at startup in Phase P1). Later phases change
/// this: `NeedsOpen` enables lazy-open on select (P2); `NeedsVault` routes the selection
/// through the vault-unlock overlay instead of erroring; `Unreachable` is display-only.
///
/// The reducer uses this field to route a selection purely (no I/O in the reducer):
/// - `Ready` → call `navigate_to_conn` immediately (today's path).
/// - `NeedsOpen` → emit `AppEffect::OpenConnection { conn }` (P2).
/// - `NeedsVault` → open the vault-unlock overlay (P2+).
/// - `Unreachable` → show a status message (P2+).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ChoiceStatus {
    /// The backend is mounted and immediately navigable.
    #[default]
    Ready,
    /// The backend is known but not yet opened; it opens on selection (Phase P2).
    NeedsOpen,
    /// Opening requires vault unlock first; selecting emits the unlock overlay (Phase P2+).
    NeedsVault,
    /// The backend is unreachable (probe failed or timed out). Display-only (Phase P3+).
    Unreachable,
}

/// Whether a switcher entry is user-editable.
///
/// `Profile` entries correspond to a specific saved profile UUID and will be editable via the
/// connection form (Phase P4). `AutoDiscovered` entries — built-in roots and future
/// discovered connections — are display-only; the user cannot edit or remove them via the UI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionKind {
    /// A user-saved profile with the given stable UUID from `cairn.toml`.
    Profile {
        /// The profile's stable UUID (`ConnectionProfile::id` from `cairn-config`).
        id: uuid::Uuid,
    },
    /// Built-in or auto-discovered; not directly editable by the user.
    #[default]
    AutoDiscovered,
}

/// A selectable connection for the switcher: a registered backend plus a human-readable label.
///
/// Additive fields (`provenance`, `status`, `kind`) were introduced in Phase P0/P1 of RFC-0011.
/// Their defaults preserve the original behavior: `Builtin`/`Ready`/`AutoDiscovered` are the
/// right values for an eager-mounted local root, so existing construction sites can use
/// `..Default::default()` to fill in the new fields without changing semantics.
#[derive(Debug, Clone)]
pub struct ConnectionChoice {
    /// The backend connection to switch the pane to.
    pub conn: ConnectionId,
    /// Display label (e.g. `"local: /home/me"` or a profile's name).
    pub label: String,
    /// How this entry entered the switcher (for icons/badges in Phase P3+).
    pub provenance: ChoiceProvenance,
    /// Current reachability status (drives selection routing in Phase P2+).
    pub status: ChoiceStatus,
    /// Whether the entry is user-editable (gates edit/delete actions in Phase P4+).
    pub kind: ConnectionKind,
    /// Whether this entry is pinned to the top of the switcher (RFC-0011 P6).
    ///
    /// Populated by the `ConnectionCoordinator` from `[discovery].pinned` at enumeration time;
    /// toggled at runtime by [`crate::Action::PinConnection`]. Pinned entries are floated to the
    /// front of [`AppState::connections`] (both at enumeration and after a toggle) — display-only
    /// otherwise, it does not change routing.
    pub pinned: bool,
    /// Whether this entry is hidden from the switcher's default view (RFC-0011 P6).
    ///
    /// Populated by the `ConnectionCoordinator` from `[discovery].hidden`; toggled by
    /// [`crate::Action::HideConnection`]. A hidden entry still appears in
    /// [`AppState::connections`] — only [`Overlay::Connections`]'s *rendering* filters it out by
    /// default (see [`visible_connection_indices`]) — so toggling "show hidden"
    /// ([`crate::Action::ToggleShowHidden`]) can reveal it again to be un-hidden. There is
    /// deliberately no one-way trap: hiding an entry never removes the only path back to it.
    pub hidden: bool,
}

impl Default for ConnectionChoice {
    /// Returns a sentinel choice (`conn = 0`, empty label, `Builtin`/`Ready`/`AutoDiscovered`, not
    /// pinned or hidden).
    ///
    /// Primarily used in tests via `..Default::default()` to fill in the additive RFC-0011
    /// fields without having to spell them out at every existing construction site. The sentinel
    /// `ConnectionId(0)` is never assigned to a real connection.
    fn default() -> Self {
        Self {
            conn: ConnectionId(0),
            label: String::new(),
            provenance: ChoiceProvenance::Builtin,
            status: ChoiceStatus::Ready,
            kind: ConnectionKind::AutoDiscovered,
            pinned: false,
            hidden: false,
        }
    }
}

/// The indices into `connections` that the switcher should currently display, given whether
/// hidden entries are being revealed ([`Overlay::Connections::show_hidden`]).
///
/// Shared by the reducer (cursor navigation and selection in `apply_connections_action`) and the
/// renderer (the list of rows actually drawn) so the two can never disagree about which
/// `connections` entry a given cursor position refers to — the classic "renderer and reducer
/// each filter independently and drift apart" bug class.
#[must_use]
pub fn visible_connection_indices(
    connections: &[ConnectionChoice],
    show_hidden: bool,
) -> Vec<usize> {
    connections
        .iter()
        .enumerate()
        .filter(|(_, c)| show_hidden || !c.hidden)
        .map(|(i, _)| i)
        .collect()
}

/// The whole application state. Holds plain data only — no service handles, no I/O.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The two panes (`[Left, Right]`).
    pub panes: [PaneState; 2],
    /// Which pane is focused.
    pub focus: Side,
    /// A modal overlay, if one is open (captures input).
    pub overlay: Option<Overlay>,
    /// Set when the user has asked to quit.
    pub should_quit: bool,
    /// A transient status/notification line.
    pub status: Option<String>,
    /// Whether an AI plan request is in flight (suppresses duplicate requests).
    pub ai_pending: bool,
    /// Whether an approved AI plan is currently executing (so `Esc` can cancel it).
    pub ai_executing: bool,
    /// Connections the switcher can open in a pane (populated from config at startup).
    pub connections: Vec<ConnectionChoice>,
    /// The in-flight transfers (up to [`concurrency_limit`](AppState::concurrency_limit)). Each carries
    /// its own progress and pause state, addressed by a stable [`TransferId`]. A `TransferDone`/
    /// `TransferConflict` for an id removes that entry. The list is tiny (≤ a few), so it is scanned
    /// linearly rather than indexed by a map.
    pub active_transfers: Vec<ActiveTransfer>,
    /// Monotonic id source for [`active_transfers`](AppState::active_transfers). Minted by the pure
    /// reducer (no clock/RNG); starts at 1 so 0 is an obvious sentinel.
    pub next_transfer_id: TransferId,
    /// How many transfers may run at once. From config at startup; `1` reproduces strict FIFO.
    /// INVARIANT: must be `>= 1` — `0` would queue every transfer and never drain. Clamp at the
    /// config-load site when wiring this.
    pub concurrency_limit: usize,
    /// Transfers waiting for a free slot. A copy/move issued when all slots are busy is enqueued here
    /// and started (FIFO) when a slot frees.
    pub transfer_queue: VecDeque<QueuedTransfer>,
    /// User-defined shell actions, by index (populated from config at startup, like `connections`).
    /// The index aligns with the keymap's `Action::RunShellAction(idx)` and the runtime's action list.
    pub shell_actions: Vec<ShellActionMeta>,
    /// Whether the secrets vault is currently unlocked. Set by the runtime at startup (always `false`
    /// — the broker starts locked) and flipped to `true` on a successful unlock. Plain status only;
    /// no secret material. Used to gate the unlock overlay (no point re-unlocking).
    pub vault_unlocked: bool,
    /// Whether an unlock attempt is in flight (the async `Vault::open` is running). Set on submit and
    /// cleared when the result arrives; it suppresses a duplicate submit and drives the "Unlocking…"
    /// indicator. No secret material.
    pub vault_unlocking: bool,
    /// Whether one or more connections appear as [`NeedsVault`](ChoiceStatus::NeedsVault) in the
    /// switcher (vault is locked, credentials unavailable). Drives the startup status hint that
    /// points the user at the unlock flow; cleared once the vault is unlocked.
    pub has_locked_connections: bool,
    /// Whether the vault file already exists on disk. Set at startup by the runtime (a single
    /// blocking `Path::exists()` call before the event loop starts) and flipped to `true` on a
    /// successful [`AppEvent::VaultCreated`](crate::AppEvent::VaultCreated). The reducer uses
    /// this to branch `Ctrl-U` and `NeedsVault` selections between the create and unlock flows
    /// without ever doing I/O in the pure update function.
    pub vault_file_exists: bool,
    /// Per-side slot tracking which connection is awaiting an async open (Phase P2).
    ///
    /// Indexed by [`Side::index()`] — `[Left, Right]`. Set when the user selects a
    /// [`NeedsOpen`](ChoiceStatus::NeedsOpen), [`NeedsVault`](ChoiceStatus::NeedsVault), or
    /// [`Unreachable`](ChoiceStatus::Unreachable) entry; cleared when
    /// [`AppEvent::ConnectionOpened`](crate::AppEvent::ConnectionOpened) arrives for that conn.
    /// Using per-side slots allows simultaneous in-flight opens on both panes without the
    /// single-slot aliasing bug where a slow Left open could navigate Right after the user moved on.
    pub pending_conn_open: [Option<ConnectionId>; 2],
    /// Monotonic id counter for log-viewer sessions (like `next_transfer_id`). Starts at 1.
    pub next_log_viewer_id: LogViewerId,
    /// Monotonic id counter for pager sessions (like `next_log_viewer_id`). Starts at 1.
    pub next_pager_id: PagerId,
    /// Monotonic id counter for remote-edit sessions (RFC-0012 P3, like `next_pager_id`). Starts at
    /// 1; minted the moment a remote edit is confirmed underway
    /// (`AppEvent::RemoteEditNeedsDownload`'s handler).
    pub next_remote_edit_id: RemoteEditId,
    /// Active exec and port-forward sessions, keyed by stable [`SessionId`].
    ///
    /// A record is inserted when `OpenExecSession`/`OpenPortForward` is emitted by the reducer and
    /// removed when `CloseSession` is emitted (or cleaned up on `SessionEnded` when the overlay is
    /// already closed). The overlay (`ExecPane`/`PortForwardStatus`) stores only the id; the renderer
    /// and effect runner look up the record here.
    pub sessions: HashMap<SessionId, SessionRecord>,
    /// Monotonic id counter for session records (like `next_transfer_id`). Starts at 1; 0 is sentinel.
    pub next_session_id: SessionId,
    /// Whether a `SaveConnection`/`DeleteConnection` effect is currently in flight (suppresses
    /// duplicate submits from rapid key presses and drives a "Saving…" status hint).
    pub connection_saving: bool,
    /// All saved connection profiles, keyed by their stable UUID. Populated from config at startup
    /// and kept in sync by `ConnectionSaved`/`ConnectionDeleted` events. The connection form reads
    /// this to pre-populate fields when editing an existing profile.
    pub saved_profiles: HashMap<uuid::Uuid, crate::forms::ProfileData>,
    /// Detected OS credential sources, populated by [`AppEffect::DetectOsSources`](crate::AppEffect::DetectOsSources)
    /// at startup and stored in [`AppEvent::OsSourcesDetected`](crate::AppEvent::OsSourcesDetected).
    /// Used by the credential method picker to default to the best available method per scheme.
    ///
    /// **Security invariant:** contains presence/name information only — never secret bytes.
    pub os_sources: crate::forms::OsSources,
}

/// A stable identifier for an in-flight transfer, used to address progress/done events and the
/// runtime's per-transfer control (cancel token + pause sender). Minted monotonically by the reducer.
pub type TransferId = u64;

/// A stable identifier for a log-viewer session.
pub type LogViewerId = u64;

/// Max lines the log viewer keeps in memory.
pub const LOG_VIEWER_MAX_LINES: usize = 100_000;
/// Max decoded bytes the log viewer keeps in memory (~4 MiB).
pub const LOG_VIEWER_MAX_BYTES: usize = 4 * 1024 * 1024;

/// A stable identifier for a pager session (`F3` / `Enter`-to-view — RFC-0012 P1).
pub type PagerId = u64;

/// Max bytes the pager retains before it stops streaming and marks the view
/// [`Truncated`](PagerStatus::Truncated). Unlike the log viewer's live-tail ring buffer (which
/// evicts the *oldest* lines to keep the most recent ones), the pager never evicts: once the cap
/// is hit it simply stops accepting more bytes and shows "truncated — showing first N", which is
/// the correct behaviour for viewing a static file from the start (not a live tail).
pub const PAGER_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Bytes per row in [`PagerMode::Hex`] display (`offset | hex | ascii`). Shared between the
/// reducer's row assembly (`crate::update::append_pager_hex`) and the renderer's row decoding so
/// the two can never drift apart.
pub const PAGER_HEX_ROW_BYTES: usize = 16;

/// Text vs binary vs archive classification of a file, produced by [`detect_file_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// No NUL byte found in the sampled prefix — treated as text.
    Text,
    /// A NUL byte was found in the sampled prefix — treated as binary.
    Binary,
    /// The sample's magic bytes matched a recognized archive container (RFC-0013). Carries the
    /// detected format so the reducer/status line can be specific about what was mounted.
    Archive(ArchiveFormat),
}

/// An archive container format recognized by [`detect_file_kind`]'s magic-byte sniff.
///
/// Deliberately duplicated (rather than depended on) from `cairn-backend-archive`'s own internal
/// detection: `cairn-core` has no backend dependencies (every backend is wired at the binary edge,
/// `crates/cairn/src/app.rs`), so the pure sniff-for-routing check here and the authoritative
/// check `ArchiveVfs::open` performs when actually mounting are two small, independent
/// implementations of the same two-constant rule — see `docs/rfcs/0013-archive-backend.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// A zip archive (`PK\x03\x04` at offset 0).
    Zip,
    /// An uncompressed POSIX tar archive (`ustar` at byte offset 257).
    Tar,
}

/// Classify a byte sample as archive, text, or binary.
///
/// Archive detection runs first and is by **magic bytes only, never a file extension/name** — a
/// `.txt` file that happens to be a zip is browsed as an archive, and a `.zip` that is not a real
/// archive falls through to the text/binary check like any other file:
/// - zip: `PK\x03\x04` at offset 0 (the local file header signature).
/// - tar: the POSIX `ustar` magic at byte offset 257 (within the ~8 KiB sniff prefix the effect
///   runner reads, comfortably covering the fixed 512-byte tar header).
///
/// Otherwise falls back to the existing NUL-byte heuristic: any `0x00` byte means binary.
/// Legitimate text (UTF-8, Latin-1, ASCII, …) never legally contains a NUL byte, while binary
/// formats (images, executables) commonly do within their first few KiB — the same heuristic
/// `file(1)`, git, and most pagers use to decide binary handling.
///
/// Pure and I/O-free: the effect runner reads the bounded sample
/// ([`crate::AppEffect::SniffFile`]); this function only inspects the bytes already in hand.
#[must_use]
pub fn detect_file_kind(sample: &[u8]) -> FileKind {
    if sample.len() >= 4 && &sample[0..4] == b"PK\x03\x04" {
        return FileKind::Archive(ArchiveFormat::Zip);
    }
    if sample.len() >= 262 && &sample[257..262] == b"ustar" {
        return FileKind::Archive(ArchiveFormat::Tar);
    }
    if sample.contains(&0) {
        FileKind::Binary
    } else {
        FileKind::Text
    }
}

/// Which representation the pager overlay is currently rendering in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerMode {
    /// Decoded (lossy UTF-8) text, one stored line per `\n`.
    Text,
    /// Raw bytes stored [`PAGER_HEX_ROW_BYTES`] at a time, preserved exactly via a lossless
    /// byte↔char mapping. The render layer formats each stored row into an
    /// `offset | hex | ascii` display line — kept out of the reducer so `Overlay::Pager::lines`
    /// stays the same shape (a plain `String` per entry) in both modes.
    Hex,
}

/// Status of an [`Overlay::Pager`] stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PagerStatus {
    /// Still reading from the backend.
    Loading,
    /// The file was read to completion within [`PAGER_MAX_BYTES`].
    Ready,
    /// [`PAGER_MAX_BYTES`] was reached before EOF; only the first N bytes are shown.
    Truncated,
    /// The stream ended with a (redacted, non-secret) error.
    Error(String),
}

/// The recorded end-state of a session (exec or port-forward).
#[derive(Debug, Clone)]
pub struct SessionEnd {
    /// Process exit code for an exec session (`None` for port-forward teardown or unknown).
    pub exit_code: Option<i32>,
    /// A redacted (secret-free) error message if the session ended abnormally.
    pub error: Option<String>,
}

/// Runtime state for a single exec or port-forward session, keyed by [`SessionId`] in
/// [`AppState::sessions`]. The overlay holds only the id; the renderer fetches the record by id.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    /// The VFS path the session was opened on.
    pub path: cairn_types::VfsPath,
    /// Human-readable title, e.g. `"exec sh /web-1/app"` or `"pf 127.0.0.1:5432→5432"`.
    pub title: String,
    /// Accumulated output lines (stdout/stderr, lossily decoded), oldest-first.
    pub output_lines: VecDeque<String>,
    /// Incomplete last line (not yet newline-terminated).
    pub output_partial: String,
    /// Total retained bytes (for the byte-cap check, avoids summing `output_lines` each chunk).
    pub output_byte_size: usize,
    /// The bound local TCP port for port-forward sessions; `None` until bound (or for exec).
    pub local_port: Option<u16>,
    /// Set when the session has ended.
    pub ended: Option<SessionEnd>,
}

/// Max lines the session output ring buffer keeps in memory (same policy as the log viewer).
pub const SESSION_OUTPUT_MAX_LINES: usize = 100_000;
/// Max decoded bytes the session output keeps in memory (~4 MiB, same policy as the log viewer).
pub const SESSION_OUTPUT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Status of an [`Overlay::LogViewer`] stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogViewerStatus {
    /// Stream is live.
    Streaming,
    /// Stream ended cleanly.
    Done,
    /// Stream ended with a (redacted, non-secret) error.
    Error(String),
}

/// One in-flight transfer the reducer tracks for display and control. The full source/destination
/// detail lives runtime-side; this is the secret-free view the UI renders.
#[derive(Debug, Clone)]
pub struct ActiveTransfer {
    /// Stable identity (never 0).
    pub id: TransferId,
    /// Human-readable label, e.g. "Copying 3 item(s)…".
    pub label: String,
    /// Cumulative bytes transferred so far.
    pub bytes: u64,
    /// Average throughput (bytes/sec); `None` until the first progress update.
    pub rate: Option<u64>,
    /// Total bytes to transfer (from a pre-scan), if known — enables the percentage/ETA display.
    pub total: Option<u64>,
    /// Whether the user has paused this transfer.
    pub paused: bool,
}

/// The reducer's view of a shell action: just what it needs to validate and gate the run. The full
/// definition (command, args) lives runtime-side so the pure core never holds executable details.
#[derive(Debug, Clone)]
pub struct ShellActionMeta {
    /// Display name, shown in the confirm prompt and status line.
    pub name: String,
    /// Whether to confirm before running (mirrors the config `confirm` field).
    pub confirm: bool,
}

impl AppState {
    /// Create state with both panes on the given connections at the given directory (both Loading).
    #[must_use]
    pub fn new(left: ConnectionId, right: ConnectionId, cwd: VfsPath) -> Self {
        Self {
            panes: [
                PaneState::new(left, cwd.clone()),
                PaneState::new(right, cwd),
            ],
            focus: Side::Left,
            overlay: None,
            should_quit: false,
            status: None,
            ai_pending: false,
            ai_executing: false,
            connections: Vec::new(),
            active_transfers: Vec::new(),
            next_transfer_id: 1,
            concurrency_limit: 1,
            transfer_queue: VecDeque::new(),
            shell_actions: Vec::new(),
            vault_unlocked: false,
            vault_unlocking: false,
            has_locked_connections: false,
            vault_file_exists: false,
            pending_conn_open: [None; 2],
            next_log_viewer_id: 1,
            next_pager_id: 1,
            next_remote_edit_id: 1,
            sessions: HashMap::new(),
            next_session_id: SessionId(1),
            connection_saving: false,
            saved_profiles: HashMap::new(),
            os_sources: crate::forms::OsSources::default(),
        }
    }

    /// Whether any transfer is currently running.
    #[must_use]
    pub fn has_active_transfer(&self) -> bool {
        !self.active_transfers.is_empty()
    }

    /// The active pane.
    #[must_use]
    pub fn active(&self) -> &PaneState {
        &self.panes[self.focus.index()]
    }

    /// The active pane, mutably.
    pub fn active_mut(&mut self) -> &mut PaneState {
        &mut self.panes[self.focus.index()]
    }

    /// A pane by side.
    #[must_use]
    pub fn pane(&self, side: Side) -> &PaneState {
        &self.panes[side.index()]
    }

    /// A pane by side, mutably.
    pub fn pane_mut(&mut self, side: Side) -> &mut PaneState {
        &mut self.panes[side.index()]
    }

    /// Whether keystrokes should be routed to a text field rather than resolved as actions — either a
    /// text-entry overlay is open, or (with no overlay) the active pane is editing its filter live.
    ///
    /// Filter editing yields to *any* open overlay: a non-prompt overlay (e.g. an AI plan that
    /// arrived asynchronously while the user was filtering) owns input, so its keys aren't swallowed.
    #[must_use]
    pub fn capturing_text(&self) -> bool {
        match &self.overlay {
            // Text prompts, vault unlock/create fields, and exec panes capture keystrokes as
            // `Msg::Text`. VaultCreate always captures: both passphrase and confirm are text
            // fields, and `Ctrl-R` is the only bypass (routed to `Action::ToggleRemember` in
            // `map_input` before `capturing_text()` is consulted).
            Some(
                Overlay::Prompt { .. }
                | Overlay::VaultUnlock { .. }
                | Overlay::VaultCreate { .. }
                | Overlay::ExecPane { .. },
            ) => true,
            // The connection form captures text only while the user is filling in endpoint or
            // credential fields. The scheme picker and method picker are cursor-based.
            Some(Overlay::ConnectionForm {
                stage: ConnectionFormStage::Fields | ConnectionFormStage::CredentialFields,
                ..
            }) => true,
            Some(_) => false,
            None => self.active().filter_editing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_secrets::ExposeSecret;

    #[test]
    fn masked_input_edits_and_reports_length_only() {
        let mut m = MaskedInput::new();
        assert!(m.is_empty());
        for c in "s3cr3t".chars() {
            m.push(c);
        }
        assert_eq!(m.len(), 6);
        m.backspace();
        assert_eq!(m.len(), 5);
        assert!(!m.is_empty());
    }

    #[test]
    fn masked_input_debug_never_reveals_the_secret() {
        let mut m = MaskedInput::new();
        for c in "hunter2".chars() {
            m.push(c);
        }
        let dbg = format!("{m:?}");
        assert!(
            !dbg.contains("hunter2"),
            "Debug leaked the passphrase: {dbg}"
        );
        // Embedding it in an overlay (and thus AppState) must not leak it either.
        let overlay = Overlay::VaultUnlock {
            input: m,
            error: None,
            pending_conn: None,
            pending_save: None,
        };
        assert!(!format!("{overlay:?}").contains("hunter2"));
    }

    #[test]
    fn take_secret_yields_the_value_and_empties_the_field() {
        let mut m = MaskedInput::new();
        for c in "open-sesame".chars() {
            m.push(c);
        }
        let secret = m.take_secret();
        assert_eq!(secret.expose_secret(), "open-sesame");
        assert!(m.is_empty(), "the field is wiped after taking the secret");
    }

    #[test]
    fn detect_file_kind_treats_plain_text_as_text() {
        assert_eq!(
            detect_file_kind(b"hello, world\nsecond line\n"),
            FileKind::Text
        );
    }

    #[test]
    fn detect_file_kind_treats_nul_byte_as_binary() {
        // A PNG-like header with an embedded NUL.
        assert_eq!(
            detect_file_kind(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"),
            FileKind::Binary
        );
    }

    #[test]
    fn detect_file_kind_empty_sample_is_text() {
        assert_eq!(detect_file_kind(b""), FileKind::Text);
    }

    #[test]
    fn detect_file_kind_nul_anywhere_in_sample_is_binary() {
        // A NUL byte deep in the sample (not just at the front) must still be caught.
        let mut sample = vec![b'a'; 100];
        sample.push(0);
        sample.extend_from_slice(b"more text after the nul");
        assert_eq!(detect_file_kind(&sample), FileKind::Binary);
    }

    #[test]
    fn detect_file_kind_recognizes_zip_magic_at_offset_zero() {
        let mut sample = b"PK\x03\x04".to_vec();
        sample.extend_from_slice(&[0u8; 100]); // zip local headers are themselves binary
        assert_eq!(
            detect_file_kind(&sample),
            FileKind::Archive(ArchiveFormat::Zip)
        );
    }

    #[test]
    fn detect_file_kind_recognizes_tar_ustar_magic_at_offset_257() {
        let mut sample = vec![0u8; 262];
        sample[257..262].copy_from_slice(b"ustar");
        assert_eq!(
            detect_file_kind(&sample),
            FileKind::Archive(ArchiveFormat::Tar)
        );
    }

    #[test]
    fn detect_file_kind_is_by_magic_bytes_not_extension() {
        // No `.zip`/`.tar` name is ever inspected here — a `Text` file (no NUL, no archive magic)
        // must classify as `Text` regardless of whatever a caller might have named it.
        assert_eq!(detect_file_kind(b"just some text"), FileKind::Text);
        // Conversely a genuine zip magic is detected even though the sample carries no filename at
        // all — the function only ever sees bytes.
        assert_eq!(
            detect_file_kind(b"PK\x03\x04rest-does-not-matter"),
            FileKind::Archive(ArchiveFormat::Zip)
        );
    }

    #[test]
    fn detect_file_kind_short_sample_does_not_false_positive_on_archive() {
        // Shorter than the tar magic's offset+len: must not panic (slice indexing) or misclassify.
        assert_eq!(detect_file_kind(b"PK"), FileKind::Text);
        assert_eq!(detect_file_kind(&[0u8; 10]), FileKind::Binary);
    }

    // --- RemoteVersion (RFC-0012 P3) ---

    fn entry_with_etag(tag: &str) -> Entry {
        let mut e = Entry::new("f", cairn_types::EntryKind::File);
        e.etag = Some(tag.into());
        e
    }

    fn entry_with_mtime_size(secs: u64, size: u64) -> Entry {
        let mut e = Entry::new("f", cairn_types::EntryKind::File);
        e.modified = Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs));
        e.size = Some(size);
        e
    }

    #[test]
    fn remote_version_prefers_etag_over_mtime_size() {
        let mut e = entry_with_etag("abc123");
        e.modified = Some(SystemTime::UNIX_EPOCH);
        e.size = Some(10);
        assert_eq!(
            RemoteVersion::from_entry(&e),
            RemoteVersion::ETag("abc123".to_owned())
        );
    }

    #[test]
    fn remote_version_falls_back_to_mtime_size() {
        let e = entry_with_mtime_size(1000, 42);
        assert_eq!(
            RemoteVersion::from_entry(&e),
            RemoteVersion::MTimeSize {
                modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000),
                size: 42
            }
        );
    }

    #[test]
    fn remote_version_unknown_when_neither_signal_present() {
        let e = Entry::new("f", cairn_types::EntryKind::File);
        assert_eq!(RemoteVersion::from_entry(&e), RemoteVersion::Unknown);
        // Only one of modified/size present is still Unknown — both are required for the pair signal.
        let mut e2 = Entry::new("f", cairn_types::EntryKind::File);
        e2.size = Some(5);
        assert_eq!(RemoteVersion::from_entry(&e2), RemoteVersion::Unknown);
    }

    #[test]
    fn remote_version_etag_equality() {
        let a = RemoteVersion::ETag("v1".to_owned());
        let b = RemoteVersion::ETag("v1".to_owned());
        let c = RemoteVersion::ETag("v2".to_owned());
        assert!(a.confirmed_equal(&b));
        assert!(!a.confirmed_equal(&c));
    }

    #[test]
    fn remote_version_mtime_size_equality() {
        let m = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(500);
        let a = RemoteVersion::MTimeSize {
            modified: m,
            size: 10,
        };
        let b = RemoteVersion::MTimeSize {
            modified: m,
            size: 10,
        };
        let different_size = RemoteVersion::MTimeSize {
            modified: m,
            size: 11,
        };
        let different_time = RemoteVersion::MTimeSize {
            modified: m + std::time::Duration::from_secs(1),
            size: 10,
        };
        assert!(a.confirmed_equal(&b));
        assert!(!a.confirmed_equal(&different_size));
        assert!(!a.confirmed_equal(&different_time));
    }

    #[test]
    fn remote_version_unknown_is_never_confirmed_equal() {
        // Not even to itself — an unverifiable version can never be treated as "unchanged".
        assert!(!RemoteVersion::Unknown.confirmed_equal(&RemoteVersion::Unknown));
        let known = RemoteVersion::ETag("v1".to_owned());
        assert!(!RemoteVersion::Unknown.confirmed_equal(&known));
        assert!(!known.confirmed_equal(&RemoteVersion::Unknown));
    }

    #[test]
    fn remote_version_mismatched_variants_are_never_confirmed_equal() {
        let etag = RemoteVersion::ETag("v1".to_owned());
        let mtime = RemoteVersion::MTimeSize {
            modified: SystemTime::UNIX_EPOCH,
            size: 1,
        };
        assert!(!etag.confirmed_equal(&mtime));
        assert!(!mtime.confirmed_equal(&etag));
    }

    #[test]
    fn writeback_choice_labels_and_order() {
        assert_eq!(WritebackChoice::ALL.len(), 4);
        assert_eq!(WritebackChoice::ALL[0], WritebackChoice::Overwrite);
        assert_eq!(WritebackChoice::ALL[3], WritebackChoice::Discard);
        for c in WritebackChoice::ALL {
            assert!(!c.label().is_empty());
        }
    }
}
