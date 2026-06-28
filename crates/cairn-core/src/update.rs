//! The pure reducer: `update(&mut AppState, Msg) -> Vec<AppEffect>`. No I/O, no `.await`.

use crate::msg::{Action, AppEffect, AppEvent, Msg, TextEdit};
use crate::state::{AppState, Listing, Overlay, PromptKind, Side, SortMode};
use cairn_types::{Entry, VfsPath};
use std::sync::Arc;

/// Apply a message to the state, returning any effects to run. Deterministic and side-effect-free.
#[must_use]
pub fn update(state: &mut AppState, msg: Msg) -> Vec<AppEffect> {
    match msg {
        Msg::Action(action) => apply_action(state, action),
        Msg::Text(edit) => apply_text(state, edit),
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
    // While the assistant is preparing a plan, suppress actions that would open a competing overlay:
    // the proposal opens its own review overlay when it arrives and must not clobber another modal
    // (e.g. a half-typed prompt or a delete confirmation). AiPropose has its own pending guard below.
    if state.ai_pending
        && matches!(
            action,
            Action::MakeDir | Action::Rename | Action::Delete | Action::OpenConnections
        )
    {
        state.status = Some("The assistant is preparing a plan…".to_owned());
        return Vec::new();
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
            // Open a freeform prompt; the request is sent when the user submits it.
            state.overlay = Some(Overlay::Prompt {
                kind: PromptKind::AiPrompt,
                input: String::new(),
            });
            Vec::new()
        }
        Action::OpenConnections => {
            if state.connections.is_empty() {
                state.status = Some("No other connections configured".to_owned());
            } else {
                state.overlay = Some(Overlay::Connections { cursor: 0 });
            }
            Vec::new()
        }
        Action::MakeDir => {
            state.overlay = Some(Overlay::Prompt {
                kind: PromptKind::MakeDir,
                input: String::new(),
            });
            Vec::new()
        }
        Action::Rename => {
            // Rename targets the entry under the cursor; pre-fill its current name.
            let Some(entry) = state.active().current() else {
                state.status = Some("Nothing to rename".to_owned());
                return Vec::new();
            };
            let name = entry.name.to_string();
            let Ok(from) = state.active().cwd.join(&name) else {
                state.status = Some("Cannot rename this entry".to_owned());
                return Vec::new();
            };
            state.overlay = Some(Overlay::Prompt {
                kind: PromptKind::Rename { from },
                input: name,
            });
            Vec::new()
        }
        // With no overlay, Cancel (Esc) aborts an in-flight transfer if one is running.
        Action::Cancel if state.transfer_bytes.is_some() => {
            state.status = Some("Cancelling transfer…".to_owned());
            vec![AppEffect::CancelTransfer]
        }
        // No overlay open: confirm/cancel and the plan-only actions are otherwise no-ops.
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
                // Re-find the focused entry in the (possibly filtered) visible view.
                if let Some(idx) = p.visible().iter().position(|e| e.name == name) {
                    p.cursor = idx;
                }
            }
            p.clamp_cursor();
            state.status = Some(format!("Sort: {}", new_sort.label()));
            Vec::new()
        }
        Action::Filter => {
            // Begin (or re-focus) filter-as-you-type on the active pane. Keystrokes now edit the
            // filter live via `Msg::Text` until Enter (confirm) or Esc (clear).
            let p = state.active_mut();
            p.filter.get_or_insert_with(String::new);
            p.filter_editing = true;
            state.status = Some("Filter: (type to filter, Enter to keep, Esc to clear)".to_owned());
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

/// Handle a text-editing keystroke. Routed to the open text prompt, or — if none — to the active
/// pane's live filter when it is being edited. A stray keystroke with neither active is a no-op.
fn apply_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    if matches!(state.overlay, Some(Overlay::Prompt { .. })) {
        return apply_prompt_text(state, edit);
    }
    // Filter editing only owns input when no overlay is open (lock-step with `capturing_text`).
    if state.overlay.is_none() && state.active().filter_editing {
        return apply_filter_text(state, edit);
    }
    Vec::new()
}

