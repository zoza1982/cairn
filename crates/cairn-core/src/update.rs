//! The pure reducer: `update(&mut AppState, Msg) -> Vec<AppEffect>`. No I/O, no `.await`.

use crate::msg::{Action, AppEffect, AppEvent, Msg, TextEdit};
use crate::state::{
    ActiveTransfer, AppState, Listing, LogViewerStatus, MaskedInput, Overlay, PromptKind,
    QueuedTransfer, SessionEnd, SessionRecord, Side, SortMode, TransferId,
    SESSION_OUTPUT_MAX_BYTES, SESSION_OUTPUT_MAX_LINES,
};
use cairn_types::{Entry, SessionId, VfsPath};
use std::collections::VecDeque;
use std::sync::Arc;

/// Apply a message to the state, returning any effects to run. Deterministic and side-effect-free.
#[must_use]
pub fn update(state: &mut AppState, msg: Msg) -> Vec<AppEffect> {
    let mut effects = match msg {
        Msg::Action(action) => apply_action(state, action),
        Msg::Text(edit) => apply_text(state, edit),
        Msg::Event(event) => apply_event(state, event),
        Msg::Tick => Vec::new(),
    };
    // Drain the transfer queue whenever a slot is free — covers the case where an overlay that was
    // open when a transfer finished has since closed. `advance_queue` is a no-op while all slots are
    // full, an overlay is open, or the queue is empty, so this is safe after any msg.
    effects.extend(advance_queue(state));
    effects
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
            Action::MakeDir
                | Action::Rename
                | Action::Delete
                | Action::OpenConnections
                | Action::OpenQueue
                | Action::RunShellAction(_)
                | Action::VaultUnlock
        )
    {
        state.status = Some("The assistant is preparing a plan…".to_owned());
        return Vec::new();
    }
    // While an approved plan executes, refuse to start a competing operation or plan — that would
    // mutate the filesystem concurrently and (for a second plan) orphan the first's cancel token.
    // `Esc`/Cancel is intentionally excluded so it still aborts the running plan.
    if state.ai_executing
        && matches!(
            action,
            Action::AiPropose
                | Action::Copy
                | Action::Move
                | Action::Delete
                | Action::MakeDir
                | Action::Rename
                | Action::OpenConnections
                | Action::OpenQueue
                | Action::RunShellAction(_)
                | Action::VaultUnlock
        )
    {
        state.status = Some("A plan is executing — press Esc to cancel it first".to_owned());
        return Vec::new();
    }
    // A navigation/selection keystroke dismisses any lingering transient status (now that the status
    // line renders it when idle): the key hints return and a stale "Copied…"/"error:" line doesn't
    // persist across unrelated moves. An arm that sets its own status below overwrites this.
    if matches!(
        action,
        Action::CursorUp
            | Action::CursorDown
            | Action::CursorTop
            | Action::CursorBottom
            | Action::Enter
            | Action::Leave
            | Action::SwitchPane
            | Action::ToggleMark
    ) {
        state.status = None;
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
        Action::VaultUnlock => {
            if state.vault_unlocked {
                state.status = Some("Vault already unlocked".to_owned());
            } else {
                state.overlay = Some(Overlay::VaultUnlock {
                    input: MaskedInput::new(),
                    error: None,
                });
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
        // With no overlay, Cancel (Esc) aborts an executing AI plan, else an in-flight transfer.
        Action::Cancel if state.ai_executing => {
            state.status = Some("Cancelling plan…".to_owned());
            vec![AppEffect::CancelAiPlan]
        }
        // Esc cancels *all* active transfers (with N>1 there's no cursor to pick one; per-transfer
        // cancel is in the queue overlay). Degenerates to "cancel the one" at concurrency 1.
        Action::Cancel if state.has_active_transfer() => {
            let n = state.active_transfers.len();
            state.status = Some(if n == 1 {
                "Cancelling transfer…".to_owned()
            } else {
                format!("Cancelling {n} transfers…")
            });
            state
                .active_transfers
                .iter()
                .map(|t| AppEffect::CancelTransfer { id: t.id })
                .collect()
        }
        // Pause/resume *all* active transfers: pause if any is running, else resume. No-op (with a
        // hint) when none is running. Per-transfer pause is in the queue overlay.
        Action::TogglePause => {
            if !state.has_active_transfer() {
                state.status = Some("No transfer to pause".to_owned());
                return Vec::new();
            }
            let new_paused = state.active_transfers.iter().any(|t| !t.paused);
            for t in &mut state.active_transfers {
                t.paused = new_paused;
            }
            let n = state.active_transfers.len();
            state.status = Some(match (new_paused, n) {
                (true, 1) => "Transfer paused".to_owned(),
                (true, n) => format!("{n} transfers paused"),
                (false, 1) => "Transfer resumed".to_owned(),
                (false, n) => format!("{n} transfers resumed"),
            });
            state
                .active_transfers
                .iter()
                .map(|t| AppEffect::SetTransferPaused {
                    id: t.id,
                    paused: new_paused,
                })
                .collect()
        }
        Action::RunShellAction(index) => run_shell_action(state, index),
        // No overlay open: confirm/cancel and the plan-only actions are otherwise no-ops.
        // Queue reorder only acts inside the queue overlay; a no-op with no overlay open.
        // PageUp/PageDown are no-ops when no overlay is open.
        Action::Confirm
        | Action::Cancel
        | Action::ApproveAll
        | Action::Reject
        | Action::QueueMoveUp
        | Action::QueueMoveDown
        | Action::PageUp
        | Action::PageDown => Vec::new(),
        Action::OpenLogViewer => open_log_viewer(state),
        Action::OpenExecSession => open_exec_session(state),
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
        Action::OpenQueue => {
            state.overlay = Some(Overlay::TransferQueue { cursor: 0 });
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
    if matches!(state.overlay, Some(Overlay::VaultUnlock { .. })) {
        return apply_vault_unlock_text(state, edit);
    }
    if matches!(state.overlay, Some(Overlay::ExecPane { .. })) {
        return apply_exec_pane_text(state, edit);
    }
    // Filter editing only owns input when no overlay is open (lock-step with `capturing_text`).
    if state.overlay.is_none() && state.active().filter_editing {
        return apply_filter_text(state, edit);
    }
    Vec::new()
}

/// Edit the vault-unlock passphrase field. The typed value lives only in the [`MaskedInput`] (no-echo,
/// redacted in `Debug`, zeroized on drop); this never copies it into the status line or anywhere else.
fn apply_vault_unlock_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    let Some(Overlay::VaultUnlock { input, error }) = &mut state.overlay else {
        return Vec::new();
    };
    match edit {
        TextEdit::Insert(c) => {
            // Reject control characters; everything else is part of the passphrase.
            if !c.is_control() {
                input.push(c);
                // Typing dismisses a stale error so the box reads cleanly again.
                *error = None;
            }
            Vec::new()
        }
        TextEdit::Backspace => {
            input.backspace();
            Vec::new()
        }
        // Closing the overlay drops the `MaskedInput`, which zeroizes the buffer.
        TextEdit::Cancel => {
            state.overlay = None;
            Vec::new()
        }
        TextEdit::Submit => submit_vault_unlock(state),
        // `CloseStdin` is only meaningful inside `ExecPane`; ignore it everywhere else.
        TextEdit::CloseStdin => Vec::new(),
    }
}

/// Submit the entered passphrase: emit [`AppEffect::UnlockVault`], keeping the overlay open (with the
/// field wiped) so the async unlock can report success/failure back into it. An empty passphrase is
/// rejected in-place without an effect.
fn submit_vault_unlock(state: &mut AppState) -> Vec<AppEffect> {
    // Ignore a second submit while an unlock is already running: `Vault::open` runs Argon2id (slow),
    // so without this a quick double-Enter would spawn a duplicate task and double-open connections.
    if state.vault_unlocking {
        return Vec::new();
    }
    let Some(Overlay::VaultUnlock { input, error }) = &mut state.overlay else {
        return Vec::new();
    };
    if input.is_empty() {
        *error = Some("Enter the vault passphrase".to_owned());
        return Vec::new();
    }
    // Take the secret out of the field (which wipes it) and hand it to the effect runner.
    let passphrase = input.take_secret();
    *error = None;
    state.vault_unlocking = true;
    state.status = Some("Unlocking vault…".to_owned());
    vec![AppEffect::UnlockVault { passphrase }]
}

/// Route text-editing keystrokes to the exec pane's input field.
///
/// `Enter` submits the current line (appending `\n`) as a [`AppEffect::SendSessionInput`]; `Esc`
/// clears the field (to cancel in-progress typing, not to close the overlay — that is `Ctrl-]`,
/// mapped to `Action::Cancel` upstream). `CloseStdin` (`Ctrl-D`) closes the session's stdin.
fn apply_exec_pane_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    let Some(Overlay::ExecPane { id, input, .. }) = &mut state.overlay else {
        return Vec::new();
    };
    let session_id: SessionId = *id;
    match edit {
        TextEdit::Insert(c) => {
            if !c.is_control() {
                input.push(c);
            }
            Vec::new()
        }
        TextEdit::Backspace => {
            input.pop();
            Vec::new()
        }
        // Esc clears the field (doesn't close the overlay — Ctrl-] closes it via Action::Cancel).
        TextEdit::Cancel => {
            input.clear();
            Vec::new()
        }
        TextEdit::Submit => {
            // Take the input line, clear the field, and emit SendSessionInput with a newline.
            let line = std::mem::take(input);
            if line.is_empty() {
                return Vec::new();
            }
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            vec![AppEffect::SendSessionInput {
                id: session_id,
                bytes,
            }]
        }
        TextEdit::CloseStdin => {
            // Signal EOF to the remote process by dropping stdin — the process may exit on EOF
            // (e.g. a shell's `exit`). Unlike `CloseSession`, the overlay stays open to show
            // remaining output; `SessionEnded` arrives when the process exits.
            vec![AppEffect::CloseStdin { id: session_id }]
        }
    }
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
        // `CloseStdin` is only meaningful inside `ExecPane`; ignore it everywhere else.
        TextEdit::CloseStdin => Vec::new(),
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
        // `CloseStdin` is only meaningful inside `ExecPane`; ignore it everywhere else.
        TextEdit::CloseStdin => {}
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
        Some(Overlay::ConfirmOverwrite { .. }) => apply_confirm_overwrite_action(state, action),
        Some(Overlay::ConfirmShellAction { .. }) => apply_confirm_shell_action(state, action),
        Some(Overlay::AiPlan { .. }) => apply_ai_plan_action(state, action),
        Some(Overlay::Connections { .. }) => apply_connections_action(state, action),
        Some(Overlay::TransferQueue { .. }) => apply_transfer_queue_action(state, action),
        Some(Overlay::LogViewer { .. }) => apply_log_viewer_action(state, action),
        Some(Overlay::ExecPane { .. }) => apply_exec_pane_action(state, action),
        Some(Overlay::PortForwardStatus { .. }) => apply_port_forward_status_action(state, action),
        // A text prompt / the vault-unlock field capture keystrokes as `Msg::Text`; non-quit actions
        // don't reach them.
        Some(Overlay::Prompt { .. } | Overlay::VaultUnlock { .. }) | None => Vec::new(),
    }
}

