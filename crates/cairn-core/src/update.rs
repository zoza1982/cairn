//! The pure reducer: `update(&mut AppState, Msg) -> Vec<AppEffect>`. No I/O, no `.await`.

use crate::msg::{Action, AppEffect, AppEvent, Msg, DEMO_AI_PROMPT};
use crate::state::{AppState, Listing, Overlay, Side, SortMode};
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
                all: pane.show_hidden,
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
            if state.ai_pending {
                return Vec::new(); // a request is already in flight
            }
            state.ai_pending = true;
            state.status = Some("Asking the assistant…".to_owned());
            vec![AppEffect::RequestAiPlan {
                prompt: DEMO_AI_PROMPT.to_owned(),
            }]
        }
        Action::OpenConnections => {
            if state.connections.is_empty() {
                state.status = Some("No other connections configured".to_owned());
            } else {
                state.overlay = Some(Overlay::Connections { cursor: 0 });
            }
            Vec::new()
        }
        // No overlay open: confirm/cancel and the plan-only actions are no-ops.
        Action::Confirm | Action::Cancel | Action::ApproveAll | Action::Reject => Vec::new(),
        Action::Refresh => reload(state, state.focus),
        Action::CycleSort => {
            let p = state.active_mut();
            let new_sort = p.sort.next();
            p.sort = new_sort;
            // Keep the cursor on the same entry across the re-order (MC convention).
            let focused = p.current().map(|e| e.name.clone());
            // Re-order the already-loaded entries in place — no re-list needed. `Arc::make_mut`
            // mutates without cloning while the listing isn't shared (the render path only borrows
            // it transiently between frames), so a 100k-entry pane isn't copied on every keypress.
            if let Listing::Ready(entries) = &mut p.listing {
                let v: &mut Vec<Entry> = Arc::make_mut(entries);
                sort_entries(v, new_sort);
            }
            // Marks are positional; a re-order invalidates them.
            p.marked.clear();
            if let Some(name) = focused {
                if let Some(idx) = p.listing.entries().iter().position(|e| e.name == name) {
                    p.cursor = idx;
                }
            }
            p.clamp_cursor();
            state.status = Some(format!("Sort: {}", new_sort.label()));
            Vec::new()
        }
        Action::ToggleHidden => {
            let p = state.active_mut();
            p.show_hidden = !p.show_hidden;
            let shown = p.show_hidden;
            // Hidden entries come from the backend (`ListOpts::all`), so re-list to fetch/drop them.
            let fx = reload(state, state.focus);
            state.status = Some(if shown {
                "Hidden files: on".to_owned()
            } else {
                "Hidden files: off".to_owned()
            });
            fx
        }
        Action::Quit => {
            state.should_quit = true;
            Vec::new()
        }
    }
}

/// Handle an action while a modal overlay is open. Routes to the handler for the open overlay.
fn apply_overlay_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    // Quit (q / Ctrl-C) always quits immediately, even from within an overlay — close it first so
    // the terminal is restored cleanly. Esc/Cancel only dismisses the overlay.
    if action == Action::Quit {
        state.overlay = None;
        state.should_quit = true;
        return Vec::new();
    }
    match &state.overlay {
        Some(Overlay::ConfirmDelete { .. }) => apply_confirm_delete_action(state, action),
        Some(Overlay::AiPlan { .. }) => apply_ai_plan_action(state, action),
        Some(Overlay::Connections { .. }) => apply_connections_action(state, action),
        None => Vec::new(),
    }
}