/// Edit the open text prompt.
fn apply_prompt_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    let Some(Overlay::Prompt { input, kind }) = &mut state.overlay else {
        return Vec::new();
    };
    match edit {
        TextEdit::Insert(c) => {
            // Always reject control characters. A filename prompt also rejects `/` at the source (it
            // is a single path component, re-validated on submit); a freeform prompt accepts it.
            if !(c.is_control() || (kind.is_filename() && c == '/')) {
                input.push(c);
            }
            Vec::new()
        }
        TextEdit::Backspace => {
            input.pop();
            Vec::new()
        }
        TextEdit::Cancel => {
            state.overlay = None;
            Vec::new()
        }
        TextEdit::Submit => submit_prompt(state),
    }
}

/// Edit the active pane's live filter. Each text change re-indexes the visible view, so marks (which
/// index that view) are cleared and the cursor is re-clamped.
fn apply_filter_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    let p = state.active_mut();
    let Some(filter) = &mut p.filter else {
        p.filter_editing = false;
        return Vec::new();
    };
    match edit {
        TextEdit::Insert(c) => {
            if !c.is_control() {
                filter.push(c);
                p.marked.clear();
                p.cursor = 0;
            }
        }
        TextEdit::Backspace => {
            // Only re-index the view if the filter text actually changed.
            if filter.pop().is_some() {
                p.marked.clear();
                p.cursor = 0;
            }
        }
        TextEdit::Submit => {
            // Keep the filter applied but stop editing; an empty filter is dropped entirely.
            if filter.is_empty() {
                p.filter = None;
            }
            p.filter_editing = false;
        }
        TextEdit::Cancel => {
            // Clear the filter and show the full listing again.
            p.filter = None;
            p.filter_editing = false;
            p.marked.clear();
            p.cursor = 0;
        }
    }
    state.active_mut().clamp_cursor();
    Vec::new()
}

/// Dispatch a submitted prompt to its per-kind handler. The single exhaustive match means a new
/// [`PromptKind`] cannot be silently dropped. The clone ends the borrow on `state.overlay` before the
/// handlers take `&mut state` (and is cheap — `AiPrompt`/`MakeDir` are trivial, `Rename` clones one
/// `VfsPath`, the same allocation the rename path already made).
fn submit_prompt(state: &mut AppState) -> Vec<AppEffect> {
    let Some(Overlay::Prompt { kind, .. }) = &state.overlay else {
        return Vec::new();
    };
    match kind.clone() {
        PromptKind::AiPrompt => submit_ai_prompt(state),
        PromptKind::MakeDir | PromptKind::Rename { .. } => submit_filename_prompt(state),
    }
}

/// Submit a freeform AI request: accepts arbitrary text (only non-empty required), drives a plan
/// request rather than a filesystem effect.
fn submit_ai_prompt(state: &mut AppState) -> Vec<AppEffect> {
    let Some(Overlay::Prompt { input, .. }) = &state.overlay else {
        return Vec::new();
    };
    let prompt = input.trim().to_owned();
    if prompt.is_empty() {
        state.status = Some("Enter a request for the assistant".to_owned());
        return Vec::new();
    }
    state.overlay = None;
    state.ai_pending = true;
    state.status = Some("Asking the assistant…".to_owned());
    vec![AppEffect::RequestAiPlan { prompt }]
}

/// Submit a filename prompt (new directory / rename). On an invalid name the prompt stays open and a
/// status message explains why.
fn submit_filename_prompt(state: &mut AppState) -> Vec<AppEffect> {
    let Some(Overlay::Prompt { kind, input }) = &state.overlay else {
        return Vec::new();
    };
    let name = input.trim();
    if let Err(why) = validate_name(name) {
        state.status = Some(why.to_owned());
        return Vec::new();
    }
    let side = state.focus;
    let conn = state.pane(side).conn;
    let effect = match kind {
        PromptKind::MakeDir => {
            let Ok(path) = state.pane(side).cwd.join(name) else {
                state.status = Some("Invalid directory name".to_owned());
                return Vec::new();
            };
            state.status = Some(format!("Creating {name}…"));
            AppEffect::CreateDir { conn, path }
        }
        PromptKind::Rename { from } => {
            // The new path lives in the same directory as the original.
            let parent = from.parent().unwrap_or_else(VfsPath::root);
            let Ok(to) = parent.join(name) else {
                state.status = Some("Invalid name".to_owned());
                return Vec::new();
            };
            if &to == from {
                // No change — just close the prompt without an effect.
                state.overlay = None;
                return Vec::new();
            }
            state.status = Some(format!("Renaming to {name}…"));
            AppEffect::Rename {
                conn,
                from: from.clone(),
                to,
            }
        }
        // `submit_prompt` routes AiPrompt to `submit_ai_prompt`; unreachable here.
        PromptKind::AiPrompt => return Vec::new(),
    };
    state.overlay = None;
    vec![effect]
}

