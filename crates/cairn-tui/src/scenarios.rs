//! Deterministic UI scenarios: named [`AppState`] fixtures rendered to a plain-text frame.
//!
//! This module is the **single source of truth** for two things that keep Cairn's terminal UI
//! testable as ordinary text, without ever attaching to a real TTY:
//!
//! * the insta snapshot tests (below), which render every scenario at a couple of terminal sizes
//!   and assert the exact cell grid — so a rendering regression shows up as a readable `.snap`
//!   diff; and
//! * the `--frame-dump` CLI flag (see the `cairn` binary), which prints one scenario's frame to
//!   stdout — a one-shot way to inspect any screen from a script or an agent.
//!
//! Because both consumers share this catalog, a snapshot and a `--frame-dump` of the same scenario
//! can never drift. To cover a new screen, add a [`Scenario`] here and accept the generated
//! snapshot; both the tests and the dump flag pick it up automatically.

use crate::render;
use crate::theme::Theme;
use cairn_core::{
    AppState, ConnectionFormStage, Listing, MaskedInput, Overlay, PagerMode, PagerStatus,
    PromptKind,
};
use cairn_types::{ConnectionId, Entry, EntryKind, SessionId, UnixPerms, VfsPath};
use std::collections::HashMap;
use std::sync::Arc;

/// A named, self-contained UI state plus a one-line description of what it exercises.
pub struct Scenario {
    /// Stable kebab-case identifier used by `--frame-dump <name>` and as the snapshot base name.
    pub name: &'static str,
    /// One-line human description (shown by `--frame-dump-list`).
    pub description: &'static str,
    /// Builds the fixture state. A `fn` pointer (not a closure) so [`Scenario`] stays trivially
    /// copyable and the catalog is a plain `const`-style list.
    build: fn() -> AppState,
}

impl Scenario {
    /// Build this scenario's state and render it at `w`×`h`.
    #[must_use]
    pub fn render(&self, w: u16, h: u16) -> String {
        render_to_string(&(self.build)(), &Theme::default(), w, h)
    }
}

/// The full catalog of UI scenarios, in a stable order.
#[must_use]
pub fn all() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "dual-pane",
            description: "the main two-pane browser with a ready listing",
            build: dual_pane,
        },
        Scenario {
            name: "pane-columns",
            description: "pane rows with MC-style permission + date columns (and a metadata-less remote object)",
            build: pane_columns,
        },
        Scenario {
            name: "remote-pane",
            description: "left pane on a remote SSH connection (scheme://user@host header), right pane local",
            build: remote_pane,
        },
        Scenario {
            name: "loading-error",
            description: "left pane loading, right pane showing a backend error",
            build: loading_error,
        },
        Scenario {
            name: "huge-listing",
            description: "a 10k-entry directory scrolled to the middle (row virtualization)",
            build: huge_listing,
        },
        Scenario {
            name: "filter",
            description: "a pane with a live filter being typed into the border",
            build: filter,
        },
        Scenario {
            name: "transfer-active",
            description: "the status bar showing a live transfer with rate and ETA",
            build: transfer_active,
        },
        Scenario {
            name: "pager-text",
            description: "the read-only pager (F3) in text mode",
            build: pager_text,
        },
        Scenario {
            name: "pager-hex",
            description: "the read-only pager (F3) in hex mode on binary bytes",
            build: pager_hex,
        },
        Scenario {
            name: "log-viewer",
            description: "the streaming log viewer overlay",
            build: log_viewer,
        },
        Scenario {
            name: "ai-plan-safe",
            description: "an AI plan of safe steps (bulk-approve offered)",
            build: ai_plan_safe,
        },
        Scenario {
            name: "ai-plan-irreversible",
            description: "an AI plan containing an irreversible step (no bulk-approve)",
            build: ai_plan_irreversible,
        },
        Scenario {
            name: "transfer-queue-pending-selected",
            description: "the transfer dialog with the cursor on the pending row (per-item selection)",
            build: transfer_queue_pending_selected,
        },
        Scenario {
            name: "transfer-queue",
            description: "the transfer progress dialog: a running transfer with a progress bar + rate/ETA, a paused transfer with an indeterminate bar, and a queued transfer",
            build: transfer_queue,
        },
        Scenario {
            name: "prompt-mkdir",
            description: "the new-directory text prompt",
            build: prompt_mkdir,
        },
        Scenario {
            name: "vault-unlock",
            description: "the vault-unlock overlay with a masked passphrase and an error",
            build: vault_unlock,
        },
        Scenario {
            name: "connections",
            description: "the connection switcher listing open connections",
            build: connections,
        },
        Scenario {
            name: "connections-pin-hide",
            description: "RFC-0011 P6: pinned badge, a revealed hidden entry, and a failed test-connection status line",
            build: connections_pin_hide,
        },
        Scenario {
            name: "confirm-delete",
            description: "the delete-confirmation dialog",
            build: confirm_delete,
        },
        Scenario {
            name: "confirm-overwrite",
            description: "the overwrite-confirmation dialog for a conflicting transfer",
            build: confirm_overwrite,
        },
        Scenario {
            name: "confirm-writeback",
            description: "RFC-0012 P3: remote edit write-back conflict (remote file changed)",
            build: confirm_writeback,
        },
        Scenario {
            name: "scheme-picker",
            description: "the add-connection scheme picker (RFC-0011 P4)",
            build: scheme_picker,
        },
        Scenario {
            name: "vault-create",
            description: "the first-run vault-create overlay (two masked fields)",
            build: vault_create,
        },
        Scenario {
            name: "exec-pane",
            description: "the interactive exec/shell session pane",
            build: exec_pane,
        },
    ]
}

