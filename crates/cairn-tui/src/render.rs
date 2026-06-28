//! Rendering the [`AppState`] with ratatui. Pure: takes `&AppState`, performs no I/O.

use cairn_ai::{Plan, Reversibility, StepStatus, Verb};
use cairn_core::{AppState, Listing, Overlay, PaneState, Side};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

/// Render the whole application: two panes over a one-line status bar.
pub fn render(frame: &mut Frame, state: &AppState) {
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
    render_pane(frame, left, state, Side::Left);
    render_pane(frame, right, state, Side::Right);
    render_status(frame, status, state);
    if let Some(overlay) = &state.overlay {
        match overlay {
            // The connection switcher needs the choice list, which lives on the AppState.
            Overlay::Connections { cursor } => {
                render_connections(frame, &state.connections, *cursor);
            }
            other => render_overlay(frame, other),
        }
    }
}

/// Draw the connection switcher: a centered list of the configured connections.
fn render_connections(
    frame: &mut Frame,
    connections: &[cairn_core::ConnectionChoice],
    cursor: usize,
) {
    let h = (connections.len() as u16)
        .saturating_add(2)
        .min(frame.area().height);
    let area = centered(frame.area(), 50, h.max(3));
    frame.render_widget(Clear, area);
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

/// Draw a modal overlay centered over the screen.
fn render_overlay(frame: &mut Frame, overlay: &Overlay) {
    match overlay {
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
        Overlay::AiPlan { plan, cursor } => render_ai_plan(frame, plan, *cursor),
        // Rendered by `render` (it needs the AppState's connection list).
        Overlay::Connections { .. } => {}
    }
}

/// Draw the AI plan → confirm overlay: the summary, each step with its approval status and
/// reversibility, and the available actions (bulk-approve only when no step is irreversible).
fn render_ai_plan(frame: &mut Frame, plan: &Plan, cursor: usize) {
    let h = (plan.steps.len() as u16)
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

fn render_pane(frame: &mut Frame, area: Rect, state: &AppState, side: Side) {
    let pane = state.pane(side);
    let focused = state.focus == side;
    let border = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let title = format!(" {} ", pane.cwd.as_str());
    let block = Block::bordered()
        .title(title)
        .border_style(Style::default().fg(border));

    match &pane.listing {
        Listing::Loading => {
            frame.render_widget(Paragraph::new("Loading…").block(block), area);
        }
        Listing::Error(msg) => {
            let p = Paragraph::new(Line::from(format!("error: {msg}")))
                .style(Style::default().fg(Color::Red))
                .block(block);
            frame.render_widget(p, area);
        }
        Listing::Ready(entries) => {
            let items: Vec<ListItem> = entries
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let mark = if pane.marked.contains(&i) { '*' } else { ' ' };
                    let suffix = if e.is_dir() { "/" } else { "" };
                    let text = format!("{mark}{}{suffix}", e.name);
                    let style = if e.is_dir() {
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    ListItem::new(text).style(style)
                })
                .collect();

            let highlight = if focused {
                Style::default().bg(Color::Cyan).fg(Color::Black)
            } else {
                Style::default().add_modifier(Modifier::REVERSED)
            };
            let list = List::new(items)
                .block(block)
                .highlight_style(highlight)
                .highlight_symbol("> ");

            let mut list_state = ListState::default();
            if !entries.is_empty() {
                list_state.select(Some(pane.cursor.min(entries.len() - 1)));
            }
            frame.render_stateful_widget(list, area, &mut list_state);
        }
    }
}

fn render_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let pane = state.active();
    let count = pane_count_label(pane);
    let help = "q quit · Tab · ↵ open · Space mark · c copy · m move · d del · r refresh · ^O conn · ^A ai";
    let line = Line::from(format!(" {count}   {help}"));
    frame.render_widget(
        Paragraph::new(line).style(Style::default().fg(Color::Gray)),
        area,
    );
}

fn pane_count_label(pane: &PaneState) -> String {
    match &pane.listing {
        Listing::Ready(_) => format!(
            "{}/{}",
            pane.cursor.saturating_add(1).min(pane.len().max(1)),
            pane.len()
        ),
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
        terminal.draw(|f| render(f, &state)).unwrap();
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
        terminal.draw(|f| render(f, state)).unwrap();
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
        terminal.draw(|f| render(f, &s)).unwrap();
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
