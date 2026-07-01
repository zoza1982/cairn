//! The application state model.

use cairn_ai::Plan;
use cairn_secrets::SecretString;
use cairn_types::SessionId;
use cairn_types::{ConnectionId, Entry, VfsPath};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
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

/// The two stages of the connection form overlay.
///
/// `SchemePicker` shows a scrollable list of known backends; once the user selects one the form
/// advances to `Fields` where they fill in the scheme-specific endpoint parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionFormStage {
    /// Step 1: choose a backend scheme from the list.
    SchemePicker,
    /// Step 2: fill in the endpoint fields for the chosen scheme.
    Fields,
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
        /// The selection cursor into [`AppState::connections`].
        cursor: usize,
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
    VaultUnlock {
        /// The passphrase entered so far — masked on screen, redacted in `Debug`, zeroized on drop.
        input: MaskedInput,
        /// A retryable error from the last attempt, shown in the overlay; `None` before the first try.
        error: Option<String>,
        /// The connection that triggered this unlock, if any (Phase P2+).
        pending_conn: Option<ConnectionId>,
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
        creating: bool,
        /// The connection that triggered this overlay (auto-opened after vault creation).
        pending_conn: Option<ConnectionId>,
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
    /// The add-/edit-connection form (Phase P4 of RFC-0011).
    ///
    /// A two-stage overlay: `SchemePicker` presents a scrollable list of backends; once the user
    /// selects one the form advances to `Fields` for the scheme-specific endpoint parameters.
    /// Credential capture is deferred to P5 — the form collects endpoint data only and shows a
    /// one-line hint about upcoming credential support.
    ///
    /// `editing_id` is `None` for a new connection and `Some(id)` when editing an existing profile.
    /// `existing_secret_ref` carries the `secret_ref` from the live profile so it is not silently
    /// dropped when the user edits and re-saves a connection that already has credentials.
    ConnectionForm {
        /// Current stage.
        stage: ConnectionFormStage,
        /// The chosen scheme id (e.g. `"ssh"`), populated when advancing to `Fields`.
        scheme: String,
        /// Live field values keyed by [`crate::forms::FieldSpec::key`].
        values: HashMap<String, String>,
        /// Which field (by index into the scheme's field list) currently has focus.
        focus: usize,
        /// Per-field validation errors, shown inline. Cleared when the user edits the field.
        field_errors: HashMap<String, String>,
        /// `None` = new connection; `Some(id)` = editing an existing profile.
        editing_id: Option<uuid::Uuid>,
        /// The existing `secret_ref` from the profile being edited, preserved on save. `None`
        /// for new connections (credentials are configured in P5).
        existing_secret_ref: Option<uuid::Uuid>,
    },
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
}

impl Default for ConnectionChoice {
    /// Returns a sentinel choice (`conn = 0`, empty label, `Builtin`/`Ready`/`AutoDiscovered`).
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
        }
    }
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
            sessions: HashMap::new(),
            next_session_id: SessionId(1),
            connection_saving: false,
            saved_profiles: HashMap::new(),
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
            // The connection form captures text only while the user is filling in fields.
            Some(Overlay::ConnectionForm {
                stage: ConnectionFormStage::Fields,
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
}