/// The names of all scenarios, in catalog order. Used by the in-crate invariant tests.
#[cfg(test)]
#[must_use]
fn names() -> Vec<&'static str> {
    all().into_iter().map(|s| s.name).collect()
}

/// Render `name` at `w`×`h`, or `None` if there is no such scenario.
#[must_use]
pub fn render_named(name: &str, w: u16, h: u16) -> Option<String> {
    all()
        .into_iter()
        .find(|s| s.name == name)
        .map(|s| s.render(w, h))
}

/// Render `state` at `w`×`h` via ratatui's headless `TestBackend`, returning the frame as one
/// line per terminal row with trailing spaces trimmed — a stable, diffable ASCII grid.
///
/// Never panics: an (essentially impossible) in-memory backend failure is rendered as a visible
/// `<render error: …>` marker rather than unwinding, since this is reachable from the `--frame-dump`
/// CLI path.
#[must_use]
pub(crate) fn render_to_string(state: &AppState, theme: &Theme, w: u16, h: u16) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut terminal = match Terminal::new(TestBackend::new(w, h)) {
        Ok(t) => t,
        Err(e) => return format!("<render error: {e}>\n"),
    };
    if let Err(e) = terminal.draw(|f| render(f, state, theme)) {
        return format!("<render error: {e}>\n");
    }
    frame_to_string(terminal.backend().buffer())
}

