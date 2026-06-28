//! The pure reducer: `update(&mut AppState, Msg) -> Vec<AppEffect>`. No I/O, no `.await`.

use crate::msg::{Action, AppEffect, AppEvent, Msg, DEMO_AI_PROMPT};
use crate::state::{AppState, Listing, Overlay, Side};
use cairn_ai::StepStatus;
use cairn_types::{Entry, VfsPath};
use std::sync::Arc;

/// Apply a message to the state, returning any effects to run. Deterministic and side-effect-free.
#[must_use]
pub fn update(state: &mut AppState, msg: Msg) -> Vec<AppEffect> {
    match msg {
        Msg::Action(action) => apply_action(state, action),
        Msg::Event(event) => apply_event(state, event),
        Msg::Tick => Vec::new(),
    }
}

/// The effects needed to populate both panes at startup.
#[must_use]
pub fn initial_effects(state: &AppState) -> Vec<AppEffect> {
    [Side::Left, Side::Right]
        .into_iter()
        .map(|side| {
            let pane = state.pane(side);
            AppEffect::List {
                pane: side,
                conn: pane.conn,
                dir: pane.cwd.clone(),
            }
        })
        .collect()
}

fn apply_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    if state.overlay.is_some() {
        return apply_overlay_action(state, action);
    }
    match action {
        Action::CursorUp => {
            let p = state.active_mut();
            p.cursor = p.cursor.saturating_sub(1);
            Vec::new()
        }
        Action::CursorDown => {
            let p = state.active_mut();
            if p.cursor + 1 < p.len() {
                p.cursor += 1;
            }
            Vec::new()
        }
        Action::CursorTop => {
            state.active_mut().cursor = 0;
            Vec::new()
        }
        Action::CursorBottom => {
            let p = state.active_mut();
            p.cursor = p.len().saturating_sub(1);
            Vec::new()
        }
        Action::SwitchPane => {
            state.focus = state.focus.other();
            Vec::new()
        }
        Action::ToggleMark => {
            let p = state.active_mut();
            if p.cursor < p.len() && !p.marked.remove(&p.cursor) {
                p.marked.insert(p.cursor);
            }
            Vec::new()
        }
        Action::Enter => enter_dir(state),
        Action::Leave => leave_dir(state),
        Action::Copy => start_transfer(state, false),
        Action::Move => start_transfer(state, true),
        Action::Delete => confirm_delete(state),
        Action::AiPropose => {
            state.status = Some("Asking the assistant…".to_owned());
            vec![AppEffect::RequestAiPlan {
                prompt: DEMO_AI_PROMPT.to_owned(),
            }]
        }
        // No overlay open: confirm/cancel and the plan-only actions are no-ops.
        Action::Confirm | Action::Cancel | Action::ApproveAll | Action::Reject => Vec::new(),
        Action::Refresh => reload(state, state.focus),
        Action::Quit => {
            state.should_quit = true;
            Vec::new()
        }
    }
}

/// Handle an action while a modal overlay is open. Routes to the handler for the open overlay.
fn apply_overlay_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    match &state.overlay {
        Some(Overlay::ConfirmDelete { .. }) => apply_confirm_delete_action(state, action),
        Some(Overlay::AiPlan { .. }) => apply_ai_plan_action(state, action),
        None => Vec::new(),
    }
}

