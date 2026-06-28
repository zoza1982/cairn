//! The application state model.

use cairn_ai::Plan;
use cairn_types::{ConnectionId, Entry, VfsPath};
use std::collections::BTreeSet;
use std::sync::Arc;

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
    /// The cursor index into the (loaded) entries.
    pub cursor: usize,
    /// Marked (multi-selected) entry indices.
    pub marked: BTreeSet<usize>,
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
        }
    }

    /// The number of loaded entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.listing.entries().len()
    }

    /// Whether the loaded listing is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entry under the cursor, if any.
    #[must_use]
    pub fn current(&self) -> Option<&Entry> {
        self.listing.entries().get(self.cursor)
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
    /// Connections the switcher can open in a pane (populated from config at startup).
    pub connections: Vec<ConnectionChoice>,
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
            connections: Vec::new(),
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
}
