//! Rendering the [`AppState`] with ratatui. Pure: takes `&AppState` + `&Theme`, performs no I/O.

use crate::theme::Theme;
use cairn_ai::{Plan, Reversibility, StepStatus, Verb};
use cairn_core::{AppState, Listing, Overlay, PaneState, PromptKind, Side};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
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

/// Draw the connection switcher: a centered list of the configured connections.
fn render_connections(
    frame: &mut Frame,
    connections: &[cairn_core::ConnectionChoice],
    cursor: usize,
) {
    let h = u16::try_from(connections.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(frame.area().height);
    let area = centered(frame.area(), 50, h.max(3));
    frame.render_widget(Clear, area);
    // Overlays use fixed semantic accents (not the user's pane palette) so prompts stay distinct.
    let block = Block::bordered()
        .title(" Open connection ")
        .border_style(Style::default().fg(Color::Cyan));
    let items: Vec<ListItem> = connections
        .iter()
        .map(|c| ListItem::new(c.label.clone()))
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black))
        .highlight_symbol("> ");
    let mut st = ListState::default();
    if !connections.is_empty() {
        st.select(Some(cursor.min(connections.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut st);
}

/// Draw the active modal overlay (if any) centered over the screen. Takes `&AppState` so overlays
/// that need extra state (the connection switcher's choice list) dispatch from a single site.
fn render_overlay(frame: &mut Frame, state: &AppState) {
    let Some(overlay) = &state.overlay else {
        return;
    };
    match overlay {
        Overlay::Connections { cursor } => render_connections(frame, &state.connections, *cursor),
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
        Overlay::AiPlan { plan, cursor } => render_ai_plan(frame, plan, *cursor),
        Overlay::Prompt { kind, input } => render_prompt(frame, kind, input),
    }
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
    // A live transfer takes over the status line with its running byte total (and queue depth).
    let line = if let Some(bytes) = state.transfer_bytes {
        let queued = state.transfer_queue.len();
        let suffix = if queued > 0 {
            format!(" (+{queued} queued)")
        } else {
            String::new()
        };
        Line::from(format!(
            " {count}   ⇅ transferring… {}{suffix}",
            human_bytes(bytes)
        ))
    } else {
        let help = "q quit · Tab · ↵ open · Space mark · c copy · m move · d del · r refresh · ^O conn · ^A ai";
        Line::from(format!(" {count}   {help}"))
    };
    frame.render_widget(
        Paragraph::new(line).style(Style::default().fg(theme.status)),
        area,
    );
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
        s.transfer_bytes = Some(2 * 1024 * 1024);
        let text = render_text(&s, 100, 24);
        assert!(text.contains("transferring"), "expected transfer indicator");
        assert!(
            text.contains("2.0 MiB"),
            "expected human-readable byte total"
        );
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
    fn connection_switcher_lists_choices() {
        let mut s = ready_state();
        s.connections = vec![
            cairn_core::ConnectionChoice {
                conn: ConnectionId(3),
                label: "local: /".into(),
            },
            cairn_core::ConnectionChoice {
                conn: ConnectionId(4),
                label: "local: ~/work".into(),
            },
        ];
        s.overlay = Some(cairn_core::Overlay::Connections { cursor: 0 });
        let text = render_text(&s, 80, 24);
        assert!(text.contains("Open connection"));
        assert!(text.contains("local: /"));
        assert!(text.contains("work"));
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
}
