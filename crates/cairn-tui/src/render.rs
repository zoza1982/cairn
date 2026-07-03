//! Rendering the [`AppState`] with ratatui. Pure: takes `&AppState` + `&Theme`, performs no I/O.

use crate::theme::Theme;
use cairn_ai::{Plan, Reversibility, StepStatus, Verb};
use cairn_core::{
    credential_method_fields, credential_methods, scheme_fields, AppState, ChoiceProvenance,
    ConnectionFormStage, ConnectionKind, CredentialMethod, FieldValue, Listing, LogViewerStatus,
    MaskedInput, Overlay, PagerMode, PagerStatus, PaneState, PromptKind, SessionEnd, SessionRecord,
    Side, WritebackChoice, WritebackConflictReason, KNOWN_SCHEMES, PAGER_HEX_ROW_BYTES,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

/// Render the whole application: two panes over a one-line status bar, themed by `theme`.
pub fn render(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
    render_pane(frame, left, state, Side::Left, theme);
    render_pane(frame, right, state, Side::Right, theme);
    render_status(frame, status, state, theme);
    render_overlay(frame, state);
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
fn render_overlay(frame: &mut Frame, state: &AppState) {
    let Some(overlay) = &state.overlay else {
        return;
    };
    match overlay {
        Overlay::Connections {
            cursor,
            show_hidden,
        } => render_connections(frame, &state.connections, *cursor, *show_hidden),
        Overlay::ConfirmDelete { paths, .. } => {
            let area = centered(frame.area(), 44, 6);
            frame.render_widget(Clear, area);
            let block = Block::bordered()
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
        Overlay::ConfirmOverwrite { conflicts, .. } => {
            let area = centered(frame.area(), 48, 6);
            frame.render_widget(Clear, area);
            let block = Block::bordered()
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
        } => render_confirm_writeback(frame, path, temp_path, *reason, *cursor),
        Overlay::TransferQueue { cursor } => render_transfer_queue(frame, state, *cursor),
        Overlay::AiPlan { plan, cursor } => render_ai_plan(frame, plan, *cursor),
        Overlay::Prompt { kind, input } => render_prompt(frame, kind, input),
        Overlay::VaultUnlock { input, error, .. } => {
            render_vault_unlock(frame, input, error.as_deref(), state.vault_unlocking)
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
        ),
        Overlay::LogViewer {
            title,
            lines,
            follow,
            scroll,
            status,
            ..
        } => render_log_viewer(frame, title, lines, *follow, *scroll, status),
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
        ),
        Overlay::ExecPane {
            id,
            input,
            scroll,
            follow,
        } => {
            if let Some(rec) = state.sessions.get(id) {
                render_exec_pane(frame, rec, input, *scroll, *follow);
            }
        }
        Overlay::PortForwardStatus { id } => {
            if let Some(rec) = state.sessions.get(id) {
                render_port_forward_status(frame, rec);
            }
        }
        Overlay::ConfirmDeleteConnection { display_name, .. } => {
            render_confirm_delete_connection(frame, display_name)
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
        ),
    }
}

/// Draw the confirm-delete-connection overlay: a red-bordered prompt asking the user to confirm
/// before permanently removing a saved connection profile.
fn render_confirm_delete_connection(frame: &mut Frame, display_name: &str) {
    let msg = format!("Delete connection '{display_name}'? This cannot be undone.");
    let h = 5u16;
    let w = u16::try_from(msg.len() + 6)
        .unwrap_or(64)
        .min(frame.area().width);
    let area = centered(frame.area(), w, h);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
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
) {
    // 2 borders + reason + path + temp path + blank + 4 choices + hint.
    let area = centered(frame.area(), 66, 12);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
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
fn render_vault_unlock(frame: &mut Frame, input: &MaskedInput, error: Option<&str>, busy: bool) {
    // 7 rows: 2 borders + masked field + blank + error/spacer + hint + breathing room.
    let area = centered(frame.area(), 50, 7);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
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
fn render_vault_create(
    frame: &mut Frame,
    passphrase: &MaskedInput,
    confirm: &MaskedInput,
    focus: u8,
    remember: bool,
    error: Option<&str>,
    creating: bool,
) {
    // 11 rows: 2 borders + 9 content rows (passphrase label + passphrase field + blank +
    // confirm label + confirm field + blank + remember toggle + error/status + hint).
    let area = centered(frame.area(), 54, 11);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
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
) {
    match stage {
        ConnectionFormStage::SchemePicker => render_scheme_picker(frame, focus),
        ConnectionFormStage::Fields => {
            render_form_fields(frame, scheme, values, focus, field_errors, is_edit)
        }
        ConnectionFormStage::CredentialMethodPicker => {
            render_credential_method_picker(frame, scheme, is_edit, cred_method_cursor)
        }
        ConnectionFormStage::CredentialFields => {
            render_credential_fields(frame, cred_method, cred_fields, cred_focus)
        }
    }
}

/// Draw the scheme-picker stage: a scrollable list of known backend types.
fn render_scheme_picker(frame: &mut Frame, focus: usize) {
    let h = u16::try_from(KNOWN_SCHEMES.len())
        .unwrap_or(u16::MAX)
        .saturating_add(5) // 2 borders + 1 blank + 1 hint + 1 breathing room
        .min(frame.area().height);
    let area = centered(frame.area(), 50, h.max(5));
    frame.render_widget(Clear, area);
    let block = Block::bordered()
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
fn render_credential_method_picker(frame: &mut Frame, scheme: &str, is_edit: bool, cursor: usize) {
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

/// Draw the transfer-queue view: the active transfer(s) plus the pending list, with the selection
/// cursor marked on the pending rows.
fn render_transfer_queue(frame: &mut Frame, state: &AppState, cursor: usize) {
    let pending = &state.transfer_queue;
    let active = &state.active_transfers;
    // Active section is `active.len()` rows, or 1 row ("active: (none)") when empty.
    let active_rows = active.len().max(1);
    let rows = active_rows.saturating_add(pending.len());
    let h = u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .saturating_add(4) // 2 borders + blank separator + hint line
        .min(frame.area().height);
    let area = centered(frame.area(), 60, h.max(5));
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Transfer queue ")
        .border_style(Style::default().fg(Color::Cyan));

    let mut lines: Vec<Line> = Vec::new();
    if active.is_empty() {
        lines.push(Line::from("active: (none)".to_owned()));
    } else {
        for t in active {
            let state_label = if t.paused {
                "paused"
            } else {
                "transferring…"
            };
            let pct = match t.total {
                Some(total) if total > 0 => format!(" ({}%)", pct_of(t.bytes, total)),
                _ => String::new(),
            };
            lines.push(Line::from(format!(
                "active: {state_label} {}{pct}",
                human_bytes(t.bytes)
            )));
        }
    }
    for (i, q) in pending.iter().enumerate() {
        let verb = if q.is_move { "move" } else { "copy" };
        let marker = if i == cursor { "> " } else { "  " };
        let line = format!("{marker}{}. {verb} {} item(s)", i + 1, q.items.len());
        let style = if i == cursor {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::styled(line, style));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(if pending.is_empty() {
        "[Esc] Close".to_owned()
    } else {
        "[↑↓] select  [K/J] move  [d] drop  [x] clear all  [Esc] close".to_owned()
    }));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw a single-line text prompt (new directory, rename) with the entered text and a block cursor.
fn render_prompt(frame: &mut Frame, kind: &PromptKind, input: &str) {
    // 6 rows: 2 borders + 3 content lines + 1 of breathing space (matches the ConfirmDelete box).
    let area = centered(frame.area(), 50, 6);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
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
fn render_ai_plan(frame: &mut Frame, plan: &Plan, cursor: usize) {
    let h = u16::try_from(plan.steps.len())
        .unwrap_or(u16::MAX)
        .saturating_add(6)
        .min(frame.area().height);
    let area = centered(frame.area(), 64, h);
    frame.render_widget(Clear, area);

    let block = Block::bordered()
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
    let title = format!(" {} ", pane.cwd.as_str());
    // Bottom-right status: current sort mode, plus a `+hidden` flag when dotfiles are shown.
    let hidden = if pane.show_hidden { " +hidden" } else { "" };
    let status = format!(" sort: {}{hidden} ", pane.sort.label());
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
            let items: Vec<ListItem> = visible[top..end]
                .iter()
                .enumerate()
                .map(|(offset, e)| {
                    let i = top + offset; // index back into the visible view (marks are absolute)
                    let mark = if pane.marked.contains(&i) { '*' } else { ' ' };
                    let suffix = if e.is_dir() { "/" } else { "" };
                    let text = format!("{mark}{}{suffix}", e.name);
                    let style = if e.is_dir() {
                        Style::default().fg(theme.dir).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    ListItem::new(text).style(style)
                })
                .collect();

            let highlight = if focused {
                Style::default()
                    .bg(theme.selection_bg)
                    .fg(theme.selection_fg)
            } else {
                Style::default().add_modifier(Modifier::REVERSED)
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

fn render_status(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let pane = state.active();
    let count = pane_count_label(pane);
    // Priority: a live transfer (single or aggregate) takes over the status line; otherwise the
    // transient status message (if any); otherwise the key hints.
    let line = if !state.active_transfers.is_empty() {
        Line::from(format!(" {count}   {}", transfer_status(state)))
    } else if let Some(msg) = &state.status {
        Line::from(format!(" {count}   {msg}"))
    } else {
        let help = "q quit · Tab · ↵ open · F3 view · F4 edit · Space mark · c copy · m move · d del · p pause · r refresh · ^O conn · ^T queue · ^A ai";
        Line::from(format!(" {count}   {help}"))
    };
    frame.render_widget(
        Paragraph::new(line).style(Style::default().fg(theme.status)),
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

    // Aggregate byte/total/rate across active transfers. `total` is `None` if any is unknown
    // (a partial percentage would mislead); `rate` sums only running (non-paused) transfers.
    let bytes: u64 = active
        .iter()
        .fold(0u64, |acc, t| acc.saturating_add(t.bytes));
    let total: Option<u64> = active
        .iter()
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
        if t.paused {
            return format!("⏸ paused {amount}{suffix}");
        }
        let rate = match t.rate {
            Some(r) => format!(" at {}/s", human_bytes(r)),
            None => String::new(),
        };
        let eta = match (t.total, t.rate) {
            (Some(tot), Some(r)) if r > 0 && tot > t.bytes => {
                let secs = (tot - t.bytes) / r;
                if secs > 0 {
                    format!(", ETA {}", human_duration(secs))
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };
        return format!("⇅ transferring… {amount}{rate}{eta}{suffix}");
    }

    // Multiple transfers: a compact aggregate (per-transfer detail is in the Ctrl-T overlay).
    let n = active.len();
    if paused == n {
        return format!("⏸ {n} paused · {amount}{suffix}");
    }
    let running_rate: u64 = active
        .iter()
        .filter(|t| !t.paused)
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
fn render_port_forward_status(frame: &mut Frame, rec: &SessionRecord) {
    // 7 rows: 2 borders + title/port row + blank + status + blank + hint.
    let area = centered(frame.area(), 56, 7);
    frame.render_widget(Clear, area);

    let block = Block::bordered()
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
            label: "Copying 1 item(s)…".to_owned(),
            bytes,
            rate,
            total,
            paused,
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
        assert!(text.contains("Transfer queue"));
        assert!(text.contains("active"));
        assert!(text.contains("move"));
        assert!(text.contains("drop")); // the [d] drop control
        assert!(text.contains("move")); // the [K/J] reorder control
        assert!(text.contains("clear all"));
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