/// Drive the transfer-queue overlay: navigate the pending list, drop the selected pending transfer
/// (`Delete`), clear all pending (`Reject`), or close (`Cancel`/`Confirm`/`Enter`). The active
/// transfer is never touched here.
fn apply_transfer_queue_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    let len = state.transfer_queue.len();
    let Some(Overlay::TransferQueue { cursor }) = &mut state.overlay else {
        return Vec::new();
    };
    match action {
        Action::CursorUp => {
            *cursor = cursor.saturating_sub(1);
            Vec::new()
        }
        Action::CursorDown => {
            if *cursor + 1 < len {
                *cursor += 1;
            }
            Vec::new()
        }
        Action::QueueMoveUp => {
            if *cursor > 0 && *cursor < len {
                let c = *cursor;
                state.transfer_queue.swap(c - 1, c);
                if let Some(Overlay::TransferQueue { cursor }) = &mut state.overlay {
                    *cursor -= 1;
                }
            }
            Vec::new()
        }
        Action::QueueMoveDown => {
            if *cursor + 1 < len {
                let c = *cursor;
                state.transfer_queue.swap(c, c + 1);
                if let Some(Overlay::TransferQueue { cursor }) = &mut state.overlay {
                    *cursor += 1;
                }
            }
            Vec::new()
        }
        Action::Delete => {
            // Drop just the selected pending transfer (the active one keeps running).
            let idx = *cursor;
            if idx < len {
                state.transfer_queue.remove(idx);
                state.status = Some("Removed 1 queued transfer".to_owned());
                // Re-clamp the cursor and close the view if the queue is now empty.
                if state.transfer_queue.is_empty() {
                    state.overlay = None;
                } else if let Some(Overlay::TransferQueue { cursor }) = &mut state.overlay {
                    *cursor = (*cursor).min(state.transfer_queue.len() - 1);
                }
            }
            Vec::new()
        }
        Action::Reject => {
            let n = state.transfer_queue.len();
            state.transfer_queue.clear();
            if n > 0 {
                state.status = Some(format!("Cleared {n} queued transfer(s)"));
            }
            state.overlay = None;
            Vec::new()
        }
        Action::Cancel | Action::Confirm | Action::Enter => {
            state.overlay = None;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Confirm (or cancel) overwriting existing destinations. On confirm, re-issue the transfer with
/// `overwrite: true`; on cancel, abandon it without touching the destinations.
fn apply_confirm_overwrite_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    match action {
        Action::Confirm | Action::Enter => {
            let Some(Overlay::ConfirmOverwrite {
                src_conn,
                dst_conn,
                items,
                is_move,
                ..
            }) = state.overlay.take()
            else {
                return Vec::new();
            };
            let id = arm_transfer(state, is_move, items.len());
            vec![AppEffect::Transfer {
                id,
                src_conn,
                dst_conn,
                items,
                is_move,
                overwrite: true,
            }]
        }
        Action::Cancel => {
            state.overlay = None;
            state.status = Some("Transfer cancelled".to_owned());
            // The queue is drained by the tail call in `update` now that the overlay is closed.
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Drive the shell-action confirm dialog: `Confirm`/`Enter` runs it, `Cancel` abandons it.
fn apply_confirm_shell_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    match action {
        Action::Confirm | Action::Enter => {
            let Some(Overlay::ConfirmShellAction {
                index,
                name,
                conn,
                target,
            }) = state.overlay.take()
            else {
                return Vec::new();
            };
            state.status = Some(format!("Running '{name}'…"));
            vec![AppEffect::RunShellAction {
                index,
                conn,
                target,
            }]
        }
        Action::Cancel => {
            state.overlay = None;
            state.status = Some("Action cancelled".to_owned());
            Vec::new()
        }
        _ => Vec::new(),
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
            state.status = Some(format!(
                "Executing {} step(s)… (Esc to cancel)",
                plan.steps.len()
            ));
            state.ai_executing = true;
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

/// Begin a user-defined shell action against the entry under the cursor. If the action requires
/// confirmation (the default), open the confirm overlay; otherwise emit the run effect directly. A
/// no-op (with a hint) when the index is unknown or nothing is selected. The reducer stays pure — it
/// never resolves real paths or command details; the runtime does, gating on a local backend.
fn run_shell_action(state: &mut AppState, index: usize) -> Vec<AppEffect> {
    let Some(meta) = state.shell_actions.get(index) else {
        // An out-of-range index means the keymap and the action list drifted — a wiring bug, not user
        // error. Assert in debug to catch it in tests; in release, no-op rather than panic.
        debug_assert!(false, "RunShellAction index {index} out of range");
        return Vec::new();
    };
    let (name, confirm) = (meta.name.clone(), meta.confirm);
    let pane = state.active();
    let Some(entry) = pane.current() else {
        state.status = Some(format!("'{name}': nothing selected"));
        return Vec::new();
    };
    let Ok(target) = pane.cwd.join(entry.name.as_ref()) else {
        state.status = Some(format!("'{name}': cannot resolve the selected entry"));
        return Vec::new();
    };
    let conn = pane.conn;
    if confirm {
        state.overlay = Some(Overlay::ConfirmShellAction {
            index,
            name,
            conn,
            target,
        });
        Vec::new()
    } else {
        state.status = Some(format!("Running '{name}'…"));
        vec![AppEffect::RunShellAction {
            index,
            conn,
            target,
        }]
    }
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
    if items.is_empty() {
        return Vec::new();
    }
    // Up to `concurrency_limit` transfers run at once: if every slot is busy, queue this one and
    // start it (FIFO) when a slot frees. (`start_transfer` only runs with no overlay open.)
    if state.active_transfers.len() >= state.concurrency_limit {
        state.transfer_queue.push_back(QueuedTransfer {
            src_conn,
            dst_conn,
            items,
            is_move,
        });
        state.status = Some(format!("Queued ({} pending)", state.transfer_queue.len()));
        return Vec::new();
    }
    // Marks are NOT cleared here: if the pre-check finds a conflict and the user cancels the
    // overwrite prompt, nothing was transferred and the selection must survive. `finish_op` clears
    // marks on actual completion (`TransferDone`).
    let id = arm_transfer(state, is_move, items.len());
    // First pass does not overwrite: the effect runner checks for collisions and reports
    // `TransferConflict` (→ confirm overlay) instead of clobbering existing destinations.
    vec![AppEffect::Transfer {
        id,
        src_conn,
        dst_conn,
        items,
        is_move,
        overwrite: false,
    }]
}

/// Start queued transfers into any free slots (up to `concurrency_limit`). Called as a tail step of
/// `update` so the queue keeps draining when a transfer finishes or an overwrite prompt is dismissed.
/// While a modal overlay is open we start nothing new (a `ConfirmOverwrite` is a question about
/// existing work) — already-running transfers in other slots are untouched.
fn advance_queue(state: &mut AppState) -> Vec<AppEffect> {
    // Block queue advancement only while a modal overlay demands immediate user attention.
    // Passive/long-lived overlays (LogViewer, ExecPane, PortForwardStatus) coexist with
    // ongoing transfers — they must not starve the queue.
    let blocks_queue = matches!(
        &state.overlay,
        Some(
            Overlay::ConfirmDelete { .. }
                | Overlay::ConfirmOverwrite { .. }
                | Overlay::ConfirmShellAction { .. }
                | Overlay::AiPlan { .. }
                | Overlay::Connections { .. }
                | Overlay::TransferQueue { .. }
                | Overlay::Prompt { .. }
                | Overlay::VaultUnlock { .. }
        )
    );
    if blocks_queue {
        return Vec::new();
    }
    let mut effects = Vec::new();
    while state.active_transfers.len() < state.concurrency_limit {
        let Some(next) = state.transfer_queue.pop_front() else {
            break;
        };
        let id = arm_transfer(state, next.is_move, next.items.len());
        effects.push(AppEffect::Transfer {
            id,
            src_conn: next.src_conn,
            dst_conn: next.dst_conn,
            items: next.items,
            is_move: next.is_move,
            overwrite: false,
        });
    }
    effects
}

/// Page size for the log viewer scroll actions.
const LOG_VIEWER_PAGE: usize = 20;

/// Open a log-viewer overlay for the entry under the cursor, minting a fresh session id.
fn open_log_viewer(state: &mut AppState) -> Vec<AppEffect> {
    let pane = state.active();
    let Some(entry) = pane.current() else {
        state.status = Some("Nothing selected".to_owned());
        return Vec::new();
    };
    let Ok(path) = pane.cwd.join(entry.name.as_ref()) else {
        state.status = Some("Cannot open log viewer for this entry".to_owned());
        return Vec::new();
    };
    let conn = pane.conn;
    let title = format!("{} — logs", entry.name);
    let id = state.next_log_viewer_id;
    state.next_log_viewer_id += 1;

    state.overlay = Some(Overlay::LogViewer {
        id,
        title: title.clone(),
        lines: VecDeque::new(),
        partial: String::new(),
        byte_size: 0,
        follow: true,
        scroll: 0,
        status: crate::state::LogViewerStatus::Streaming,
    });

    vec![AppEffect::OpenLogViewer {
        id,
        conn,
        path,
        title,
    }]
}

/// Open a cooked exec session on the entry under the cursor.
///
/// Mints a fresh [`SessionId`], inserts a [`SessionRecord`] into [`AppState::sessions`], opens
/// the [`Overlay::ExecPane`], and emits [`AppEffect::OpenExecSession`] with `argv = ["sh"]`.
/// Backend startup failures arrive later as [`AppEvent::SessionEnded`] with an error.
fn open_exec_session(state: &mut AppState) -> Vec<AppEffect> {
    let pane = state.active();
    let Some(entry) = pane.current() else {
        state.status = Some("Nothing selected".to_owned());
        return Vec::new();
    };
    let Ok(path) = pane.cwd.join(entry.name.as_ref()) else {
        state.status = Some("Cannot open exec session for this entry".to_owned());
        return Vec::new();
    };
    let conn = pane.conn;
    let title = format!("{} — exec", entry.name);
    let id = state.next_session_id;
    state.next_session_id = SessionId(id.0 + 1);

    state.sessions.insert(
        id,
        SessionRecord {
            path: path.clone(),
            title: title.clone(),
            output_lines: VecDeque::new(),
            output_partial: String::new(),
            output_byte_size: 0,
            local_port: None,
            ended: None,
        },
    );
    state.overlay = Some(Overlay::ExecPane {
        id,
        input: String::new(),
        scroll: 0,
        follow: true,
    });
    vec![AppEffect::OpenExecSession {
        id,
        conn,
        path,
        argv: vec!["sh".to_owned()],
        tty: false,
        title,
    }]
}

/// Handle actions while the log-viewer overlay is open.
fn apply_log_viewer_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    let Some(Overlay::LogViewer {
        id,
        lines,
        partial,
        follow,
        scroll,
        ..
    }) = &mut state.overlay
    else {
        return Vec::new();
    };
    let total_lines = lines.len() + if partial.is_empty() { 0 } else { 1 };
    match action {
        Action::CursorUp => {
            *scroll = scroll.saturating_sub(1);
            *follow = false;
            Vec::new()
        }
        Action::PageUp => {
            *scroll = scroll.saturating_sub(LOG_VIEWER_PAGE);
            *follow = false;
            Vec::new()
        }
        Action::CursorDown => {
            if *scroll + 1 < total_lines {
                *scroll += 1;
            }
            Vec::new()
        }
        Action::PageDown => {
            let new_scroll = (*scroll + LOG_VIEWER_PAGE).min(total_lines.saturating_sub(1));
            *scroll = new_scroll;
            if new_scroll + 1 >= total_lines {
                *follow = true;
                *scroll = total_lines.saturating_sub(LOG_VIEWER_PAGE);
            }
            Vec::new()
        }
        Action::CursorTop => {
            *scroll = 0;
            *follow = false;
            Vec::new()
        }
        Action::CursorBottom => {
            *follow = true;
            Vec::new()
        }
        Action::Cancel => {
            let viewer_id = *id;
            state.overlay = None;
            vec![AppEffect::CloseLogViewer { id: viewer_id }]
        }
        _ => Vec::new(),
    }
}

/// Page size for exec pane scroll actions (mirrors log viewer).
const EXEC_PANE_PAGE: usize = 20;

/// Handle non-text actions while the exec pane overlay is open.
///
/// While `ExecPane` is active, most key events are routed as `Msg::Text` (the overlay captures
/// text). The only actions that reach here are: `Quit` (handled by the shared path before this
/// function), and `Action::Cancel` — the explicit detach chord (`Ctrl-]`) intercepted in
/// `map_input` before the text-capture path, or forwarded from the overlay for Esc-outside-field.
///
/// `Cancel` detaches the pane: the overlay closes but the remote process keeps running. No
/// `CloseSession` effect is emitted; the session record stays alive in `AppState::sessions` until
/// `SessionEnded` arrives (which cleans it up regardless of whether the overlay is open).
fn apply_exec_pane_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    match action {
        Action::Cancel => {
            // Detach: close the overlay, leave the remote process running. If the session has
            // already ended there is no relay task left, so clean up the record now.
            if let Some(Overlay::ExecPane { id, .. }) = state.overlay.take() {
                if state.sessions.get(&id).is_some_and(|r| r.ended.is_some()) {
                    state.sessions.remove(&id);
                }
            }
            Vec::new()
        }
        Action::PageUp => {
            if let Some(Overlay::ExecPane { scroll, follow, .. }) = &mut state.overlay {
                *scroll = scroll.saturating_sub(EXEC_PANE_PAGE);
                *follow = false;
            }
            Vec::new()
        }
        Action::PageDown => {
            let total = exec_pane_total_lines(state);
            if let Some(Overlay::ExecPane { scroll, follow, .. }) = &mut state.overlay {
                let new_scroll = (*scroll + EXEC_PANE_PAGE).min(total.saturating_sub(1));
                *scroll = new_scroll;
                if new_scroll + 1 >= total {
                    *follow = true;
                    *scroll = total.saturating_sub(EXEC_PANE_PAGE);
                }
            }
            Vec::new()
        }
        Action::CursorUp => {
            if let Some(Overlay::ExecPane { scroll, follow, .. }) = &mut state.overlay {
                *scroll = scroll.saturating_sub(1);
                *follow = false;
            }
            Vec::new()
        }
        Action::CursorDown => {
            let total = exec_pane_total_lines(state);
            if let Some(Overlay::ExecPane { scroll, follow, .. }) = &mut state.overlay {
                if *scroll + 1 < total {
                    *scroll += 1;
                }
                if *scroll + 1 >= total {
                    *follow = true;
                    *scroll = total.saturating_sub(EXEC_PANE_PAGE);
                }
            }
            Vec::new()
        }
        Action::CursorBottom => {
            if let Some(Overlay::ExecPane { follow, .. }) = &mut state.overlay {
                *follow = true;
            }
            Vec::new()
        }
        Action::CursorTop => {
            if let Some(Overlay::ExecPane { scroll, follow, .. }) = &mut state.overlay {
                *scroll = 0;
                *follow = false;
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Total visible lines in the exec pane (complete lines + 1 for partial if non-empty).
fn exec_pane_total_lines(state: &AppState) -> usize {
    let Some(Overlay::ExecPane { id, .. }) = &state.overlay else {
        return 0;
    };
    let Some(rec) = state.sessions.get(id) else {
        return 0;
    };
    rec.output_lines.len() + if rec.output_partial.is_empty() { 0 } else { 1 }
}

/// Handle actions while the port-forward status overlay is open.
///
/// `Confirm`/`Enter` are intentional no-ops: the status pane is informational and a live
/// port-forward must not be torn down by accident. Only an explicit `Cancel` (Esc) closes it.
fn apply_port_forward_status_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    match action {
        Action::Confirm | Action::Enter => Vec::new(),
        Action::Cancel => {
            let Some(Overlay::PortForwardStatus { id }) = state.overlay.take() else {
                return Vec::new();
            };
            // If the session has already ended (relay task gone), clean up the record.
            if state.sessions.get(&id).is_some_and(|r| r.ended.is_some()) {
                state.sessions.remove(&id);
            }
            vec![AppEffect::CloseSession { id }]
        }
        _ => Vec::new(),
    }
}

/// Append a chunk of text into the log buffer (lines + partial), enforcing the byte and line caps.
///
/// The chunk may contain zero, one, or many newlines. Lines are accumulated in `lines` (oldest first);
/// the final unterminated fragment lives in `partial`. `byte_size` tracks the total retained bytes so
/// the cap check is O(1) rather than summing `lines` every call.
fn append_log_chunk(
    lines: &mut VecDeque<String>,
    partial: &mut String,
    byte_size: &mut usize,
    text: &str,
) {
    use crate::state::{LOG_VIEWER_MAX_BYTES, LOG_VIEWER_MAX_LINES};

    // Split on newlines. For each full line, append partial+line as one complete line. The last
    // segment (possibly empty) becomes the new partial.
    let mut segments = text.split('\n');
    // The first segment continues the current partial line.
    if let Some(first) = segments.next() {
        *byte_size += first.len();
        partial.push_str(first);
    }
    for seg in segments {
        // We have a newline: flush partial into lines.
        let complete = std::mem::take(partial);
        *byte_size += 1; // the '\n' itself (we track it symbolically; line lengths sum to byte_size)
        lines.push_back(complete);
        *byte_size += seg.len();
        partial.push_str(seg);
        // Enforce caps: evict oldest lines until within limits.
        while (lines.len() > LOG_VIEWER_MAX_LINES || *byte_size > LOG_VIEWER_MAX_BYTES)
            && !lines.is_empty()
        {
            if let Some(evicted) = lines.pop_front() {
                *byte_size = byte_size.saturating_sub(evicted.len() + 1); // +1 for '\n'
            }
        }
    }
}

/// Append a decoded text chunk into the session output ring buffer.
///
/// Returns the number of complete lines that were evicted from the front to enforce the size
/// caps. The caller must subtract this from any stored `scroll` offset to prevent the scroll
/// position from drifting after eviction.
fn append_session_output(rec: &mut SessionRecord, text: &str) -> usize {
    let lines = &mut rec.output_lines;
    let partial = &mut rec.output_partial;
    let byte_size = &mut rec.output_byte_size;
    let mut evicted = 0usize;
    let mut segments = text.split('\n');
    if let Some(first) = segments.next() {
        *byte_size += first.len();
        partial.push_str(first);
    }
    for seg in segments {
        let complete = std::mem::take(partial);
        *byte_size += 1;
        lines.push_back(complete);
        *byte_size += seg.len();
        partial.push_str(seg);
        while (lines.len() > SESSION_OUTPUT_MAX_LINES || *byte_size > SESSION_OUTPUT_MAX_BYTES)
            && !lines.is_empty()
        {
            if let Some(e) = lines.pop_front() {
                *byte_size = byte_size.saturating_sub(e.len() + 1);
                evicted += 1;
            }
        }
    }
    evicted
}

/// Mint a transfer id, push a fresh [`ActiveTransfer`] tracking it, set the status line, and return
/// the id. Shared by the initial attempt, the queue drain, and the post-confirm re-issue so they
/// can't drift. The id is monotonic (no clock/RNG — keeps the reducer pure and tests deterministic).
fn arm_transfer(state: &mut AppState, is_move: bool, count: usize) -> TransferId {
    let id = state.next_transfer_id;
    state.next_transfer_id += 1;
    let label = format!(
        "{} {count} item(s)…",
        if is_move { "Moving" } else { "Copying" },
    );
    state.status = Some(label.clone());
    state.active_transfers.push(ActiveTransfer {
        id,
        label,
        bytes: 0,
        rate: None,
        total: None,
        paused: false,
    });
    id
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
        AppEvent::AiPlanExecuted { status, error } => {
            state.ai_executing = false;
            // Refresh both panes so any filesystem changes the plan made are reflected.
            finish_op(state, &status, error)
        }
        AppEvent::TransferProgress {
            id,
            bytes,
            rate_bps,
            total,
        } => {
            // Advisory display only; ignore an update for a transfer that already finished.
            if let Some(t) = state.active_transfers.iter_mut().find(|t| t.id == id) {
                t.bytes = bytes;
                t.rate = Some(rate_bps);
                t.total = total;
            }
            Vec::new()
        }
        AppEvent::OpDone { status, error } => finish_op(state, &status, error),
        AppEvent::TransferConflict {
            id,
            src_conn,
            dst_conn,
            items,
            is_move,
            conflicts,
        } => {
            // The transfer didn't run (no overwrite); release its slot.
            state.active_transfers.retain(|t| t.id != id);
            // Don't clobber an overlay the user already has open (e.g. a delete confirmation): put
            // the transfer back at the front of the queue so the tail-drain retries it (and shows
            // the overwrite prompt) once that overlay closes.
            if state.overlay.is_some() {
                state.transfer_queue.push_front(QueuedTransfer {
                    src_conn,
                    dst_conn,
                    items,
                    is_move,
                });
                // A row was inserted at the front; if the queue view is open, keep the selection on
                // the same logical item so a subsequent drop targets what the user sees.
                if let Some(Overlay::TransferQueue { cursor }) = &mut state.overlay {
                    *cursor += 1;
                }
                return Vec::new();
            }
            state.status = None;
            state.overlay = Some(Overlay::ConfirmOverwrite {
                src_conn,
                dst_conn,
                items,
                is_move,
                conflicts,
            });
            Vec::new()
        }
        AppEvent::TransferDone { id, status, error } => {
            // Release this transfer's slot — addressed by id so an unrelated op or another transfer
            // finishing can't clear the wrong one. The queue tail-drain then fills the freed slot.
            state.active_transfers.retain(|t| t.id != id);
            finish_op(state, &status, error)
        }
        AppEvent::ShellActionDone { status, error } => {
            // A shell action may have changed the filesystem (created/removed files), so refresh both
            // panes via the normal op-completion path.
            finish_op(state, &status, error)
        }
        AppEvent::LogChunk { id, text } => {
            if let Some(Overlay::LogViewer {
                id: ov_id,
                lines,
                partial,
                byte_size,
                follow,
                scroll,
                ..
            }) = &mut state.overlay
            {
                if *ov_id == id {
                    append_log_chunk(lines, partial, byte_size, &text);
                    let new_total = lines.len() + if partial.is_empty() { 0 } else { 1 };
                    if *scroll >= new_total && new_total > 0 {
                        *scroll = new_total - 1;
                    }
                    if *follow {
                        // Approximate the last-page top so un-follow transitions feel natural when scroll is restored.
                        *scroll = new_total.saturating_sub(LOG_VIEWER_PAGE);
                    }
                }
            }
            Vec::new()
        }
        AppEvent::LogStreamEnded { id, error } => {
            if let Some(Overlay::LogViewer {
                id: ov_id, status, ..
            }) = &mut state.overlay
            {
                if *ov_id == id {
                    *status = match error {
                        Some(msg) => LogViewerStatus::Error(msg),
                        None => LogViewerStatus::Done,
                    };
                }
            }
            Vec::new()
        }
        AppEvent::VaultUnlocked { result } => {
            // The attempt is no longer in flight, whatever the outcome.
            state.vault_unlocking = false;
            match result {
                Ok(opened) => {
                    state.vault_unlocked = true;
                    state.has_locked_connections = false;
                    let n = opened.len();
                    // Surface the now-reachable connections in the switcher.
                    state.connections.extend(opened);
                    // Only close *our* overlay: the unlock runs in a detached task, so by the time it
                    // finishes the user may have closed the unlock box and opened a different one
                    // (e.g. the connection switcher) — which we must not clobber.
                    if matches!(state.overlay, Some(Overlay::VaultUnlock { .. })) {
                        state.overlay = None;
                    }
                    state.status = Some(if n == 0 {
                        "Vault unlocked".to_owned()
                    } else {
                        format!("Vault unlocked — {n} connection(s) opened")
                    });
                    Vec::new()
                }
                Err(msg) => {
                    // Keep the unlock overlay open with a retryable error and a wiped field; if the
                    // user has since closed it, fall back to the status line. The message is
                    // secret-free.
                    if let Some(Overlay::VaultUnlock { input, error }) = &mut state.overlay {
                        input.clear();
                        *error = Some(msg);
                    } else {
                        state.status = Some(format!("Vault unlock failed: {msg}"));
                    }
                    Vec::new()
                }
            }
        }
        AppEvent::SessionOutput { id, bytes } => {
            // Decode lossily and append to the ring buffer, same mechanism as the log viewer.
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if let Some(rec) = state.sessions.get_mut(&id) {
                let evicted = append_session_output(rec, &text);
                let new_total =
                    rec.output_lines.len() + if rec.output_partial.is_empty() { 0 } else { 1 };
                if let Some(Overlay::ExecPane {
                    id: ov_id,
                    scroll,
                    follow,
                    ..
                }) = &mut state.overlay
                {
                    if *ov_id == id {
                        // Adjust scroll for evicted lines to prevent view drift.
                        *scroll = scroll.saturating_sub(evicted);
                        if *follow {
                            *scroll = new_total.saturating_sub(EXEC_PANE_PAGE);
                        } else if *scroll >= new_total && new_total > 0 {
                            *scroll = new_total - 1;
                        }
                    }
                }
            }
            Vec::new()
        }
        AppEvent::SessionEnded {
            id,
            exit_code,
            error,
        } => {
            if let Some(rec) = state.sessions.get_mut(&id) {
                rec.ended = Some(SessionEnd { exit_code, error });
            }
            // If the overlay is still showing this session the user can read the exit code; it will
            // be cleaned up when they close the overlay. Otherwise clean up immediately.
            let showing = matches!(&state.overlay, Some(Overlay::ExecPane { id: ov_id, .. }) if *ov_id == id)
                || matches!(&state.overlay, Some(Overlay::PortForwardStatus { id: ov_id }) if *ov_id == id);
            if !showing {
                state.sessions.remove(&id);
            }
            Vec::new()
        }
        AppEvent::PortForwardBound { id, local_port } => {
            if let Some(rec) = state.sessions.get_mut(&id) {
                rec.local_port = Some(local_port);
            }
            Vec::new()
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
    use crate::state::ShellActionMeta;
    use cairn_types::{ConnectionId, Entry, EntryKind, VfsPath};
    use cairn_vfs::{ListPage, VfsError};

    fn state() -> AppState {
        AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root())
    }

    // Single-transfer accessors mirroring the old scalar fields (concurrency is 1 in these tests):
    // `Some(bytes)`/`None` exactly as `transfer_bytes` was, so existing assertions read naturally.
    fn t_bytes(s: &AppState) -> Option<u64> {
        s.active_transfers.first().map(|t| t.bytes)
    }
    fn t_rate(s: &AppState) -> Option<u64> {
        s.active_transfers.first().and_then(|t| t.rate)
    }
    fn t_paused(s: &AppState) -> bool {
        s.active_transfers.first().is_some_and(|t| t.paused)
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
        assert_eq!(t_bytes(&s), Some(0), "transfer tracking starts");
        // Progress updates the running total and the rate.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferProgress {
                id: 1,
                bytes: 4096,
                rate_bps: 2048,
                total: None,
            }),
        );
        assert_eq!(t_bytes(&s), Some(4096));
        assert_eq!(t_rate(&s), Some(2048));
        // The transfer's own completion clears it.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferDone {
                id: 1,
                status: "Copied 1 file(s)".to_owned(),
                error: false,
            }),
        );
        assert_eq!(t_bytes(&s), None);
        assert_eq!(t_rate(&s), None, "rate cleared on completion");
    }

    #[test]
    fn transfer_progress_after_completion_is_ignored() {
        let mut s = state();
        // No transfer tracked: a late/stray progress event must not resurrect the indicator.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferProgress {
                id: 1,
                bytes: 10,
                rate_bps: 0,
                total: None,
            }),
        );
        assert_eq!(t_bytes(&s), None);
    }

    #[test]
    fn an_unrelated_op_completing_does_not_clear_a_live_transfer() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy));
        assert_eq!(t_bytes(&s), Some(0));
        // A delete/mkdir/rename finishing (generic OpDone) must NOT wipe the transfer indicator…
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::OpDone {
                status: "Deleted 1 item(s)".to_owned(),
                error: false,
            }),
        );
        assert_eq!(t_bytes(&s), Some(0));
        // …and subsequent progress still lands.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferProgress {
                id: 1,
                bytes: 8192,
                rate_bps: 0,
                total: None,
            }),
        );
        assert_eq!(t_bytes(&s), Some(8192));
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
        assert!(matches!(&fx[..], [AppEffect::CancelTransfer { .. }]));
        // Exact wording preserved from the single-transfer era (singular at n == 1).
        assert_eq!(s.status.as_deref(), Some("Cancelling transfer…"));
    }

    #[test]
    fn toggle_pause_flips_state_and_emits_effect_only_while_transferring() {
        let mut s = state();
        // No transfer running: TogglePause is a no-op (with a hint) and leaves the flag clear.
        let fx = update(&mut s, Msg::Action(Action::TogglePause));
        assert!(fx.is_empty());
        assert!(!t_paused(&s));
        assert!(s.status.as_deref().unwrap().contains("No transfer"));
        // Start a transfer, then pause: flag set, SetTransferPaused(true) emitted.
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy));
        let fx = update(&mut s, Msg::Action(Action::TogglePause));
        assert!(matches!(
            &fx[..],
            [AppEffect::SetTransferPaused { paused: true, .. }]
        ));
        assert!(t_paused(&s));
        assert_eq!(s.status.as_deref(), Some("Transfer paused"));
        // Toggle again: resume.
        let fx = update(&mut s, Msg::Action(Action::TogglePause));
        assert!(matches!(
            &fx[..],
            [AppEffect::SetTransferPaused { paused: false, .. }]
        ));
        assert!(!t_paused(&s));
        assert_eq!(s.status.as_deref(), Some("Transfer resumed"));
    }

    #[test]
    fn a_transient_status_is_cleared_by_navigation() {
        // The status line renders `state.status` when idle; a navigation keystroke must dismiss it so
        // a stale "Copied…"/"error:" message doesn't linger (and the key hints return).
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        s.status = Some("Copied 1 file(s)".to_owned());
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        assert!(
            s.status.is_none(),
            "navigation dismisses a transient status"
        );
    }

    #[test]
    fn concurrent_transfers_run_together_and_are_addressed_by_id() {
        // With concurrency 2, a second transfer runs alongside the first (not queued); progress and
        // completion are addressed by id, so they never touch the wrong transfer.
        let mut s = state();
        s.concurrency_limit = 2;
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a", EntryKind::File),
                Entry::new("b", EntryKind::File),
            ],
        );
        // First copy (cursor on "a") → active id 1.
        let _ = update(&mut s, Msg::Action(Action::Copy));
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        // Second copy (cursor on "b") → active id 2, runs concurrently (not queued).
        let fx = update(&mut s, Msg::Action(Action::Copy));
        assert!(matches!(&fx[..], [AppEffect::Transfer { .. }]));
        assert_eq!(s.active_transfers.len(), 2, "both run; nothing queued");
        assert!(s.transfer_queue.is_empty());
        let ids: Vec<_> = s.active_transfers.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1, 2], "monotonic, distinct ids");
        // Progress for id 2 updates only the second transfer.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferProgress {
                id: 2,
                bytes: 4096,
                rate_bps: 1024,
                total: None,
            }),
        );
        assert_eq!(s.active_transfers[0].bytes, 0, "id 1 untouched");
        assert_eq!(s.active_transfers[1].bytes, 4096, "id 2 updated");
        // Completing id 1 removes only it; id 2 keeps running.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferDone {
                id: 1,
                status: "Copied".to_owned(),
                error: false,
            }),
        );
        assert_eq!(s.active_transfers.len(), 1);
        assert_eq!(s.active_transfers[0].id, 2);
        assert_eq!(s.active_transfers[0].bytes, 4096);
    }

    #[test]
    fn transfer_conflict_clears_the_paused_flag() {
        // A `p` pressed during the pre-flight check (transfer_bytes briefly Some) then a bounce-back
        // conflict must not strand a stale paused flag.
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy));
        let _ = update(&mut s, Msg::Action(Action::TogglePause));
        assert!(t_paused(&s));
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferConflict {
                id: 1,
                src_conn: ConnectionId(1),
                dst_conn: ConnectionId(2),
                items: vec![(VfsPath::root(), VfsPath::root())],
                is_move: false,
                conflicts: 1,
            }),
        );
        assert!(!t_paused(&s), "a conflict clears the paused flag");
    }

    #[test]
    fn transfer_done_clears_the_paused_flag() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy));
        let _ = update(&mut s, Msg::Action(Action::TogglePause));
        assert!(t_paused(&s));
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferDone {
                id: 1,
                status: "Copied 1 file(s)".to_owned(),
                error: false,
            }),
        );
        assert!(!t_paused(&s), "completion clears the paused flag");
    }

    fn with_action(s: &mut AppState, confirm: bool) {
        s.shell_actions = vec![ShellActionMeta {
            name: "Checksum".to_owned(),
            confirm,
        }];
    }

    #[test]
    fn shell_action_with_confirm_opens_the_confirm_overlay() {
        let mut s = state();
        with_action(&mut s, true);
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::RunShellAction(0)));
        assert!(fx.is_empty(), "confirm defers the run");
        assert!(matches!(
            s.overlay,
            Some(Overlay::ConfirmShellAction { index: 0, .. })
        ));
        // Confirming dispatches the run effect against the selected entry.
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(matches!(
            &fx[..],
            [AppEffect::RunShellAction { index: 0, .. }]
        ));
        assert!(s.overlay.is_none());
    }

    #[test]
    fn shell_action_confirm_cancel_abandons_it() {
        let mut s = state();
        with_action(&mut s, true);
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        let _ = update(&mut s, Msg::Action(Action::RunShellAction(0)));
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none(), "cancel closes the overlay");
    }

    #[test]
    fn shell_action_without_confirm_runs_directly() {
        let mut s = state();
        with_action(&mut s, false);
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::RunShellAction(0)));
        assert!(matches!(
            &fx[..],
            [AppEffect::RunShellAction { index: 0, .. }]
        ));
        assert!(s.overlay.is_none());
    }

    #[test]
    fn shell_action_with_nothing_selected_is_a_no_op() {
        let mut s = state();
        with_action(&mut s, false);
        // No `deliver` → the pane is empty / Loading, nothing under the cursor.
        let fx = update(&mut s, Msg::Action(Action::RunShellAction(0)));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
        assert!(s.status.as_deref().unwrap().contains("nothing selected"));
    }

    #[test]
    fn shell_action_done_refreshes_panes() {
        let mut s = state();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::ShellActionDone {
                status: "Checksum: exit 0".to_owned(),
                error: false,
            }),
        );
        // finish_op re-lists both panes.
        assert_eq!(fx.len(), 2);
        assert!(fx.iter().all(|e| matches!(e, AppEffect::List { .. })));
        assert_eq!(s.status.as_deref(), Some("Checksum: exit 0"));
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "out of range")]
    fn shell_action_out_of_range_index_asserts_in_debug() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        // No actions registered → index 0 is out of range (a keymap/list wiring bug).
        let _ = update(&mut s, Msg::Action(Action::RunShellAction(0)));
    }

    #[test]
    fn transfer_conflict_opens_overwrite_confirm_then_reissues_with_overwrite() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy));
        // The effect runner reports a collision instead of clobbering.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferConflict {
                id: 1,
                src_conn: ConnectionId(1),
                dst_conn: ConnectionId(2),
                items: vec![(VfsPath::root(), VfsPath::root())],
                is_move: false,
                conflicts: 1,
            }),
        );
        assert!(matches!(s.overlay, Some(Overlay::ConfirmOverwrite { .. })));
        assert_eq!(t_bytes(&s), None, "no transfer is running yet");
        // Confirming re-issues the transfer with overwrite enabled.
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(s.overlay.is_none());
        assert_eq!(t_bytes(&s), Some(0));
        assert!(matches!(
            &fx[..],
            [AppEffect::Transfer {
                overwrite: true,
                ..
            }]
        ));
    }

    #[test]
    fn cancelling_overwrite_confirm_keeps_the_marked_selection() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a", EntryKind::File),
                Entry::new("b", EntryKind::File),
            ],
        );
        // Mark both files, then attempt a copy.
        let _ = update(&mut s, Msg::Action(Action::ToggleMark));
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let _ = update(&mut s, Msg::Action(Action::ToggleMark));
        assert_eq!(s.active().marked.len(), 2);
        let _ = update(&mut s, Msg::Action(Action::Copy));
        // A conflict comes back; the user cancels — the selection must survive (nothing transferred).
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferConflict {
                id: 1,
                src_conn: ConnectionId(1),
                dst_conn: ConnectionId(2),
                items: vec![(VfsPath::root(), VfsPath::root())],
                is_move: false,
                conflicts: 2,
            }),
        );
        let _ = update(&mut s, Msg::Action(Action::Cancel));
        assert_eq!(
            s.active().marked.len(),
            2,
            "marks survive a cancelled overwrite"
        );
    }

    #[test]
    fn overwrite_confirm_cancel_abandons_the_transfer() {
        let mut s = state();
        s.overlay = Some(Overlay::ConfirmOverwrite {
            src_conn: ConnectionId(1),
            dst_conn: ConnectionId(2),
            items: vec![(VfsPath::root(), VfsPath::root())],
            is_move: true,
            conflicts: 2,
        });
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
        assert!(s.status.as_deref().unwrap().contains("cancelled"));
    }

    #[test]
    fn a_second_transfer_is_queued_then_starts_when_the_first_finishes() {
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
        // A second transfer while the first runs is queued, not refused (no effect now).
        let fx = update(&mut s, Msg::Action(Action::Move));
        assert!(fx.is_empty());
        assert_eq!(s.transfer_queue.len(), 1);
        assert!(s.status.as_deref().unwrap().contains("Queued"));
        // When the first finishes, the queued one starts (its own Transfer effect is emitted).
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::TransferDone {
                id: 1,
                status: "Copied".to_owned(),
                error: false,
            }),
        );
        assert!(s.transfer_queue.is_empty());
        assert_eq!(t_bytes(&s), Some(0), "the queued transfer is now active");
        assert!(fx.iter().any(|e| matches!(e, AppEffect::Transfer { .. })));
    }

    #[test]
    fn queue_overlay_opens_and_clears_pending_without_touching_the_active_transfer() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a", EntryKind::File),
                Entry::new("b", EntryKind::File),
            ],
        );
        let _ = update(&mut s, Msg::Action(Action::Copy)); // A active
        let _ = update(&mut s, Msg::Action(Action::Move)); // B queued
        assert_eq!(s.transfer_queue.len(), 1);
        // Open the queue view.
        let _ = update(&mut s, Msg::Action(Action::OpenQueue));
        assert!(matches!(s.overlay, Some(Overlay::TransferQueue { .. })));
        // Reject clears the pending queue but leaves the active transfer running.
        let _ = update(&mut s, Msg::Action(Action::Reject));
        assert!(s.transfer_queue.is_empty());
        assert_eq!(t_bytes(&s), Some(0), "the active transfer is untouched");
        assert!(s.overlay.is_none());
    }

    #[test]
    fn queue_overlay_esc_just_closes() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::OpenQueue));
        assert!(matches!(s.overlay, Some(Overlay::TransferQueue { .. })));
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
    }

    #[test]
    fn queue_overlay_navigates_and_drops_the_selected_pending_transfer() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("a", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy)); // A active
        let _ = update(&mut s, Msg::Action(Action::Move)); // queued #1
        let _ = update(&mut s, Msg::Action(Action::Copy)); // queued #2
        assert_eq!(s.transfer_queue.len(), 2);
        let _ = update(&mut s, Msg::Action(Action::OpenQueue));
        // Move the cursor to the second pending entry, then drop it.
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        assert!(matches!(
            s.overlay,
            Some(Overlay::TransferQueue { cursor: 1 })
        ));
        let _ = update(&mut s, Msg::Action(Action::Delete));
        assert_eq!(
            s.transfer_queue.len(),
            1,
            "only the selected one was dropped"
        );
        assert_eq!(t_bytes(&s), Some(0), "the active transfer is untouched");
        // The cursor re-clamps to the remaining entry; the view stays open.
        assert!(matches!(
            s.overlay,
            Some(Overlay::TransferQueue { cursor: 0 })
        ));
        // Dropping the last one closes the view.
        let _ = update(&mut s, Msg::Action(Action::Delete));
        assert!(s.transfer_queue.is_empty());
        assert!(s.overlay.is_none());
    }

    #[test]
    fn queue_reorder_moves_the_selected_pending_transfer() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("a", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy)); // A active
        let _ = update(&mut s, Msg::Action(Action::Move)); // queued #1 (a move)
        let _ = update(&mut s, Msg::Action(Action::Copy)); // queued #2 (a copy)
        assert_eq!(s.transfer_queue.len(), 2);
        assert!(s.transfer_queue[0].is_move && !s.transfer_queue[1].is_move);
        let _ = update(&mut s, Msg::Action(Action::OpenQueue));
        // Select the 2nd pending (the copy) and move it up; the cursor follows it.
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let _ = update(&mut s, Msg::Action(Action::QueueMoveUp));
        assert!(!s.transfer_queue[0].is_move, "the copy moved to the front");
        assert!(s.transfer_queue[1].is_move);
        assert!(matches!(
            s.overlay,
            Some(Overlay::TransferQueue { cursor: 0 })
        ));
        // Moving up at the top is a no-op.
        let _ = update(&mut s, Msg::Action(Action::QueueMoveUp));
        assert!(matches!(
            s.overlay,
            Some(Overlay::TransferQueue { cursor: 0 })
        ));
        // Move it back down: the copy returns to index 1, cursor follows.
        let _ = update(&mut s, Msg::Action(Action::QueueMoveDown));
        assert!(s.transfer_queue[0].is_move && !s.transfer_queue[1].is_move);
        assert!(matches!(
            s.overlay,
            Some(Overlay::TransferQueue { cursor: 1 })
        ));
        // Moving down at the bottom is a no-op.
        let _ = update(&mut s, Msg::Action(Action::QueueMoveDown));
        assert!(matches!(
            s.overlay,
            Some(Overlay::TransferQueue { cursor: 1 })
        ));
        // QueueMove with no overlay open is a harmless no-op that leaves the queue unchanged.
        let before = s.transfer_queue.len();
        s.overlay = None;
        assert!(update(&mut s, Msg::Action(Action::QueueMoveDown)).is_empty());
        assert_eq!(s.transfer_queue.len(), before);
    }

    #[test]
    fn queue_cursor_tracks_its_item_when_a_conflict_requeues_at_the_front() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("a", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy)); // A active
        let _ = update(&mut s, Msg::Action(Action::Move)); // queued: [B]
        let _ = update(&mut s, Msg::Action(Action::OpenQueue));
        assert!(matches!(
            s.overlay,
            Some(Overlay::TransferQueue { cursor: 0 })
        ));
        // A conflicts while the queue view is open → A is re-queued at the front: [A, B].
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferConflict {
                id: 1,
                src_conn: ConnectionId(1),
                dst_conn: ConnectionId(2),
                items: vec![(VfsPath::root(), VfsPath::root())],
                is_move: false,
                conflicts: 1,
            }),
        );
        assert_eq!(s.transfer_queue.len(), 2);
        // The cursor followed its item (B) from index 0 to index 1, so a drop hits B, not A.
        assert!(matches!(
            s.overlay,
            Some(Overlay::TransferQueue { cursor: 1 })
        ));
    }

    #[test]
    fn clearing_an_empty_queue_shows_no_misleading_status() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::OpenQueue));
        let _ = update(&mut s, Msg::Action(Action::Reject));
        assert!(s.overlay.is_none());
        assert!(
            s.status.is_none(),
            "no 'Cleared 0' message on an empty queue"
        );
    }

    #[test]
    fn open_queue_is_suppressed_while_a_plan_is_pending() {
        let mut s = state();
        request_ai(&mut s, "do something"); // ai_pending = true
        let fx = update(&mut s, Msg::Action(Action::OpenQueue));
        assert!(fx.is_empty());
        assert!(
            s.overlay.is_none(),
            "queue view must not clobber an arriving plan overlay"
        );
    }

    #[test]
    fn queue_drains_after_an_unrelated_overlay_opened_during_the_active_transfer() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a", EntryKind::File),
                Entry::new("b", EntryKind::File),
            ],
        );
        let _ = update(&mut s, Msg::Action(Action::Copy)); // A active
        let _ = update(&mut s, Msg::Action(Action::Move)); // B queued
        assert_eq!(s.transfer_queue.len(), 1);
        // A delete confirmation is opened over the running transfer.
        let _ = update(&mut s, Msg::Action(Action::Delete));
        assert!(matches!(s.overlay, Some(Overlay::ConfirmDelete { .. })));
        // A finishes while the dialog is open: B can't start under the overlay.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferDone {
                id: 1,
                status: "Copied".to_owned(),
                error: false,
            }),
        );
        assert_eq!(
            s.transfer_queue.len(),
            1,
            "B waits while the overlay is open"
        );
        // Dismissing the dialog drains the queue (tail-drain in `update`).
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(
            s.transfer_queue.is_empty(),
            "B starts once the overlay closes"
        );
        assert_eq!(t_bytes(&s), Some(0));
        assert!(fx.iter().any(|e| matches!(e, AppEffect::Transfer { .. })));
    }

    #[test]
    fn a_conflict_does_not_clobber_an_open_overlay_and_retries_after_it_closes() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("a", EntryKind::File)]);
        let _ = update(&mut s, Msg::Action(Action::Copy)); // A active
                                                           // User opens a delete dialog over the running transfer.
        let _ = update(&mut s, Msg::Action(Action::Delete));
        assert!(matches!(s.overlay, Some(Overlay::ConfirmDelete { .. })));
        // A reports a conflict while the delete dialog is open — it must NOT replace the dialog.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferConflict {
                id: 1,
                src_conn: ConnectionId(1),
                dst_conn: ConnectionId(2),
                items: vec![(VfsPath::root(), VfsPath::root())],
                is_move: false,
                conflicts: 1,
            }),
        );
        assert!(
            matches!(s.overlay, Some(Overlay::ConfirmDelete { .. })),
            "the delete dialog is preserved"
        );
        assert_eq!(
            s.transfer_queue.len(),
            1,
            "the conflicted transfer is re-queued"
        );
        // Dismissing the delete dialog retries the transfer (tail-drain re-issues its Transfer
        // effect; the effect runner then re-detects the conflict and shows the overwrite prompt).
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(s.transfer_queue.is_empty());
        assert_eq!(t_bytes(&s), Some(0));
        assert!(fx.iter().any(|e| matches!(e, AppEffect::Transfer { .. })));
    }

    #[test]
    fn cancelling_an_overwrite_prompt_drains_the_queue() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a", EntryKind::File),
                Entry::new("b", EntryKind::File),
            ],
        );
        // First transfer starts; a second is queued behind it.
        let _ = update(&mut s, Msg::Action(Action::Copy));
        let _ = update(&mut s, Msg::Action(Action::Move));
        assert_eq!(s.transfer_queue.len(), 1);
        // The first transfer then hits a conflict → overwrite prompt (clears the active slot).
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::TransferConflict {
                id: 1,
                src_conn: ConnectionId(1),
                dst_conn: ConnectionId(2),
                items: vec![(VfsPath::root(), VfsPath::root())],
                is_move: false,
                conflicts: 1,
            }),
        );
        assert!(matches!(s.overlay, Some(Overlay::ConfirmOverwrite { .. })));
        // Cancelling the prompt abandons that transfer and drains the queue: the next one starts.
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(s.overlay.is_none());
        assert!(s.transfer_queue.is_empty());
        assert_eq!(t_bytes(&s), Some(0));
        assert!(fx.iter().any(|e| matches!(e, AppEffect::Transfer { .. })));
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
                output: None,
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
    fn executing_plan_can_be_cancelled_with_esc_and_clears_on_done() {
        let mut s = state();
        open_plan(&mut s, &["list", "copy"]); // both safe → bulk-approvable
        let fx = update(&mut s, Msg::Action(Action::ApproveAll));
        assert!(matches!(&fx[..], [AppEffect::ExecutePlan { .. }]));
        assert!(s.ai_executing, "execution is in progress");
        // Esc while executing requests cancellation.
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(matches!(&fx[..], [AppEffect::CancelAiPlan]));
        assert!(
            s.ai_executing,
            "flag stays until the execution actually ends"
        );
        // The execution's completion event clears the flag.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::AiPlanExecuted {
                status: "Plan cancelled after 1 step(s)".to_owned(),
                error: false,
            }),
        );
        assert!(!s.ai_executing);
    }

    #[test]
    fn esc_with_nothing_running_is_a_no_op() {
        let mut s = state();
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(fx.is_empty());
    }

    #[test]
    fn competing_ops_are_refused_while_a_plan_executes() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("f", EntryKind::File)]);
        open_plan(&mut s, &["list", "copy"]);
        let _ = update(&mut s, Msg::Action(Action::ApproveAll)); // ai_executing = true
        assert!(s.ai_executing);
        // A second plan request and a manual copy must both be refused (no effect, no new state).
        let fx = update(&mut s, Msg::Action(Action::AiPropose));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none(), "no second AI prompt opens");
        let fx = update(&mut s, Msg::Action(Action::Copy));
        assert!(fx.is_empty(), "no competing transfer starts");
        assert!(t_bytes(&s).is_none());
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

    // ---- M3-7: vault-unlock overlay --------------------------------------------------------------

    #[test]
    fn vault_unlock_action_opens_and_cancel_closes_the_overlay() {
        let mut s = state();
        let fx = update(&mut s, Msg::Action(Action::VaultUnlock));
        assert!(fx.is_empty());
        assert!(matches!(s.overlay, Some(Overlay::VaultUnlock { .. })));
        assert!(
            s.capturing_text(),
            "the passphrase field captures keystrokes"
        );
        // Esc/Cancel closes it (the masked input is dropped → zeroized).
        let _ = update(&mut s, Msg::Text(TextEdit::Cancel));
        assert!(s.overlay.is_none());
    }

    #[test]
    fn vault_unlock_action_is_a_noop_when_already_unlocked() {
        let mut s = state();
        s.vault_unlocked = true;
        let fx = update(&mut s, Msg::Action(Action::VaultUnlock));
        assert!(fx.is_empty());
        assert!(
            s.overlay.is_none(),
            "no overlay opens when already unlocked"
        );
        assert_eq!(s.status.as_deref(), Some("Vault already unlocked"));
    }

    #[test]
    fn vault_unlock_passphrase_never_echoes_into_debug_and_submit_carries_the_secret() {
        use cairn_secrets::ExposeSecret;
        const SECRET: &str = "correct horse battery staple";
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::VaultUnlock));
        type_text(&mut s, SECRET);
        // The typed passphrase must not be visible in a `{:?}` of the whole state (or the overlay).
        assert!(
            !format!("{s:?}").contains("staple"),
            "AppState Debug leaked the passphrase"
        );
        // Submitting emits exactly one UnlockVault effect carrying the entered secret…
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        let passphrase = match &fx[..] {
            [AppEffect::UnlockVault { passphrase }] => passphrase,
            other => panic!("expected UnlockVault, got {other:?}"),
        };
        assert_eq!(passphrase.expose_secret(), SECRET);
        // …and even the effect's Debug must not reveal it (SecretString redacts).
        assert!(
            !format!("{:?}", fx[0]).contains("staple"),
            "effect Debug leaked the passphrase"
        );
        // The overlay stays open (awaiting the async result) with the field wiped.
        match &s.overlay {
            Some(Overlay::VaultUnlock { input, error }) => {
                assert!(input.is_empty(), "field is wiped after submit");
                assert!(error.is_none());
            }
            other => panic!("overlay should remain open, got {other:?}"),
        }
        assert_eq!(s.status.as_deref(), Some("Unlocking vault…"));
    }

    #[test]
    fn vault_unlock_empty_passphrase_keeps_the_overlay_with_an_error() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::VaultUnlock));
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty(), "an empty passphrase emits no effect");
        match &s.overlay {
            Some(Overlay::VaultUnlock { error, .. }) => {
                assert_eq!(error.as_deref(), Some("Enter the vault passphrase"));
            }
            other => panic!("overlay should stay open, got {other:?}"),
        }
    }

    #[test]
    fn vault_unlock_failure_keeps_the_overlay_and_shows_the_error() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::VaultUnlock));
        type_text(&mut s, "wrong-pass");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        // The async unlock reports a wrong passphrase.
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::VaultUnlocked {
                result: Err("decryption failed (wrong passphrase or corrupt vault)".to_owned()),
            }),
        );
        assert!(fx.is_empty());
        assert!(!s.vault_unlocked);
        match &s.overlay {
            Some(Overlay::VaultUnlock { input, error }) => {
                assert!(input.is_empty(), "the field is wiped for a retry");
                assert!(error
                    .as_deref()
                    .is_some_and(|e| e.contains("wrong passphrase")));
            }
            other => panic!("overlay should stay open for a retry, got {other:?}"),
        }
    }

    #[test]
    fn vault_unlock_success_closes_overlay_and_adds_connections() {
        let mut s = state();
        s.has_locked_connections = true;
        let _ = update(&mut s, Msg::Action(Action::VaultUnlock));
        type_text(&mut s, "right-pass");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        let opened = vec![crate::ConnectionChoice {
            conn: cairn_types::ConnectionId(7),
            label: "ssh: bastion".to_owned(),
        }];
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::VaultUnlocked { result: Ok(opened) }),
        );
        assert!(fx.is_empty());
        assert!(s.overlay.is_none(), "overlay closes on success");
        assert!(s.vault_unlocked);
        assert!(!s.vault_unlocking, "the in-flight flag is cleared");
        assert!(!s.has_locked_connections);
        assert_eq!(s.connections.len(), 1);
        assert_eq!(s.connections[0].label, "ssh: bastion");
        assert_eq!(
            s.status.as_deref(),
            Some("Vault unlocked — 1 connection(s) opened")
        );
    }

    #[test]
    fn vault_unlock_double_submit_does_not_spawn_a_second_effect() {
        let mut s = state();
        let _ = update(&mut s, Msg::Action(Action::VaultUnlock));
        type_text(&mut s, "pw");
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert_eq!(fx.len(), 1, "first submit emits the unlock effect");
        assert!(s.vault_unlocking);
        // While the unlock is in flight, retyping and submitting again must not spawn a duplicate.
        type_text(&mut s, "pw");
        let fx2 = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx2.is_empty(), "a second submit is ignored while unlocking");
    }

    #[test]
    fn late_unlock_success_does_not_clobber_a_different_overlay() {
        let mut s = state();
        s.connections = vec![crate::ConnectionChoice {
            conn: cairn_types::ConnectionId(3),
            label: "local: /".to_owned(),
        }];
        let _ = update(&mut s, Msg::Action(Action::VaultUnlock));
        type_text(&mut s, "pw");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // unlock task spawned (detached)
                                                             // User closes the unlock box and opens the connection switcher before the result lands.
        let _ = update(&mut s, Msg::Text(TextEdit::Cancel));
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        assert!(matches!(s.overlay, Some(Overlay::Connections { .. })));
        // The late success must update vault state but leave the unrelated overlay untouched.
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::VaultUnlocked {
                result: Ok(Vec::new()),
            }),
        );
        assert!(fx.is_empty());
        assert!(s.vault_unlocked);
        assert!(!s.vault_unlocking);
        assert!(
            matches!(s.overlay, Some(Overlay::Connections { .. })),
            "the connection switcher must not be closed by a late unlock result"
        );
    }

    #[test]
    fn vault_unlock_blocked_while_a_plan_is_executing() {
        let mut s = state();
        s.ai_executing = true;
        let fx = update(&mut s, Msg::Action(Action::VaultUnlock));
        assert!(fx.is_empty());
        assert!(
            s.overlay.is_none(),
            "no overlay opens while a plan executes"
        );
    }

    // ── Log viewer tests ──────────────────────────────────────────────────────────────

    /// Helper: create state with a LogViewer overlay already open.
    fn state_with_log_viewer(id: crate::state::LogViewerId) -> AppState {
        use crate::state::{LogViewerStatus, Overlay};
        let mut s = state();
        s.overlay = Some(Overlay::LogViewer {
            id,
            title: "test — logs".to_owned(),
            lines: std::collections::VecDeque::new(),
            partial: String::new(),
            byte_size: 0,
            follow: true,
            scroll: 0,
            status: LogViewerStatus::Streaming,
        });
        s
    }

    #[test]
    fn log_chunk_appends_lines_and_clips_partial() {
        let mut s = state_with_log_viewer(1);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::LogChunk {
                id: 1,
                text: "line1\nline2\npar".to_owned(),
            }),
        );
        {
            let Some(crate::state::Overlay::LogViewer {
                ref lines,
                ref partial,
                ..
            }) = s.overlay
            else {
                panic!("overlay gone");
            };
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0], "line1");
            assert_eq!(lines[1], "line2");
            assert_eq!(partial, "par");
        }
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::LogChunk {
                id: 1,
                text: "tial\nline4\n".to_owned(),
            }),
        );
        {
            let Some(crate::state::Overlay::LogViewer {
                ref lines,
                ref partial,
                ..
            }) = s.overlay
            else {
                panic!("overlay gone");
            };
            assert_eq!(lines.len(), 4);
            assert_eq!(lines[2], "partial");
            assert_eq!(lines[3], "line4");
            assert_eq!(partial, "");
        }
    }

    #[test]
    fn log_buffer_cap_drops_oldest_lines() {
        use crate::state::LOG_VIEWER_MAX_LINES;
        let mut s = state_with_log_viewer(2);
        // Push LOG_VIEWER_MAX_LINES + 5 lines.
        let big_chunk = "x\n".repeat(LOG_VIEWER_MAX_LINES + 5);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::LogChunk {
                id: 2,
                text: big_chunk,
            }),
        );
        let Some(crate::state::Overlay::LogViewer { ref lines, .. }) = s.overlay else {
            panic!("overlay gone");
        };
        assert!(
            lines.len() <= LOG_VIEWER_MAX_LINES,
            "lines.len() = {} > cap {}",
            lines.len(),
            LOG_VIEWER_MAX_LINES
        );
    }

    #[test]
    fn log_scroll_up_disables_follow() {
        let mut s = state_with_log_viewer(3);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::LogChunk {
                id: 3,
                text: "a\nb\nc\n".to_owned(),
            }),
        );
        let _ = update(&mut s, Msg::Action(Action::CursorUp));
        let Some(crate::state::Overlay::LogViewer { follow, .. }) = &s.overlay else {
            panic!("overlay gone");
        };
        assert!(!follow, "CursorUp must disable follow");
    }

    #[test]
    fn log_scroll_to_bottom_re_enables_follow() {
        let mut s = state_with_log_viewer(4);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::LogChunk {
                id: 4,
                text: "a\nb\nc\n".to_owned(),
            }),
        );
        let _ = update(&mut s, Msg::Action(Action::CursorUp));
        let _ = update(&mut s, Msg::Action(Action::CursorBottom));
        let Some(crate::state::Overlay::LogViewer { follow, .. }) = &s.overlay else {
            panic!("overlay gone");
        };
        assert!(*follow, "CursorBottom must re-enable follow");
    }

    #[test]
    fn log_close_emits_cancel_effect() {
        let mut s = state_with_log_viewer(5);
        let effects = update(&mut s, Msg::Action(Action::Cancel));
        assert!(s.overlay.is_none(), "overlay should be closed");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, AppEffect::CloseLogViewer { id: 5 })),
            "CloseLogViewer effect must be emitted"
        );
    }

    #[test]
    fn log_stream_error_sets_status() {
        let mut s = state_with_log_viewer(6);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::LogStreamEnded {
                id: 6,
                error: Some("connection lost".to_owned()),
            }),
        );
        let Some(crate::state::Overlay::LogViewer { status, .. }) = &s.overlay else {
            panic!("overlay gone");
        };
        assert_eq!(
            *status,
            crate::state::LogViewerStatus::Error("connection lost".to_owned())
        );
    }

    #[test]
    fn log_stream_ended_sets_done_status() {
        let mut s = state_with_log_viewer(7);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::LogStreamEnded { id: 7, error: None }),
        );
        let Some(crate::state::Overlay::LogViewer { status, .. }) = &s.overlay else {
            panic!("overlay gone");
        };
        assert_eq!(*status, crate::state::LogViewerStatus::Done);
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use cairn_types::{ConnectionId, VfsPath};
    use std::collections::VecDeque;

    fn base_state() -> AppState {
        AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root())
    }

    fn insert_session(state: &mut AppState, id: SessionId) {
        state.sessions.insert(
            id,
            SessionRecord {
                path: VfsPath::root(),
                title: "test session".to_owned(),
                output_lines: VecDeque::new(),
                output_partial: String::new(),
                output_byte_size: 0,
                local_port: None,
                ended: None,
            },
        );
    }

    #[test]
    fn session_output_appends_lines() {
        let mut state = base_state();
        let id = SessionId(1);
        insert_session(&mut state, id);

        let effects = update(
            &mut state,
            Msg::Event(AppEvent::SessionOutput {
                id,
                bytes: b"hello\nworld\n".to_vec(),
            }),
        );
        assert!(effects.is_empty());
        let rec = state.sessions.get(&id).unwrap();
        assert_eq!(rec.output_lines.len(), 2);
        assert_eq!(rec.output_lines[0], "hello");
        assert_eq!(rec.output_lines[1], "world");
        assert_eq!(rec.output_partial, "");
    }

    #[test]
    fn session_output_partial_accumulates_until_newline() {
        let mut state = base_state();
        let id = SessionId(1);
        insert_session(&mut state, id);

        let _ = update(
            &mut state,
            Msg::Event(AppEvent::SessionOutput {
                id,
                bytes: b"hel".to_vec(),
            }),
        );
        let _ = update(
            &mut state,
            Msg::Event(AppEvent::SessionOutput {
                id,
                bytes: b"lo\n".to_vec(),
            }),
        );
        let rec = state.sessions.get(&id).unwrap();
        assert_eq!(rec.output_lines.len(), 1);
        assert_eq!(rec.output_lines[0], "hello");
    }

    #[test]
    fn session_output_ring_evicts_oldest_beyond_line_cap() {
        let mut state = base_state();
        let id = SessionId(1);
        insert_session(&mut state, id);
        // Push slightly more than the cap via repeated single-line chunks.
        let chunk = "x".repeat(10) + "\n";
        let over_cap = SESSION_OUTPUT_MAX_LINES + 10;
        for _ in 0..over_cap {
            let _ = update(
                &mut state,
                Msg::Event(AppEvent::SessionOutput {
                    id,
                    bytes: chunk.as_bytes().to_vec(),
                }),
            );
        }
        let rec = state.sessions.get(&id).unwrap();
        assert!(
            rec.output_lines.len() <= SESSION_OUTPUT_MAX_LINES,
            "ring buffer must not exceed the line cap: got {}",
            rec.output_lines.len()
        );
    }

    #[test]
    fn session_ended_sets_exit_code_and_error() {
        let mut state = base_state();
        let id = SessionId(1);
        insert_session(&mut state, id);
        // Set up an ExecPane overlay showing this session so the record is preserved
        // when SessionEnded arrives (Fix 4a: records without a visible overlay are
        // cleaned up immediately on SessionEnded).
        state.overlay = Some(Overlay::ExecPane {
            id,
            input: String::new(),
            scroll: 0,
            follow: true,
        });

        let effects = update(
            &mut state,
            Msg::Event(AppEvent::SessionEnded {
                id,
                exit_code: Some(1),
                error: Some("process exited".to_owned()),
            }),
        );
        assert!(effects.is_empty());
        let rec = state.sessions.get(&id).unwrap();
        let ended = rec.ended.as_ref().expect("ended must be set");
        assert_eq!(ended.exit_code, Some(1));
        assert_eq!(ended.error.as_deref(), Some("process exited"));
    }

    #[test]
    fn session_ended_with_no_overlay_removes_record() {
        let mut state = base_state();
        let id = SessionId(1);
        insert_session(&mut state, id);
        // No overlay — Fix 4a removes the record immediately when no pane is showing it.

        let effects = update(
            &mut state,
            Msg::Event(AppEvent::SessionEnded {
                id,
                exit_code: Some(0),
                error: None,
            }),
        );
        assert!(effects.is_empty());
        assert!(
            !state.sessions.contains_key(&id),
            "session record should be cleaned up when no overlay is showing it"
        );
    }

    #[test]
    fn port_forward_bound_sets_port() {
        let mut state = base_state();
        let id = SessionId(2);
        insert_session(&mut state, id);

        let _ = update(
            &mut state,
            Msg::Event(AppEvent::PortForwardBound {
                id,
                local_port: 5432,
            }),
        );
        let rec = state.sessions.get(&id).unwrap();
        assert_eq!(rec.local_port, Some(5432));
    }

    #[test]
    fn exec_pane_text_appends_to_input_field() {
        let mut state = base_state();
        let id = SessionId(1);
        insert_session(&mut state, id);
        state.overlay = Some(Overlay::ExecPane {
            id,
            input: String::new(),
            scroll: 0,
            follow: true,
        });

        // Character keys route to the field.
        let _ = update(&mut state, Msg::Text(TextEdit::Insert('h')));
        let _ = update(&mut state, Msg::Text(TextEdit::Insert('i')));
        if let Some(Overlay::ExecPane { input, .. }) = &state.overlay {
            assert_eq!(input, "hi");
        } else {
            panic!("overlay should still be open");
        }
    }

    #[test]
    fn exec_pane_enter_emits_send_session_input_and_clears_field() {
        let mut state = base_state();
        let id = SessionId(1);
        insert_session(&mut state, id);
        state.overlay = Some(Overlay::ExecPane {
            id,
            input: "ls".to_owned(),
            scroll: 0,
            follow: true,
        });

        let effects = update(&mut state, Msg::Text(TextEdit::Submit));
        assert_eq!(effects.len(), 1, "expected exactly one effect");
        match &effects[0] {
            AppEffect::SendSessionInput {
                id: eff_id, bytes, ..
            } => {
                assert_eq!(*eff_id, id);
                assert_eq!(bytes, b"ls\n");
            }
            other => panic!("expected SendSessionInput, got {other:?}"),
        }
        // Field is cleared.
        if let Some(Overlay::ExecPane { input, .. }) = &state.overlay {
            assert!(
                input.is_empty(),
                "input field should be cleared after submit"
            );
        }
    }

    #[test]
    fn exec_pane_cancel_action_detaches_without_close_session() {
        let mut state = base_state();
        let id = SessionId(1);
        insert_session(&mut state, id);
        state.overlay = Some(Overlay::ExecPane {
            id,
            input: String::new(),
            scroll: 0,
            follow: true,
        });

        // Action::Cancel = Ctrl-] detach: closes overlay, no CloseSession effect.
        let effects = update(&mut state, Msg::Action(Action::Cancel));
        assert!(
            effects.is_empty(),
            "detach must not emit CloseSession: {effects:?}"
        );
        assert!(
            state.overlay.is_none(),
            "overlay should be closed after detach"
        );
        // Session record survives (the remote process is still running).
        assert!(
            state.sessions.contains_key(&id),
            "session record must survive detach"
        );
    }

    #[test]
    fn exec_pane_close_stdin_signals_eof_and_keeps_overlay_open() {
        let mut state = base_state();
        let id = SessionId(1);
        insert_session(&mut state, id);
        state.overlay = Some(Overlay::ExecPane {
            id,
            input: String::new(),
            scroll: 0,
            follow: true,
        });

        let effects = update(&mut state, Msg::Text(TextEdit::CloseStdin));
        // Overlay stays open to show remaining output; SessionEnded arrives when the process exits.
        assert!(
            state.overlay.is_some(),
            "overlay must stay open after CloseStdin (process may still be running)"
        );
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(&effects[0], AppEffect::CloseStdin { id: eid } if *eid == id),
            "expected CloseStdin effect (not CloseSession): {effects:?}"
        );
    }

    #[test]
    fn port_forward_status_cancel_emits_close_session() {
        let mut state = base_state();
        let id = SessionId(3);
        insert_session(&mut state, id);
        state.overlay = Some(Overlay::PortForwardStatus { id });

        let effects = update(&mut state, Msg::Action(Action::Cancel));
        assert!(state.overlay.is_none(), "overlay must close");
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(&effects[0], AppEffect::CloseSession { id: eid } if *eid == id),
            "expected CloseSession on PortForwardStatus cancel: {effects:?}"
        );
    }

    #[test]
    fn open_exec_session_mints_id_and_opens_overlay() {
        use cairn_types::{Entry, EntryKind};
        use std::sync::Arc;

        let mut state = base_state();
        // Populate the active pane with one entry so the action has a cursor target.
        state.panes[0].listing =
            Listing::Ready(Arc::new(vec![Entry::new("web-1", EntryKind::Dir)]));
        state.panes[0].cursor = 0;

        let effects = update(&mut state, Msg::Action(Action::OpenExecSession));

        // Exactly one effect: OpenExecSession with the minted id and default argv.
        assert_eq!(effects.len(), 1, "expected exactly one effect: {effects:?}");
        match &effects[0] {
            AppEffect::OpenExecSession { id, argv, .. } => {
                assert_eq!(*id, SessionId(1), "first session must have id 1");
                assert_eq!(argv, &["sh"], "default argv must be [\"sh\"]");
            }
            other => panic!("expected OpenExecSession, got {other:?}"),
        }

        // The overlay must be ExecPane with the new session id.
        assert!(
            matches!(&state.overlay, Some(Overlay::ExecPane { id, .. }) if *id == SessionId(1)),
            "overlay must be ExecPane(SessionId(1)), got {:?}",
            state.overlay,
        );

        // The session record must be inserted.
        assert!(
            state.sessions.contains_key(&SessionId(1)),
            "session record must be inserted into state.sessions"
        );

        // The id counter must have been advanced.
        assert_eq!(
            state.next_session_id,
            SessionId(2),
            "next_session_id must be incremented to 2"
        );
    }
}
