//! Rendering the [`AppState`] with ratatui. Pure: takes `&AppState` + `&Theme`, performs no I/O.

use crate::theme::Theme;
use cairn_ai::{Plan, Reversibility, StepStatus, Verb};
use cairn_core::{
    credential_method_fields, credential_methods, scheme_fields, AppState, ChoiceProvenance,
    ConnectionFormStage, ConnectionKind, CredentialMethod, FieldValue, Listing, LogViewerStatus,
    MaskedInput, OpKind, Overlay, PagerMode, PagerStatus, PaneState, PromptKind, SessionEnd,
    SessionRecord, Side, TransferPhase, WritebackChoice, WritebackConflictReason, KNOWN_SCHEMES,
    PAGER_HEX_ROW_BYTES,
};
use cairn_types::{Entry, EntryKind, UnixPerms};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::time::SystemTime;

/// Render the whole application: two panes over a one-line status bar, themed by `theme`.
pub fn render(frame: &mut Frame, state: &AppState, theme: &Theme) {
    // Paint the themed base background/foreground over the whole terminal first, so presets whose
    // identity is their background (Midnight Commander's blue, the light theme) fill every surface a
    // more-specific widget doesn't. `None` leaves the terminal default (the `dark` preset), so this
    // is a no-op there. Every downstream widget only sets `.fg()` (bar the explicit selection/
    // reversed styles), so ratatui's per-field style merge keeps this base without clobbering them.
    if theme.background.is_some() || theme.foreground.is_some() {
        frame.render_widget(Block::new().style(overlay_base(theme)), frame.area());
    }
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
    render_pane(frame, left, state, Side::Left, theme);
    render_pane(frame, right, state, Side::Right, theme);
    render_status(frame, status, state, theme);
    render_overlay(frame, state, theme);
}

/// The base style for an overlay/dialog: the theme's background/foreground, so a dialog matches the
/// active theme instead of showing the terminal default that `Clear` leaves. Applied as each overlay
/// block's `.style(...)` (a `Block`'s style fills its whole area, including the interior). Semantic
/// border accents (red for delete, yellow for confirm, …) are kept on top for clarity.
fn overlay_base(theme: &Theme) -> Style {
    let mut s = Style::default();
    if let Some(bg) = theme.background {
        s = s.bg(bg);
    }
    if let Some(fg) = theme.foreground {
        s = s.fg(fg);
    }
    s
}

/// Compute the display label for a connection choice in the switcher.
///
/// Auto-discovered entries (provenance `Discovered { .. }`) are prefixed with `[auto]` so users
/// can tell environment-sourced entries apart from manually-configured ones. Pinned entries get a
/// leading `[pinned]` badge; hidden entries (only ever visible when "show hidden" is on) get a
/// trailing `[hidden]` badge so a revealed row is never mistaken for a normal one (RFC-0011 P6).
///
/// Badges are bracket tags, not emoji: Cairn has no Nerd-Font-vs-ASCII fallback system yet (that's
/// tracked separately), and an emoji glyph risks rendering as a `?`/tofu box or double-width
/// misalignment on terminals/fonts without color-emoji support — a bracket tag is plain ASCII and
/// renders identically everywhere, consistent with the existing `[auto]`/`[hidden]` tags.
///
/// P4: remove `[auto]` prefix once the sectioned SAVED / DISCOVERED / LOCAL switcher layout lands.
fn connection_display_label(c: &cairn_core::ConnectionChoice) -> String {
    let base = match &c.provenance {
        ChoiceProvenance::Discovered { .. } => format!("[auto] {}", c.label),
        _ => c.label.clone(),
    };
    let pinned = if c.pinned { "[pinned] " } else { "" };
    let hidden = if c.hidden { " [hidden]" } else { "" };
    format!("{pinned}{base}{hidden}")
}

/// Draw the connection switcher: a centered list of the configured connections.
///
/// `connections` is the raw list (`AppState::connections`); this filters to the entries
/// `show_hidden` currently reveals (see [`cairn_core::visible_connection_indices`]) — the same
/// filter the reducer uses, so `cursor` (already an index into that visible subset) lines up with
/// exactly the row the user sees highlighted.
fn render_connections(
    frame: &mut Frame,
    connections: &[cairn_core::ConnectionChoice],
    cursor: usize,
    show_hidden: bool,
    theme: &Theme,
) {
    let visible_indices = cairn_core::visible_connection_indices(connections, show_hidden);
    let visible: Vec<&cairn_core::ConnectionChoice> =
        visible_indices.iter().map(|&i| &connections[i]).collect();

    // +3 = 2 borders + 1 hint line at the bottom.
    let h = u16::try_from(visible.len())
        .unwrap_or(u16::MAX)
        .saturating_add(3)
        .min(frame.area().height);
    let area = centered(frame.area(), 56, h.max(4));
    frame.render_widget(Clear, area);
    // Overlays use fixed semantic accents (not the user's pane palette) so prompts stay distinct.
    let title = if show_hidden {
        " Open connection (showing hidden) "
    } else {
        " Open connection "
    };
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    // Split the inner area: list rows above, one hint line below.
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    let items: Vec<ListItem> = visible
        .iter()
        // P3: auto-discovered entries get an [auto] provenance badge and a dimmed style so
        // users understand they come from the environment and cannot be edited in the config.
        // P4: remove [auto] prefix when the sectioned SAVED/DISCOVERED/LOCAL switcher lands.
        // P6: a revealed-but-hidden entry is dimmed too, marking it as "not normally shown".
        .map(|c| {
            let label = connection_display_label(c);
            let item = ListItem::new(label);
            match &c.provenance {
                ChoiceProvenance::Discovered { .. } => {
                    item.style(Style::default().add_modifier(Modifier::DIM))
                }
                _ if c.hidden => item.style(Style::default().add_modifier(Modifier::DIM)),
                _ => item,
            }
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black))
        .highlight_symbol("> ");
    let mut st = ListState::default();
    if !visible.is_empty() {
        st.select(Some(cursor.min(visible.len() - 1)));
    }
    frame.render_stateful_widget(list, list_area, &mut st);

    // Contextual hint: profile entries show edit/delete, auto-discovered entries are read-only.
    // Test/pin/hide/show-hidden apply to every entry regardless of kind.
    let selected_kind = visible
        .get(cursor.min(visible.len().saturating_sub(1)))
        .map(|c| &c.kind);
    let editable = matches!(selected_kind, Some(ConnectionKind::Profile { .. }));
    let hint = connections_hint(editable, hint_area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(Style::default().fg(Color::Gray)),
        hint_area,
    );
}

/// Build the connection-switcher's hint line for the given available `width` (columns).
///
/// `[Esc] Close` is always first: `Paragraph` right-truncates a line that overflows its area
/// (no wrap), so anything placed later can be cut off on a narrow terminal — and `Esc` is the
/// only way to discover how to leave the overlay, so it must never be the part that disappears.
/// The remaining hints are added one whole token at a time, in descending priority, and a token
/// that would not fully fit is dropped rather than sliced — a half-shown `"[S] Show hi"` is worse
/// than not showing it at all.
fn connections_hint(editable: bool, width: usize) -> String {
    let mut tokens: Vec<&str> = vec!["[Esc] Close", "[Ctrl-N] New"];
    if editable {
        tokens.push("[e] Edit");
        tokens.push("[d] Delete");
    }
    tokens.extend(["[t] Test", "[P] Pin", "[H] Hide", "[S] Show hidden"]);

    let mut hint = String::new();
    for tok in tokens {
        let candidate_len = if hint.is_empty() {
            tok.len()
        } else {
            hint.len() + 2 + tok.len()
        };
        if candidate_len > width {
            break;
        }
        if !hint.is_empty() {
            hint.push_str("  ");
        }
        hint.push_str(tok);
    }
    hint
}

/// Draw the active modal overlay (if any) centered over the screen. Takes `&AppState` so overlays
/// that need extra state (the connection switcher's choice list) dispatch from a single site.
fn render_overlay(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let Some(overlay) = &state.overlay else {
        return;
    };
    match overlay {
        Overlay::Connections {
            cursor,
            show_hidden,
        } => render_connections(frame, &state.connections, *cursor, *show_hidden, theme),
        Overlay::ConfirmDelete { paths, .. } => {
            let area = centered(frame.area(), 44, 6);
            frame.render_widget(Clear, area);
            let block = Block::bordered()
                .style(overlay_base(theme))
                .title(" Confirm delete ")
                .border_style(Style::default().fg(Color::Red));
            let body = Paragraph::new(vec![
                Line::from(format!("Delete {} item(s)?", paths.len())),
                Line::from(""),
                Line::from("[y] Yes    [n] No"),
            ])
            .block(block)
            .alignment(Alignment::Center);
            frame.render_widget(body, area);
        }
        Overlay::FolderStats {
            name,
            computing,
            bytes,
            files,
            dirs,
            partial,
            .. // `id` is a correlation token, not rendered
        } => render_folder_stats(
            frame, name, *computing, *bytes, *files, *dirs, *partial, theme,
        ),
        Overlay::ConfirmOverwrite { conflicts, .. } => {
            let area = centered(frame.area(), 48, 6);
            frame.render_widget(Clear, area);
            let block = Block::bordered()
                .style(overlay_base(theme))
                .title(" Overwrite? ")
                .border_style(Style::default().fg(Color::Yellow));
            let body = Paragraph::new(vec![
                Line::from(format!("{conflicts} destination(s) already exist.")),
                Line::from(""),
                Line::from("[y] Overwrite    [n] Cancel"),
            ])
            .block(block)
            .alignment(Alignment::Center);
            frame.render_widget(body, area);
        }
        Overlay::ConfirmShellAction { name, target, .. } => {
            // h=7: 2 borders + 5 content rows, exactly matching the 5 lines below.
            let area = centered(frame.area(), 56, 7);
            frame.render_widget(Clear, area);
            let block = Block::bordered()
                .style(overlay_base(theme))
                .title(" Run shell action? ")
                .border_style(Style::default().fg(Color::Yellow));
            let body = Paragraph::new(vec![
                Line::from(format!("Run '{name}' on")),
                Line::from(target.as_str()),
                // `target` is the virtual VFS path; the real OS path is resolved on confirm
                // by the effect runner (via `Vfs::local_path`). What you see here is always
                // forwarded as-is — no truncation or substitution happens at this stage.
                Line::from("(virtual path — real OS path resolved on confirm)"),
                Line::from(""),
                Line::from("[y] Run    [n] Cancel"),
            ])
            .block(block)
            .alignment(Alignment::Center);
            frame.render_widget(body, area);
        }
        Overlay::ConfirmWriteback {
            path,
            temp_path,
            reason,
            cursor,
            ..
        } => render_confirm_writeback(frame, path, temp_path, *reason, *cursor, theme),
        Overlay::TransferQueue { cursor } => render_transfer_queue(frame, state, *cursor, theme),
        Overlay::AiPlan { plan, cursor } => render_ai_plan(frame, plan, *cursor, theme),
        Overlay::Prompt { kind, input } => render_prompt(frame, kind, input, theme),
        Overlay::VaultUnlock { input, error, .. } => {
            render_vault_unlock(frame, input, error.as_deref(), state.vault_unlocking, theme)
        }
        Overlay::VaultCreate {
            passphrase,
            confirm,
            focus,
            remember,
            error,
            creating,
            ..
        } => render_vault_create(
            frame,
            passphrase,
            confirm,
            *focus,
            *remember,
            error.as_deref(),
            *creating,
            theme,
        ),
        Overlay::LogViewer {
            title,
            lines,
            follow,
            scroll,
            status,
            ..
        } => render_log_viewer(frame, title, lines, *follow, *scroll, status, theme),
        Overlay::Pager {
            title,
            mode,
            lines,
            byte_size,
            total_size,
            scroll,
            status,
            wrap,
            ..
        } => render_pager(
            frame,
            title,
            *mode,
            lines,
            *byte_size,
            *total_size,
            *scroll,
            status,
            *wrap,
            theme,
        ),
        Overlay::ExecPane {
            id,
            input,
            scroll,
            follow,
        } => {
            if let Some(rec) = state.sessions.get(id) {
                render_exec_pane(frame, rec, input, *scroll, *follow, theme);
            }
        }
        Overlay::PortForwardStatus { id } => {
            if let Some(rec) = state.sessions.get(id) {
                render_port_forward_status(frame, rec, theme);
            }
        }
        Overlay::ConfirmDeleteConnection { display_name, .. } => {
            render_confirm_delete_connection(frame, display_name, theme)
        }
        Overlay::ConnectionForm {
            stage,
            scheme,
            values,
            focus,
            field_errors,
            editing_id,
            existing_secret_ref: _,
            cred_method_cursor,
            cred_method,
            cred_fields,
            cred_focus,
        } => render_connection_form(
            frame,
            stage,
            scheme,
            values,
            *focus,
            field_errors,
            editing_id.is_some(),
            *cred_method_cursor,
            cred_method.as_ref(),
            cred_fields,
            *cred_focus,
            theme,
        ),
    }
}

/// Draw the confirm-delete-connection overlay: a red-bordered prompt asking the user to confirm
/// before permanently removing a saved connection profile.
fn render_confirm_delete_connection(frame: &mut Frame, display_name: &str, theme: &Theme) {
    let msg = format!("Delete connection '{display_name}'? This cannot be undone.");
    let h = 5u16;
    let w = u16::try_from(msg.len() + 6)
        .unwrap_or(64)
        .min(frame.area().width);
    let area = centered(frame.area(), w, h);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(" Confirm delete ")
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [msg_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(Paragraph::new(msg.as_str()), msg_area);
    frame.render_widget(
        Paragraph::new("[Enter] Delete  [Esc] Cancel").style(Style::default().fg(Color::DarkGray)),
        hint_area,
    );
}

/// Draw the RFC-0012 P3 write-back conflict overlay: explains why (the remote file changed, or
/// the zero-length guard tripped), shows the remote path and the still-present local temp path,
/// then a cursor-selectable list of [`WritebackChoice::ALL`] with the current selection
/// highlighted — mirrors [`render_connections`]'s cursor-list convention.
fn render_confirm_writeback(
    frame: &mut Frame,
    path: &cairn_types::VfsPath,
    temp_path: &std::path::Path,
    reason: WritebackConflictReason,
    cursor: usize,
    theme: &Theme,
) {
    // 2 borders + reason + path + temp path + blank + 4 choices + hint.
    let area = centered(frame.area(), 66, 12);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(" Write back? ")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [info_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(WritebackChoice::ALL.len() as u16),
        Constraint::Length(1),
    ])
    .areas(inner);
    let info = Paragraph::new(vec![
        Line::from(reason.message()),
        Line::from(format!("remote: {path}")),
        Line::from(format!("local copy kept at: {}", temp_path.display())),
        Line::from(""),
    ]);
    frame.render_widget(info, info_area);
    let items: Vec<ListItem> = WritebackChoice::ALL
        .iter()
        .map(|c| ListItem::new(c.label()))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Yellow).fg(Color::Black))
        .highlight_symbol("> ");
    let mut st = ListState::default();
    st.select(Some(cursor.min(WritebackChoice::ALL.len() - 1)));
    frame.render_stateful_widget(list, list_area, &mut st);
    frame.render_widget(
        Paragraph::new("[↑/↓] Select  [Enter] Confirm  [Esc] Keep editing")
            .style(Style::default().fg(Color::DarkGray)),
        hint_area,
    );
}