/// Validate a single-component entry name (new directory / rename target).
fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Name cannot be empty");
    }
    if name == "." || name == ".." {
        return Err("Name cannot be '.' or '..'");
    }
    if name.contains('/') {
        return Err("Name cannot contain '/'");
    }
    Ok(())
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
        // A text prompt captures keystrokes as `Msg::Text`; non-quit actions don't reach it.
        Some(Overlay::Prompt { .. }) | None => Vec::new(),
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
    // Operate on the visible (filtered) view; cursor and marks index into it.
    let entries = pane.visible();
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
    // One transfer at a time: the progress indicator is a single slot, so refuse to start a second
    // while one runs rather than corrupt the display or have its completion clear the wrong one.
    if state.transfer_bytes.is_some() {
        state.status = Some("A transfer is already in progress".to_owned());
        return Vec::new();
    }
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
    // Begin tracking transfer progress (updated by `TransferProgress`, cleared on `TransferDone`).
    state.transfer_bytes = Some(0);
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
    // A new directory starts unfiltered.
    p.filter = None;
    p.filter_editing = false;
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
                // Stop any live filter editing so the plan overlay owns input cleanly (its keys
                // aren't captured by the filter, and the editing indicator doesn't linger).
                state.active_mut().filter_editing = false;
                state.overlay = Some(Overlay::AiPlan { plan, cursor: 0 });
            }
            Vec::new()
        }
        AppEvent::AiPlanProposed(Err(msg)) => {
            state.ai_pending = false;
            state.status = Some(format!("assistant error: {msg}"));
            Vec::new()
        }
        AppEvent::TransferProgress { bytes } => {
            // Advisory display only; ignore if no transfer is tracked (a late event after OpDone).
            if state.transfer_bytes.is_some() {
                state.transfer_bytes = Some(bytes);
            }
            Vec::new()
        }
        AppEvent::OpDone { status, error } => finish_op(state, &status, error),
        AppEvent::TransferDone { status, error } => {
            // Only a transfer's own completion clears its progress indicator — so an unrelated op
            // finishing mid-transfer can't wipe it (and a stray late `TransferProgress` is ignored
            // by its `is_some` guard once this resets the slot).
            state.transfer_bytes = None;
            finish_op(state, &status, error)
        }
    }
}

/// Apply a finished operation: set the status line and refresh both panes. An op that adds, removes,
/// or reorders entries (mkdir/rename/delete/move) invalidates the positional marks, so clear them —
/// a stale mark would make the next copy/move/delete act on the wrong entry.
fn finish_op(state: &mut AppState, status: &str, error: bool) -> Vec<AppEffect> {
    state.status = Some(if error {
        format!("error: {status}")
    } else {
        status.to_owned()
    });
    [Side::Left, Side::Right]
        .into_iter()
        .map(|side| {
            let p = state.pane_mut(side);
            p.marked.clear();
            AppEffect::List {
                pane: side,
                conn: p.conn,
                dir: p.cwd.clone(),
                all: p.show_hidden,
            }
        })
        .collect()
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
            // Group by extension (case-insensitive); no-extension entries sort first, then by name.
            SortMode::Type => extension(a).cmp(&extension(b)).then_with(|| cmp_name(a, b)),
        })
    });
}

/// Case-insensitive name comparison — the stable secondary key for every sort mode.
fn cmp_name(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    a.name.to_lowercase().cmp(&b.name.to_lowercase())
}