/// Drive the connection switcher: navigate the choice list and open the selected connection in the
/// active pane.
fn apply_connections_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    let n = state.connections.len();
    let Some(Overlay::Connections { cursor }) = &mut state.overlay else {
        return Vec::new();
    };
    match action {
        Action::CursorUp => {
            *cursor = cursor.saturating_sub(1);
            Vec::new()
        }
        Action::CursorDown => {
            if *cursor + 1 < n {
                *cursor += 1;
            }
            Vec::new()
        }
        Action::Confirm | Action::Enter => {
            let Some(Overlay::Connections { cursor }) = state.overlay.take() else {
                return Vec::new();
            };
            let choice = state.connections[cursor].clone();
            state.status = Some(format!("Opened {}", choice.label));
            let side = state.focus;
            navigate_to_conn(state, side, choice.conn)
        }
        Action::Cancel => {
            state.overlay = None;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Re-point a pane to a different connection, resetting it to the root and reloading (delegates to
/// [`navigate`] after switching the connection so any future per-navigation reset is shared).
fn navigate_to_conn(
    state: &mut AppState,
    side: Side,
    conn: cairn_types::ConnectionId,
) -> Vec<AppEffect> {
    state.pane_mut(side).conn = conn;
    navigate(state, side, VfsPath::root())
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
        Action::Cancel => {
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
        Reject,
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
                // Advance to the next still-pending step (forward, wrapping), if any.
                if let Some(n) = plan.next_pending_from(i) {
                    *cursor = n;
                }
                if plan.is_all_approved() {
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
                // Execution requires every step approved, so rejecting one makes the plan
                // unrunnable; abort the whole plan rather than leave a silent dead-end.
                let i = *cursor;
                let _ = plan.reject_step(i);
                plan.abort();
                Next::Reject
            }
            Action::Cancel => {
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
        Next::Reject => {
            state.overlay = None;
            state.status = Some("Plan rejected".to_owned());
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
        all: p.show_hidden,
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
        all: p.show_hidden,
    }]
}

fn apply_event(state: &mut AppState, event: AppEvent) -> Vec<AppEffect> {
    match event {
        AppEvent::Listed {
            pane,
            conn,
            dir,
            result,
        } => {
            let p = state.pane_mut(pane);
            // Ignore a stale result for a directory/connection we've since navigated away from.
            // The connection check matters because switching backends resets the pane to `/`, so
            // `dir` alone cannot distinguish a listing from the previous connection's root.
            if p.cwd != dir || p.conn != conn {
                return Vec::new();
            }
            match result {
                Ok(page) => {
                    let mut entries = page.entries;
                    sort_entries(&mut entries, p.sort);
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
            state.ai_pending = false;
            if plan.steps.is_empty() {
                state.status = Some("Assistant proposed no steps".to_owned());
            } else {
                state.status = Some(format!("Assistant proposed {} step(s)", plan.steps.len()));
                state.overlay = Some(Overlay::AiPlan { plan, cursor: 0 });
            }
            Vec::new()
        }
        AppEvent::AiPlanProposed(Err(msg)) => {
            state.ai_pending = false;
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
                        all: p.show_hidden,
                    }
                })
                .collect()
        }
    }
}

/// Sort entries directories-first, then by the pane's [`SortMode`] within each group.
///
/// Directories always precede files regardless of mode. Ties (and the secondary key for every mode)
/// fall back to case-insensitive name order, so the result is deterministic. `Size`/`Modified` order
/// the most-relevant first (largest / newest); entries missing that field sort after those that have
/// it, then by name.
fn sort_entries(entries: &mut [Entry], mode: SortMode) {
    // Every mode has `cmp_name` as a total secondary key, so the comparator is a total order and
    // stability is unobservable — `sort_unstable_by` is the cheaper choice.
    entries.sort_unstable_by(|a, b| {
        // Directories first.
        b.is_dir().cmp(&a.is_dir()).then_with(|| match mode {
            SortMode::Name => cmp_name(a, b),
            // Largest first; `Option`'s natural order puts `None` (unknown size) last — and keeps a
            // known empty file (`Some(0)`) ahead of an unknown one. Ties fall back to name.
            SortMode::Size => b.size.cmp(&a.size).then_with(|| cmp_name(a, b)),
            // Newest first; `None` (unknown time) sorts last, then by name.
            SortMode::Modified => b.modified.cmp(&a.modified).then_with(|| cmp_name(a, b)),
        })
    });
}

/// Case-insensitive name comparison — the stable secondary key for every sort mode.
fn cmp_name(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    a.name.to_lowercase().cmp(&b.name.to_lowercase())
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
        let conn = s.pane(side).conn;
        let _ = update(
            s,
            Msg::Event(AppEvent::Listed {
                pane: side,
                conn,
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

    fn file_sized(name: &str, size: u64) -> Entry {
        let mut e = Entry::new(name, EntryKind::File);
        e.size = Some(size);
        e
    }

    fn names(s: &AppState, side: Side) -> Vec<String> {
        s.pane(side)
            .listing
            .entries()
            .iter()
            .map(|e| e.name.to_string())
            .collect()
    }

    #[test]
    fn sort_size_orders_largest_first_within_files_dirs_still_first() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                file_sized("small.txt", 10),
                Entry::new("adir", EntryKind::Dir),
                file_sized("big.txt", 9000),
                file_sized("mid.txt", 500),
            ],
        );
        // Cycle Name -> Size.
        let _ = update(&mut s, Msg::Action(Action::CycleSort));
        assert_eq!(s.active().sort, SortMode::Size);
        assert_eq!(
            names(&s, Side::Left),
            vec!["adir", "big.txt", "mid.txt", "small.txt"]
        );
    }

    #[test]
    fn sort_modified_orders_newest_first_with_unknown_times_last() {
        use std::time::{Duration, SystemTime};
        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(9_000);
        let mut a = Entry::new("old.txt", EntryKind::File);
        a.modified = Some(older);
        let mut b = Entry::new("new.txt", EntryKind::File);
        b.modified = Some(newer);
        let none = Entry::new("undated.txt", EntryKind::File); // modified: None
        let mut s = state();
        deliver(&mut s, Side::Left, vec![a, none, b]);
        // Name -> Size -> Modified.
        let _ = update(&mut s, Msg::Action(Action::CycleSort));
        let _ = update(&mut s, Msg::Action(Action::CycleSort));
        assert_eq!(s.active().sort, SortMode::Modified);
        assert_eq!(
            names(&s, Side::Left),
            vec!["new.txt", "old.txt", "undated.txt"]
        );
    }

    #[test]
    fn cycle_sort_reorders_in_place_without_a_list_effect_and_clears_marks() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![file_sized("a.txt", 1), file_sized("z.txt", 9000)],
        );
        // Mark an entry, then cycle to Size — marks must clear and no re-list is needed.
        let _ = update(&mut s, Msg::Action(Action::ToggleMark));
        assert!(!s.active().marked.is_empty());
        let fx = update(&mut s, Msg::Action(Action::CycleSort));
        assert!(fx.is_empty(), "cycling sort must not trigger I/O");
        assert!(s.active().marked.is_empty(), "marks clear on re-sort");
        assert_eq!(names(&s, Side::Left), vec!["z.txt", "a.txt"]);
    }

    #[test]
    fn cycle_sort_keeps_the_cursor_on_the_same_entry() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                file_sized("a.txt", 1),
                file_sized("m.txt", 50),
                file_sized("z.txt", 9000),
            ],
        );
        // Put the cursor on "a.txt" (smallest, first by name).
        assert_eq!(s.active().current().unwrap().name, "a.txt");
        // Cycle to Size: order becomes z, m, a — the cursor must follow "a.txt" to its new index.
        let _ = update(&mut s, Msg::Action(Action::CycleSort));
        assert_eq!(s.active().current().unwrap().name, "a.txt");
        assert_eq!(s.active().cursor, 2);
    }

    #[test]
    fn sort_set_while_loading_applies_to_the_arriving_listing() {
        let mut s = state();
        // Pane starts Loading; cycle to Size before any listing arrives (no re-sort happens yet).
        let fx = update(&mut s, Msg::Action(Action::CycleSort));
        assert!(fx.is_empty());
        assert_eq!(s.active().sort, SortMode::Size);
        // When the listing arrives it is sorted with the pane's current mode.
        deliver(
            &mut s,
            Side::Left,
            vec![file_sized("small", 1), file_sized("big", 9000)],
        );
        assert_eq!(names(&s, Side::Left), vec!["big", "small"]);
    }

    #[test]
    fn sort_mode_cycles_back_to_name() {
        assert_eq!(SortMode::Name.next(), SortMode::Size);
        assert_eq!(SortMode::Size.next(), SortMode::Modified);
        assert_eq!(SortMode::Modified.next(), SortMode::Name);
    }

    #[test]
    fn toggle_hidden_flips_flag_and_relists_with_the_all_flag() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("a", EntryKind::File)]);
        assert!(!s.active().show_hidden);
        let fx = update(&mut s, Msg::Action(Action::ToggleHidden));
        assert!(s.active().show_hidden);
        // Re-lists the active pane with the new flag.
        assert!(matches!(
            &fx[..],
            [AppEffect::List { pane, all: true, .. }] if *pane == Side::Left
        ));
        // Toggling back clears it and re-lists with all=false.
        let fx = update(&mut s, Msg::Action(Action::ToggleHidden));
        assert!(!s.active().show_hidden);
        assert!(matches!(&fx[..], [AppEffect::List { all: false, .. }]));
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
        let conn = s.pane(Side::Left).conn;
        // Deliver a result for a directory the pane is not in.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::Listed {
                pane: Side::Left,
                conn,
                dir: VfsPath::parse("/elsewhere").unwrap(),
                result: Ok(page(vec![Entry::new("ghost", EntryKind::File)])),
            }),
        );
        assert!(s.pane(Side::Left).listing.entries().is_empty());
    }

    #[test]
    fn listing_from_a_previous_connection_is_ignored() {
        // After switching a pane to another connection (also at root `/`), a slow listing from the
        // old connection's root must not be applied to the new one.
        let mut s = state();
        s.connections = vec![crate::state::ConnectionChoice {
            conn: ConnectionId(9),
            label: "other".into(),
        }];
        let old_conn = s.active().conn;
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let _ = update(&mut s, Msg::Action(Action::Confirm)); // now on conn 9, cwd "/"
        assert_eq!(s.active().conn, ConnectionId(9));
        // The old connection's root listing arrives late — must be ignored.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::Listed {
                pane: Side::Left,
                conn: old_conn,
                dir: VfsPath::root(),
                result: Ok(page(vec![Entry::new("ghost", EntryKind::File)])),
            }),
        );
        assert!(s.active().listing.entries().is_empty());
        assert!(matches!(s.active().listing, Listing::Loading));
    }

    #[test]
    fn error_listing_is_recorded_redacted() {
        let mut s = state();
        let dir = s.pane(Side::Left).cwd.clone();
        let conn = s.pane(Side::Left).conn;
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::Listed {
                pane: Side::Left,
                conn,
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
                error: None,
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
    fn connection_switcher_opens_and_repoints_pane() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.connections = vec![
            ConnectionChoice {
                conn: ConnectionId(1),
                label: "local: /a".into(),
            },
            ConnectionChoice {
                conn: ConnectionId(7),
                label: "local: /b".into(),
            },
        ];
        // Open the switcher.
        let fx = update(&mut s, Msg::Action(Action::OpenConnections));
        assert!(fx.is_empty());
        assert!(matches!(
            s.overlay,
            Some(Overlay::Connections { cursor: 0 })
        ));
        // Move to the second choice and select it.
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(s.overlay.is_none());
        // The active pane now points at the chosen connection, at the root, loading.
        assert_eq!(s.active().conn, ConnectionId(7));
        assert_eq!(s.active().cwd.as_str(), "/");
        assert!(matches!(s.active().listing, Listing::Loading));
        assert!(matches!(&fx[..], [AppEffect::List { conn, .. }] if *conn == ConnectionId(7)));
    }

    #[test]
    fn connection_switcher_with_no_choices_is_a_noop() {
        let mut s = state();
        let fx = update(&mut s, Msg::Action(Action::OpenConnections));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
        assert!(s
            .status
            .as_deref()
            .unwrap()
            .contains("No other connections"));
    }

    #[test]
    fn connection_switcher_cursor_clamps_and_quit_quits() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.connections = vec![
            ConnectionChoice {
                conn: ConnectionId(3),
                label: "a".into(),
            },
            ConnectionChoice {
                conn: ConnectionId(4),
                label: "b".into(),
            },
        ];
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        // Up at the top clamps; down past the end clamps.
        let _ = update(&mut s, Msg::Action(Action::CursorUp));
        assert!(matches!(
            s.overlay,
            Some(Overlay::Connections { cursor: 0 })
        ));
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        assert!(matches!(
            s.overlay,
            Some(Overlay::Connections { cursor: 1 })
        ));
        // Quit from within the overlay quits the app.
        let _ = update(&mut s, Msg::Action(Action::Quit));
        assert!(s.should_quit);
        assert!(s.overlay.is_none());
    }

    #[test]
    fn connection_switcher_cancel_closes() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.connections = vec![ConnectionChoice {
            conn: ConnectionId(1),
            label: "local".into(),
        }];
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
    }

    #[test]
    fn quit_from_overlay_quits_the_app() {
        let mut s = state();
        open_plan(&mut s, &["copy"]);
        let _ = update(&mut s, Msg::Action(Action::Quit));
        assert!(s.should_quit);
        assert!(s.overlay.is_none());
    }

    #[test]
    fn rejecting_a_step_aborts_the_plan() {
        let mut s = state();
        open_plan(&mut s, &["copy", "delete"]);
        let fx = update(&mut s, Msg::Action(Action::Reject));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
        assert_eq!(s.status.as_deref(), Some("Plan rejected"));
    }

    #[test]
    fn empty_plan_does_not_open_overlay() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::AiPropose)); // sets ai_pending
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::AiPlanProposed(Ok(make_plan(&[])))),
        );
        assert!(s.overlay.is_none());
        assert!(!s.ai_pending);
        assert_eq!(s.status.as_deref(), Some("Assistant proposed no steps"));
    }

    #[test]
    fn ai_propose_is_suppressed_while_a_request_is_in_flight() {
        let mut s = state();
        let fx = update(&mut s, Msg::Action(Action::AiPropose));
        assert_eq!(fx.len(), 1); // first request goes out
        assert!(s.ai_pending);
        let fx = update(&mut s, Msg::Action(Action::AiPropose));
        assert!(fx.is_empty()); // second is suppressed
                                // The proposal clears the pending flag.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::AiPlanProposed(Ok(make_plan(&["list"])))),
        );
        assert!(!s.ai_pending);
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