/// Draw the vault-unlock overlay: a masked passphrase field (one `•` per typed character — the
/// passphrase itself is never rendered), a status/error line (an in-flight "Unlocking…" note or a
/// failed-attempt error), and the action hint.
fn render_vault_unlock(
    frame: &mut Frame,
    input: &MaskedInput,
    error: Option<&str>,
    busy: bool,
    theme: &Theme,
) {
    // 7 rows: 2 borders + masked field + blank + error/spacer + hint + breathing room.
    let area = centered(frame.area(), 50, 7);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(" Unlock vault ")
        .border_style(Style::default().fg(Color::Cyan));
    // One bullet per entered character; a trailing block stands in for the cursor. Never the value.
    let masked = "\u{2022}".repeat(input.len());
    let mut lines = vec![Line::from(format!("{masked}\u{258f}")), Line::from("")];
    // Priority: a live "Unlocking…" note while the async open runs, else a failed-attempt error.
    if busy {
        lines.push(Line::styled(
            "Unlocking…",
            Style::default().fg(Color::Yellow),
        ));
    } else if let Some(err) = error {
        lines.push(Line::styled(
            err.to_owned(),
            Style::default().fg(Color::Red),
        ));
    } else {
        lines.push(Line::from(""));
    }
    lines.push(Line::from("[Enter] Unlock    [Esc] Cancel"));
    let body = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(body, area);
}

/// Draw the vault-create overlay: two masked passphrase fields, a "remember" toggle, a status/error
/// line, and the action hints.
///
/// **Security invariants:** only the *count* of typed characters is exposed (as `•` bullets) —
/// the actual bytes are never rendered. The `MaskedInput` API provides only `len()` for this.
#[allow(clippy::too_many_arguments)]
fn render_vault_create(
    frame: &mut Frame,
    passphrase: &MaskedInput,
    confirm: &MaskedInput,
    focus: u8,
    remember: bool,
    error: Option<&str>,
    creating: bool,
    theme: &Theme,
) {
    // 11 rows: 2 borders + 9 content rows (passphrase label + passphrase field + blank +
    // confirm label + confirm field + blank + remember toggle + error/status + hint).
    let area = centered(frame.area(), 54, 11);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(" Create vault ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout: 9 content rows inside the border.
    let [pp_label, pp_field, blank1, cf_label, cf_field, blank2, remember_row, status_row, hint_row] =
        Layout::vertical([
            Constraint::Length(1), // "New passphrase:"
            Constraint::Length(1), // bullets + cursor
            Constraint::Length(1), // blank
            Constraint::Length(1), // "Confirm passphrase:"
            Constraint::Length(1), // bullets + cursor
            Constraint::Length(1), // blank
            Constraint::Length(1), // "[Ctrl-R] Remember: Yes/No"
            Constraint::Length(1), // error or "Creating…"
            Constraint::Length(1), // hint
        ])
        .areas(inner);

    let pp_style = if focus == 0 {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let cf_style = if focus == 1 {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    frame.render_widget(Paragraph::new("New passphrase:").style(pp_style), pp_label);
    let pp_bullets = "\u{2022}".repeat(passphrase.len());
    // Append the block cursor only to the focused field.
    let pp_cursor = if focus == 0 { "\u{258f}" } else { "" };
    frame.render_widget(
        Paragraph::new(format!("{pp_bullets}{pp_cursor}")).style(pp_style),
        pp_field,
    );
    frame.render_widget(Paragraph::new(""), blank1);
    frame.render_widget(
        Paragraph::new("Confirm passphrase:").style(cf_style),
        cf_label,
    );
    let cf_bullets = "\u{2022}".repeat(confirm.len());
    let cf_cursor = if focus == 1 { "\u{258f}" } else { "" };
    frame.render_widget(
        Paragraph::new(format!("{cf_bullets}{cf_cursor}")).style(cf_style),
        cf_field,
    );
    frame.render_widget(Paragraph::new(""), blank2);

    let remember_text = if remember { "Yes (keychain)" } else { "No" };
    frame.render_widget(
        Paragraph::new(format!("[Ctrl-R] Remember: {remember_text}"))
            .style(Style::default().fg(Color::DarkGray)),
        remember_row,
    );

    // Status/error row: "Creating…" while Argon2id runs; a red error if the last submit failed.
    if creating {
        frame.render_widget(
            Paragraph::new("Creating…").style(Style::default().fg(Color::Yellow)),
            status_row,
        );
    } else if let Some(err) = error {
        frame.render_widget(
            Paragraph::new(err.to_owned()).style(Style::default().fg(Color::Red)),
            status_row,
        );
    } else {
        frame.render_widget(
            Paragraph::new("Passphrase must be ≥ 12 chars")
                .style(Style::default().fg(Color::DarkGray)),
            status_row,
        );
    }

    frame.render_widget(
        Paragraph::new("[Tab] Next field    [Enter] Create    [Esc] Cancel")
            .style(Style::default().fg(Color::DarkGray)),
        hint_row,
    );
}

/// Top-level dispatcher for the connection form overlay (add/edit).
///
/// Delegates to [`render_scheme_picker`] in the `SchemePicker` stage and [`render_form_fields`]
/// in the `Fields` stage.
#[allow(clippy::too_many_arguments)]
fn render_connection_form(
    frame: &mut Frame,
    stage: &ConnectionFormStage,
    scheme: &str,
    values: &std::collections::HashMap<String, String>,
    focus: usize,
    field_errors: &std::collections::HashMap<String, String>,
    is_edit: bool,
    cred_method_cursor: usize,
    cred_method: Option<&CredentialMethod>,
    cred_fields: &std::collections::HashMap<String, FieldValue>,
    cred_focus: usize,
    theme: &Theme,
) {
    match stage {
        ConnectionFormStage::SchemePicker => render_scheme_picker(frame, focus, theme),
        ConnectionFormStage::Fields => {
            render_form_fields(frame, scheme, values, focus, field_errors, is_edit, theme)
        }
        ConnectionFormStage::CredentialMethodPicker => {
            render_credential_method_picker(frame, scheme, is_edit, cred_method_cursor, theme)
        }
        ConnectionFormStage::CredentialFields => {
            render_credential_fields(frame, cred_method, cred_fields, cred_focus, theme)
        }
    }
}

/// Draw the scheme-picker stage: a scrollable list of known backend types.
fn render_scheme_picker(frame: &mut Frame, focus: usize, theme: &Theme) {
    let h = u16::try_from(KNOWN_SCHEMES.len())
        .unwrap_or(u16::MAX)
        .saturating_add(5) // 2 borders + 1 blank + 1 hint + 1 breathing room
        .min(frame.area().height);
    let area = centered(frame.area(), 50, h.max(5));
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(" New connection — choose backend ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    let items: Vec<ListItem> = KNOWN_SCHEMES
        .iter()
        .map(|(_, label)| ListItem::new(*label))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black))
        .highlight_symbol("> ");
    let mut st = ListState::default();
    if !KNOWN_SCHEMES.is_empty() {
        st.select(Some(focus.min(KNOWN_SCHEMES.len() - 1)));
    }
    frame.render_stateful_widget(list, list_area, &mut st);
    frame.render_widget(
        Paragraph::new(Line::from("[Enter] Select  [Esc] Cancel"))
            .style(Style::default().fg(Color::Gray)),
        hint_area,
    );
}

/// Draw the fields stage: a labelled form with one row per [`FieldSpec`] and an inline credential
/// hint reminding the user that credentials are configured separately (Phase P5).
fn render_form_fields(
    frame: &mut Frame,
    scheme: &str,
    values: &std::collections::HashMap<String, String>,
    focus: usize,
    field_errors: &std::collections::HashMap<String, String>,
    is_edit: bool,
    theme: &Theme,
) {
    let fields = scheme_fields(scheme);
    // +5 = 2 borders + 1 cred hint + 1 blank + 1 action hint line
    let h = u16::try_from(fields.len())
        .unwrap_or(u16::MAX)
        .saturating_add(5)
        .min(frame.area().height);
    let area = centered(frame.area(), 60, h.max(6));
    frame.render_widget(Clear, area);

    let title = if is_edit {
        " Edit connection "
    } else {
        " New connection — fill in details "
    };
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Layout: field rows, then cred-hint, then blank line, then action hint.
    let n_fields = fields.len();
    let mut constraints: Vec<Constraint> = (0..n_fields).map(|_| Constraint::Length(1)).collect();
    constraints.push(Constraint::Length(1)); // credential hint
    constraints.push(Constraint::Min(0)); // spacer
    constraints.push(Constraint::Length(1)); // action hint
    let areas = Layout::vertical(constraints).split(inner);

    for (i, field) in fields.iter().enumerate() {
        let is_focused = i == focus;
        let value = values.get(field.key).map(String::as_str).unwrap_or("");
        let error = field_errors.get(field.key).map(String::as_str);

        let label_style = if is_focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let field_text = if value.is_empty() {
            // Greyed-out placeholder.
            format!("{}: {}", field.label, field.placeholder)
        } else {
            let cursor = if is_focused { "\u{258f}" } else { "" };
            if field.secret {
                let masked = "\u{2022}".repeat(value.chars().count());
                format!("{}: {}{cursor}", field.label, masked)
            } else {
                format!("{}: {}{cursor}", field.label, value)
            }
        };
        let field_style = if value.is_empty() {
            Style::default().fg(Color::DarkGray)
        } else if is_focused {
            Style::default().fg(Color::White)
        } else {
            Style::default()
        };

        let mut spans = vec![
            ratatui::text::Span::styled(if is_focused { "> " } else { "  " }, label_style),
            ratatui::text::Span::styled(field_text, field_style),
        ];
        if let Some(err) = error {
            spans.push(ratatui::text::Span::styled(
                format!("  ⚠ {err}"),
                Style::default().fg(Color::Red),
            ));
        }
        if areas.get(i).is_some() {
            frame.render_widget(Paragraph::new(Line::from(spans)), areas[i]);
        }
    }

    // Credential hint: P5 — indicates next step is credential picker.
    if let Some(hint_area) = areas.get(n_fields) {
        let hint = if cairn_core::forms::scheme_needs_credentials(scheme) {
            "  Next: choose authentication method →"
        } else {
            "  No credentials required for this backend."
        };
        frame.render_widget(
            Paragraph::new(Line::from(hint)).style(Style::default().fg(Color::DarkGray)),
            *hint_area,
        );
    }

    // Action hint: Esc goes back to the scheme picker for new connections; closes for edits.
    let back_hint = if is_edit {
        "  [Esc] Cancel"
    } else {
        "  [Esc] Back"
    };
    let action_hint = format!("[Tab/↑↓] Navigate fields  [Enter] Next{back_hint}");
    if let Some(ahint_area) = areas.last() {
        frame.render_widget(
            Paragraph::new(Line::from(action_hint)).style(Style::default().fg(Color::Gray)),
            *ahint_area,
        );
    }
}

/// Draw the credential method picker stage: a list of auth methods for the chosen scheme.
fn render_credential_method_picker(
    frame: &mut Frame,
    scheme: &str,
    is_edit: bool,
    cursor: usize,
    theme: &Theme,
) {
    let methods = credential_methods(scheme, is_edit);
    let n = methods.len();
    if n == 0 {
        return; // No-credential schemes skip this stage entirely.
    }
    let h = u16::try_from(n)
        .unwrap_or(u16::MAX)
        .saturating_add(5) // 2 borders + 1 blank + 1 hint + 1 breathing room
        .min(frame.area().height);
    let area = centered(frame.area(), 60, h.max(5));
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(" Choose authentication method ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    let items: Vec<ListItem> = methods
        .iter()
        .map(|m| {
            let suffix = if m.is_field_capture_deferred() {
                " (coming soon)"
            } else {
                ""
            };
            ListItem::new(format!("{}{suffix}", m.label()))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black))
        .highlight_symbol("> ");
    let mut st = ListState::default();
    if !methods.is_empty() {
        st.select(Some(cursor.min(n - 1)));
    }
    frame.render_stateful_widget(list, list_area, &mut st);
    frame.render_widget(
        Paragraph::new(Line::from("[Enter] Select  [↑↓] Navigate  [Esc] Back"))
            .style(Style::default().fg(Color::Gray)),
        hint_area,
    );
}

/// Draw the credential fields stage: labelled inputs for the chosen auth method.
///
/// Secret fields show bullet characters; plain fields show the typed text. Deferred and
/// delegation methods should not reach this stage (the picker auto-advances past them).
fn render_credential_fields(
    frame: &mut Frame,
    cred_method: Option<&CredentialMethod>,
    cred_fields: &std::collections::HashMap<String, FieldValue>,
    cred_focus: usize,
    theme: &Theme,
) {
    let Some(method) = cred_method else {
        return;
    };
    let fields = credential_method_fields(method);

    if method.is_field_capture_deferred() {
        // Deferred method: show a placeholder message.
        let area = centered(frame.area(), 60, 5);
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .style(overlay_base(theme))
            .title(" Credentials ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("  {}: coming in a future update.", method.label())),
                Line::from(""),
                Line::from("  [Enter] Save without credentials  [Esc] Back")
                    .style(Style::default().fg(Color::Gray)),
            ]),
            inner,
        );
        return;
    }

    if fields.is_empty() {
        // Delegation method: no fields, auto-advances. Show nothing (shouldn't be reached
        // normally but guard for safety).
        return;
    }

    let h = u16::try_from(fields.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4) // 2 borders + 1 blank + 1 hint
        .min(frame.area().height);
    let area = centered(frame.area(), 60, h.max(5));
    frame.render_widget(Clear, area);
    let title = format!(" Credentials — {} ", method.label());
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let n_fields = fields.len();
    let mut constraints: Vec<Constraint> = (0..n_fields).map(|_| Constraint::Length(1)).collect();
    constraints.push(Constraint::Min(0)); // spacer
    constraints.push(Constraint::Length(1)); // hint
    let areas = Layout::vertical(constraints).split(inner);

    for (i, spec) in fields.iter().enumerate() {
        let is_focused = i == cred_focus;
        let label_style = if is_focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        let (display_text, value_style) = match cred_fields.get(spec.key) {
            None => {
                // No value entered yet — show placeholder.
                (
                    format!("{}: {}", spec.label, spec.placeholder),
                    Style::default().fg(Color::DarkGray),
                )
            }
            Some(fv) if fv.is_empty() => {
                // Entered but empty (e.g. just cleared) — show placeholder.
                (
                    format!("{}: {}", spec.label, spec.placeholder),
                    Style::default().fg(Color::DarkGray),
                )
            }
            Some(FieldValue::Plain(s)) => {
                let cursor = if is_focused { "\u{258f}" } else { "" };
                (
                    format!("{}: {}{cursor}", spec.label, s),
                    if is_focused {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default()
                    },
                )
            }
            Some(FieldValue::Secret(m)) => {
                let cursor = if is_focused { "\u{258f}" } else { "" };
                let masked = "\u{2022}".repeat(m.len());
                (
                    format!("{}: {}{cursor}", spec.label, masked),
                    if is_focused {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default()
                    },
                )
            }
        };

        let spans = vec![
            ratatui::text::Span::styled(if is_focused { "> " } else { "  " }, label_style),
            ratatui::text::Span::styled(display_text, value_style),
        ];
        if let Some(row_area) = areas.get(i) {
            frame.render_widget(Paragraph::new(Line::from(spans)), *row_area);
        }
    }

    if let Some(ahint_area) = areas.last() {
        frame.render_widget(
            Paragraph::new(Line::from("[Tab/↑↓] Navigate  [Enter] Save  [Esc] Back"))
                .style(Style::default().fg(Color::Gray)),
            *ahint_area,
        );
    }
}

/// Draw the transfer progress dialog (MC-style): each active transfer as a 3–4 line block — label
/// (+ paused marker), a text progress bar, the byte/rate/ETA line, and (while scanning) an extra line
/// for the path being walked — followed by the pending queue with the reorder cursor, and hint lines.
/// Auto-opened by the reducer whenever a transfer starts (`arm_transfer`); backgrounded with `b`,
/// brought back with `Ctrl-T`.
fn render_transfer_queue(frame: &mut Frame, state: &AppState, cursor: usize, theme: &Theme) {
    let pending = &state.transfer_queue;
    let active = &state.active_transfers;

    let frame_area = frame.area();
    // 70 wide (68 interior) so each control-hint line fits at typical sizes; clamped to the frame so
    // a narrow terminal (e.g. 40 cols) still fits and content truncates gracefully. Computed up front
    // so lines can be built before the box is sized to its final line count (a scanning transfer uses
    // a variable number of rows).
    let width = 70u16.min(frame_area.width);
    let content_width = usize::from(width.saturating_sub(2));
    let bar_width = content_width.saturating_sub(2).max(1);

    // Selection marks only the *label* line, and with **bold** (plus the `>` marker), not
    // reverse-video: a reversed run paints a solid background block over the label text that looks
    // just like the filled progress bar below it, which reads as a rendering glitch. Bold + the marker
    // is an unambiguous, unobtrusive cue, and the bar/detail lines always render in the normal style.
    let sel = Style::default().add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = Vec::new();
    if active.is_empty() {
        // Idle-but-open can happen momentarily (e.g. `Ctrl-T` right after the last transfer finished
        // but before the auto-close event is processed).
        lines.push(Line::from("No active transfers".to_owned()));
    } else {
        for (ai, t) in active.iter().enumerate() {
            // A blank line *between* transfers so consecutive blocks don't run together and the label
            // never sits flush against the previous bar. Not after the last one — the pre-hint blank
            // below is the single separator before the hints.
            if ai > 0 {
                lines.push(Line::from(""));
            }
            let is_sel = ai == cursor;
            let marker = if is_sel { "> " } else { "  " };
            let paused_marker = if t.paused { "  ⏸ paused" } else { "" };
            // Truncate the label (not the marker/paused tail) so a long "what → where" can't push the
            // `⏸ paused` indicator off the right edge at narrow widths — the state must stay visible.
            let label_budget = content_width
                .saturating_sub(marker.chars().count() + paused_marker.chars().count());
            lines.push(Line::styled(
                format!(
                    "{marker}{}{paused_marker}",
                    truncate_to(&t.label, label_budget)
                ),
                if is_sel { sel } else { Style::default() },
            ));

            // The bar is phase-driven, not a raw byte ratio: `Counting` has no total yet
            // (indeterminate), `Finalizing` asserts an honest 100% (bytes are all written; the flush/
            // verify tail moves none), and only `Copying` derives a percentage from bytes/total — so
            // an over-counted scan total can never pin the bar at 99% at the end.
            let pct = match t.phase {
                // Pre-flight scan and delete both have no total → indeterminate bar.
                TransferPhase::Counting | TransferPhase::Deleting => None,
                // The reducer only enters Finalizing once the whole transfer's bytes are written, so
                // this is an honest 100% — but derive it from bytes/total anyway so a hypothetical
                // early Finalizing can never claim more than the bytes justify.
                TransferPhase::Finalizing => match t.total {
                    Some(total) if total > 0 => Some(pct_of(t.bytes, total)),
                    _ => Some(100),
                },
                TransferPhase::Copying => match t.total {
                    Some(total) if total > 0 => Some(pct_of(t.bytes, total)),
                    _ => None,
                },
            };
            lines.push(Line::from(format!(
                "  {}",
                progress_bar(pct, bar_width, t.pulse)
            )));

            match t.phase {
                // Live pre-flight walk: a running item count + bytes found on one line, and the path
                // currently being visited on its own line (left-truncated so the meaningful tail
                // shows, e.g. `…/functions/foo.js`) — so a long path never overflows the row.
                TransferPhase::Counting => {
                    // `truncate_to` guards a pathological item/byte count from overflowing a narrow pane.
                    let head = format!(
                        "Scanning {} items · {}",
                        t.scan_entries,
                        human_bytes(t.bytes)
                    );
                    lines.push(Line::from(format!(
                        "  {}",
                        truncate_to(&head, content_width.saturating_sub(2))
                    )));
                    lines.push(Line::from(format!(
                        "  {}",
                        truncate_left(&t.scan_path, content_width.saturating_sub(2))
                    )));
                }
                // Live delete walk: a running item count and the path currently being removed.
                TransferPhase::Deleting => {
                    let head = format!("Deleted {} item(s)", t.scan_entries);
                    lines.push(Line::from(format!(
                        "  {}",
                        truncate_to(&head, content_width.saturating_sub(2))
                    )));
                    lines.push(Line::from(format!(
                        "  {}",
                        truncate_left(&t.scan_path, content_width.saturating_sub(2))
                    )));
                }
                // Bytes all written; the flush/verify tail is opaque backend work — say so instead of
                // sitting at a stalled ratio with a rate/ETA that no longer applies.
                TransferPhase::Finalizing => {
                    lines.push(Line::from(format!(
                        "  {}   Finalizing…",
                        human_bytes(t.bytes)
                    )));
                }
                TransferPhase::Copying => {
                    let amount = match t.total {
                        Some(total) if total > 0 => {
                            format!("{} / {}", human_bytes(t.bytes), human_bytes(total))
                        }
                        _ => human_bytes(t.bytes),
                    };
                    // Rate/ETA shown only when meaningful (shared with the status line via
                    // `transfer_rate_eta`: never for a paused/finished/scanning/unknown-rate transfer).
                    let (rate_bps, eta_secs) = transfer_rate_eta(t);
                    let rate = rate_bps
                        .map(|r| format!("   {}/s", human_bytes(r)))
                        .unwrap_or_default();
                    let eta = eta_secs
                        .map(|s| format!("   ETA {}", human_duration(s)))
                        .unwrap_or_default();
                    lines.push(Line::from(format!("  {amount}{rate}{eta}")));
                }
            }
        }
    }
    // Separate the active section from the pending queue with a blank line (only when both exist).
    if !active.is_empty() && !pending.is_empty() {
        lines.push(Line::from(""));
    }
    for (i, q) in pending.iter().enumerate() {
        let is_sel = active.len() + i == cursor;
        let verb = if q.is_move { "move" } else { "copy" };
        let marker = if is_sel { "> " } else { "  " };
        let line = format!("{marker}{}. {verb} {} item(s)", i + 1, q.items.len());
        lines.push(Line::styled(
            line,
            if is_sel { sel } else { Style::default() },
        ));
    }
    lines.push(Line::from(""));
    // Two hint lines so every live control stays discoverable: per-transfer controls act on the
    // selected row; `Esc` is the abort-all panic-stop.
    lines.push(Line::from(
        "[b] background · [p] pause sel · [Esc] abort all".to_owned(),
    ));
    lines.push(Line::from(
        "[↑↓] select · [d] cancel/drop · [K/J] reorder · [x] clear".to_owned(),
    ));

    // Size the box to the built line count (+2 borders), so a scanning transfer's extra path line is
    // accommodated instead of clipped.
    let h = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(frame_area.height);
    // `.max(6)` is a defensive floor: the minimum line count (idle row + blank + 2 hints) already
    // yields h = 6, so this only matters if that layout ever shrinks.
    let area = centered(frame_area, width, h.max(6));
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(" Transfer ")
        .border_style(Style::default().fg(Color::Cyan));
    let content = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), content);
}