fn apply_confirm_delete_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    match action {
        Action::Confirm | Action::Enter => match state.overlay.take() {
            Some(Overlay::ConfirmDelete { conn, paths }) => {
                state.status = Some(format!("Deleting {} item(s)…", paths.len()));
                let focus = state.focus;
                state.pane_mut(focus).marked.clear();
                vec![AppEffect::Delete { conn, paths }]
            }
            _ => Vec::new(),
        },
        Action::Cancel | Action::Quit => {
            state.overlay = None;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Drive the plan → confirm overlay. The plan's per-step approval state lives in the overlay; this
/// only mutates it (and emits [`AppEffect::ExecutePlan`] once every step is approved).
fn apply_ai_plan_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    /// What to do after releasing the borrow on the overlay's plan.
    enum Next {
        Stay,
        Execute,
        Abort,
        BulkBlocked,
    }

    let next = {
        let Some(Overlay::AiPlan { plan, cursor }) = &mut state.overlay else {
            return Vec::new();
        };
        match action {
            Action::CursorUp => {
                *cursor = cursor.saturating_sub(1);
                Next::Stay
            }
            Action::CursorDown => {
                if *cursor + 1 < plan.steps.len() {
                    *cursor += 1;
                }
                Next::Stay
            }
            Action::Confirm | Action::Enter => {
                let i = *cursor;
                let _ = plan.approve_step(i);
                // Advance to the next still-pending step, if any.
                if let Some(n) = plan
                    .steps
                    .iter()
                    .position(|s| s.status == StepStatus::Pending)
                {
                    *cursor = n;
                }
                if all_approved(plan) {
                    Next::Execute
                } else {
                    Next::Stay
                }
            }
            Action::ApproveAll => {
                if plan.approve_all().is_ok() {
                    Next::Execute
                } else {
                    Next::BulkBlocked
                }
            }
            Action::Reject => {
                let i = *cursor;
                let _ = plan.reject_step(i);
                Next::Stay
            }
            Action::Cancel | Action::Quit => {
                plan.abort();
                Next::Abort
            }
            _ => Next::Stay,
        }
    };

    match next {
        Next::Stay => Vec::new(),
        Next::BulkBlocked => {
            state.status =
                Some("Plan has irreversible steps — approve each individually".to_owned());
            Vec::new()
        }
        Next::Abort => {
            state.overlay = None;
            state.status = Some("Plan aborted".to_owned());
            Vec::new()
        }
        Next::Execute => {
            let Some(Overlay::AiPlan { plan, .. }) = state.overlay.take() else {
                return Vec::new();
            };
            state.status = Some(format!("Executing {} step(s)…", plan.steps.len()));
            vec![AppEffect::ExecutePlan { plan }]
        }
    }
}

/// Whether every step in the plan has been approved (the precondition for execution).
fn all_approved(plan: &cairn_ai::Plan) -> bool {
    !plan.steps.is_empty() && plan.steps.iter().all(|s| s.status == StepStatus::Approved)
}

/// The `(source-full-path, leaf-name)` pairs targeted by an operation: the marked entries, or the
/// entry under the cursor if nothing is marked.
fn op_targets(state: &AppState, side: Side) -> Vec<(VfsPath, String)> {
    let pane = state.pane(side);
    let entries = pane.listing.entries();
    let indices: Vec<usize> = if pane.marked.is_empty() {
        if pane.cursor < entries.len() {
            vec![pane.cursor]
        } else {
            vec![]
        }
    } else {
        pane.marked
            .iter()
            .copied()
            .filter(|&i| i < entries.len())
            .collect()
    };
    indices
        .into_iter()
        .filter_map(|i| {
            let name = entries[i].name.to_string();
            pane.cwd.join(&name).ok().map(|full| (full, name))
        })
        .collect()
}

fn start_transfer(state: &mut AppState, is_move: bool) -> Vec<AppEffect> {
    let src = state.focus;
    let dst = src.other();
    let targets = op_targets(state, src);
    if targets.is_empty() {
        return Vec::new();
    }
    let dst_cwd = state.pane(dst).cwd.clone();
    let src_conn = state.pane(src).conn;
    let dst_conn = state.pane(dst).conn;
    let mut items = Vec::new();
    for (from, name) in &targets {
        if let Ok(to) = dst_cwd.join(name) {
            items.push((from.clone(), to));
        }
    }
    state.status = Some(format!(
        "{} {} item(s)…",
        if is_move { "Moving" } else { "Copying" },
        items.len()
    ));
    state.pane_mut(src).marked.clear();
    vec![AppEffect::Transfer {
        src_conn,
        dst_conn,
        items,
        is_move,
    }]
}

fn confirm_delete(state: &mut AppState) -> Vec<AppEffect> {
    let side = state.focus;
    let targets = op_targets(state, side);
    if targets.is_empty() {
        return Vec::new();
    }
    let conn = state.pane(side).conn;
    let paths = targets.into_iter().map(|(full, _)| full).collect();
    state.overlay = Some(Overlay::ConfirmDelete { conn, paths });
    Vec::new()
}

fn enter_dir(state: &mut AppState) -> Vec<AppEffect> {
    let side = state.focus;
    let target = {
        let p = state.pane(side);
        p.current().and_then(|e| {
            if e.is_dir() {
                p.cwd.join(&e.name).ok()
            } else {
                None
            }
        })
    };
    match target {
        Some(dir) => navigate(state, side, dir),
        None => Vec::new(),
    }
}

fn leave_dir(state: &mut AppState) -> Vec<AppEffect> {
    let side = state.focus;
    let parent = state.pane(side).cwd.parent();
    match parent {
        Some(dir) => navigate(state, side, dir),
        None => Vec::new(),
    }
}

fn navigate(state: &mut AppState, side: Side, dir: cairn_types::VfsPath) -> Vec<AppEffect> {
    let p = state.pane_mut(side);
    p.cwd = dir.clone();
    p.listing = Listing::Loading;
    p.cursor = 0;
    p.marked.clear();
    vec![AppEffect::List {
        pane: side,
        conn: p.conn,
        dir,
    }]
}

fn reload(state: &mut AppState, side: Side) -> Vec<AppEffect> {
    let p = state.pane_mut(side);
    let dir = p.cwd.clone();
    p.listing = Listing::Loading;
    p.marked.clear();
    vec![AppEffect::List {
        pane: side,
        conn: p.conn,
        dir,
    }]
}

fn apply_event(state: &mut AppState, event: AppEvent) -> Vec<AppEffect> {
    match event {
        AppEvent::Listed { pane, dir, result } => {
            let p = state.pane_mut(pane);
            // Ignore a stale result for a directory we've since navigated away from.
            if p.cwd != dir {
                return Vec::new();
            }
            match result {
                Ok(page) => {
                    let mut entries = page.entries;
                    sort_entries(&mut entries);
                    p.listing = Listing::Ready(Arc::new(entries));
                    p.clamp_cursor();
                }
                Err(e) => {
                    p.listing = Listing::Error(e.redacted().to_string());
                }
            }
            Vec::new()
        }
        AppEvent::AiPlanProposed(Ok(plan)) => {
            state.status = Some(format!("Assistant proposed {} step(s)", plan.steps.len()));
            state.overlay = Some(Overlay::AiPlan { plan, cursor: 0 });
            Vec::new()
        }
        AppEvent::AiPlanProposed(Err(msg)) => {
            state.status = Some(format!("assistant error: {msg}"));
            Vec::new()
        }
        AppEvent::OpDone { status, error } => {
            state.status = Some(if error {
                format!("error: {status}")
            } else {
                status
            });
            // Refresh both panes so the result of the operation is reflected.
            [Side::Left, Side::Right]
                .into_iter()
                .map(|side| {
                    let p = state.pane(side);
                    AppEffect::List {
                        pane: side,
                        conn: p.conn,
                        dir: p.cwd.clone(),
                    }
                })
                .collect()
        }
    }
}

/// Sort entries directories-first, then case-insensitively by name.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_types::{ConnectionId, Entry, EntryKind, VfsPath};
    use cairn_vfs::{ListPage, VfsError};

    fn state() -> AppState {
        AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root())
    }

    fn page(entries: Vec<Entry>) -> ListPage {
        ListPage {
            entries,
            cursor: None,
            done: true,
        }
    }

    fn deliver(s: &mut AppState, side: Side, entries: Vec<Entry>) {
        let dir = s.pane(side).cwd.clone();
        let _ = update(
            s,
            Msg::Event(AppEvent::Listed {
                pane: side,
                dir,
                result: Ok(page(entries)),
            }),
        );
    }

    #[test]
    fn initial_effects_list_both_panes() {
        let s = state();
        let fx = initial_effects(&s);
        assert_eq!(fx.len(), 2);
    }

    #[test]
    fn listed_sorts_dirs_first() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("zebra.txt", EntryKind::File),
                Entry::new("apple", EntryKind::Dir),
                Entry::new("banana.txt", EntryKind::File),
            ],
        );
        let names: Vec<_> = s
            .pane(Side::Left)
            .listing
            .entries()
            .iter()
            .map(|e| e.name.to_string())
            .collect();
        assert_eq!(names, vec!["apple", "banana.txt", "zebra.txt"]);
    }

    #[test]
    fn cursor_movement_clamps() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a", EntryKind::File),
                Entry::new("b", EntryKind::File),
            ],
        );
        let _ = update(&mut s, Msg::Action(Action::CursorUp)); // stays at 0
        assert_eq!(s.active().cursor, 0);
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        assert_eq!(s.active().cursor, 1);
        let _ = update(&mut s, Msg::Action(Action::CursorDown)); // clamps at last
        assert_eq!(s.active().cursor, 1);
        let _ = update(&mut s, Msg::Action(Action::CursorBottom));
        assert_eq!(s.active().cursor, 1);
        let _ = update(&mut s, Msg::Action(Action::CursorTop));
        assert_eq!(s.active().cursor, 0);
    }

    #[test]
    fn enter_directory_emits_list_effect() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("dir", EntryKind::Dir),
                Entry::new("file", EntryKind::File),
            ],
        );
        // cursor at 0 = "dir" (dirs sorted first)
        let fx = update(&mut s, Msg::Action(Action::Enter));
        assert_eq!(fx.len(), 1);
        assert_eq!(s.active().cwd.as_str(), "/dir");
        assert!(matches!(s.active().listing, Listing::Loading));
    }

    #[test]
    fn enter_file_does_nothing() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("file", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::Enter));
        assert!(fx.is_empty());
        assert_eq!(s.active().cwd.as_str(), "/");
    }

    #[test]
    fn leave_at_root_does_nothing() {
        let mut s = state();
        let fx = update(&mut s, Msg::Action(Action::Leave));
        assert!(fx.is_empty());
    }

    #[test]
    fn switch_pane_and_marks() {
        let mut s = state();
        assert_eq!(s.focus, Side::Left);
        let _ = update(&mut s, Msg::Action(Action::SwitchPane));
        assert_eq!(s.focus, Side::Right);
        deliver(&mut s, Side::Right, vec![Entry::new("a", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::ToggleMark));
        assert!(s.active().marked.contains(&0));
        let _ = update(&mut s, Msg::Action(Action::ToggleMark));
        assert!(!s.active().marked.contains(&0));
    }

    #[test]
    fn stale_listing_is_ignored() {
        let mut s = state();
        // Deliver a result for a directory the pane is not in.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::Listed {
                pane: Side::Left,
                dir: VfsPath::parse("/elsewhere").unwrap(),
                result: Ok(page(vec![Entry::new("ghost", EntryKind::File)])),
            }),
        );
        assert!(s.pane(Side::Left).listing.entries().is_empty());
    }

    #[test]
    fn error_listing_is_recorded_redacted() {
        let mut s = state();
        let dir = s.pane(Side::Left).cwd.clone();
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::Listed {
                pane: Side::Left,
                dir,
                result: Err(VfsError::Forbidden(VfsPath::parse("/secret").unwrap())),
            }),
        );
        match &s.pane(Side::Left).listing {
            Listing::Error(msg) => assert!(!msg.contains("secret")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn quit_sets_flag() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::Quit));
        assert!(s.should_quit);
    }

    #[test]
    fn copy_emits_transfer_effect_for_current_entry() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::Copy));
        assert_eq!(fx.len(), 1);
        match &fx[0] {
            AppEffect::Transfer { items, is_move, .. } => {
                assert!(!is_move);
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].0.as_str(), "/f.txt");
            }
            other => panic!("expected Transfer, got {other:?}"),
        }
    }

    #[test]
    fn delete_confirm_flow() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("doomed", EntryKind::File)],
        );
        // First Delete opens the confirm overlay, emits nothing.
        let fx = update(&mut s, Msg::Action(Action::Delete));
        assert!(fx.is_empty());
        assert!(s.overlay.is_some());
        // Confirm emits the Delete effect and closes the overlay.
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert_eq!(fx.len(), 1);
        assert!(matches!(fx[0], AppEffect::Delete { .. }));
        assert!(s.overlay.is_none());
    }

    #[test]
    fn delete_cancel_closes_overlay_without_effect() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("safe", EntryKind::File)],
        );
        let _ = update(&mut s, Msg::Action(Action::Delete));
        assert!(s.overlay.is_some());
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
        // The entry was not deleted (no effect emitted); cursor navigation still works.
        let fx = update(&mut s, Msg::Action(Action::CursorDown));
        assert!(fx.is_empty());
    }

    /// Build a proposed plan from a list of tool names (capability resolved from the tool registry).
    fn make_plan(tools: &[&str]) -> cairn_ai::Plan {
        use cairn_ai::{capability_for, Plan, PlanState, PlanStep, StepStatus};
        let steps = tools
            .iter()
            .map(|tool| PlanStep {
                tool: (*tool).to_owned(),
                input: serde_json::Value::Null,
                description: format!("{tool} something"),
                capability: capability_for(tool).expect("known tool"),
                status: StepStatus::Pending,
            })
            .collect();
        Plan {
            summary: "test plan".to_owned(),
            steps,
            state: PlanState::Proposed,
        }
    }

    fn open_plan(s: &mut AppState, tools: &[&str]) {
        let _ = update(
            s,
            Msg::Event(AppEvent::AiPlanProposed(Ok(make_plan(tools)))),
        );
    }

    #[test]
    fn ai_propose_emits_request_effect() {
        let mut s = state();
        let fx = update(&mut s, Msg::Action(Action::AiPropose));
        assert_eq!(fx.len(), 1);
        assert!(matches!(fx[0], AppEffect::RequestAiPlan { .. }));
    }

    #[test]
    fn proposed_plan_opens_overlay() {
        let mut s = state();
        open_plan(&mut s, &["list", "copy"]);
        assert!(matches!(s.overlay, Some(Overlay::AiPlan { .. })));
    }

    #[test]
    fn safe_plan_bulk_approves_and_executes() {
        let mut s = state();
        open_plan(&mut s, &["list", "copy", "move"]); // all bulk-approvable
        let fx = update(&mut s, Msg::Action(Action::ApproveAll));
        assert_eq!(fx.len(), 1);
        assert!(matches!(fx[0], AppEffect::ExecutePlan { .. }));
        assert!(s.overlay.is_none());
    }

    #[test]
    fn destructive_plan_blocks_bulk_approve() {
        let mut s = state();
        open_plan(&mut s, &["copy", "delete"]); // delete is irreversible
        let fx = update(&mut s, Msg::Action(Action::ApproveAll));
        assert!(fx.is_empty());
        // Overlay stays open; status explains why bulk-approve was refused.
        assert!(matches!(s.overlay, Some(Overlay::AiPlan { .. })));
        assert!(s.status.as_deref().unwrap().contains("individually"));
    }

    #[test]
    fn destructive_plan_executes_after_stepping_through() {
        let mut s = state();
        open_plan(&mut s, &["copy", "delete"]);
        // Approve each step in turn; only the last approval triggers execution.
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(fx.is_empty());
        assert!(matches!(s.overlay, Some(Overlay::AiPlan { .. })));
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert_eq!(fx.len(), 1);
        assert!(matches!(fx[0], AppEffect::ExecutePlan { .. }));
        assert!(s.overlay.is_none());
    }

    #[test]
    fn plan_abort_closes_overlay_without_effect() {
        let mut s = state();
        open_plan(&mut s, &["delete"]);
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
        assert_eq!(s.status.as_deref(), Some("Plan aborted"));
    }

    #[test]
    fn op_done_refreshes_both_panes() {
        let mut s = state();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::OpDone {
                status: "Copied 1 item".into(),
                error: false,
            }),
        );
        assert_eq!(fx.len(), 2);
        assert_eq!(s.status.as_deref(), Some("Copied 1 item"));
    }
}