/// The lower-cased file extension used for [`SortMode::Type`] ordering. A leading dot (dotfile with no
/// further dot, e.g. `.bashrc`) is treated as having no extension, so such files group with the
/// extensionless entries rather than each forming their own type.
fn extension(e: &Entry) -> String {
    e.name
        .rsplit_once('.')
        .map(|(stem, ext)| {
            if stem.is_empty() {
                String::new() // ".bashrc" → no extension
            } else {
                ext.to_lowercase()
            }
        })
        .unwrap_or_default()
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

    fn names_visible(s: &AppState, side: Side) -> Vec<String> {
        s.pane(side)
            .visible()
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

    fn type_text(s: &mut AppState, text: &str) {
        for c in text.chars() {
            let _ = update(s, Msg::Text(TextEdit::Insert(c)));
        }
    }

    #[test]
    fn make_dir_prompt_submits_a_create_dir_effect() {
        let mut s = state();
        let fx = update(&mut s, Msg::Action(Action::MakeDir));
        assert!(fx.is_empty());
        assert!(s.capturing_text());
        type_text(&mut s, "newdir");
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(s.overlay.is_none(), "prompt closes on submit");
        match &fx[..] {
            [AppEffect::CreateDir { conn, path }] => {
                assert_eq!(*conn, ConnectionId(1));
                assert_eq!(path.as_str(), "/newdir");
            }
            other => panic!("expected CreateDir, got {other:?}"),
        }
    }

    #[test]
    fn prompt_rejects_slash_and_invalid_names() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::MakeDir));
        // '/' is rejected at the keystroke level.
        type_text(&mut s, "a/b");
        let Some(Overlay::Prompt { input, .. }) = &s.overlay else {
            panic!("prompt should still be open");
        };
        assert_eq!(input, "ab");
        // Submitting an empty name keeps the prompt open with a status.
        for _ in 0..2 {
            let _ = update(&mut s, Msg::Text(TextEdit::Backspace));
        }
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty());
        assert!(s.overlay.is_some(), "stays open on invalid name");
        assert!(s.status.as_deref().unwrap().contains("empty"));
    }

    #[test]
    fn rename_prompt_prefills_name_and_renames_to_a_sibling_path() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("old.txt", EntryKind::File)],
        );
        let _ = update(&mut s, Msg::Action(Action::Rename));
        let Some(Overlay::Prompt { input, .. }) = &s.overlay else {
            panic!("rename prompt should be open");
        };
        assert_eq!(input, "old.txt"); // pre-filled with the current name
                                      // Replace with a new name.
        for _ in 0.."old.txt".len() {
            let _ = update(&mut s, Msg::Text(TextEdit::Backspace));
        }
        type_text(&mut s, "new.txt");
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        match &fx[..] {
            [AppEffect::Rename { from, to, .. }] => {
                assert_eq!(from.as_str(), "/old.txt");
                assert_eq!(to.as_str(), "/new.txt");
            }
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn rename_to_the_same_name_closes_without_an_effect() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("keep", EntryKind::Dir)]);
        let _ = update(&mut s, Msg::Action(Action::Rename));
        // Submit unchanged.
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
    }

    #[test]
    fn prompt_rejects_dot_names() {
        for bad in [".", ".."] {
            let mut s = state();
            let _ = update(&mut s, Msg::Action(Action::MakeDir));
            type_text(&mut s, bad);
            let fx = update(&mut s, Msg::Text(TextEdit::Submit));
            assert!(fx.is_empty(), "{bad} should not submit");
            assert!(s.overlay.is_some(), "{bad} keeps the prompt open");
        }
    }

    #[test]
    fn apply_text_is_a_no_op_without_a_prompt_overlay() {
        let mut s = state();
        // A confirm-delete overlay is open, not a prompt: text edits are ignored.
        s.overlay = Some(Overlay::ConfirmDelete {
            conn: ConnectionId(1),
            paths: vec![VfsPath::root()],
        });
        let fx = update(&mut s, Msg::Text(TextEdit::Insert('x')));
        assert!(fx.is_empty());
        assert!(matches!(s.overlay, Some(Overlay::ConfirmDelete { .. })));
    }

    #[test]
    fn transfer_progress_tracks_then_clears_on_completion() {
        let mut s = state();
        // Set up a copy: mark a file on the left, copy to the right.
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        let fx = update(&mut s, Msg::Action(Action::Copy));
        assert!(matches!(&fx[..], [AppEffect::Transfer { .. }]));
        assert_eq!(s.transfer_bytes, Some(0), "transfer tracking starts");
        // Progress updates the running total.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferProgress { bytes: 4096 }),
        );
        assert_eq!(s.transfer_bytes, Some(4096));
        // The transfer's own completion clears it.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferDone {
                status: "Copied 1 file(s)".to_owned(),
                error: false,
            }),
        );
        assert_eq!(s.transfer_bytes, None);
    }

    #[test]
    fn transfer_progress_after_completion_is_ignored() {
        let mut s = state();
        // No transfer tracked: a late/stray progress event must not resurrect the indicator.
        let _ = update(&mut s, Msg::Event(AppEvent::TransferProgress { bytes: 10 }));
        assert_eq!(s.transfer_bytes, None);
    }

    #[test]
    fn an_unrelated_op_completing_does_not_clear_a_live_transfer() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy));
        assert_eq!(s.transfer_bytes, Some(0));
        // A delete/mkdir/rename finishing (generic OpDone) must NOT wipe the transfer indicator…
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::OpDone {
                status: "Deleted 1 item(s)".to_owned(),
                error: false,
            }),
        );
        assert_eq!(s.transfer_bytes, Some(0));
        // …and subsequent progress still lands.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferProgress { bytes: 8192 }),
        );
        assert_eq!(s.transfer_bytes, Some(8192));
    }

    #[test]
    fn cancel_aborts_an_in_flight_transfer_else_is_a_no_op() {
        // Pure-reducer assertion: that `CancelTransfer` is emitted. The token actually aborting the
        // transfer is covered by the app-level `cancelled_transfer_reports_a_non_error_completion`.
        let mut s = state();
        // No transfer, no overlay: Cancel does nothing.
        assert!(update(&mut s, Msg::Action(Action::Cancel)).is_empty());
        // Start a transfer, then Cancel emits CancelTransfer.
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy));
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(matches!(&fx[..], [AppEffect::CancelTransfer]));
        assert!(s.status.as_deref().unwrap().contains("Cancelling"));
    }

    #[test]
    fn a_second_transfer_is_refused_while_one_is_in_flight() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a", EntryKind::File),
                Entry::new("b", EntryKind::File),
            ],
        );
        let fx = update(&mut s, Msg::Action(Action::Copy));
        assert!(matches!(&fx[..], [AppEffect::Transfer { .. }]));
        // A second transfer while the first runs is refused (no effect, status explains).
        let fx = update(&mut s, Msg::Action(Action::Move));
        assert!(fx.is_empty());
        assert!(s.status.as_deref().unwrap().contains("already in progress"));
    }

    #[test]
    fn op_done_clears_stale_marks_on_both_panes() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a", EntryKind::File),
                Entry::new("b", EntryKind::File),
            ],
        );
        let _ = update(&mut s, Msg::Action(Action::ToggleMark));
        assert!(!s.pane(Side::Left).marked.is_empty());
        // An operation completes; the refresh must drop the now-stale positional marks.
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::OpDone {
                status: "done".to_owned(),
                error: false,
            }),
        );
        assert_eq!(fx.len(), 2, "refreshes both panes");
        assert!(s.pane(Side::Left).marked.is_empty());
        assert!(s.pane(Side::Right).marked.is_empty());
    }

    #[test]
    fn prompt_cancel_closes_without_an_effect() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::MakeDir));
        type_text(&mut s, "discard");
        let fx = update(&mut s, Msg::Text(TextEdit::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
    }

    #[test]
    fn sort_mode_cycles_back_to_name() {
        assert_eq!(SortMode::Name.next(), SortMode::Size);
        assert_eq!(SortMode::Size.next(), SortMode::Modified);
        assert_eq!(SortMode::Modified.next(), SortMode::Type);
        assert_eq!(SortMode::Type.next(), SortMode::Name);
    }

    #[test]
    fn filter_narrows_the_visible_view_live_and_confirms() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("apple.txt", EntryKind::File),
                Entry::new("banana.txt", EntryKind::File),
                Entry::new("apricot.rs", EntryKind::File),
            ],
        );
        // Start filtering — keystrokes now route to the filter.
        let _ = update(&mut s, Msg::Action(Action::Filter));
        assert!(s.capturing_text());
        type_text(&mut s, "ap");
        // "ap" matches apple.txt and apricot.rs (case-insensitive substring).
        assert_eq!(
            names_visible(&s, Side::Left),
            vec!["apple.txt", "apricot.rs"]
        );
        // Enter keeps the filter applied but stops editing.
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(!s.capturing_text());
        assert_eq!(s.pane(Side::Left).filter.as_deref(), Some("ap"));
        assert_eq!(
            names_visible(&s, Side::Left),
            vec!["apple.txt", "apricot.rs"]
        );
    }

    #[test]
    fn filter_is_case_insensitive() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("Apple.txt", EntryKind::File),
                Entry::new("banana.txt", EntryKind::File),
            ],
        );
        let _ = update(&mut s, Msg::Action(Action::Filter));
        type_text(&mut s, "AP"); // upper-case needle matches "Apple.txt"
        assert_eq!(names_visible(&s, Side::Left), vec!["Apple.txt"]);
    }

    #[test]
    fn an_arriving_plan_overlay_takes_input_from_a_live_filter() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("a", EntryKind::File)]);
        // Request a plan, then start filtering during the in-flight window.
        request_ai(&mut s, "do something");
        let _ = update(&mut s, Msg::Action(Action::Filter));
        type_text(&mut s, "a");
        assert!(s.capturing_text()); // filter owns input for now
                                     // The plan arrives: it opens its overlay and reclaims input.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::AiPlanProposed(Ok(make_plan(&["list"])))),
        );
        assert!(matches!(s.overlay, Some(Overlay::AiPlan { .. })));
        assert!(
            !s.capturing_text(),
            "the plan overlay owns input, not the filter"
        );
        assert!(!s.active().filter_editing, "filter editing was stopped");
        // A keystroke now drives the plan overlay (it is not swallowed by the filter).
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        assert!(matches!(s.overlay, Some(Overlay::AiPlan { .. })));
    }

    #[test]
    fn filter_cancel_restores_the_full_listing() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a", EntryKind::File),
                Entry::new("b", EntryKind::File),
            ],
        );
        let _ = update(&mut s, Msg::Action(Action::Filter));
        type_text(&mut s, "a");
        assert_eq!(names_visible(&s, Side::Left), vec!["a"]);
        let _ = update(&mut s, Msg::Text(TextEdit::Cancel));
        assert!(s.pane(Side::Left).filter.is_none());
        assert!(!s.capturing_text());
        assert_eq!(names_visible(&s, Side::Left), vec!["a", "b"]);
    }

    #[test]
    fn empty_filter_on_submit_is_dropped() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("x", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Filter));
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(s.pane(Side::Left).filter.is_none());
    }

    #[test]
    fn navigating_clears_an_active_filter() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("sub", EntryKind::Dir)]);
        let _ = update(&mut s, Msg::Action(Action::Filter));
        type_text(&mut s, "s");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(s.pane(Side::Left).filter.is_some());
        // Entering the directory resets the filter.
        let _ = update(&mut s, Msg::Action(Action::Enter));
        assert!(s.pane(Side::Left).filter.is_none());
        assert!(!s.pane(Side::Left).filter_editing);
    }

    #[test]
    fn op_targets_and_enter_respect_the_filter() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("keep", EntryKind::Dir),
                Entry::new("zzz", EntryKind::Dir),
            ],
        );
        let _ = update(&mut s, Msg::Action(Action::Filter));
        type_text(&mut s, "keep");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        // Only "keep" is visible, so the cursor (index 0) resolves to it.
        assert_eq!(s.active().current().unwrap().name, "keep");
        let fx = update(&mut s, Msg::Action(Action::Enter));
        // Entered "keep" (a new List effect for /keep), filter cleared.
        assert!(matches!(&fx[..], [AppEffect::List { dir, .. }] if dir.as_str() == "/keep"));
    }

    #[test]
    fn extension_handles_edge_cases() {
        let ext = |name: &str| extension(&Entry::new(name, EntryKind::File));
        assert_eq!(ext("readme"), ""); // no dot
        assert_eq!(ext(".bashrc"), ""); // leading-dot dotfile → no extension
        assert_eq!(ext("file."), ""); // trailing dot → empty extension
        assert_eq!(ext("a.tar.gz"), "gz"); // splits on the last dot
        assert_eq!(ext("FILE.TXT"), "txt"); // lower-cased
        assert_eq!(ext(".bashrc.bak"), "bak"); // dotfile with a real extension
        assert_eq!(ext(""), ""); // empty name
    }

    #[test]
    fn sort_type_groups_by_extension_then_name() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("readme", EntryKind::File), // no extension → first
                Entry::new("b.txt", EntryKind::File),
                Entry::new("a.rs", EntryKind::File),
                Entry::new("a.txt", EntryKind::File),
                Entry::new(".bashrc", EntryKind::File), // dotfile → no extension
            ],
        );
        // Name → Size → Modified → Type.
        for _ in 0..3 {
            let _ = update(&mut s, Msg::Action(Action::CycleSort));
        }
        assert_eq!(s.active().sort, SortMode::Type);
        assert_eq!(
            names(&s, Side::Left),
            // extensionless (".bashrc", "readme") first by name, then ".rs", then ".txt" by name
            vec![".bashrc", "readme", "a.rs", "a.txt", "b.txt"]
        );
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
    fn ai_propose_opens_a_freeform_prompt_then_submit_requests_a_plan() {
        let mut s = state();
        // AiPropose opens a prompt rather than firing immediately.
        let fx = update(&mut s, Msg::Action(Action::AiPropose));
        assert!(fx.is_empty());
        assert!(matches!(
            s.overlay,
            Some(Overlay::Prompt {
                kind: PromptKind::AiPrompt,
                ..
            })
        ));
        // The freeform prompt accepts `/` (unlike a filename prompt, which rejects path separators).
        type_text(&mut s, "archive /var/log to the other pane");
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(s.ai_pending);
        assert!(s.overlay.is_none());
        match &fx[..] {
            [AppEffect::RequestAiPlan { prompt }] => {
                assert_eq!(prompt, "archive /var/log to the other pane");
            }
            other => panic!("expected RequestAiPlan, got {other:?}"),
        }
    }

    #[test]
    fn ai_prompt_rejects_an_empty_request() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::AiPropose));
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty());
        assert!(!s.ai_pending);
        assert!(s.overlay.is_some(), "stays open on empty request");
    }

    #[test]
    fn ai_prompt_rejects_a_whitespace_only_request() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::AiPropose));
        type_text(&mut s, "   ");
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty());
        assert!(!s.ai_pending);
        assert!(s.overlay.is_some(), "stays open on whitespace-only request");
    }

    #[test]
    fn cancelling_the_ai_prompt_leaves_no_pending_request() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::AiPropose));
        type_text(&mut s, "never mind");
        let fx = update(&mut s, Msg::Text(TextEdit::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
        assert!(!s.ai_pending, "cancel must not strand a pending request");
    }

    #[test]
    fn overlay_openers_are_suppressed_while_a_plan_is_pending() {
        let mut s = state();
        request_ai(&mut s, "do something"); // ai_pending = true, no overlay
        for action in [
            Action::MakeDir,
            Action::Rename,
            Action::Delete,
            Action::OpenConnections,
        ] {
            let fx = update(&mut s, Msg::Action(action));
            assert!(fx.is_empty(), "{action:?} should be suppressed");
            assert!(
                s.overlay.is_none(),
                "{action:?} must not open a competing overlay while a plan is pending"
            );
        }
        // The arriving plan can therefore safely open its review overlay.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::AiPlanProposed(Ok(make_plan(&["list"])))),
        );
        assert!(matches!(s.overlay, Some(Overlay::AiPlan { .. })));
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

    /// Drive the AI flow up through submitting a request: returns once `ai_pending` is set.
    fn request_ai(s: &mut AppState, prompt: &str) {
        let _ = update(s, Msg::Action(Action::AiPropose));
        type_text(s, prompt);
        let _ = update(s, Msg::Text(TextEdit::Submit));
    }

    #[test]
    fn empty_plan_does_not_open_overlay() {
        let mut s = state();
        request_ai(&mut s, "do something"); // sets ai_pending
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
        request_ai(&mut s, "do something"); // first request goes out
        assert!(s.ai_pending);
        // While pending, AiPropose neither opens a prompt nor fires again.
        let fx = update(&mut s, Msg::Action(Action::AiPropose));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
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