/// Render the recursive folder-size stats popup ([`Overlay::FolderStats`]). While `computing` it
/// shows a "Calculating…" header with the running totals (so a large/remote walk shows progress);
/// once done it shows the final size + file/subdirectory counts, with a note if the count is partial
/// (some entries couldn't be read).
#[allow(clippy::too_many_arguments)]
fn render_folder_stats(
    frame: &mut Frame,
    name: &str,
    computing: bool,
    bytes: u64,
    files: u64,
    dirs: u64,
    partial: bool,
    theme: &Theme,
) {
    // Clamp the box to the real frame width *first*, then derive the interior width from that — so a
    // long folder name is truncated to the actual content width on a narrow terminal instead of
    // overflowing the right border.
    let width: u16 = 46.min(frame.area().width);
    let inner_w = usize::from(width).saturating_sub(2); // strip the two border columns
    let mut lines = vec![
        Line::from(truncate_to(name, inner_w)).style(Modifier::BOLD),
        Line::from(""),
    ];
    // Keep the header within the content width; a `partial` walk is flagged with a short "(partial)"
    // rather than a long sentence that would clip on the default box.
    lines.push(Line::from(if computing {
        "Calculating…".to_owned()
    } else if partial {
        "Total size (partial):".to_owned()
    } else {
        "Total size:".to_owned()
    }));
    lines.push(
        Line::from(format!("  {}  ({} bytes)", human_bytes(bytes), bytes)).style(Modifier::BOLD),
    );
    lines.push(Line::from(""));
    lines.push(Line::from(format!("  Files:   {files}")));
    lines.push(Line::from(format!("  Folders: {dirs}")));
    if partial && !computing {
        lines.push(Line::from("  (some entries unreadable — lower bound)"));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(if computing {
        "[Esc] cancel"
    } else {
        "[Esc] close"
    }));

    let h = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let area = centered(frame.area(), width, h);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(" Folder size ")
        .border_style(Style::default().fg(Color::Cyan));
    let content = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), content);
}

/// Width of the sweeping block on an indeterminate (unknown-total) bar.
const MARQUEE_BLOCK: usize = 3;

/// Render one MC-style text progress bar spanning exactly `width` columns: `█` for the filled
/// portion, `░` for the rest, followed by a right-aligned ` NN%` suffix.
///
/// When `pct` is `None` (the total isn't known — a delete, the pre-scan `Counting` phase, or a copy
/// with no pre-scan result) the bar is *indeterminate*: instead of sitting empty it renders a fixed
/// block that sweeps back and forth, positioned by `pulse` (a monotonic per-transfer tick), so it
/// visibly moves to signal activity. `pulse` is ignored for a determinate bar. The suffix is `--%`.
fn progress_bar(pct: Option<u64>, width: usize, pulse: u64) -> String {
    let suffix = match pct {
        Some(p) => format!(" {}%", p.min(100)),
        None => " --%".to_owned(),
    };
    // Reserve room for the suffix; always leave at least one column for the bar itself so a very
    // narrow dialog (e.g. 40-wide) still renders *something* rather than just the percentage.
    let bar_width = width.saturating_sub(suffix.chars().count()).max(1);
    let bar = match pct {
        Some(p) => {
            let filled = ((p.min(100) as usize) * bar_width) / 100;
            std::iter::repeat_n('█', filled)
                .chain(std::iter::repeat_n('░', bar_width - filled))
                .collect()
        }
        None => indeterminate_bar(bar_width, pulse),
    };
    format!("{bar}{suffix}")
}

/// The sweeping fill for an indeterminate bar: a `MARQUEE_BLOCK`-wide run of `█` that bounces left↔
/// right across `bar_width`, its left edge placed by `pulse` (one cell per progress tick). The motion
/// is a triangle wave (ping-pong), not a wrap, so the block never jump-cuts from the right edge back
/// to the left. The count of `█` is always `min(MARQUEE_BLOCK, bar_width)` regardless of `pulse`.
fn indeterminate_bar(bar_width: usize, pulse: u64) -> String {
    let block = MARQUEE_BLOCK.min(bar_width);
    let span = bar_width - block; // highest left-edge position (0..=span)
    let pos = if span == 0 {
        0 // bar too narrow to move: render a static block
    } else {
        let period = 2 * span;
        let phase = (pulse % period as u64) as usize;
        if phase <= span {
            phase
        } else {
            period - phase
        }
    };
    std::iter::repeat_n('░', pos)
        .chain(std::iter::repeat_n('█', block))
        .chain(std::iter::repeat_n('░', bar_width - pos - block))
        .collect()
}

/// The throughput rate (bytes/sec) and ETA (whole seconds) worth *displaying* for one transfer, or
/// `None` for each when it shouldn't be shown: a paused transfer, an unknown rate, or an ETA that
/// isn't meaningful (no known total, zero rate, or already complete / sub-second). Shared by the
/// progress dialog and the status line so the two can't drift on when a number appears.
fn transfer_rate_eta(t: &cairn_core::ActiveTransfer) -> (Option<u64>, Option<u64>) {
    // Rate/ETA are meaningful only while bytes are actually flowing: never while paused, and never in
    // the Counting (pre-scan) or Finalizing (flush/verify) phases where no bytes move.
    let flowing = !t.paused && t.phase == TransferPhase::Copying;
    let rate = match t.rate {
        Some(r) if flowing => Some(r),
        _ => None,
    };
    let eta = match (t.total, t.rate) {
        (Some(tot), Some(r)) if flowing && r > 0 && tot > t.bytes => {
            let secs = (tot - t.bytes) / r;
            (secs > 0).then_some(secs)
        }
        _ => None,
    };
    (rate, eta)
}

/// Draw a single-line text prompt (new directory, rename) with the entered text and a block cursor.
fn render_prompt(frame: &mut Frame, kind: &PromptKind, input: &str, theme: &Theme) {
    // 6 rows: 2 borders + 3 content lines + 1 of breathing space (matches the ConfirmDelete box).
    let area = centered(frame.area(), 50, 6);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(format!(" {} ", kind.title()))
        .border_style(Style::default().fg(Color::Cyan));
    let body = Paragraph::new(vec![
        // A `▏` block stands in for the cursor at the end of the field.
        Line::from(format!("{input}\u{258f}")),
        Line::from(""),
        Line::from("[Enter] OK    [Esc] Cancel"),
    ])
    .block(block)
    .alignment(Alignment::Center);
    frame.render_widget(body, area);
}