/// Flatten a rendered [`ratatui::buffer::Buffer`] into `\n`-separated rows (trailing spaces
/// trimmed for snapshot stability).
fn frame_to_string(buf: &ratatui::buffer::Buffer) -> String {
    let width = buf.area().width as usize;
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for row in buf.content().chunks(width) {
        let line: String = row.iter().map(ratatui::buffer::Cell::symbol).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Scenario fixtures. Kept small and declarative; these mirror the states the reducer produces.
// ─────────────────────────────────────────────────────────────────────────────────────────────

fn base() -> AppState {
    AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root())
}

fn dual_pane() -> AppState {
    let mut s = base();
    let entries = Arc::new(vec![
        Entry::new("src", EntryKind::Dir),
        Entry::new("docs", EntryKind::Dir),
        Entry::new("Cargo.toml", EntryKind::File),
        Entry::new("README.md", EntryKind::File),
    ]);
    s.panes[0].listing = Listing::Ready(entries.clone());
    s.panes[1].listing = Listing::Ready(entries);
    s
}

/// The listing with MC-style permission + UTC-date columns: a directory, an executable, a `0644`
/// file, an owner-only dotfile, and a remote object with no permission/mtime metadata (blank
/// columns). Exercises `format_perms`/`format_mtime`/`entry_columns` in the pane rows.
fn pane_columns() -> AppState {
    let mut s = base();
    let at = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    let mk = |name: &str, kind, mode: u32, mtime: u64| {
        let mut e = Entry::new(name, kind);
        e.perms = Some(UnixPerms::from_mode(mode));
        e.modified = Some(at(mtime));
        e
    };
    let entries = Arc::new(vec![
        mk("src", EntryKind::Dir, 0o755, 1_720_137_600), // 2024-07-05
        mk("build.sh", EntryKind::File, 0o755, 1_719_792_000), // 2024-07-01
        mk("Cargo.toml", EntryKind::File, 0o644, 1_720_224_000), // 2024-07-06
        mk(".env", EntryKind::File, 0o600, 1_717_200_000), // 2024-06-01
        // A remote object store exposes no perms/mtime → the columns render blank for it.
        Entry::new("bucket-object.bin", EntryKind::File),
    ]);
    s.panes[0].listing = Listing::Ready(entries);
    s.panes[1].listing = Listing::Ready(Arc::new(vec![
        Entry::new("docs", EntryKind::Dir),
        Entry::new("README.md", EntryKind::File),
    ]));
    s
}

fn loading_error() -> AppState {
    let mut s = base();
    s.panes[0].listing = Listing::Loading;
    s.panes[1].listing = Listing::Error("permission denied".into());
    s
}

fn huge_listing() -> AppState {
    let mut s = base();
    let entries: Vec<_> = (0..10_000)
        .map(|i| Entry::new(format!("file{i:05}"), EntryKind::File))
        .collect();
    s.panes[0].listing = Listing::Ready(Arc::new(entries));
    s.panes[0].cursor = 5000;
    s
}

fn filter() -> AppState {
    let mut s = dual_pane();
    s.panes[0].filter = Some("src".to_owned());
    s.panes[0].filter_editing = true;
    s
}

fn transfer_active() -> AppState {
    let mut s = dual_pane();
    let id = s.next_transfer_id;
    s.next_transfer_id += 1;
    // 4 MiB of 8 MiB at 2 MiB/s → 50%, ETA 2s.
    s.active_transfers.push(cairn_core::ActiveTransfer {
        id,
        label: "Copying release.tar.gz → /srv/www".to_owned(),
        bytes: 4 * 1024 * 1024,
        rate: Some(2 * 1024 * 1024),
        total: Some(8 * 1024 * 1024),
        paused: false,
    });
    s
}

fn pager_text() -> AppState {
    let mut s = dual_pane();
    let mut lines = std::collections::VecDeque::new();
    lines.push_back("fn main() {".to_owned());
    lines.push_back("    println!(\"hello, cairn\");".to_owned());
    lines.push_back("}".to_owned());
    s.overlay = Some(Overlay::Pager {
        id: 1,
        title: "main.rs — view".to_owned(),
        mode: PagerMode::Text,
        lines,
        partial: String::new(),
        byte_size: 32,
        total_size: Some(64),
        scroll: 0,
        status: PagerStatus::Ready,
        wrap: true,
    });
    s
}

fn pager_hex() -> AppState {
    let mut s = dual_pane();
    let mut lines = std::collections::VecDeque::new();
    // Hex mode stores raw bytes one-per-`char` (lossless for 0..=255); the render layer formats
    // each stored row into `offset | hex | ascii`. Seed two rows of representative bytes.
    let row_a: String = (0u8..16).map(char::from).collect();
    let row_b: String = (0x41u8..0x51).map(char::from).collect();
    lines.push_back(row_a);
    lines.push_back(row_b);
    s.overlay = Some(Overlay::Pager {
        id: 1,
        title: "logo.png — view".to_owned(),
        mode: PagerMode::Hex,
        lines,
        partial: String::new(),
        byte_size: 32,
        total_size: Some(32),
        scroll: 0,
        status: PagerStatus::Ready,
        wrap: false,
    });
    s
}

fn log_viewer() -> AppState {
    let mut s = dual_pane();
    let mut lines = std::collections::VecDeque::new();
    lines.push_back("2026-07-02T10:00:00 starting up".to_owned());
    lines.push_back("2026-07-02T10:00:01 listening on :8080".to_owned());
    s.overlay = Some(Overlay::LogViewer {
        id: 1,
        title: "my-pod — logs".to_owned(),
        lines,
        partial: String::new(),
        byte_size: 0,
        follow: true,
        scroll: 0,
        status: cairn_core::LogViewerStatus::Streaming,
    });
    s
}

fn ai_plan_safe() -> AppState {
    let mut s = dual_pane();
    s.overlay = Some(Overlay::AiPlan {
        plan: plan(&["list", "copy"]),
        cursor: 0,
    });
    s
}

fn ai_plan_irreversible() -> AppState {
    let mut s = dual_pane();
    s.overlay = Some(Overlay::AiPlan {
        plan: plan(&["copy", "delete"]),
        cursor: 0,
    });
    s
}

fn transfer_queue() -> AppState {
    let mut s = transfer_active();
    // A second active transfer, paused, with an unknown total — exercises the indeterminate bar
    // (no fake percentage) and the "⏸ paused" marker/dropped rate+ETA line alongside the first
    // transfer's normal (known-total, running) bar in the same dialog.
    let id2 = s.next_transfer_id;
    s.next_transfer_id += 1;
    s.active_transfers.push(cairn_core::ActiveTransfer {
        id: id2,
        label: "Moving 2 items → dietpi6:/backups".to_owned(),
        bytes: 1024 * 1024,
        rate: None,
        total: None,
        paused: true,
    });
    s.transfer_queue.push_back(cairn_core::QueuedTransfer {
        src_conn: ConnectionId(1),
        dst_conn: ConnectionId(2),
        items: vec![(VfsPath::root(), VfsPath::root())],
        is_move: true,
    });
    s.overlay = Some(Overlay::TransferQueue { cursor: 0 });
    s
}

/// The transfer dialog with the selection cursor on the **pending** row (combined index 2: two
/// active transfers precede the single queued one) — covers the pending-row highlight/marker, the
/// counterpart to `transfer_queue`'s active-row selection.
fn transfer_queue_pending_selected() -> AppState {
    let mut s = transfer_queue();
    s.overlay = Some(Overlay::TransferQueue { cursor: 2 });
    s
}

fn prompt_mkdir() -> AppState {
    let mut s = dual_pane();
    s.overlay = Some(Overlay::Prompt {
        kind: PromptKind::MakeDir,
        input: "new-folder".to_owned(),
    });
    s
}

fn vault_unlock() -> AppState {
    let mut s = dual_pane();
    let mut input = MaskedInput::new();
    for c in "hunter2".chars() {
        input.push(c);
    }
    s.overlay = Some(Overlay::VaultUnlock {
        input,
        error: Some("decryption failed (wrong passphrase or corrupt vault)".to_owned()),
        pending_conn: None,
        pending_save: None,
    });
    s
}

fn connections() -> AppState {
    let mut s = dual_pane();
    s.connections = vec![
        cairn_core::ConnectionChoice {
            conn: ConnectionId(3),
            label: "local: /".into(),
            ..Default::default()
        },
        cairn_core::ConnectionChoice {
            conn: ConnectionId(4),
            label: "local: ~/work".into(),
            ..Default::default()
        },
    ];
    s.overlay = Some(Overlay::Connections {
        cursor: 0,
        show_hidden: false,
    });
    s
}

/// RFC-0011 P6: pin/hide/test-connection badges in the switcher — a pinned entry (badge +
/// floated-to-front ordering), a hidden entry revealed via `show_hidden`, and a status line left
/// over from a failed test-connection probe (`ChoiceStatus::Unreachable`).
fn connections_pin_hide() -> AppState {
    let mut s = dual_pane();
    s.connections = vec![
        cairn_core::ConnectionChoice {
            conn: ConnectionId(3),
            label: "local: /".into(),
            pinned: true,
            ..Default::default()
        },
        cairn_core::ConnectionChoice {
            conn: ConnectionId(4),
            label: "ssh: bastion".into(),
            provenance: cairn_core::ChoiceProvenance::Saved,
            status: cairn_core::ChoiceStatus::Unreachable,
            ..Default::default()
        },
        cairn_core::ConnectionChoice {
            conn: ConnectionId(5),
            label: "docker: (default)".into(),
            provenance: cairn_core::ChoiceProvenance::Discovered {
                source: cairn_core::DiscoverySource::Docker,
            },
            status: cairn_core::ChoiceStatus::NeedsOpen,
            hidden: true,
            ..Default::default()
        },
    ];
    s.overlay = Some(Overlay::Connections {
        cursor: 1,
        show_hidden: true,
    });
    s.status = Some("ssh: bastion — unreachable: connection failed".to_owned());
    s
}

/// Pane connection headers: the left pane is on a remote SSH connection (rendered as a full
/// `ssh://user@host:path` locator in the accent color) while the right pane is on the local
/// filesystem (path only) — so the two are instantly distinguishable.
fn remote_pane() -> AppState {
    let mut s = dual_pane();
    // Left pane → an SSH profile; right pane → the built-in local root.
    s.panes[0].conn = ConnectionId(10);
    s.panes[1].conn = ConnectionId(2);
    let home_dietpi = VfsPath::root()
        .join("home")
        .and_then(|p| p.join("dietpi"))
        .unwrap_or_else(|_| VfsPath::root());
    let home_me = VfsPath::root()
        .join("home")
        .and_then(|p| p.join("me"))
        .unwrap_or_else(|_| VfsPath::root());
    s.panes[0].cwd = home_dietpi;
    s.panes[1].cwd = home_me;

    let profile_id = uuid::Uuid::from_u128(0xD1E7);
    let mut endpoint = std::collections::BTreeMap::new();
    endpoint.insert("host".to_owned(), "dietpi6".to_owned());
    endpoint.insert("user".to_owned(), "root".to_owned());
    s.saved_profiles.insert(
        profile_id,
        cairn_core::ProfileData {
            id: profile_id,
            scheme: "ssh".to_owned(),
            display_name: "dietpi6".to_owned(),
            endpoint,
            secret_ref: None,
        },
    );
    s.connections = vec![
        cairn_core::ConnectionChoice {
            conn: ConnectionId(10),
            label: "ssh: dietpi6".into(),
            scheme: "ssh".into(),
            provenance: cairn_core::ChoiceProvenance::Saved,
            kind: cairn_core::ConnectionKind::Profile { id: profile_id },
            ..Default::default()
        },
        cairn_core::ConnectionChoice {
            conn: ConnectionId(2),
            label: "local: /".into(),
            scheme: "local".into(),
            ..Default::default()
        },
    ];
    s
}

fn confirm_delete() -> AppState {
    let mut s = dual_pane();
    // Build the path off the root rather than `parse(...).unwrap()`: this module ships in the binary
    // (reachable via `--frame-dump`), so no `unwrap`/`expect` on a runtime path (CLAUDE.md §9). The
    // literal is always valid, but fall back to root rather than panic if that ever changes.
    let target = VfsPath::root()
        .join("README.md")
        .unwrap_or_else(|_| VfsPath::root());
    s.overlay = Some(Overlay::ConfirmDelete {
        conn: ConnectionId(1),
        paths: vec![target],
    });
    s
}

fn confirm_overwrite() -> AppState {
    let mut s = dual_pane();
    let from = VfsPath::root()
        .join("README.md")
        .unwrap_or_else(|_| VfsPath::root());
    let to = VfsPath::root()
        .join("README.md")
        .unwrap_or_else(|_| VfsPath::root());
    s.overlay = Some(Overlay::ConfirmOverwrite {
        src_conn: ConnectionId(1),
        dst_conn: ConnectionId(2),
        items: vec![(from, to)],
        is_move: false,
        conflicts: 1,
    });
    s
}

fn confirm_writeback() -> AppState {
    let mut s = dual_pane();
    let path = VfsPath::root()
        .join("notes.txt")
        .unwrap_or_else(|_| VfsPath::root());
    s.overlay = Some(Overlay::ConfirmWriteback {
        id: 1,
        conn: ConnectionId(1),
        path,
        temp_path: std::path::PathBuf::from("/run/user/1000/.cairn-edit-abc123/notes.txt"),
        v0: cairn_core::RemoteVersion::ETag("v1".to_owned()),
        orig_size: 128,
        orig_perms: None,
        download_hash: [0u8; 32],
        hash: [0u8; 32],
        reason: cairn_core::WritebackConflictReason::RemoteChanged,
        cursor: 0,
    });
    s
}

fn scheme_picker() -> AppState {
    let mut s = dual_pane();
    s.overlay = Some(Overlay::ConnectionForm {
        stage: ConnectionFormStage::SchemePicker,
        scheme: String::new(),
        values: HashMap::new(),
        focus: 0,
        field_errors: HashMap::new(),
        editing_id: None,
        existing_secret_ref: None,
        cred_method_cursor: 0,
        cred_method: None,
        cred_fields: HashMap::new(),
        cred_focus: 0,
    });
    s
}

fn vault_create() -> AppState {
    let mut s = dual_pane();
    let mut passphrase = MaskedInput::new();
    for c in "correct-horse".chars() {
        passphrase.push(c);
    }
    s.overlay = Some(Overlay::VaultCreate {
        passphrase,
        confirm: MaskedInput::new(),
        focus: 0,
        remember: true,
        error: None,
        creating: false,
        pending_conn: None,
        pending_save: None,
    });
    s
}

fn exec_pane() -> AppState {
    let mut s = dual_pane();
    let id = SessionId(1);
    let mut record = cairn_core::SessionRecord {
        path: VfsPath::root(),
        title: "my-pod — exec".to_owned(),
        output_lines: std::collections::VecDeque::new(),
        output_partial: String::new(),
        output_byte_size: 0,
        local_port: None,
        ended: None,
    };
    record.output_lines.push_back("$ ls -la".to_owned());
    record.output_lines.push_back("total 8".to_owned());
    s.sessions.insert(id, record);
    s.overlay = Some(Overlay::ExecPane {
        id,
        input: "cat /etc/hostname".to_owned(),
        scroll: 0,
        follow: true,
    });
    s
}

/// Build a representative AI [`Plan`] over the named tools (mirrors the render-test fixture).
///
/// Unknown tool names are silently skipped (`filter_map`) rather than panicking — this module is
/// reachable from the `--frame-dump` CLI path, so it holds to the no-`unwrap`/`expect` rule
/// (CLAUDE.md §9). Every tool used by the scenarios below is a known builtin, so in practice no
/// step is dropped.
fn plan(tools: &[&str]) -> cairn_ai::Plan {
    use cairn_ai::{capability_for, Plan, PlanState, PlanStep, StepStatus};
    let steps = tools
        .iter()
        .filter_map(|t| {
            let capability = capability_for(t)?;
            Some(PlanStep {
                tool: (*t).to_owned(),
                input: serde_json::Value::Null,
                description: format!("{t} the things"),
                capability,
                status: StepStatus::Pending,
                error: None,
                output: None,
            })
        })
        .collect();
    Plan {
        summary: "archive old logs".to_owned(),
        steps,
        state: PlanState::Proposed,
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// Snapshot every scenario at a standard 80×24 and a narrow 40×12 (to catch layout breaks in
    /// tight terminals). insta writes one `.snap` file per (scenario, size) under `src/snapshots/`;
    /// a rendering change surfaces as a readable ASCII diff. Regenerate with `cargo insta accept`
    /// (or `INSTA_UPDATE=always cargo test`) after an intentional UI change.
    #[test]
    fn every_scenario_matches_its_snapshot() {
        for scenario in all() {
            for (w, h) in [(80u16, 24u16), (40u16, 12u16)] {
                let frame = scenario.render(w, h);
                insta::assert_snapshot!(format!("{}__{w}x{h}", scenario.name), frame);
            }
        }
    }

    /// The catalog must have unique, non-empty, kebab-case names (they are used as CLI arguments
    /// and snapshot file names).
    #[test]
    fn scenario_names_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for name in names() {
            assert!(!name.is_empty(), "scenario name must not be empty");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "scenario name '{name}' must be kebab-case"
            );
            assert!(seen.insert(name), "duplicate scenario name '{name}'");
        }
    }

    /// `render_named` resolves catalog entries and rejects unknown names.
    #[test]
    fn render_named_resolves_and_rejects() {
        assert!(render_named("dual-pane", 80, 24).is_some());
        assert!(render_named("does-not-exist", 80, 24).is_none());
    }

    /// Every scenario renders `h` rows at both standard and narrow sizes — the frame is a real grid.
    #[test]
    fn every_scenario_has_one_line_per_row() {
        for scenario in all() {
            for (w, h) in [(80u16, 24u16), (40, 12)] {
                let frame = scenario.render(w, h);
                assert_eq!(
                    frame.lines().count(),
                    h as usize,
                    "{} at {w}x{h} must render exactly {h} rows",
                    scenario.name
                );
            }
        }
    }

    /// Every scenario must render without panicking at *degenerate* terminal sizes (1×1 up to 6×6).
    /// This is the real guard behind `render_to_string`'s "never panics" contract: the `--frame-dump`
    /// CLI path is user-reachable, so a future scenario (or a `render` regression) that panics on a
    /// tiny grid must fail CI here rather than abort the binary in a user's hands.
    #[test]
    fn every_scenario_survives_tiny_terminals() {
        for scenario in all() {
            for w in 1u16..=6 {
                for h in 1u16..=6 {
                    let frame = scenario.render(w, h);
                    assert_eq!(
                        frame.lines().count(),
                        h as usize,
                        "{} at {w}x{h} must still be a {h}-row grid",
                        scenario.name
                    );
                }
            }
        }
    }
}
