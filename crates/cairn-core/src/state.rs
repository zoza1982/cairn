//! The application state model.

use cairn_ai::Plan;
use cairn_types::{ConnectionId, Entry, VfsPath};
use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

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
    #[must_use]
    pub fn visible(&self) -> Vec<&Entry> {
        let entries = self.listing.entries();
        match &self.filter {
            None => entries.iter().collect(),
            Some(f) => {
                let needle = f.to_lowercase();
                entries
                    .iter()
                    .filter(|e| e.name.to_lowercase().contains(&needle))
                    .collect()
            }
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
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match &self.filter {
            None => self.listing.entries().is_empty(),
            Some(f) => {
                let needle = f.to_lowercase();
                !self
                    .listing
                    .entries()
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

/// A selectable connection for the switcher: a registered backend plus a human-readable label.
#[derive(Debug, Clone)]
pub struct ConnectionChoice {
    /// The backend connection to switch the pane to.
    pub conn: ConnectionId,
    /// Display label (e.g. `"local: /home/me"` or a profile's name).
    pub label: String,
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
    /// Cumulative bytes of the **single** in-flight transfer (`Some` while one runs), for the
    /// progress display. Transfers run one at a time; only a `TransferDone` clears it.
    pub transfer_bytes: Option<u64>,
    /// Transfers waiting behind the active one. A copy/move issued while one is running is enqueued
    /// here and started (FIFO) when the active transfer finishes.
    pub transfer_queue: VecDeque<QueuedTransfer>,
    /// Average throughput (bytes/sec) of the active transfer, for the status display. `Some` only
    /// once at least one progress update has arrived.
    pub transfer_rate: Option<u64>,
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
            transfer_bytes: None,
            transfer_queue: VecDeque::new(),
            transfer_rate: None,
        }
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
            Some(Overlay::Prompt { .. }) => true,
            Some(_) => false,
            None => self.active().filter_editing,
        }
    }
}