/// Draw the AI plan → confirm overlay: the summary, each step with its approval status and
/// reversibility, and the available actions (bulk-approve only when no step is irreversible).
fn render_ai_plan(frame: &mut Frame, plan: &Plan, cursor: usize, theme: &Theme) {
    let h = u16::try_from(plan.steps.len())
        .unwrap_or(u16::MAX)
        .saturating_add(6)
        .min(frame.area().height);
    let area = centered(frame.area(), 64, h);
    frame.render_widget(Clear, area);

    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(" AI plan — review before running ")
        .border_style(Style::default().fg(Color::Magenta));
    // Lay content out within the block's interior so nothing overwrites the border.
    let content = block.inner(area);
    frame.render_widget(block, area);

    let [summary_area, steps_area, help_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(content);

    frame.render_widget(
        Paragraph::new(Line::from(plan.summary.clone()))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        summary_area,
    );

    let items: Vec<ListItem> = plan
        .steps
        .iter()
        .map(|s| {
            let (marker, color) = match s.status {
                StepStatus::Pending => ('·', Color::Gray),
                StepStatus::Approved => ('✓', Color::Green),
                StepStatus::Rejected => ('✗', Color::Red),
                StepStatus::Done => ('●', Color::Cyan),
                StepStatus::Failed => ('!', Color::Red),
            };
            let rev = match s.capability.reversibility {
                Reversibility::Safe => "safe",
                Reversibility::Recoverable => "recoverable",
                Reversibility::Irreversible => "IRREVERSIBLE",
            };
            let line = format!(
                "{marker} {:<8} [{rev}]  {}",
                verb_label(s.capability.verb),
                s.description
            );
            ListItem::new(line).style(Style::default().fg(color))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Magenta).fg(Color::Black))
        .highlight_symbol("▶ ");
    let mut list_state = ListState::default();
    if !plan.steps.is_empty() {
        list_state.select(Some(cursor.min(plan.steps.len() - 1)));
    }
    frame.render_stateful_widget(list, steps_area, &mut list_state);

    let help = if plan.can_bulk_approve() {
        "↵ approve · a approve all · x reject · esc abort"
    } else {
        "↵ approve · x reject · esc abort · no bulk (irreversible)"
    };
    frame.render_widget(
        Paragraph::new(Line::from(help)).style(Style::default().fg(Color::Gray)),
        help_area,
    );
}

/// A short label for a tool verb shown in the plan overlay.
fn verb_label(verb: Verb) -> &'static str {
    match verb {
        Verb::List => "list",
        Verb::Stat => "stat",
        Verb::Read => "read",
        Verb::Copy => "copy",
        Verb::Move => "move",
        Verb::Delete => "delete",
        Verb::Exec => "exec",
        Verb::OpenConnection => "connect",
    }
}

/// Draw the log-stream viewer overlay: a scrollable list of buffered log lines plus a status
/// indicator (`Streaming…` / `Done` / `Error: …`) in the border title.
///
/// `scroll` is the 0-based index of the topmost visible line (managed by the reducer). When
/// `follow` is true the reducer keeps it pinned to the last page; any scroll-up disables it.
fn render_log_viewer(
    frame: &mut Frame,
    title: &str,
    lines: &std::collections::VecDeque<String>,
    follow: bool,
    scroll: usize,
    status: &LogViewerStatus,
    theme: &Theme,
) {
    let area = centered(
        frame.area(),
        80,
        frame.area().height.saturating_sub(2).max(3),
    );
    frame.render_widget(Clear, area);

    let status_label = match status {
        LogViewerStatus::Streaming => " Streaming… ".to_owned(),
        LogViewerStatus::Done => " Done ".to_owned(),
        LogViewerStatus::Error(msg) => format!(" Error: {msg} "),
    };
    let follow_hint = if follow { " [follow] " } else { "" };
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(format!(" {} ", title))
        .title_bottom(Line::from(format!("{follow_hint}{status_label}")).right_aligned())
        .border_style(Style::default().fg(Color::Cyan));

    // Viewport: subtract 2 for the borders.
    let viewport = usize::from(area.height.saturating_sub(2));
    // When following, ignore scroll and compute the last page directly so we always fill the
    // viewport — otherwise scroll=last_line_idx would render only 1 visible line.
    let top = if follow {
        lines.len().saturating_sub(viewport)
    } else {
        scroll.min(lines.len().saturating_sub(1))
    };
    let end = (top + viewport).min(lines.len());
    let visible: Vec<ListItem> = lines
        .iter()
        .skip(top)
        .take(end.saturating_sub(top))
        .map(|l| ListItem::new(l.as_str()))
        .collect();

    frame.render_widget(List::new(visible).block(block), area);
}

/// Draw the read-only file pager overlay (`F3` / `Enter` on a file — RFC-0012 P1): a scrollable
/// view of the buffered content in `Text` or `Hex` mode, plus a status indicator and a
/// line/percentage position in the border.
///
/// `scroll` is the 0-based index of the topmost visible *stored* line/row (managed by the
/// reducer). In `Text` mode with `wrap` enabled, a long stored line can still occupy more than one
/// terminal row inside the viewport — the visible window is a windowed approximation, not an
/// exact line-for-row mapping, matching the log viewer's existing trade-off for the same reason:
/// precise re-flow bookkeeping would require the pure reducer to know the render width, which it
/// must not depend on.
#[allow(clippy::too_many_arguments)]
fn render_pager(
    frame: &mut Frame,
    title: &str,
    mode: PagerMode,
    lines: &std::collections::VecDeque<String>,
    byte_size: usize,
    total_size: Option<u64>,
    scroll: usize,
    status: &PagerStatus,
    wrap: bool,
    theme: &Theme,
) {
    let area = centered(
        frame.area(),
        80,
        frame.area().height.saturating_sub(2).max(3),
    );
    frame.render_widget(Clear, area);

    let status_label = match status {
        PagerStatus::Loading => " Loading… ".to_owned(),
        PagerStatus::Ready => " Ready ".to_owned(),
        PagerStatus::Truncated => {
            format!(
                " Truncated — showing first {} ",
                human_bytes(byte_size as u64)
            )
        }
        PagerStatus::Error(msg) => format!(" Error: {msg} "),
    };
    // Position: `line/total (pct%)` when the entry's size is known, else just `line/total`.
    let total_lines = lines.len().max(1);
    let current_line = scroll.min(total_lines - 1) + 1;
    let position = match total_size {
        Some(total) if total > 0 => {
            format!(
                " {current_line}/{total_lines} ({}%) ",
                pct_of(byte_size as u64, total)
            )
        }
        _ => format!(" {current_line}/{total_lines} "),
    };
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(format!(" {title} "))
        .title_bottom(Line::from(status_label).right_aligned())
        .title_bottom(Line::from(position).left_aligned())
        .border_style(Style::default().fg(Color::Cyan));

    let viewport = usize::from(area.height.saturating_sub(2));
    let top = scroll.min(lines.len().saturating_sub(1));
    let end = (top + viewport).min(lines.len());

    match mode {
        PagerMode::Text => {
            let text: Vec<Line> = lines
                .iter()
                .skip(top)
                .take(end.saturating_sub(top))
                .map(|l| Line::from(l.as_str()))
                .collect();
            let mut p = Paragraph::new(text).block(block);
            if wrap {
                p = p.wrap(Wrap { trim: false });
            }
            frame.render_widget(p, area);
        }
        PagerMode::Hex => {
            let items: Vec<ListItem> = lines
                .iter()
                .skip(top)
                .take(end.saturating_sub(top))
                .enumerate()
                .map(|(i, raw)| {
                    let offset = (top + i) * PAGER_HEX_ROW_BYTES;
                    ListItem::new(format_hex_row(offset, raw))
                })
                .collect();
            frame.render_widget(List::new(items).block(block), area);
        }
    }
}

/// Decode one raw pager row — [`PAGER_HEX_ROW_BYTES`] bytes stored one `char` per byte (see
/// `cairn_core`'s pager row-assembly convention) — into an `offset | hex | ascii` display line.
/// A short trailing row (fewer than [`PAGER_HEX_ROW_BYTES`] bytes) pads the hex column with
/// spaces so the ascii column still lines up.
fn format_hex_row(offset: usize, raw: &str) -> String {
    let bytes: Vec<u8> = raw.chars().map(|c| c as u32 as u8).collect();
    let mut hex = String::with_capacity(PAGER_HEX_ROW_BYTES * 3);
    for i in 0..PAGER_HEX_ROW_BYTES {
        if i > 0 {
            hex.push(' ');
        }
        match bytes.get(i) {
            Some(b) => hex.push_str(&format!("{b:02x}")),
            None => hex.push_str("  "),
        }
    }
    let ascii: String = bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{offset:08x}  {hex}  {ascii}")
}

/// A centered rect of at most `w`×`h`, clamped to `area`.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn render_pane(frame: &mut Frame, area: Rect, state: &AppState, side: Side, theme: &Theme) {
    let pane = state.pane(side);
    let focused = state.focus == side;
    let border = if focused {
        theme.focused_border
    } else {
        theme.unfocused_border
    };
    // Top title: the pane's connection-aware location. Remote backends render a full
    // `scheme://user@host:path` locator in an accent color so the user can tell which connection a
    // pane is on and distinguish it from the local filesystem (which shows just the path).
    let location = state.pane_location(side);
    let title_style = if location.is_remote {
        Style::default().fg(theme.remote)
    } else {
        Style::default().fg(border)
    };
    let title = Line::from(Span::styled(format!(" {} ", location.text), title_style));
    // Bottom-right status: current sort mode, plus a `+hidden` flag when dotfiles are shown.
    let hidden = if pane.show_hidden { " +hidden" } else { "" };
    let status = format!(" sort: {}{hidden} ", pane.sort.label());
    // No `overlay_base` here: a pane isn't an overlay — it already sits on the theme background painted
    // over the whole frame at the top of `render()`, so it needs no fill of its own.
    let mut block = Block::bordered()
        .title(title)
        .title_bottom(Line::from(status).right_aligned())
        .border_style(Style::default().fg(border));
    // Bottom-left: the active filter (a trailing `_` marks live editing).
    if let Some(f) = &pane.filter {
        let cursor = if pane.filter_editing { "_" } else { "" };
        block = block.title_bottom(Line::from(format!(" filter: {f}{cursor} ")).left_aligned());
    }

    match &pane.listing {
        Listing::Loading => {
            frame.render_widget(Paragraph::new("Loading…").block(block), area);
        }
        Listing::Error(msg) => {
            let p = Paragraph::new(Line::from(format!("error: {msg}")))
                .style(Style::default().fg(theme.error))
                .block(block);
            frame.render_widget(p, area);
        }
        Listing::Ready(_) => {
            // Render the visible (filtered) view; cursor and marks index into it. Only the on-screen
            // window of rows is materialized into `ListItem`s (virtualization), so a 100k-entry
            // directory costs O(viewport), not O(entries), per frame.
            let visible = pane.visible();
            let rows = usize::from(area.height.saturating_sub(2)); // minus top/bottom borders
            let top = list_window(pane.cursor, visible.len(), rows);
            let end = top.saturating_add(rows).min(visible.len());
            // Row content width = pane interior minus the 2-col highlight-symbol gutter the List
            // reserves for the selection. The name fills the left; permission + date columns are
            // right-aligned and drop out responsively on a narrow pane (see `entry_columns`).
            let row_w = usize::from(area.width.saturating_sub(4));
            let items: Vec<ListItem> = visible[top..end]
                .iter()
                .enumerate()
                .map(|(offset, e)| {
                    let i = top + offset; // index back into the visible view (marks are absolute)
                    let mark = if pane.marked.contains(&i) { '*' } else { ' ' };
                    let suffix = if e.is_dir() { "/" } else { "" };
                    let name_style = entry_style(e, theme);
                    let perms = format_perms(e.kind, e.perms);
                    let date = e.modified.map(format_mtime).unwrap_or_default();
                    let cols = entry_columns(&perms, &date, row_w);
                    let name_w = row_w.saturating_sub(cols.chars().count());
                    let namepart = truncate_to(&format!("{mark}{}{suffix}", e.name), name_w);
                    let pad = " ".repeat(name_w.saturating_sub(namepart.chars().count()));
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{namepart}{pad}"), name_style),
                        Span::styled(cols, Style::default().fg(theme.status)),
                    ]))
                })
                .collect();

            // Both panes get an explicit bg+fg cursor bar (not `REVERSED`): now that every entry has
            // its own fg color, a bare `REVERSED` would swap each cell's color through and produce a
            // ragged multicolor bar on the inactive pane. The focused pane uses the bright selection
            // fill; the unfocused pane uses a dimmer (`unfocused_border`) fill so the active pane
            // stays obvious.
            let highlight = if focused {
                Style::default()
                    .bg(theme.selection_bg)
                    .fg(theme.selection_fg)
            } else {
                Style::default()
                    .bg(theme.unfocused_border)
                    .fg(theme.selection_fg)
            };
            let list = List::new(items)
                .block(block)
                .highlight_style(highlight)
                .highlight_symbol("> ");

            let mut list_state = ListState::default();
            if !visible.is_empty() {
                // Selection is relative to the windowed slice.
                list_state.select(Some(pane.cursor.saturating_sub(top)));
            }
            frame.render_stateful_widget(list, area, &mut list_state);
        }
    }
}

/// The first visible-view index to render so the cursor stays on screen, given the viewport `rows`.
///
/// Keeps the cursor roughly centred and clamps so the last page fills the viewport (no blank space
/// past the end). Stateless — derived from the cursor each frame, so no scroll offset to persist.
fn list_window(cursor: usize, total: usize, rows: usize) -> usize {
    if rows == 0 || total <= rows {
        return 0;
    }
    let half = rows / 2;
    cursor.saturating_sub(half).min(total - rows)
}

/// Join `tokens` with ` · ` separators, stopping before the result would exceed `width` columns — so
/// the hint bar truncates at whole-token boundaries rather than mid-word. (Same approach as
/// [`connections_hint`].)
fn fit_tokens(tokens: &[&str], width: usize) -> String {
    let mut out = String::new();
    for tok in tokens {
        let added = if out.is_empty() {
            tok.chars().count()
        } else {
            out.chars().count() + 3 + tok.chars().count()
        };
        if added > width {
            break;
        }
        if !out.is_empty() {
            out.push_str(" · ");
        }
        out.push_str(tok);
    }
    out
}

/// The bottom status/menu bar (two rows). Row 1 shows the entry count and, in priority order, a live
/// transfer (single or aggregate), a transient status message, or the first line of key hints. Row 2
/// always shows the second line of key hints — so the menu is always at least partly visible, and a
/// full two-row key reference shows when idle. Each hint row fits whole tokens to the width.
fn render_status(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Two rows of key hints (MC-style), kept in sync with `keymap::action_for`. Ordered by priority
    // so the least-important tokens are the first to drop on a narrow terminal.
    const HELP_1: &[&str] = &[
        "q quit",
        "Tab",
        "↵ open",
        "v view",
        "e edit",
        "r rename",
        "c copy",
        "m move",
        "d del",
        "Space mark",
    ];
    const HELP_2: &[&str] = &[
        "^S size",
        "F7 mkdir",
        "/ filter",
        "^R refresh",
        "p pause",
        "s sort",
        ". hidden",
        "T theme",
        "^O conn",
        "^T xfer",
        "^A ai",
    ];

    let pane = state.active();
    let count = pane_count_label(pane);
    let prefix_w = 1 + count.chars().count() + 3; // ` {count}   `
    let avail = usize::from(area.width).saturating_sub(prefix_w);
    let row1_content = if !state.active_transfers.is_empty() {
        transfer_status(state)
    } else if let Some(msg) = &state.status {
        msg.clone()
    } else {
        fit_tokens(HELP_1, avail)
    };
    let line1 = format!(" {count}   {row1_content}");
    // Indent row 2's hints to line up under row 1's content.
    let indent = " ".repeat(prefix_w);
    let line2 = format!("{indent}{}", fit_tokens(HELP_2, avail));
    frame.render_widget(
        Paragraph::new(vec![Line::from(line1), Line::from(line2)])
            .style(Style::default().fg(theme.status)),
        area,
    );
}

/// The transfer segment of the status line. One active transfer renders exactly as before
/// (`⇅ X / Y (Z%) at R/s, ETA Ns`); multiple render an aggregate (`⇅ N active · …`). A `(+K queued)`
/// suffix is appended when transfers wait for a free slot.
fn transfer_status(state: &AppState) -> String {
    let active = &state.active_transfers;
    let queued = state.transfer_queue.len();
    let suffix = if queued > 0 {
        format!(" (+{queued} queued)")
    } else {
        String::new()
    };
    let paused = active.iter().filter(|t| t.paused).count();

    // Aggregate byte/total/rate across active transfers. Exclude non-byte ops (delete counts items,
    // not bytes) and a transfer still in `Counting` (its `bytes` is scan-discovered, not transferred)
    // — otherwise the footer would inflate the byte total with counts nothing has actually copied.
    // `total` is `None` if any is unknown (a partial percentage would mislead).
    let bytes: u64 = active
        .iter()
        .filter(|t| t.kind.counts_bytes() && t.phase != TransferPhase::Counting)
        .fold(0u64, |acc, t| acc.saturating_add(t.bytes));
    // Exclude deletes here too: a delete's `total` is always `None`, and an un-filtered `try_fold`
    // would short-circuit the whole aggregate to `None` — hiding a concurrent copy's percentage.
    let total: Option<u64> = active
        .iter()
        .filter(|t| t.kind.counts_bytes())
        .try_fold(0u64, |acc, t| t.total.map(|x| acc.saturating_add(x)));
    let amount = match total {
        Some(total) if total > 0 => {
            format!(
                "{} / {} ({}%)",
                human_bytes(bytes),
                human_bytes(total),
                pct_of(bytes, total)
            )
        }
        _ => human_bytes(bytes),
    };

    if active.len() == 1 {
        // Single transfer: identical format to the pre-concurrency status line.
        let t = &active[0];
        if t.kind == OpKind::Delete {
            // Delete moves no bytes — show the live item count instead of a byte/rate line.
            return format!("Deleting… {} items{suffix}", t.scan_entries);
        }
        if t.phase == TransferPhase::Counting {
            // Pre-flight scan: no bytes are moving yet, so say what's actually happening rather than
            // "transferring… 0 B" (which reads as a stall).
            return format!("⇅ scanning… {} items{suffix}", t.scan_entries);
        }
        if t.paused {
            return format!("⏸ paused {amount}{suffix}");
        }
        // Same rate/ETA gating as the progress dialog (`transfer_rate_eta`); `t` is non-paused here
        // (the paused case returned above), just with the status-line's ` at …`/`, ETA …` phrasing.
        let (rate_bps, eta_secs) = transfer_rate_eta(t);
        let rate = rate_bps
            .map(|r| format!(" at {}/s", human_bytes(r)))
            .unwrap_or_default();
        let eta = eta_secs
            .map(|s| format!(", ETA {}", human_duration(s)))
            .unwrap_or_default();
        return format!("⇅ transferring… {amount}{rate}{eta}{suffix}");
    }

    // Multiple transfers: a compact aggregate (per-transfer detail is in the Ctrl-T overlay).
    let n = active.len();
    if paused == n {
        return format!("⏸ {n} paused · {amount}{suffix}");
    }
    // Only transfers actually moving bytes contribute to the rate: a paused, scanning, or finalizing
    // transfer keeps a stale `rate` field, and counting it would show a phantom throughput. Mirrors
    // the single-transfer `transfer_rate_eta` gate.
    let running_rate: u64 = active
        .iter()
        .filter(|t| t.phase == TransferPhase::Copying && !t.paused)
        .filter_map(|t| t.rate)
        .sum();
    let rate = if running_rate > 0 {
        format!(" at {}/s", human_bytes(running_rate))
    } else {
        String::new()
    };
    let head = if paused > 0 {
        format!("⇅ {n} active, {paused} paused")
    } else {
        format!("⇅ {n} active")
    };
    format!("{head} · {amount}{rate}{suffix}")
}

/// Integer percentage of `bytes` out of `total`, clamped to `[0, 100]`. Caller ensures `total > 0`.
fn pct_of(bytes: u64, total: u64) -> u64 {
    ((bytes as f64 / total as f64) * 100.0).min(100.0) as u64
}

/// Format a duration in seconds compactly (`45s`, `3m12s`, `1h05m`).
fn human_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Format a byte count compactly (`512 B`, `3.4 KiB`, `1.2 GiB`).
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    // Guard the unit boundary: `{:.1}` could round e.g. 1023.97 KiB up to "1024.0 KiB"; bump to the
    // next unit so it reads "1.0 MiB" instead.
    if unit < UNITS.len() - 1 && (v * 10.0).round() >= 10240.0 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// The display style for a listing entry, keyed off its kind so the *type* reads at a glance: blue
/// folders, amber archives (by extension), green executables, cyan symlinks, purple streams, red
/// specials. A hidden (`.`-prefixed) directory or plain file uses the dimmed variant of its color so
/// it recedes but still reads as its kind. Directories/archives/executables are bold, symlinks
/// italic. The `..` navigation sentinel styles as an ordinary directory.
fn entry_style(e: &Entry, theme: &Theme) -> Style {
    let hidden = e.name.starts_with('.') && !e.is_dotdot_sentinel();
    match e.kind {
        EntryKind::Dir => Style::default()
            .fg(if hidden { theme.hidden_dir } else { theme.dir })
            .add_modifier(Modifier::BOLD),
        EntryKind::Symlink => Style::default()
            .fg(theme.symlink)
            .add_modifier(Modifier::ITALIC),
        EntryKind::Stream => Style::default().fg(theme.stream),
        EntryKind::Special => Style::default().fg(theme.special),
        EntryKind::File => {
            // An executable bit wins over an archive extension (a runnable file is the more useful
            // signal); a hidden plain file dims. Object stores expose no perms, so exec never trips
            // there.
            if e.perms.is_some_and(|p| p.mode & 0o111 != 0) {
                Style::default()
                    .fg(theme.executable)
                    .add_modifier(Modifier::BOLD)
            } else if is_archive_name(&e.name) {
                Style::default()
                    .fg(theme.archive)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(if hidden {
                    theme.hidden_file
                } else {
                    theme.file
                })
            }
        }
    }
}

/// Whether a filename looks like an archive, by extension — for **coloring only**. `Enter` still
/// detects real archives by magic bytes (`detect_file_kind`), so a mis-named file is never mounted
/// on the strength of its extension; this only tints the row.
fn is_archive_name(name: &str) -> bool {
    const EXTS: &[&str] = &[
        ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".tar.zst", ".tzst",
        ".zip", ".gz", ".bz2", ".xz", ".zst", ".7z", ".rar",
    ];
    let lower = name.to_ascii_lowercase();
    EXTS.iter().any(|ext| lower.ends_with(ext))
}

/// Format an entry's permissions MC-style as a 10-char `drwxr-xr-x`: a leading type character (from
/// the entry `kind`, since the raw mode may not carry the file-type bits) followed by the `rwxrwxrwx`
/// user/group/other bits, with the standard `ls -l` set-uid/set-gid/sticky substitutions in the
/// execute positions (`s`/`S` for set-uid/set-gid, `t`/`T` for sticky; uppercase when the underlying
/// execute bit is off). Returns an empty string for a backend with no permission model (object
/// stores) so the column renders blank rather than a misleading `---------`.
fn format_perms(kind: EntryKind, perms: Option<UnixPerms>) -> String {
    let Some(p) = perms else {
        return String::new();
    };
    let type_char = match kind {
        EntryKind::Dir => 'd',
        EntryKind::Symlink => 'l',
        EntryKind::Special => 's',
        EntryKind::Stream => 'p',
        EntryKind::File => '-',
    };
    let m = p.mode;
    let on = |mask: u32| m & mask != 0;
    // The execute-position char, folding in a set-id/sticky bit: `set` (lowercase) when the execute
    // bit is also on, `unset` (uppercase) when it isn't, else the usual `x`/`-`.
    let exec = |x_mask: u32, special: bool, set: char, unset: char| -> char {
        match (special, on(x_mask)) {
            (true, true) => set,
            (true, false) => unset,
            (false, true) => 'x',
            (false, false) => '-',
        }
    };
    let mut s = String::with_capacity(10);
    s.push(type_char);
    s.push(if on(0o400) { 'r' } else { '-' });
    s.push(if on(0o200) { 'w' } else { '-' });
    s.push(exec(0o100, on(0o4000), 's', 'S')); // set-uid
    s.push(if on(0o040) { 'r' } else { '-' });
    s.push(if on(0o020) { 'w' } else { '-' });
    s.push(exec(0o010, on(0o2000), 's', 'S')); // set-gid
    s.push(if on(0o004) { 'r' } else { '-' });
    s.push(if on(0o002) { 'w' } else { '-' });
    s.push(exec(0o001, on(0o1000), 't', 'T')); // sticky
    s
}

/// Format a last-modified time as a `YYYY-MM-DD` **UTC** date. Deterministic and clock-free (a pure
/// function of the timestamp) so `render` stays pure and snapshots are stable regardless of the host
/// timezone. UTC (rather than local) is a deliberate tradeoff for that determinism; date-only keeps
/// the UTC-vs-local gap to a narrow near-midnight window. See the `format_mtime` tests.
fn format_mtime(t: SystemTime) -> String {
    let secs = match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    };
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `(year, month, day)` in UTC from a day count relative to the Unix epoch (1970-01-01 = 0), via
/// Howard Hinnant's well-known `civil_from_days` algorithm. Valid across the full range of practical
/// file timestamps.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Minimum columns reserved for an entry's name before the metadata columns may appear.
const ENTRY_NAME_MIN: usize = 12;

/// Truncate `s` to at most `max` **char** positions, appending `…` when it doesn't fit.
///
/// KNOWN LIMITATION: this (and the row padding in `render_pane`) counts codepoints, not terminal
/// display columns, so a filename with wide (CJK) or zero-width glyphs can shift the metadata
/// columns' alignment by a few cells. Cosmetic only — the buffer still clips safely and never
/// panics. Making it display-width-aware (via `unicode-width`) is a tracked follow-up.
fn truncate_to(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Like [`truncate_to`] but keeps the **tail**, prefixing a leading `…` — for a long path where the
/// meaningful part is the end (`…/fusedpro/functions/foo.js`), not the common `/home/user` prefix.
fn truncate_left(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    // Keep the last `max - 1` chars, prefixed by the ellipsis.
    let tail: String = s.chars().skip(len - (max - 1)).collect();
    format!("…{tail}")
}

/// The right-hand metadata columns (` perms  date`) that fit in a row of `row_w` columns while
/// leaving at least [`ENTRY_NAME_MIN`] for the name. Drops columns responsively: both when there's
/// room, then date-only, then perms-only, then nothing on a very narrow pane. Each present column is
/// prefixed by a single-space gap; empty inputs (a backend with no perms / no mtime) are skipped.
fn entry_columns(perms: &str, date: &str, row_w: usize) -> String {
    let candidate = |cols: &[&str]| -> Option<String> {
        let s: String = cols
            .iter()
            .filter(|c| !c.is_empty())
            .map(|c| format!(" {c}"))
            .collect();
        (!s.is_empty() && ENTRY_NAME_MIN + s.chars().count() <= row_w).then_some(s)
    };
    candidate(&[perms, date])
        .or_else(|| candidate(&[date]))
        .or_else(|| candidate(&[perms]))
        .unwrap_or_default()
}

fn pane_count_label(pane: &PaneState) -> String {
    match &pane.listing {
        Listing::Ready(_) => {
            // Compute the visible count once — `len()` allocates while a filter is active.
            let n = pane.len();
            format!("{}/{}", pane.cursor.saturating_add(1).min(n.max(1)), n)
        }
        Listing::Loading => "…".to_owned(),
        Listing::Error(_) => "!".to_owned(),
    }
}

/// Draw the interactive exec-session pane: a scrollable output buffer (stdout/stderr combined) and
/// a single-line input field at the bottom for cooked-mode (non-TTY) line input.
///
/// The layout mirrors the log viewer: the same 80-column-wide, nearly full-height area, but with an
/// extra input row pinned at the bottom. `scroll` is the 0-based topmost visible line; `follow`
/// keeps it pinned to the last page when new output arrives.
fn render_exec_pane(
    frame: &mut Frame,
    rec: &SessionRecord,
    input: &str,
    scroll: usize,
    follow: bool,
    theme: &Theme,
) {
    let area = centered(
        frame.area(),
        80,
        frame.area().height.saturating_sub(2).max(3),
    );
    frame.render_widget(Clear, area);

    // Status suffix: show exit code / error when the session has ended.
    let status_label = match &rec.ended {
        None => " Running ".to_owned(),
        Some(SessionEnd {
            exit_code: Some(0),
            error: None,
        }) => " Exited (0) ".to_owned(),
        Some(SessionEnd {
            exit_code: Some(n),
            error: None,
        }) => format!(" Exited ({n}) "),
        Some(SessionEnd { error: Some(e), .. }) => format!(" Error: {e} "),
        Some(_) => " Ended ".to_owned(),
    };
    let follow_hint = if follow { " [follow] " } else { "" };
    let hint_bottom = if rec.ended.is_none() {
        " [Enter] send  [^D] close stdin  [^]] detach  [^C] quit "
    } else {
        " [Esc] close "
    };
    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(format!(" {} ", rec.title))
        .title_bottom(Line::from(format!("{follow_hint}{status_label}")).right_aligned())
        .title_bottom(Line::from(hint_bottom).left_aligned())
        .border_style(Style::default().fg(Color::Green));

    // Viewport: subtract 2 for borders + 1 for the input row.
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let output_rows = inner.height.saturating_sub(1);
    let viewport = usize::from(output_rows);
    let lines = &rec.output_lines;
    // Total virtual line count: complete lines + 1 if there is a partial (unterminated) line.
    let partial_line = if rec.output_partial.is_empty() {
        None
    } else {
        Some(rec.output_partial.as_str())
    };
    let total_virtual = lines.len() + if partial_line.is_some() { 1 } else { 0 };
    let top = if follow {
        total_virtual.saturating_sub(viewport)
    } else {
        scroll.min(total_virtual.saturating_sub(1))
    };
    let end = (top + viewport).min(total_virtual);
    let line_end = end.min(lines.len());
    let mut visible: Vec<ListItem> = lines
        .iter()
        .skip(top)
        .take(line_end.saturating_sub(top))
        .map(|l| ListItem::new(l.as_str()))
        .collect();
    // If the partial line falls in the visible range, append it dimmed.
    if end > lines.len() {
        if let Some(p) = partial_line {
            visible.push(ListItem::new(p).style(Style::default().fg(Color::DarkGray)));
        }
    }
    let output_area = Rect {
        height: output_rows,
        ..inner
    };
    frame.render_widget(List::new(visible), output_area);

    // Input row pinned at the bottom of the inner area.
    let input_area = Rect {
        y: inner.y + output_rows,
        height: 1,
        ..inner
    };
    // `▏` block cursor at the end of the field (same pattern as `render_prompt`).
    frame.render_widget(
        Paragraph::new(Line::from(format!("> {input}\u{258f}"))),
        input_area,
    );
}

/// Draw the port-forward status overlay: shows the title, the bound local port (or "binding…"
/// until [`AppEvent::PortForwardBound`] arrives), and the ended state if applicable.
fn render_port_forward_status(frame: &mut Frame, rec: &SessionRecord, theme: &Theme) {
    // 7 rows: 2 borders + title/port row + blank + status + blank + hint.
    let area = centered(frame.area(), 56, 7);
    frame.render_widget(Clear, area);

    let block = Block::bordered()
        .style(overlay_base(theme))
        .title(format!(" {} ", rec.title))
        .border_style(Style::default().fg(Color::Cyan));

    let port_line = match rec.local_port {
        Some(p) => format!("Forwarding local port {p}"),
        None => "Binding…".to_owned(),
    };
    let status_line = match &rec.ended {
        None => String::new(),
        Some(SessionEnd {
            exit_code: Some(0),
            error: None,
        }) => "Closed cleanly".to_owned(),
        Some(SessionEnd { error: Some(e), .. }) => format!("Error: {e}"),
        Some(_) => "Ended".to_owned(),
    };
    let hint = if rec.ended.is_none() {
        "[Esc] close forward"
    } else {
        "[Esc] dismiss"
    };
    let body = Paragraph::new(vec![
        Line::from(port_line),
        Line::from(""),
        Line::styled(status_line, Style::default().fg(Color::Yellow)),
        Line::from(""),
        Line::from(hint),
    ])
    .block(block)
    .alignment(Alignment::Center);
    frame.render_widget(body, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_types::{ConnectionId, Entry, EntryKind, VfsPath};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn ready_state() -> AppState {
        let mut s = AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root());
        let entries = Arc::new(vec![
            Entry::new("src", EntryKind::Dir),
            Entry::new("README.md", EntryKind::File),
        ]);
        s.panes[0].listing = Listing::Ready(entries);
        s
    }

    /// Push an active transfer with the given progress, for status-line/overlay render tests. Mints a
    /// fresh id from `next_transfer_id` so repeated pushes get distinct ids (mirrors the reducer).
    fn set_transfer(
        s: &mut AppState,
        bytes: u64,
        rate: Option<u64>,
        total: Option<u64>,
        paused: bool,
    ) {
        let id = s.next_transfer_id;
        s.next_transfer_id += 1;
        s.active_transfers.push(cairn_core::ActiveTransfer {
            id,
            kind: OpKind::Copy,
            label: "Copying 1 item(s)…".to_owned(),
            // These helpers model a live byte copy (rate/ETA/percentage assertions), so the phase is
            // Copying, not the initial Counting.
            phase: TransferPhase::Copying,
            scan_entries: 0,
            scan_path: String::new(),
            bytes,
            rate,
            total,
            paused,
            pulse: 0,
        });
    }

    #[test]
    fn renders_without_panicking_and_shows_entries() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = ready_state();
        terminal
            .draw(|f| render(f, &state, &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("README.md"));
        assert!(text.contains("src"));
        assert!(text.contains("quit"));
    }

    #[test]
    fn themed_background_paints_cells_without_changing_glyphs() {
        // The base-background fill must be purely a color change: it paints every cell's background
        // but never alters a glyph — which is why a theme swap can't move a `.snap` (the snapshot
        // harness records only `cell.symbol()`). This locks both halves of that contract.
        let state = ready_state();
        let render_with = |theme: &Theme| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|f| render(f, &state, theme)).unwrap();
            terminal.backend().buffer().clone()
        };
        let plain = render_with(&Theme::DARK); // background: None
        let themed = render_with(&Theme {
            background: Some(Color::Blue),
            ..Theme::DARK
        });

        // (a) Every glyph is identical → a background can never move a snapshot.
        let glyphs = |b: &ratatui::buffer::Buffer| -> String {
            b.content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect()
        };
        assert_eq!(
            glyphs(&plain),
            glyphs(&themed),
            "fill must not touch glyphs"
        );

        // (b) …but the fill actually did something: at least one cell's background differs.
        let changed = plain
            .content()
            .iter()
            .zip(themed.content().iter())
            .any(|(p, t)| p.style().bg != t.style().bg);
        assert!(changed, "themed background must paint at least one cell");
    }

    #[test]
    fn overlays_pick_up_the_theme_background() {
        // A dialog must match the active theme, not the terminal default that its `Clear` would leave.
        // The overlay's own block fills its interior with the theme background; assert a cell on the
        // dialog's title row carries it.
        let mut s = ready_state();
        s.overlay = Some(cairn_core::Overlay::Prompt {
            kind: cairn_core::PromptKind::MakeDir,
            input: "x".to_owned(),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    &s,
                    &Theme {
                        background: Some(Color::Blue),
                        ..Theme::DARK
                    },
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let width = usize::from(buf.area().width);
        let cells: Vec<_> = buf.content().iter().collect();
        let mut themed = false;
        for row in cells.chunks(width) {
            let text: String = row.iter().map(|c| c.symbol()).collect();
            if text.contains("New directory") {
                themed = row.iter().any(|c| c.style().bg == Some(Color::Blue));
                break;
            }
        }
        assert!(
            themed,
            "the overlay's interior must be painted with the theme background"
        );
    }

    #[test]
    fn transfer_dialog_highlights_only_the_selected_label_line() {
        // The selected transfer marks only its *label* row, and with bold — never reverse-video,
        // which paints a background block that looks like the filled progress bar. The bar/stats rows
        // stay in the normal style. The snapshot harness records only glyphs, so assert on styles.
        let mut s = ready_state();
        set_transfer(
            &mut s,
            2 * 1024 * 1024,
            Some(1024),
            Some(8 * 1024 * 1024),
            false,
        );
        s.overlay = Some(cairn_core::Overlay::TransferQueue { cursor: 0 });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &s, &Theme::default())).unwrap();
        let buf = terminal.backend().buffer().clone();

        let width = usize::from(buf.area().width);
        // For the row containing `needle`, return `(any bold cell, any reverse-video cell)`.
        let row_style = |needle: &str| -> Option<(bool, bool)> {
            let cells: Vec<_> = buf.content().iter().collect();
            for row in cells.chunks(width) {
                let text: String = row.iter().map(|c| c.symbol()).collect();
                if text.contains(needle) {
                    let bold = row
                        .iter()
                        .any(|c| c.style().add_modifier.contains(Modifier::BOLD));
                    let rev = row
                        .iter()
                        .any(|c| c.style().add_modifier.contains(Modifier::REVERSED));
                    return Some((bold, rev));
                }
            }
            None
        };
        // The label row ("Copying …") is bold and NOT reverse-video…
        assert_eq!(
            row_style("Copying"),
            Some((true, false)),
            "the selected label must be bold, not reverse-video"
        );
        // …and the progress-bar row is neither bold nor reversed (normal style).
        assert_eq!(
            row_style("░"),
            Some((false, false)),
            "the progress bar must render in the normal style"
        );
    }

    fn plan(tools: &[&str]) -> cairn_ai::Plan {
        use cairn_ai::{capability_for, Plan, PlanState, PlanStep, StepStatus};
        let steps = tools
            .iter()
            .map(|t| PlanStep {
                tool: (*t).to_owned(),
                input: serde_json::Value::Null,
                description: format!("{t} the things"),
                capability: capability_for(t).unwrap(),
                status: StepStatus::Pending,
                error: None,
                output: None,
            })
            .collect();
        Plan {
            summary: "archive old logs".to_owned(),
            steps,
            state: PlanState::Proposed,
        }
    }

    fn render_text(state: &AppState, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| render(f, state, &Theme::default()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn ai_plan_overlay_shows_steps_and_bulk_approve_for_safe_plan() {
        let mut s = ready_state();
        s.overlay = Some(cairn_core::Overlay::AiPlan {
            plan: plan(&["list", "copy"]),
            cursor: 0,
        });
        let text = render_text(&s, 80, 24);
        assert!(text.contains("AI plan"));
        assert!(text.contains("archive old logs"));
        assert!(text.contains("approve all")); // safe plan → bulk-approve offered
    }

    #[test]
    fn list_window_keeps_cursor_visible_and_clamps() {
        // Everything fits: no scrolling.
        assert_eq!(list_window(0, 5, 10), 0);
        assert_eq!(list_window(4, 5, 10), 0);
        // Cursor near the top stays at offset 0.
        assert_eq!(list_window(2, 100, 20), 0);
        // Mid-list: cursor is roughly centred (cursor - rows/2).
        assert_eq!(list_window(50, 100, 20), 40);
        // Near the end: clamped so the last page fills the viewport.
        assert_eq!(list_window(99, 100, 20), 80);
        // Degenerate viewport.
        assert_eq!(list_window(5, 100, 0), 0);
    }

    #[test]
    fn huge_listing_renders_the_cursor_row_without_panicking() {
        let mut s = ready_state();
        let entries: Vec<_> = (0..10_000)
            .map(|i| Entry::new(format!("file{i:05}"), EntryKind::File))
            .collect();
        s.panes[0].listing = Listing::Ready(std::sync::Arc::new(entries));
        s.panes[0].cursor = 5000;
        let text = render_text(&s, 80, 24);
        // The cursor's row is within the rendered window even though only ~22 rows are materialized.
        assert!(text.contains("file05000"), "cursor row should be visible");
        // A far-away row is NOT in the window.
        assert!(
            !text.contains("file00001"),
            "off-screen rows are not rendered"
        );
    }

    #[test]
    fn cursor_at_end_of_a_huge_listing_stays_visible() {
        let mut s = ready_state();
        let entries: Vec<_> = (0..10_000)
            .map(|i| Entry::new(format!("file{i:05}"), EntryKind::File))
            .collect();
        s.panes[0].listing = Listing::Ready(std::sync::Arc::new(entries));
        s.panes[0].cursor = 9999;
        let text = render_text(&s, 80, 24);
        assert!(text.contains("file09999"), "last row should be visible");
        assert!(
            text.contains("file09998"),
            "the last page fills the viewport"
        );
    }

    #[test]
    fn a_mark_inside_the_window_renders_on_the_right_row() {
        let mut s = ready_state();
        let entries: Vec<_> = (0..10_000)
            .map(|i| Entry::new(format!("file{i:05}"), EntryKind::File))
            .collect();
        s.panes[0].listing = Listing::Ready(std::sync::Arc::new(entries));
        s.panes[0].cursor = 5000;
        s.panes[0].marked.insert(5001); // an absolute visible index within the window
        let text = render_text(&s, 80, 24);
        assert!(
            text.contains("*file05001"),
            "marked row shows its '*' under the window offset"
        );
    }

    #[test]
    fn transfer_queue_overlay_lists_pending() {
        let mut s = ready_state();
        set_transfer(&mut s, 1024, None, None, false);
        s.transfer_queue.push_back(cairn_core::QueuedTransfer {
            src_conn: ConnectionId(1),
            dst_conn: ConnectionId(2),
            items: vec![(VfsPath::root(), VfsPath::root())],
            is_move: true,
        });
        s.overlay = Some(cairn_core::Overlay::TransferQueue { cursor: 0 });
        let text = render_text(&s, 80, 24);
        assert!(text.contains("Transfer"), "dialog title");
        assert!(
            text.contains("Copying 1 item(s)"),
            "the active transfer's label"
        );
        assert!(text.contains("--%"), "indeterminate bar (total is unknown)");
        assert!(text.contains("move"), "the pending move is listed");
        assert!(text.contains("background")); // the [b] background control
        assert!(text.contains("drop")); // the [d] drop control
    }

    #[test]
    fn progress_bar_renders_filled_empty_and_suffix() {
        // Unknown total → `--%` suffix and an indeterminate *marquee* (a fixed block), never a
        // fabricated percentage. The bar is no longer all-empty — it shows motion.
        let none = progress_bar(None, 24, 0);
        assert!(none.ends_with(" --%"), "unknown → --%: {none}");
        assert_eq!(
            none.matches('█').count(),
            MARQUEE_BLOCK,
            "unknown → a marquee block of fixed width: {none}"
        );

        // 0% → no filled cells; 100% → no empty cells; the suffix reflects the percentage.
        let zero = progress_bar(Some(0), 24, 0);
        assert!(zero.ends_with(" 0%") && !zero.contains('█'), "{zero}");
        let full = progress_bar(Some(100), 24, 0);
        assert!(full.ends_with(" 100%") && !full.contains('░'), "{full}");
        // 50% of a 20-col bar (24 − 4 for " 50%") → 10 filled, 10 empty.
        let half = progress_bar(Some(50), 24, 0);
        assert_eq!(half.matches('█').count(), 10, "{half}");
        assert_eq!(half.matches('░').count(), 10, "{half}");

        // Over-100 input is clamped (defensive) and the bar never overfills.
        let over = progress_bar(Some(250), 24, 0);
        assert!(over.ends_with(" 100%") && !over.contains('░'), "{over}");

        // Degenerate widths never panic and always leave at least one bar cell.
        for w in [0usize, 1, 3, 5] {
            let s = progress_bar(Some(50), w, 0);
            assert!(s.contains('█') || s.contains('░'), "w={w}: {s}");
            let ind = progress_bar(None, w, 3);
            assert!(ind.contains('█') || ind.contains('░'), "w={w}: {ind}");
        }
    }

    #[test]
    fn indeterminate_bar_sweeps_back_and_forth() {
        // The marquee block bounces across the bar as `pulse` advances (triangle wave), and always
        // has exactly `min(MARQUEE_BLOCK, bar_width)` filled cells so it never grows/shrinks.
        let bar_width = 6; // span = 6 - 3 = 3, period = 6
        let positions: Vec<usize> = (0..8)
            .map(|pulse| {
                let bar = indeterminate_bar(bar_width, pulse);
                assert_eq!(bar.chars().count(), bar_width, "pulse={pulse}: {bar}");
                assert_eq!(
                    bar.matches('█').count(),
                    MARQUEE_BLOCK,
                    "pulse={pulse}: constant block width: {bar}"
                );
                bar.chars().position(|c| c == '█').unwrap()
            })
            .collect();
        // 0→1→2→3 then bounce back 2→1→0, then climb again.
        assert_eq!(positions, vec![0, 1, 2, 3, 2, 1, 0, 1]);

        // A bar too narrow to move renders a static block without panicking.
        let narrow = indeterminate_bar(2, 99);
        assert_eq!(narrow.matches('█').count(), 2, "{narrow}");
    }

    #[test]
    fn transfer_rate_eta_hides_numbers_when_not_meaningful() {
        use cairn_core::ActiveTransfer;
        let t = |bytes, rate, total, paused| ActiveTransfer {
            id: 1,
            kind: OpKind::Copy,
            label: "x".to_owned(),
            phase: TransferPhase::Copying,
            scan_entries: 0,
            scan_path: String::new(),
            bytes,
            rate,
            total,
            paused,
            pulse: 0,
        };
        let with_phase = |mut a: ActiveTransfer, phase| {
            a.phase = phase;
            a
        };
        // Running with a known total: both rate and a positive ETA.
        assert_eq!(
            transfer_rate_eta(&t(4 << 20, Some(2 << 20), Some(8 << 20), false)),
            (Some(2 << 20), Some(2))
        );
        // Paused: neither shown even though rate/total are known.
        assert_eq!(
            transfer_rate_eta(&t(4 << 20, Some(2 << 20), Some(8 << 20), true)),
            (None, None)
        );
        // Unknown total → rate still shown, ETA suppressed.
        assert_eq!(
            transfer_rate_eta(&t(4 << 20, Some(2 << 20), None, false)),
            (Some(2 << 20), None)
        );
        // Zero rate → no ETA (no division), and rate 0 is still "known" so it shows.
        assert_eq!(
            transfer_rate_eta(&t(0, Some(0), Some(8 << 20), false)),
            (Some(0), None)
        );
        // Non-Copying phases move no bytes, so rate/ETA are suppressed even with a known rate/total.
        for phase in [TransferPhase::Counting, TransferPhase::Finalizing] {
            assert_eq!(
                transfer_rate_eta(&with_phase(
                    t(4 << 20, Some(2 << 20), Some(8 << 20), false),
                    phase
                )),
                (None, None),
                "{phase:?} must hide rate/ETA"
            );
        }
    }

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
        // Unit-boundary rounding must not produce "1024.0 KiB".
        assert_eq!(human_bytes(1_048_575), "1.0 MiB");
    }

    #[test]
    fn format_perms_renders_type_char_and_rwx_bits() {
        let p = |kind, mode| format_perms(kind, Some(UnixPerms::from_mode(mode)));
        assert_eq!(p(EntryKind::Dir, 0o755), "drwxr-xr-x");
        assert_eq!(p(EntryKind::File, 0o644), "-rw-r--r--");
        assert_eq!(p(EntryKind::File, 0o600), "-rw-------");
        assert_eq!(p(EntryKind::Symlink, 0o777), "lrwxrwxrwx");
        assert_eq!(p(EntryKind::Special, 0o600), "srw-------");
        assert_eq!(p(EntryKind::Stream, 0o644), "prw-r--r--");
        // set-uid / set-gid / sticky substitutions in the execute positions (lowercase when the
        // execute bit is also set, uppercase when it isn't), matching `ls -l`.
        assert_eq!(p(EntryKind::File, 0o4755), "-rwsr-xr-x"); // set-uid, x on → 's'
        assert_eq!(p(EntryKind::File, 0o4655), "-rwSr-xr-x"); // set-uid, x off → 'S'
        assert_eq!(p(EntryKind::File, 0o2755), "-rwxr-sr-x"); // set-gid → 's'
        assert_eq!(p(EntryKind::Dir, 0o1777), "drwxrwxrwt"); // sticky, x on → 't'
        assert_eq!(p(EntryKind::Dir, 0o1776), "drwxrwxrwT"); // sticky, x off → 'T'
                                                             // A backend with no permission model → blank column (not a misleading `---------`).
        assert_eq!(format_perms(EntryKind::File, None), "");
    }

    #[test]
    fn entry_style_colors_by_type_hidden_exec_and_archive() {
        let t = &Theme::DARK;
        let fg = |name: &str, kind| entry_style(&Entry::new(name, kind), t).fg;
        assert_eq!(fg("src", EntryKind::Dir), Some(t.dir));
        assert_eq!(fg(".git", EntryKind::Dir), Some(t.hidden_dir));
        assert_eq!(fg("README.md", EntryKind::File), Some(t.file));
        assert_eq!(fg(".env", EntryKind::File), Some(t.hidden_file));
        assert_eq!(fg("latest", EntryKind::Symlink), Some(t.symlink));
        assert_eq!(fg("app.log", EntryKind::Stream), Some(t.stream));
        assert_eq!(fg("docker.sock", EntryKind::Special), Some(t.special));
        // Archive by extension (color only; Enter still sniffs magic bytes).
        assert_eq!(fg("release.tar.gz", EntryKind::File), Some(t.archive));
        // A *hidden* archive keeps the archive color (type wins over hidden dimming for archives).
        assert_eq!(fg(".backup.tar.gz", EntryKind::File), Some(t.archive));
        // The `..` sentinel is a normal directory, never dimmed as "hidden".
        assert_eq!(fg("..", EntryKind::Dir), Some(t.dir));

        // The execute bit wins over an archive extension.
        let mut exe = Entry::new("bundle.zip", EntryKind::File);
        exe.perms = Some(UnixPerms::from_mode(0o755));
        assert_eq!(entry_style(&exe, t).fg, Some(t.executable));
        // A plain (non-exec) file with perms still colors as a file, not exec.
        let mut plain = Entry::new("notes.md", EntryKind::File);
        plain.perms = Some(UnixPerms::from_mode(0o644));
        assert_eq!(entry_style(&plain, t).fg, Some(t.file));
    }

    #[test]
    fn is_archive_name_matches_common_extensions() {
        for n in [
            "a.zip",
            "a.tar",
            "a.tar.gz",
            "a.tgz",
            "a.tar.zst",
            "a.tzst",
            "a.gz",
            "A.ZIP",
            "x.7z",
        ] {
            assert!(is_archive_name(n), "{n} should be an archive");
        }
        for n in ["a.txt", "README", "archive", "a.gz.txt", ""] {
            assert!(!is_archive_name(n), "{n:?} should NOT be an archive");
        }
        // Degenerate bare-extension names are matched (harmless — color only, no panic).
        assert!(is_archive_name(".gz") && is_archive_name(".tar"));
    }

    #[test]
    fn format_mtime_is_utc_and_deterministic() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(format_mtime(UNIX_EPOCH), "1970-01-01");
        // 2024-01-01 00:00:00 UTC.
        assert_eq!(
            format_mtime(UNIX_EPOCH + Duration::from_secs(1_704_067_200)),
            "2024-01-01"
        );
        // A leap day, and just before/after midnight UTC stay on the right calendar day.
        assert_eq!(
            format_mtime(UNIX_EPOCH + Duration::from_secs(1_709_164_800)), // 2024-02-29 00:00
            "2024-02-29"
        );
        assert_eq!(
            format_mtime(UNIX_EPOCH + Duration::from_secs(1_709_251_199)), // 2024-02-29 23:59:59
            "2024-02-29"
        );
    }

    #[test]
    fn entry_columns_drops_responsively() {
        let perms = "drwxr-xr-x"; // 10
        let date = "2026-07-06"; // 10
                                 // Plenty of room → both columns, each gap-prefixed.
        assert_eq!(entry_columns(perms, date, 60), " drwxr-xr-x 2026-07-06");
        // Room for name + date only (need >= 12 + 11 = 23; not >= 12 + 22 = 34).
        assert_eq!(entry_columns(perms, date, 25), " 2026-07-06");
        // Too narrow for any column → empty (name gets the whole row).
        assert_eq!(entry_columns(perms, date, 20), "");
        // Missing perms (object store) still shows the date when it fits.
        assert_eq!(entry_columns("", date, 25), " 2026-07-06");
        // Nothing to show.
        assert_eq!(entry_columns("", "", 60), "");
    }

    #[test]
    fn truncate_to_appends_ellipsis() {
        assert_eq!(truncate_to("short", 10), "short");
        assert_eq!(truncate_to("a-very-long-name.txt", 8), "a-very-…");
        assert_eq!(truncate_to("x", 0), "");
    }

    #[test]
    fn truncate_left_keeps_the_tail() {
        // Short-enough strings pass through unchanged.
        assert_eq!(truncate_left("/a/b", 10), "/a/b");
        assert_eq!(truncate_left("x", 0), "");
        // Tightest boundary: a budget of 1 leaves room for only the ellipsis.
        assert_eq!(truncate_left("abcdef", 1), "…");
        // A long path keeps its meaningful tail behind a leading ellipsis, within the budget.
        let out = truncate_left("/home/swith/fusedpro/functions/foo.js", 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.starts_with('…'));
        assert!(out.ends_with("functions/foo.js"), "kept the tail: {out}");
    }

    #[test]
    fn status_line_shows_live_transfer_progress() {
        let mut s = ready_state();
        set_transfer(&mut s, 2 * 1024 * 1024, Some(512 * 1024), None, false);
        let text = render_text(&s, 100, 24);
        assert!(text.contains("transferring"), "expected transfer indicator");
        assert!(
            text.contains("2.0 MiB"),
            "expected human-readable byte total"
        );
        assert!(text.contains("512.0 KiB/s"), "expected throughput rate");
    }

    #[test]
    fn status_line_aggregates_multiple_active_transfers() {
        let mut s = ready_state();
        set_transfer(&mut s, 1024 * 1024, Some(512 * 1024), None, false);
        set_transfer(&mut s, 1024 * 1024, Some(512 * 1024), None, false);
        let text = render_text(&s, 100, 24);
        assert!(
            text.contains("2 active"),
            "expected aggregate header: {text}"
        );
        assert!(text.contains("2.0 MiB"), "summed bytes: {text}");
        assert!(text.contains("1.0 MiB/s"), "summed rate: {text}");
    }

    #[test]
    fn status_line_keeps_the_copy_percentage_when_a_delete_runs_alongside() {
        // A concurrent delete (total = None) must not poison the byte aggregate and hide the copy's
        // percentage — deletes are excluded from both the byte and total sums.
        let mut s = ready_state();
        // A copy: 4 MiB of 8 MiB → 50%.
        set_transfer(
            &mut s,
            4 * 1024 * 1024,
            Some(1024),
            Some(8 * 1024 * 1024),
            false,
        );
        // A delete running concurrently (items, no byte total).
        let id = s.next_transfer_id;
        s.next_transfer_id += 1;
        s.active_transfers.push(cairn_core::ActiveTransfer {
            id,
            kind: OpKind::Delete,
            label: "Deleting 3 item(s)".to_owned(),
            phase: TransferPhase::Deleting,
            scan_entries: 3,
            scan_path: "/x".to_owned(),
            bytes: 0,
            rate: None,
            total: None,
            paused: false,
            pulse: 0,
        });
        let text = render_text(&s, 120, 24);
        assert!(
            text.contains("(50%)"),
            "the copy's percentage must survive a concurrent delete: {text}"
        );
        assert!(
            text.contains("8.0 MiB"),
            "the copy's total is still shown: {text}"
        );
    }

    #[test]
    fn status_line_shows_some_paused_in_aggregate() {
        let mut s = ready_state();
        set_transfer(&mut s, 1024, Some(512), None, false);
        set_transfer(&mut s, 1024, None, None, true);
        let text = render_text(&s, 100, 24);
        assert!(
            text.contains("2 active, 1 paused"),
            "expected mixed state: {text}"
        );
    }

    #[test]
    fn status_line_all_paused_aggregate() {
        let mut s = ready_state();
        set_transfer(&mut s, 1024, None, None, true);
        set_transfer(&mut s, 1024, None, None, true);
        let text = render_text(&s, 100, 24);
        assert!(
            text.contains("2 paused"),
            "expected all-paused label: {text}"
        );
        assert!(
            !text.contains("active"),
            "no 'active' when all paused: {text}"
        );
    }

    #[test]
    fn status_line_falls_back_to_the_status_message_when_idle() {
        // With no active transfer, a transient status message is shown (it was previously invisible).
        let mut s = ready_state();
        s.status = Some("All transfers cancelled".to_owned());
        let text = render_text(&s, 100, 24);
        assert!(
            text.contains("All transfers cancelled"),
            "status message must be visible: {text}"
        );
    }

    #[test]
    fn status_line_shows_paused_and_drops_rate_eta() {
        let mut s = ready_state();
        set_transfer(
            &mut s,
            4 * 1024 * 1024,
            Some(2 * 1024 * 1024),
            Some(8 * 1024 * 1024),
            true,
        );
        let text = render_text(&s, 120, 24);
        assert!(text.contains("paused"), "expected paused label: {text}");
        assert!(text.contains("50%"), "still shows progress: {text}");
        assert!(!text.contains("ETA"), "ETA suppressed while paused: {text}");
        assert!(
            !text.contains("MiB/s"),
            "rate suppressed while paused: {text}"
        );
    }

    #[test]
    fn human_duration_formats_compactly() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(192), "3m12s");
        assert_eq!(human_duration(3900), "1h05m");
    }

    #[test]
    fn status_line_shows_percentage_and_eta_when_total_is_known() {
        let mut s = ready_state();
        // 4 MiB of 8 MiB at 2 MiB/s → 50%, ETA 2s.
        set_transfer(
            &mut s,
            4 * 1024 * 1024,
            Some(2 * 1024 * 1024),
            Some(8 * 1024 * 1024),
            false,
        );
        let text = render_text(&s, 120, 24);
        assert!(text.contains("50%"), "expected percentage: {text}");
        assert!(text.contains("ETA 2s"), "expected ETA: {text}");
    }

    #[test]
    fn filter_indicator_appears_in_the_pane_border() {
        let mut s = ready_state();
        s.panes[0].filter = Some("src".to_owned());
        s.panes[0].filter_editing = true;
        let text = render_text(&s, 80, 24);
        assert!(
            text.contains("filter: src_"),
            "expected live-filter indicator"
        );
    }

    #[test]
    fn prompt_overlay_shows_title_input_and_hint() {
        let mut s = ready_state();
        s.overlay = Some(cairn_core::Overlay::Prompt {
            kind: cairn_core::PromptKind::MakeDir,
            input: "myfolder".to_owned(),
        });
        let text = render_text(&s, 80, 24);
        assert!(text.contains("New directory"));
        assert!(text.contains("myfolder"));
        assert!(text.contains("Enter")); // the hint line is not clipped
        assert!(text.contains("Esc"));
    }

    #[test]
    fn vault_unlock_overlay_masks_the_passphrase_and_shows_errors() {
        let mut s = ready_state();
        let mut input = cairn_core::MaskedInput::new();
        for c in "topsecret".chars() {
            input.push(c);
        }
        s.overlay = Some(cairn_core::Overlay::VaultUnlock {
            input,
            error: Some("decryption failed (wrong passphrase or corrupt vault)".to_owned()),
            pending_conn: None,
            pending_save: None,
        });
        let text = render_text(&s, 80, 24);
        assert!(text.contains("Unlock vault"), "dialog title: {text}");
        // The passphrase characters must never reach the screen — only bullets.
        assert!(
            !text.contains("topsecret"),
            "passphrase leaked to the screen: {text}"
        );
        assert!(text.contains('\u{2022}'), "expected masked bullets");
        assert!(
            text.contains("wrong passphrase"),
            "error line shown: {text}"
        );
        assert!(text.contains("Enter"), "hint line present: {text}");
    }

    #[test]
    fn connection_switcher_lists_choices() {
        let mut s = ready_state();
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
        s.overlay = Some(cairn_core::Overlay::Connections {
            cursor: 0,
            show_hidden: false,
        });
        let text = render_text(&s, 80, 24);
        assert!(text.contains("Open connection"));
        assert!(text.contains("local: /"));
        assert!(text.contains("work"));
    }

    /// P6 gate fix: `[Esc] Close` must survive on both a wide and a narrow terminal — it is the
    /// only way to discover how to leave the switcher, so it must never be the hint that a
    /// width-constrained, right-truncating `Paragraph` cuts off.
    #[test]
    fn connections_hint_always_shows_esc_close_first() {
        let wide = connections_hint(false, 54);
        assert!(wide.starts_with("[Esc] Close"));
        let narrow = connections_hint(false, 38); // matches the 40x12 snapshot's inner width
        assert!(narrow.starts_with("[Esc] Close"));
        let tiny = connections_hint(true, 11); // exactly "[Esc] Close" and nothing else
        assert_eq!(tiny, "[Esc] Close");
    }

    #[test]
    fn connections_hint_drops_whole_tokens_never_slices_one() {
        // Width covers "[Esc] Close  [Ctrl-N] New" (25 chars) plus 3 more columns — not enough
        // for the next token ("[e] Edit", 8 chars + 2-space separator) — must stop there rather
        // than emit a partial token.
        let hint = connections_hint(true, 28);
        assert_eq!(hint, "[Esc] Close  [Ctrl-N] New");
        assert!(!hint.contains("[e"), "must never emit a half-shown token");
    }

    #[test]
    fn connections_hint_shows_edit_delete_only_when_editable() {
        let editable = connections_hint(true, 200);
        assert!(editable.contains("[e] Edit"));
        assert!(editable.contains("[d] Delete"));
        let readonly = connections_hint(false, 200);
        assert!(!readonly.contains("[e] Edit"));
        assert!(!readonly.contains("[d] Delete"));
        // Both still advertise the P6 actions when there's room.
        for hint in [editable, readonly] {
            assert!(hint.contains("[t] Test"));
            assert!(hint.contains("[P] Pin"));
            assert!(hint.contains("[H] Hide"));
            assert!(hint.contains("[S] Show hidden"));
        }
    }

    #[test]
    fn ai_plan_overlay_hides_bulk_approve_for_irreversible_plan() {
        let mut s = ready_state();
        s.overlay = Some(cairn_core::Overlay::AiPlan {
            plan: plan(&["copy", "delete"]),
            cursor: 0,
        });
        let text = render_text(&s, 80, 24);
        assert!(text.contains("IRREVERSIBLE"));
        assert!(text.contains("no bulk"));
    }

    /// Verify the full status-bar precedence chain: transfer > status message > help string.
    /// The existing `status_line_falls_back_to_the_status_message_when_idle` test covers the
    /// status-vs-help arm; this test covers the missing transfer-wins-over-status arm.
    #[test]
    fn status_line_transfer_wins_over_transient_status_message() {
        let mut s = ready_state();
        set_transfer(&mut s, 2 * 1024 * 1024, Some(512 * 1024), None, false);
        s.status = Some("Sort: name".to_owned());
        let text = render_text(&s, 100, 24);
        assert!(
            text.contains("transferring"),
            "live transfer must win the status bar: {text}"
        );
        assert!(
            !text.contains("Sort: name"),
            "transient status must be suppressed while a transfer is live: {text}"
        );
    }

    /// The `ConfirmShellAction` overlay must display the VFS virtual path AND the annotation
    /// clarifying that the real OS path is resolved on confirm — so the user is never misled
    /// about what the shell action will actually receive (#58).
    #[test]
    fn confirm_shell_action_shows_target_and_virtual_path_note() {
        let mut s = ready_state();
        s.overlay = Some(cairn_core::Overlay::ConfirmShellAction {
            index: 0,
            name: "compress".to_owned(),
            conn: ConnectionId(1),
            target: VfsPath::parse("/docs/report.pdf").unwrap(),
        });
        let text = render_text(&s, 80, 24);
        assert!(text.contains("Run shell action?"), "dialog title: {text}");
        assert!(text.contains("compress"), "action name: {text}");
        assert!(text.contains("/docs/report.pdf"), "VFS path: {text}");
        assert!(
            text.contains("virtual path"),
            "annotation must tell the user the shown path is virtual, not the real OS path: {text}"
        );
        assert!(text.contains("[y] Run"), "action hints: {text}");
    }

    #[test]
    fn renders_loading_and_error() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut s = AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root());
        s.panes[0].listing = Listing::Loading;
        s.panes[1].listing = Listing::Error("permission denied".into());
        terminal.draw(|f| render(f, &s, &Theme::default())).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("Loading"));
        assert!(text.contains("permission denied"));
    }

    #[test]
    fn log_viewer_overlay_shows_lines_and_indicator() {
        let mut s = ready_state();
        let mut lines = std::collections::VecDeque::new();
        lines.push_back("line one".to_owned());
        lines.push_back("line two".to_owned());
        s.overlay = Some(cairn_core::Overlay::LogViewer {
            id: 1,
            title: "my-pod — logs".to_owned(),
            lines,
            partial: String::new(),
            byte_size: 0,
            follow: true,
            scroll: 0,
            status: cairn_core::LogViewerStatus::Streaming,
        });
        let text = render_text(&s, 100, 24);
        assert!(text.contains("my-pod"), "title in border: {text}");
        assert!(text.contains("line one"), "first log line: {text}");
        assert!(text.contains("line two"), "second log line: {text}");
        assert!(text.contains("Streaming"), "status indicator: {text}");
        assert!(text.contains("follow"), "follow indicator: {text}");
    }

    #[test]
    fn pager_overlay_text_mode_shows_lines_and_status() {
        let mut s = ready_state();
        let mut lines = std::collections::VecDeque::new();
        lines.push_back("fn main() {".to_owned());
        lines.push_back("    println!(\"hi\");".to_owned());
        s.overlay = Some(cairn_core::Overlay::Pager {
            id: 1,
            title: "main.rs — view".to_owned(),
            mode: cairn_core::PagerMode::Text,
            lines,
            partial: String::new(),
            byte_size: 32,
            total_size: Some(64),
            scroll: 0,
            status: cairn_core::PagerStatus::Ready,
            wrap: true,
        });
        let text = render_text(&s, 100, 24);
        assert!(text.contains("main.rs"), "title in border: {text}");
        assert!(text.contains("fn main"), "first line: {text}");
        assert!(text.contains("println"), "second line: {text}");
        assert!(text.contains("Ready"), "status indicator: {text}");
        assert!(text.contains("50%"), "percentage position: {text}");
    }

    #[test]
    fn pager_overlay_hex_mode_shows_offset_hex_and_ascii() {
        let mut s = ready_state();
        let mut lines = std::collections::VecDeque::new();
        // "Hello, world!\0\0\0" — 16 raw bytes stored one char per byte (the reducer's
        // byte↔char convention); the last three are NUL so they show as `.` in the ascii column.
        lines.push_back("Hello, world!\u{0}\u{0}\u{0}".to_owned());
        s.overlay = Some(cairn_core::Overlay::Pager {
            id: 2,
            title: "photo.png — view".to_owned(),
            mode: cairn_core::PagerMode::Hex,
            lines,
            partial: String::new(),
            byte_size: 16,
            total_size: None,
            scroll: 0,
            status: cairn_core::PagerStatus::Loading,
            wrap: true,
        });
        let text = render_text(&s, 100, 24);
        assert!(text.contains("photo.png"), "title in border: {text}");
        assert!(text.contains("00000000"), "offset column: {text}");
        assert!(
            text.contains("48 65 6c 6c 6f"),
            "hex bytes for 'Hello' (0x48 0x65 0x6c 0x6c 0x6f): {text}"
        );
        assert!(text.contains("Hello, world!"), "ascii column: {text}");
        assert!(text.contains("Loading"), "status indicator: {text}");
    }

    #[test]
    fn pager_overlay_truncated_status_shown() {
        let mut s = ready_state();
        s.overlay = Some(cairn_core::Overlay::Pager {
            id: 3,
            title: "huge.log — view".to_owned(),
            mode: cairn_core::PagerMode::Text,
            lines: std::collections::VecDeque::new(),
            partial: String::new(),
            byte_size: 8 * 1024 * 1024,
            total_size: Some(16 * 1024 * 1024),
            scroll: 0,
            status: cairn_core::PagerStatus::Truncated,
            wrap: true,
        });
        let text = render_text(&s, 100, 24);
        assert!(text.contains("Truncated"), "truncated indicator: {text}");
    }

    fn make_session_record(title: &str) -> SessionRecord {
        SessionRecord {
            path: VfsPath::root(),
            title: title.to_owned(),
            output_lines: std::collections::VecDeque::new(),
            output_partial: String::new(),
            output_byte_size: 0,
            local_port: None,
            ended: None,
        }
    }

    #[test]
    fn exec_pane_overlay_shows_title_and_input_field() {
        use cairn_types::SessionId;
        let mut s = ready_state();
        let id = SessionId(1);
        s.sessions.insert(id, make_session_record("my-pod — exec"));
        s.overlay = Some(cairn_core::Overlay::ExecPane {
            id,
            input: "ls -la".to_owned(),
            scroll: 0,
            follow: true,
        });
        let text = render_text(&s, 100, 30);
        assert!(text.contains("my-pod"), "title in border: {text}");
        assert!(text.contains("ls -la"), "input field content: {text}");
        assert!(text.contains("Running"), "running status: {text}");
    }

    #[test]
    fn exec_pane_shows_output_lines() {
        use cairn_types::SessionId;
        let mut s = ready_state();
        let id = SessionId(2);
        let mut rec = make_session_record("bash — exec");
        rec.output_lines.push_back("hello world".to_owned());
        rec.output_lines.push_back("second line".to_owned());
        s.sessions.insert(id, rec);
        s.overlay = Some(cairn_core::Overlay::ExecPane {
            id,
            input: String::new(),
            scroll: 0,
            follow: true,
        });
        let text = render_text(&s, 100, 30);
        assert!(text.contains("hello world"), "output line 1: {text}");
        assert!(text.contains("second line"), "output line 2: {text}");
    }

    #[test]
    fn exec_pane_shows_exit_code_when_ended() {
        use cairn_types::SessionId;
        let mut s = ready_state();
        let id = SessionId(3);
        let mut rec = make_session_record("job — exec");
        rec.ended = Some(SessionEnd {
            exit_code: Some(42),
            error: None,
        });
        s.sessions.insert(id, rec);
        s.overlay = Some(cairn_core::Overlay::ExecPane {
            id,
            input: String::new(),
            scroll: 0,
            follow: false,
        });
        let text = render_text(&s, 100, 30);
        assert!(text.contains("42"), "exit code in status: {text}");
    }

    #[test]
    fn port_forward_overlay_shows_port() {
        use cairn_types::SessionId;
        let mut s = ready_state();
        let id = SessionId(10);
        let mut rec = make_session_record("postgres — port-forward");
        rec.local_port = Some(15432);
        s.sessions.insert(id, rec);
        s.overlay = Some(cairn_core::Overlay::PortForwardStatus { id });
        let text = render_text(&s, 100, 30);
        assert!(text.contains("15432"), "local port in overlay: {text}");
    }

    #[test]
    fn port_forward_overlay_shows_binding_when_no_port() {
        use cairn_types::SessionId;
        let mut s = ready_state();
        let id = SessionId(11);
        s.sessions
            .insert(id, make_session_record("svc — port-forward"));
        s.overlay = Some(cairn_core::Overlay::PortForwardStatus { id });
        let text = render_text(&s, 100, 30);
        assert!(
            text.contains("Bind") || text.contains("bind"),
            "binding indicator: {text}"
        );
    }

    // ── P3: [auto] badge via connection_display_label ────────────────────────────────────────

    fn make_choice(label: &str, provenance: ChoiceProvenance) -> cairn_core::ConnectionChoice {
        cairn_core::ConnectionChoice {
            conn: ConnectionId(42),
            label: label.to_owned(),
            scheme: String::new(),
            provenance,
            status: cairn_core::ChoiceStatus::NeedsOpen,
            kind: cairn_core::ConnectionKind::AutoDiscovered,
            pinned: false,
            hidden: false,
        }
    }

    /// Builtin entries must NOT receive the `[auto]` prefix.
    #[test]
    fn builtin_connection_label_unchanged() {
        let c = make_choice("local: /", ChoiceProvenance::Builtin);
        assert_eq!(connection_display_label(&c), "local: /");
    }

    /// Saved entries must NOT receive the `[auto]` prefix.
    #[test]
    fn saved_connection_label_unchanged() {
        let c = make_choice("sftp: my-server", ChoiceProvenance::Saved);
        assert_eq!(connection_display_label(&c), "sftp: my-server");
    }

    /// Auto-discovered entries MUST receive the `[auto]` prefix — both Docker and Kubeconfig
    /// sources use the same `Discovered` variant so one test per source is sufficient.
    #[test]
    fn discovered_docker_label_prefixed_with_auto() {
        let c = make_choice(
            "docker (default)",
            ChoiceProvenance::Discovered {
                source: cairn_core::DiscoverySource::Docker,
            },
        );
        let label = connection_display_label(&c);
        assert!(
            label.starts_with("[auto]"),
            "discovered label must start with [auto]: {label}"
        );
        assert!(
            label.contains("docker (default)"),
            "original label must be preserved after the prefix: {label}"
        );
        assert_eq!(label, "[auto] docker (default)");
    }

    #[test]
    fn discovered_kubeconfig_label_prefixed_with_auto() {
        let c = make_choice(
            "k8s: (kubeconfig)",
            ChoiceProvenance::Discovered {
                source: cairn_core::DiscoverySource::Kubeconfig,
            },
        );
        let label = connection_display_label(&c);
        assert_eq!(label, "[auto] k8s: (kubeconfig)");
    }
}
