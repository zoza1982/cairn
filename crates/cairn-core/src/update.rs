//! The pure reducer: `update(&mut AppState, Msg) -> Vec<AppEffect>`. No I/O, no `.await`.

use crate::msg::{Action, AppEffect, AppEvent, Msg, TextEdit, WriteBackMode};
use crate::state::{
    ActiveTransfer, AppState, ChoiceStatus, ConnectionFormStage, ConnectionKind, FieldValue,
    FileKind, Listing, LogViewerStatus, MaskedInput, MountFrame, Overlay, PagerMode, PagerStatus,
    PromptKind, QueuedTransfer, SessionEnd, SessionRecord, Side, SortMode, TransferId,
    WritebackChoice, PAGER_HEX_ROW_BYTES, PAGER_MAX_BYTES, SESSION_OUTPUT_MAX_BYTES,
    SESSION_OUTPUT_MAX_LINES,
};
use bytes::Bytes;
use cairn_types::{ConnectionId, Entry, EntryKind, SessionId, VfsPath};
use std::collections::VecDeque;
use std::sync::Arc;

/// Minimum passphrase length for a new vault. Enforced in the pure reducer, before the effect
/// runner even sees the secret; a 1-character vault would be trivially brute-forced even with
/// Argon2id. 8 characters is the absolute floor; the UI hint says "at least 12 recommended".
/// Minimum character count for a new vault master passphrase.
///
/// 12 characters is a conservative floor for a master passphrase that guards every stored
/// credential. Argon2id provides additional strength through its KDF, but the passphrase
/// itself must exceed a trivial length threshold.
pub const VAULT_PASSPHRASE_MIN_LEN: usize = 12;

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
    let mut effects: Vec<AppEffect> = [Side::Left, Side::Right]
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
        .collect();
    // P5: detect OS credential sources at startup so the credential picker can default to the
    // most-likely-working method for each scheme. The result arrives as OsSourcesDetected.
    effects.push(AppEffect::DetectOsSources);
    effects
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
                | Action::NewConnection
                | Action::EditConnection
                | Action::DeleteConnection
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
                | Action::NewConnection
                | Action::EditConnection
                | Action::DeleteConnection
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
            // Never mark the synthetic `..` entry — it is a navigation affordance only and
            // must not be included in copy/move/delete selections.
            let on_dotdot = p.current().is_some_and(|e| e.is_dotdot_sentinel());
            if !on_dotdot && p.cursor < p.len() && !p.marked.remove(&p.cursor) {
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
                state.overlay = Some(Overlay::Connections {
                    cursor: 0,
                    show_hidden: false,
                });
            }
            Vec::new()
        }
        Action::VaultUnlock => {
            if state.vault_unlocked {
                state.status = Some("Vault already unlocked".to_owned());
            } else if state.vault_file_exists {
                // Vault file is present on disk — prompt for the passphrase to open it.
                state.overlay = Some(Overlay::VaultUnlock {
                    input: MaskedInput::new(),
                    error: None,
                    // Explicitly triggered by the user (Ctrl-U), not a NeedsVault selection.
                    pending_conn: None,
                    pending_save: None,
                });
            } else {
                // No vault file yet — guide the user through first-run creation.
                state.overlay = Some(Overlay::VaultCreate {
                    passphrase: MaskedInput::new(),
                    confirm: MaskedInput::new(),
                    focus: 0,
                    remember: false,
                    error: None,
                    creating: false,
                    pending_conn: None,
                    pending_save: None,
                });
            }
            Vec::new()
        }
        // `ToggleRemember` is emitted by `map_input` when Ctrl-R is pressed while the
        // `VaultCreate` overlay is open. When the overlay is open `apply_overlay_action` handles
        // it; this arm covers the (shouldn't-happen) path where no overlay is open.
        Action::ToggleRemember => Vec::new(),
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
            // The synthetic `..` sentinel is not a real entry — silently ignore Rename on it,
            // consistent with the no-op behaviour of ToggleMark, Copy, and Delete on `..`.
            if entry.is_dotdot_sentinel() {
                return Vec::new();
            }
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
        Action::View => open_pager_for_view(state),
        Action::Edit => start_edit(state),
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
                // Skip the synthetic `..` sentinel at position 0 (when present) so it always
                // stays first regardless of sort mode. Sort only the real entries via a slice,
                // avoiding two O(n) shifts from remove(0)/insert(0, ..).
                let offset = usize::from(v.first().is_some_and(|e| e.is_dotdot_sentinel()));
                sort_entries(&mut v[offset..], new_sort);
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
        Action::NewConnection => {
            // Open the scheme picker. This is the global handler (no overlay open).
            open_scheme_picker(state);
            Vec::new()
        }
        // EditConnection/DeleteConnection/TestConnection/PinConnection/HideConnection/
        // ToggleShowHidden are only meaningful inside the Connections overlay (RFC-0011 P4/P6).
        // Outside it they are silent no-ops so a misrouted key does not confuse the user.
        Action::EditConnection
        | Action::DeleteConnection
        | Action::TestConnection
        | Action::PinConnection
        | Action::HideConnection
        | Action::ToggleShowHidden => Vec::new(),
    }
}

/// Handle a text-editing keystroke. Routed to the open text prompt, or — if none — to the active
/// pane's live filter when it is being edited. A stray keystroke with neither active is a no-op.
fn apply_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    // ConnectionForm in Fields or CredentialFields stage captures text.
    // (SchemePicker and CredentialMethodPicker use action keys only.)
    if matches!(
        state.overlay,
        Some(Overlay::ConnectionForm {
            stage: ConnectionFormStage::Fields | ConnectionFormStage::CredentialFields,
            ..
        })
    ) {
        return apply_connection_form_text(state, edit);
    }
    if matches!(state.overlay, Some(Overlay::Prompt { .. })) {
        return apply_prompt_text(state, edit);
    }
    if matches!(state.overlay, Some(Overlay::VaultUnlock { .. })) {
        return apply_vault_unlock_text(state, edit);
    }
    if matches!(state.overlay, Some(Overlay::VaultCreate { .. })) {
        return apply_vault_create_text(state, edit);
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
    let Some(Overlay::VaultUnlock { input, error, .. }) = &mut state.overlay else {
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
            // If this overlay was triggered by a NeedsVault connection selection,
            // `pending_conn_open[side]` was set at that point but no OpenConnection was emitted.
            // Clear the matching per-side slot now — leaving it set would cause a future
            // ConnectionOpened (from a retry) to navigate the pane to the cancelled connection.
            if let Some(Overlay::VaultUnlock {
                pending_conn: Some(conn_id),
                ..
            }) = &state.overlay
            {
                let conn_id = *conn_id;
                for slot in &mut state.pending_conn_open {
                    if *slot == Some(conn_id) {
                        *slot = None;
                    }
                }
            }
            state.overlay = None;
            Vec::new()
        }
        TextEdit::Submit => submit_vault_unlock(state),
        // `CloseStdin`/`NextField`/`PrevField` are only meaningful inside `ExecPane`/`ConnectionForm`.
        TextEdit::CloseStdin | TextEdit::NextField | TextEdit::PrevField => Vec::new(),
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
    let Some(Overlay::VaultUnlock { input, error, .. }) = &mut state.overlay else {
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

/// Edit the vault-create passphrase or confirm field.
///
/// Both fields are [`MaskedInput`] — no-echo, redacted in `Debug`, zeroized on drop. The focused
/// field (0 = passphrase, 1 = confirm) receives character input; `Tab`/`Shift-Tab` cycle focus.
/// The raw bytes NEVER leave the `MaskedInput` except via `take_secret()` inside
/// `submit_vault_create`, and even then only into a `SecretString` that is immediately handed
/// to [`AppEffect::CreateVault`].
fn apply_vault_create_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    let Some(Overlay::VaultCreate {
        passphrase,
        confirm,
        focus,
        error,
        ..
    }) = &mut state.overlay
    else {
        return Vec::new();
    };
    match edit {
        TextEdit::Insert(c) => {
            // Reject control characters; passphrase and confirm accept anything else.
            if !c.is_control() {
                if *focus == 0 {
                    passphrase.push(c);
                } else {
                    confirm.push(c);
                }
                // Any new input dismisses a stale mismatch/length error.
                *error = None;
            }
            Vec::new()
        }
        TextEdit::Backspace => {
            if *focus == 0 {
                passphrase.backspace();
            } else {
                confirm.backspace();
            }
            Vec::new()
        }
        // Tab / Shift-Tab both wrap between the two fields (there are only two, so Next and Prev
        // are symmetric: toggle). Using `NextField`/`PrevField` keeps the text-edit API
        // symmetric with `ConnectionForm`.
        TextEdit::NextField | TextEdit::PrevField => {
            *focus = 1 - *focus;
            Vec::new()
        }
        // Esc cancels the overlay (both fields zeroize on drop). Mirror VaultUnlock: clear any
        // `pending_conn_open` slot so a future ConnectionOpened doesn't navigate the pane.
        TextEdit::Cancel => {
            if let Some(Overlay::VaultCreate {
                pending_conn: Some(conn_id),
                ..
            }) = &state.overlay
            {
                let conn_id = *conn_id;
                for slot in &mut state.pending_conn_open {
                    if *slot == Some(conn_id) {
                        *slot = None;
                    }
                }
            }
            state.overlay = None;
            Vec::new()
        }
        TextEdit::Submit => submit_vault_create(state),
        // `CloseStdin` is only meaningful in ExecPane.
        TextEdit::CloseStdin => Vec::new(),
    }
}

/// Validate and submit the two passphrase fields: emit [`AppEffect::CreateVault`] or set an
/// in-place error without emitting an effect.
///
/// Security invariants:
/// - Lengths are compared first (no bytes exposed, O(1)).
/// - Only when lengths match are both secrets taken (wiping both fields) and compared via
///   `expose_secret`. If content mismatches, both `SecretString` values drop immediately
///   (zeroized before the function returns).
/// - On success only the passphrase `SecretString` is carried in the effect; the confirm
///   `SecretString` is always dropped before the effect is returned.
/// - An empty or too-short passphrase is rejected before any `take_secret` call.
///
/// Explicit scoping blocks (`{ … }`) end each mutable borrow of `state.overlay` so that
/// later phases can re-borrow it cleanly — the NLL borrow checker requires this when the
/// same field is borrowed, mutated, and then borrowed again in sequence.
fn submit_vault_create(state: &mut AppState) -> Vec<AppEffect> {
    use cairn_secrets::ExposeSecret as _;

    // Phase 1: guard against a duplicate submit while Argon2id is already running.
    if matches!(
        &state.overlay,
        Some(Overlay::VaultCreate { creating: true, .. })
    ) {
        return Vec::new();
    }

    // Phase 2: validation (lengths only — no bytes exposed). Each early return ends the borrow.
    {
        let Some(Overlay::VaultCreate {
            passphrase,
            confirm,
            error,
            ..
        }) = &mut state.overlay
        else {
            return Vec::new();
        };
        if passphrase.is_empty() {
            *error = Some("Enter a passphrase".to_owned());
            return Vec::new();
        }
        if passphrase.len() < VAULT_PASSPHRASE_MIN_LEN {
            *error = Some(format!(
                "Passphrase must be at least {VAULT_PASSPHRASE_MIN_LEN} characters"
            ));
            return Vec::new();
        }
        if confirm.is_empty() {
            *error = Some("Confirm the passphrase".to_owned());
            return Vec::new();
        }
        if passphrase.len() != confirm.len() {
            // Clear confirm so the user types it fresh; leave passphrase intact for a quicker retry.
            confirm.clear();
            *error = Some("Passphrases do not match".to_owned());
            return Vec::new();
        }
    } // ← mutable borrow ends; state.overlay is free again

    // Phase 3: take both secrets (wipes both MaskedInput buffers) and compare content.
    // The borrow ends when this block closes; `pp` and `cf` own their heap allocations.
    let (pp, cf) = {
        let Some(Overlay::VaultCreate {
            passphrase,
            confirm,
            ..
        }) = &mut state.overlay
        else {
            return Vec::new();
        };
        (passphrase.take_secret(), confirm.take_secret())
    }; // ← borrow ends

    if pp.expose_secret() != cf.expose_secret() {
        // Both values are zeroized: `cf` explicitly here, `pp` at the early return below.
        // Focus resets to the passphrase field so the user knows to re-enter both.
        drop(cf);
        if let Some(Overlay::VaultCreate { error, focus, .. }) = &mut state.overlay {
            *error = Some("Passphrases do not match — re-enter both.".to_owned());
            *focus = 0;
        }
        return Vec::new(); // `pp` zeroized here
    }
    // Confirm matched; zeroize it immediately — only `pp` proceeds.
    drop(cf);

    // Phase 4: set the in-flight flag and read `remember` in one re-borrow.
    let remember = match &mut state.overlay {
        Some(Overlay::VaultCreate {
            remember, creating, ..
        }) => {
            let r = *remember;
            *creating = true;
            r
        }
        _ => return Vec::new(),
    };

    state.status = Some("Creating vault…".to_owned());
    vec![AppEffect::CreateVault {
        passphrase: pp,
        remember,
    }]
}

/// Route text-editing keystrokes to the exec pane's input field.
///
/// `Enter` submits the current line (appending `\n`) as a [`AppEffect::SendSessionInput`]; `Esc`
/// clears the field (to cancel in-progress typing, not to close the overlay — that is `Ctrl-]`,
/// mapped to `Action::Cancel` upstream). `CloseStdin` (`Ctrl-D`) closes the session's stdin.
fn apply_exec_pane_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    // Read the session id with a shared borrow that ends here.
    let session_id = match &state.overlay {
        Some(Overlay::ExecPane { id, .. }) => *id,
        _ => return Vec::new(),
    };

    match edit {
        TextEdit::Insert(c) => {
            if !c.is_control() {
                if let Some(Overlay::ExecPane { input, .. }) = &mut state.overlay {
                    input.push(c);
                }
            }
            Vec::new()
        }
        TextEdit::Backspace => {
            if let Some(Overlay::ExecPane { input, .. }) = &mut state.overlay {
                input.pop();
            }
            Vec::new()
        }
        // Esc clears the field (doesn't close the overlay — Ctrl-] closes it via Action::Cancel).
        TextEdit::Cancel => {
            if let Some(Overlay::ExecPane { input, .. }) = &mut state.overlay {
                input.clear();
            }
            Vec::new()
        }
        TextEdit::Submit => {
            // Take the input line from the overlay (short-lived mutable borrow).
            let line = match &mut state.overlay {
                Some(Overlay::ExecPane { input, .. }) => std::mem::take(input),
                _ => return Vec::new(),
            };
            if line.is_empty() {
                return Vec::new();
            }
            // Do not emit input to a session that has already ended — the process is gone
            // and the bytes would be silently discarded by the relay task anyway.
            if state
                .sessions
                .get(&session_id)
                .is_none_or(|r| r.ended.is_some())
            {
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
            // Signal EOF to the remote process by dropping stdin — the process may exit
            // on EOF (e.g. a shell's `exit`). Unlike `CloseSession`, the overlay stays open
            // to show remaining output; `SessionEnded` arrives when the process exits.
            vec![AppEffect::CloseStdin { id: session_id }]
        }
        // `NextField`/`PrevField` are only meaningful inside `ConnectionForm`.
        TextEdit::NextField | TextEdit::PrevField => Vec::new(),
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
        // `CloseStdin`/`NextField`/`PrevField` are only meaningful inside `ExecPane`/`ConnectionForm`.
        TextEdit::CloseStdin | TextEdit::NextField | TextEdit::PrevField => Vec::new(),
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
        // `CloseStdin`/`NextField`/`PrevField` are only meaningful inside `ExecPane`/`ConnectionForm`.
        TextEdit::CloseStdin | TextEdit::NextField | TextEdit::PrevField => {}
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

/// Open the scheme-picker stage of the connection form, replacing any current overlay.
fn open_scheme_picker(state: &mut AppState) {
    state.overlay = Some(Overlay::ConnectionForm {
        stage: ConnectionFormStage::SchemePicker,
        scheme: String::new(),
        values: std::collections::HashMap::new(),
        focus: 0,
        field_errors: std::collections::HashMap::new(),
        editing_id: None,
        existing_secret_ref: None,
        cred_method_cursor: 0,
        cred_method: None,
        cred_fields: std::collections::HashMap::new(),
        cred_focus: 0,
    });
}

/// Drive the connection form overlay — both the `SchemePicker` and `Fields` stages.
///
/// Text input in the `Fields` stage is handled separately by [`apply_connection_form_text`] (routed
/// via [`apply_text`]); this function receives only action keys (cursor, enter, cancel, leave).
fn apply_connection_form_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    use crate::forms::KNOWN_SCHEMES;

    // Extract the current stage so we can decide routing without holding the borrow.
    let stage = match &state.overlay {
        Some(Overlay::ConnectionForm { stage, .. }) => stage.clone(),
        _ => return Vec::new(),
    };

    match stage {
        ConnectionFormStage::SchemePicker => {
            let n = KNOWN_SCHEMES.len();
            match action {
                Action::CursorUp => {
                    if let Some(Overlay::ConnectionForm { focus, .. }) = &mut state.overlay {
                        *focus = focus.saturating_sub(1);
                    }
                    Vec::new()
                }
                Action::CursorDown => {
                    if let Some(Overlay::ConnectionForm { focus, .. }) = &mut state.overlay {
                        if *focus + 1 < n {
                            *focus += 1;
                        }
                    }
                    Vec::new()
                }
                Action::Confirm | Action::Enter => {
                    // Commit the chosen scheme and advance to Fields.
                    let chosen = match &state.overlay {
                        Some(Overlay::ConnectionForm { focus, .. }) => KNOWN_SCHEMES
                            .get(*focus)
                            .map(|(s, _)| s.to_string())
                            .unwrap_or_default(),
                        _ => return Vec::new(),
                    };
                    // Prune stale values from a previous scheme (preserves display_name which
                    // every scheme shares). This prevents a host="example.com" typed in an SSH
                    // form from silently appearing in a subsequently selected S3 form.
                    let new_field_keys: std::collections::HashSet<&str> =
                        crate::forms::scheme_fields(&chosen)
                            .iter()
                            .map(|f| f.key)
                            .collect();
                    if let Some(Overlay::ConnectionForm {
                        stage,
                        scheme,
                        focus,
                        values,
                        field_errors,
                        ..
                    }) = &mut state.overlay
                    {
                        *scheme = chosen;
                        *stage = ConnectionFormStage::Fields;
                        *focus = 0;
                        values.retain(|k, _| new_field_keys.contains(k.as_str()));
                        field_errors.retain(|k, _| new_field_keys.contains(k.as_str()));
                    }
                    Vec::new()
                }
                Action::Cancel => {
                    state.overlay = None;
                    Vec::new()
                }
                _ => Vec::new(),
            }
        }
        ConnectionFormStage::Fields => {
            // In the Fields stage, text capture (`capturing_text` returns `true`) routes
            // Up/Down/Esc through `apply_connection_form_text` as `TextEdit` messages.
            // The `Confirm | Enter` arm below is therefore unreachable while capturing_text()
            // is true in Fields stage; kept for forward-compatibility if capturing_text() logic
            // changes in the future.
            match action {
                Action::Confirm | Action::Enter => submit_connection_form(state),
                Action::Cancel => {
                    state.overlay = None;
                    Vec::new()
                }
                _ => Vec::new(),
            }
        }
        ConnectionFormStage::CredentialMethodPicker => {
            apply_credential_method_picker_action(state, action)
        }
        ConnectionFormStage::CredentialFields => {
            // In the CredentialFields stage, text capture routes keystrokes via
            // `apply_connection_form_text`; only Confirm/Enter/Cancel reach here.
            match action {
                Action::Confirm | Action::Enter => submit_credential_draft(state),
                Action::Cancel => {
                    // Esc goes back to the method picker.
                    if let Some(Overlay::ConnectionForm {
                        stage, cred_focus, ..
                    }) = &mut state.overlay
                    {
                        *stage = ConnectionFormStage::CredentialMethodPicker;
                        *cred_focus = 0;
                    }
                    Vec::new()
                }
                _ => Vec::new(),
            }
        }
    }
}

/// Handle navigation in the credential method picker (P5).
///
/// Up/Down move the cursor; Enter/Confirm selects the method and advances; Cancel goes back.
fn apply_credential_method_picker_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    use crate::forms::{credential_method_fields, credential_methods};

    let (scheme_clone, editing_id) = match &state.overlay {
        Some(Overlay::ConnectionForm {
            scheme, editing_id, ..
        }) => (scheme.clone(), *editing_id),
        _ => return Vec::new(),
    };

    let is_edit = editing_id.is_some();
    let methods = credential_methods(&scheme_clone, is_edit);
    let n = methods.len();

    match action {
        Action::CursorUp => {
            if let Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) = &mut state.overlay
            {
                *cred_method_cursor = cred_method_cursor.saturating_sub(1);
            }
            Vec::new()
        }
        Action::CursorDown => {
            if let Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) = &mut state.overlay
            {
                if *cred_method_cursor + 1 < n {
                    *cred_method_cursor += 1;
                }
            }
            Vec::new()
        }
        Action::Confirm | Action::Enter => {
            // Read the chosen cursor position, then commit the method.
            let cursor = match &state.overlay {
                Some(Overlay::ConnectionForm {
                    cred_method_cursor, ..
                }) => *cred_method_cursor,
                _ => return Vec::new(),
            };

            let Some(chosen) = methods.get(cursor).cloned() else {
                return Vec::new();
            };

            // Delegation methods and KeepExisting have no fields — go straight to submit.
            if chosen.is_delegation() {
                // Commit the method and immediately submit.
                if let Some(Overlay::ConnectionForm {
                    cred_method,
                    cred_fields,
                    cred_focus,
                    ..
                }) = &mut state.overlay
                {
                    *cred_method = Some(chosen);
                    cred_fields.clear();
                    *cred_focus = 0;
                }
                return submit_credential_draft(state);
            }

            // Deferred methods (GcpServiceAccountJson, AzureSharedKey, …): commit the method
            // and immediately submit — the profile is saved without vault credentials; the
            // backend will prompt or fail on first open.
            if chosen.is_field_capture_deferred() {
                if let Some(Overlay::ConnectionForm { cred_method, .. }) = &mut state.overlay {
                    *cred_method = Some(chosen);
                }
                return submit_credential_draft(state);
            }

            // Field-bearing methods: advance to CredentialFields.
            if let Some(Overlay::ConnectionForm {
                stage,
                cred_method,
                cred_fields,
                cred_focus,
                ..
            }) = &mut state.overlay
            {
                *stage = ConnectionFormStage::CredentialFields;
                *cred_method = Some(chosen.clone());
                cred_fields.clear();
                // Pre-initialise field slots with the right FieldValue variant so the
                // renderer can distinguish masked from plain without consulting FieldSpec again.
                // Use `chosen` directly — never `cred_method.as_ref().unwrap()` — to avoid
                // an unwrap on a user-input path (CLAUDE.md §9).
                for spec in credential_method_fields(&chosen) {
                    cred_fields.entry(spec.key.to_owned()).or_insert_with(|| {
                        if spec.secret {
                            FieldValue::Secret(MaskedInput::new())
                        } else {
                            FieldValue::Plain(String::new())
                        }
                    });
                }
                *cred_focus = 0;
            }
            Vec::new()
        }
        Action::Cancel => {
            // Go back to the endpoint Fields stage.
            if let Some(Overlay::ConnectionForm { stage, focus, .. }) = &mut state.overlay {
                *stage = ConnectionFormStage::Fields;
                *focus = 0;
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Validate and submit the credential draft, assembling and emitting
/// [`AppEffect::ProvisionAndSaveConnection`].
///
/// Builds the [`ProfileData`](crate::forms::ProfileData) from the current endpoint fields,
/// assembles a [`CredentialDraft`](crate::forms::CredentialDraft) from `cred_fields`, and either:
/// - emits `ProvisionAndSaveConnection` directly (vault unlocked), or
/// - transitions to the `VaultUnlock` / `VaultCreate` overlay with a `pending_save`.
fn submit_credential_draft(state: &mut AppState) -> Vec<AppEffect> {
    use crate::forms::{credential_method_fields, CredentialMethod, ProfileData};

    if state.connection_saving {
        return Vec::new();
    }

    // Extract everything from the overlay in a shared-borrow pass.
    let (scheme, values, editing_id, existing_secret_ref, method, _cred_focus) =
        match &state.overlay {
            Some(Overlay::ConnectionForm {
                scheme,
                values,
                editing_id,
                existing_secret_ref,
                cred_method: Some(method),
                cred_focus,
                ..
            }) => (
                scheme.clone(),
                values.clone(),
                *editing_id,
                *existing_secret_ref,
                method.clone(),
                *cred_focus,
            ),
            _ => return Vec::new(),
        };

    // Validate credential fields for non-delegation, non-deferred methods.
    if !method.is_delegation() && !method.is_field_capture_deferred() {
        let fields = credential_method_fields(&method);
        // Borrow immutably — do NOT clone! MaskedInput::Clone returns an empty field
        // (by design, to prevent silent secret duplication), so cloning cred_fields
        // would make every Secret field appear empty and always fail validation.
        let mut has_error = false;
        let mut first_err_idx = None;
        if let Some(Overlay::ConnectionForm { cred_fields, .. }) = &state.overlay {
            for (i, spec) in fields.iter().enumerate() {
                if spec.required {
                    // For plain fields, trim whitespace before the emptiness check so that a
                    // field containing only spaces is treated as empty. Secret fields are not
                    // trimmed — leading/trailing whitespace in a passphrase is intentional.
                    let empty = cred_fields.get(spec.key).is_none_or(|fv| match fv {
                        FieldValue::Plain(s) => s.trim().is_empty(),
                        FieldValue::Secret(m) => m.is_empty(),
                    });
                    if empty && first_err_idx.is_none() {
                        first_err_idx = Some(i);
                        has_error = true;
                    }
                }
            }
        }
        if has_error {
            if let Some(Overlay::ConnectionForm { cred_focus, .. }) = &mut state.overlay {
                if let Some(idx) = first_err_idx {
                    *cred_focus = idx;
                }
            }
            state.status = Some("Please fill in all required credential fields".to_owned());
            return Vec::new();
        }
    }

    // Build the profile.
    let display_name = values.get("display_name").cloned().unwrap_or_default();
    let display_name = display_name.trim().to_owned();
    let mut endpoint = std::collections::BTreeMap::new();
    for (k, v) in &values {
        if k != "display_name" && !v.trim().is_empty() {
            endpoint.insert(k.clone(), v.trim().to_owned());
        }
    }
    let id = editing_id.unwrap_or_else(uuid::Uuid::new_v4);
    let is_edit = editing_id.is_some();
    let profile = ProfileData {
        id,
        scheme: scheme.clone(),
        display_name: display_name.clone(),
        endpoint,
        secret_ref: existing_secret_ref,
    };

    // Assemble the CredentialDraft. Secret fields are taken from `cred_fields` via
    // `MaskedInput::take_secret`, which zeroizes the field buffer.
    let draft = assemble_draft(state, &method);

    // Gate on vault availability: if the vault is locked (or absent), push the save into a
    // pending_save on the unlock/create overlay so it completes automatically after unlock.
    //
    // All methods store a vault entry — even delegation methods store a non-secret marker
    // (e.g. `SshCredential::Agent`) so the connect layer can look up which OS source to use.
    // Excludes:
    //   KeepExisting — preserves the existing secret_ref without a new vault write.
    //   is_field_capture_deferred() — deferred methods save the profile without any vault credential.
    if !state.vault_unlocked
        && !matches!(method, CredentialMethod::KeepExisting)
        && !method.is_field_capture_deferred()
    {
        let pending = Box::new(crate::state::PendingSave {
            profile,
            draft,
            is_edit,
        });
        if state.vault_file_exists {
            state.overlay = Some(Overlay::VaultUnlock {
                input: MaskedInput::new(),
                error: None,
                pending_conn: None,
                pending_save: Some(pending),
            });
        } else {
            state.overlay = Some(Overlay::VaultCreate {
                passphrase: MaskedInput::new(),
                confirm: MaskedInput::new(),
                focus: 0,
                remember: false,
                error: None,
                creating: false,
                pending_conn: None,
                pending_save: Some(pending),
            });
        }
        state.status = Some(
            "Unlock (or create) the vault to store credentials, then the connection will be saved automatically".to_owned(),
        );
        return Vec::new();
    }

    // Vault available (or keep-existing/deferred): emit the effect.
    state.connection_saving = true;
    state.status = Some(if is_edit {
        format!("Saving '{display_name}'…")
    } else {
        format!("Adding '{display_name}'…")
    });
    // Close the form now; ConnectionSaved will confirm.
    state.overlay = None;

    vec![AppEffect::ProvisionAndSaveConnection {
        profile,
        draft,
        is_edit,
    }]
}

/// Assemble a [`CredentialDraft`](crate::forms::CredentialDraft) by taking secret values from the
/// `cred_fields` map in the overlay. This drains the `MaskedInput` buffers (zeroizing them).
fn assemble_draft(
    state: &mut AppState,
    method: &crate::forms::CredentialMethod,
) -> crate::forms::CredentialDraft {
    use crate::forms::CredentialDraft;
    use crate::state::FieldValue;

    // Helper: take a secret from the cred_fields map and return it as a SecretString.
    // If the key is absent (e.g. optional field left empty), returns None.
    macro_rules! take_secret {
        ($fields:expr, $key:expr) => {
            match $fields.get_mut($key) {
                Some(FieldValue::Secret(m)) => {
                    if m.is_empty() {
                        None
                    } else {
                        Some(m.take_secret())
                    }
                }
                _ => None,
            }
        };
    }
    macro_rules! take_secret_required {
        ($fields:expr, $key:expr) => {
            match $fields.get_mut($key) {
                Some(FieldValue::Secret(m)) => m.take_secret(),
                _ => cairn_secrets::SecretString::from(String::new()),
            }
        };
    }
    macro_rules! take_plain {
        ($fields:expr, $key:expr) => {
            match $fields.get($key) {
                Some(FieldValue::Plain(s)) => s.trim().to_owned(),
                _ => String::new(),
            }
        };
    }

    let cred_fields = match &mut state.overlay {
        Some(Overlay::ConnectionForm { cred_fields, .. }) => cred_fields,
        _ => {
            debug_assert!(
                false,
                "assemble_draft called with non-ConnectionForm overlay"
            );
            return CredentialDraft::KeepExisting;
        }
    };

    match method {
        crate::forms::CredentialMethod::SshAgent => CredentialDraft::SshAgent,
        crate::forms::CredentialMethod::SshPrivateKeyFile => CredentialDraft::SshPrivateKeyFile {
            path: take_plain!(cred_fields, "cred_path"),
            passphrase: take_secret!(cred_fields, "cred_passphrase"),
        },
        crate::forms::CredentialMethod::SshInlinePem => CredentialDraft::SshInlinePem {
            key_pem: take_secret_required!(cred_fields, "cred_key_pem"),
            passphrase: take_secret!(cred_fields, "cred_passphrase"),
        },
        crate::forms::CredentialMethod::SshPassword => CredentialDraft::SshPassword {
            password: take_secret_required!(cred_fields, "cred_password"),
        },
        crate::forms::CredentialMethod::AwsDefaultChain => CredentialDraft::AwsDefaultChain,
        crate::forms::CredentialMethod::AwsProfile => CredentialDraft::AwsProfile {
            profile_name: take_plain!(cred_fields, "cred_profile_name"),
        },
        crate::forms::CredentialMethod::AwsStatic => CredentialDraft::AwsStatic {
            access_key_id: take_plain!(cred_fields, "cred_access_key_id"),
            secret_access_key: take_secret_required!(cred_fields, "cred_secret_access_key"),
            session_token: take_secret!(cred_fields, "cred_session_token"),
        },
        crate::forms::CredentialMethod::GcpApplicationDefault => {
            CredentialDraft::GcpApplicationDefault
        }
        crate::forms::CredentialMethod::GcpServiceAccountJson => {
            // Deferred: field capture not yet implemented. The binary edge maps this to
            // `None` (no vault op), so the profile is saved without a `secret_ref`; the
            // backend will prompt or fail on first open.
            CredentialDraft::GcpServiceAccountJson {
                json: cairn_secrets::SecretString::from(String::new()),
            }
        }
        crate::forms::CredentialMethod::AzureAd => CredentialDraft::AzureAd,
        crate::forms::CredentialMethod::AzureSharedKey => {
            // Deferred: saved without vault credential; backend prompts on first open.
            CredentialDraft::AzureSharedKey {
                account: String::new(),
                key: cairn_secrets::SecretString::from(String::new()),
            }
        }
        crate::forms::CredentialMethod::AzureSasToken => {
            // Deferred: saved without vault credential; backend prompts on first open.
            CredentialDraft::AzureSasToken {
                token: cairn_secrets::SecretString::from(String::new()),
            }
        }
        crate::forms::CredentialMethod::AzureConnectionString => {
            // Deferred: saved without vault credential; backend prompts on first open.
            CredentialDraft::AzureConnectionString {
                connection_string: cairn_secrets::SecretString::from(String::new()),
            }
        }
        crate::forms::CredentialMethod::KeepExisting => CredentialDraft::KeepExisting,
    }
}

/// Handle text-editing keystrokes while the connection form's `Fields` or `CredentialFields`
/// stage is active.
fn apply_connection_form_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    // Dispatch to the right sub-handler based on the current stage.
    let stage = match &state.overlay {
        Some(Overlay::ConnectionForm { stage, .. }) => stage.clone(),
        _ => return Vec::new(),
    };
    match stage {
        ConnectionFormStage::Fields => apply_endpoint_fields_text(state, edit),
        ConnectionFormStage::CredentialFields => apply_credential_fields_text(state, edit),
        // SchemePicker and CredentialMethodPicker don't capture text.
        _ => Vec::new(),
    }
}

/// Text handler for the endpoint `Fields` stage.
fn apply_endpoint_fields_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    use crate::forms::scheme_fields;

    // Extract what we need with a shared borrow first (scheme, focus, n_fields).
    let (scheme_clone, focus_val, n_fields) = match &state.overlay {
        Some(Overlay::ConnectionForm {
            stage: ConnectionFormStage::Fields,
            scheme,
            focus,
            ..
        }) => {
            let fields = scheme_fields(scheme);
            (scheme.clone(), *focus, fields.len())
        }
        _ => return Vec::new(),
    };

    match edit {
        TextEdit::Insert(c) => {
            if !c.is_control() {
                // Get the field key for the current focus position.
                let key = crate::forms::scheme_fields(&scheme_clone)
                    .get(focus_val)
                    .map(|f| f.key.to_owned());
                if let (
                    Some(key),
                    Some(Overlay::ConnectionForm {
                        values,
                        field_errors,
                        ..
                    }),
                ) = (key, &mut state.overlay)
                {
                    // Typing clears any existing error for this field.
                    field_errors.remove(&key);
                    values.entry(key).or_default().push(c);
                }
            }
            Vec::new()
        }
        TextEdit::Backspace => {
            let key = crate::forms::scheme_fields(&scheme_clone)
                .get(focus_val)
                .map(|f| f.key.to_owned());
            if let (Some(key), Some(Overlay::ConnectionForm { values, .. })) =
                (key, &mut state.overlay)
            {
                if let Some(v) = values.get_mut(&key) {
                    v.pop();
                }
            }
            Vec::new()
        }
        TextEdit::NextField => {
            if let Some(Overlay::ConnectionForm { focus, .. }) = &mut state.overlay {
                if *focus + 1 < n_fields {
                    *focus += 1;
                }
            }
            Vec::new()
        }
        TextEdit::PrevField => {
            if let Some(Overlay::ConnectionForm { focus, .. }) = &mut state.overlay {
                *focus = focus.saturating_sub(1);
            }
            Vec::new()
        }
        TextEdit::Submit => submit_connection_form(state),
        TextEdit::Cancel => {
            // For new connections in the Fields stage: Esc goes back to the scheme picker instead
            // of closing the form entirely. For edits (editing_id is Some), Esc closes the form.
            match &mut state.overlay {
                Some(Overlay::ConnectionForm {
                    stage,
                    focus,
                    editing_id: None,
                    ..
                }) if *stage == ConnectionFormStage::Fields => {
                    *stage = ConnectionFormStage::SchemePicker;
                    *focus = 0;
                    return Vec::new();
                }
                _ => {}
            }
            state.overlay = None;
            Vec::new()
        }
        // `CloseStdin` is only meaningful inside `ExecPane`; ignore it here.
        TextEdit::CloseStdin => Vec::new(),
    }
}

/// Text handler for the credential `CredentialFields` stage.
///
/// Like [`apply_endpoint_fields_text`] but operates on `cred_fields` (a `HashMap<String, FieldValue>`)
/// and uses [`credential_method_fields`](crate::forms::credential_method_fields) for the field list.
fn apply_credential_fields_text(state: &mut AppState, edit: TextEdit) -> Vec<AppEffect> {
    use crate::forms::credential_method_fields;

    // Extract what we need in a shared-borrow pass.
    let (method_clone, cred_focus_val, n_fields) = match &state.overlay {
        Some(Overlay::ConnectionForm {
            stage: ConnectionFormStage::CredentialFields,
            cred_method: Some(method),
            cred_focus,
            ..
        }) => {
            let fields = credential_method_fields(method);
            (method.clone(), *cred_focus, fields.len())
        }
        _ => return Vec::new(),
    };

    match edit {
        TextEdit::Insert(c) => {
            if !c.is_control() {
                let field_spec = credential_method_fields(&method_clone).get(cred_focus_val);
                if let (Some(spec), Some(Overlay::ConnectionForm { cred_fields, .. })) =
                    (field_spec, &mut state.overlay)
                {
                    let key = spec.key.to_owned();
                    let is_secret = spec.secret;
                    let fv = cred_fields.entry(key).or_insert_with(|| {
                        if is_secret {
                            FieldValue::Secret(MaskedInput::new())
                        } else {
                            FieldValue::Plain(String::new())
                        }
                    });
                    fv.push_char(c);
                }
            }
            Vec::new()
        }
        TextEdit::Backspace => {
            let field_spec = credential_method_fields(&method_clone).get(cred_focus_val);
            if let (Some(spec), Some(Overlay::ConnectionForm { cred_fields, .. })) =
                (field_spec, &mut state.overlay)
            {
                if let Some(fv) = cred_fields.get_mut(spec.key) {
                    fv.backspace();
                }
            }
            Vec::new()
        }
        TextEdit::NextField => {
            if let Some(Overlay::ConnectionForm { cred_focus, .. }) = &mut state.overlay {
                if *cred_focus + 1 < n_fields {
                    *cred_focus += 1;
                }
            }
            Vec::new()
        }
        TextEdit::PrevField => {
            if let Some(Overlay::ConnectionForm { cred_focus, .. }) = &mut state.overlay {
                *cred_focus = cred_focus.saturating_sub(1);
            }
            Vec::new()
        }
        TextEdit::Submit => submit_credential_draft(state),
        TextEdit::Cancel => {
            // Esc in credential fields goes back to the method picker.
            if let Some(Overlay::ConnectionForm {
                stage, cred_focus, ..
            }) = &mut state.overlay
            {
                *stage = ConnectionFormStage::CredentialMethodPicker;
                *cred_focus = 0;
            }
            Vec::new()
        }
        TextEdit::CloseStdin => Vec::new(),
    }
}

/// Validate and submit the connection form, emitting a [`AppEffect::SaveConnection`] on success.
///
/// Validates that all required fields are non-empty. On failure: sets per-field errors and a
/// status message, leaving the form open. On success: closes the overlay, sets `connection_saving`,
/// and emits the save effect.
fn submit_connection_form(state: &mut AppState) -> Vec<AppEffect> {
    use crate::forms::{scheme_fields, scheme_needs_credentials, ProfileData};

    if state.connection_saving {
        return Vec::new();
    }

    // Extract everything we need in a shared-borrow pass.
    let (scheme, values, editing_id, existing_secret_ref) = match &state.overlay {
        Some(Overlay::ConnectionForm {
            scheme,
            values,
            editing_id,
            existing_secret_ref,
            ..
        }) => (
            scheme.clone(),
            values.clone(),
            *editing_id,
            *existing_secret_ref,
        ),
        _ => return Vec::new(),
    };

    let fields = scheme_fields(&scheme);

    // Validate: collect errors for all required fields that are empty.
    let mut errors: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for field in fields {
        if field.required {
            let is_empty = values.get(field.key).is_none_or(|v| v.trim().is_empty());
            if is_empty {
                errors.insert(field.key.to_owned(), format!("{} is required", field.label));
            }
        }
    }

    if !errors.is_empty() {
        // Compute the first-error index before consuming `errors` (avoids a clone).
        let first_err_idx = fields.iter().position(|f| errors.contains_key(f.key));
        // Put errors back into the overlay and surface a generic status hint.
        if let Some(Overlay::ConnectionForm {
            field_errors,
            focus,
            ..
        }) = &mut state.overlay
        {
            *field_errors = errors; // move, not clone
            if let Some(i) = first_err_idx {
                *focus = i;
            }
        }
        state.status = Some("Please fill in all required fields".to_owned());
        return Vec::new();
    }

    // P5: if the scheme needs credentials, advance to the credential method picker
    // rather than saving immediately. The profile data is assembled the same way as
    // a direct save but the overlay stage transitions instead of emitting SaveConnection.
    if scheme_needs_credentials(&scheme) {
        let is_edit = editing_id.is_some();
        let cursor = crate::forms::default_credential_cursor(&scheme, &state.os_sources, is_edit);
        if let Some(Overlay::ConnectionForm {
            stage,
            cred_method_cursor,
            cred_method,
            cred_fields,
            cred_focus,
            ..
        }) = &mut state.overlay
        {
            *stage = ConnectionFormStage::CredentialMethodPicker;
            *cred_method_cursor = cursor;
            *cred_method = None;
            cred_fields.clear();
            *cred_focus = 0;
        }
        return Vec::new();
    }

    let display_name = values.get("display_name").cloned().unwrap_or_default();
    let display_name = display_name.trim().to_owned();

    // Build the endpoint map (everything except display_name, which lives at the top level).
    // HashMap for O(1) lookup during live field editing; ProfileData uses BTreeMap for
    // deterministic serialisation order.
    let mut endpoint = std::collections::BTreeMap::new();
    for (k, v) in &values {
        if k != "display_name" && !v.trim().is_empty() {
            endpoint.insert(k.clone(), v.trim().to_owned());
        }
    }

    let id = editing_id.unwrap_or_else(uuid::Uuid::new_v4);
    let is_edit = editing_id.is_some();

    let profile = ProfileData {
        id,
        scheme,
        display_name: display_name.clone(),
        endpoint,
        secret_ref: existing_secret_ref,
    };

    // Do NOT close the overlay here. The form stays open in "Saving…" state so that a
    // failed save (ConnectionOpFailed) can keep the user's values intact for retry.
    // The overlay is closed only on ConnectionSaved (success) or Cancel (user abort).
    state.connection_saving = true;
    state.status = Some(if is_edit {
        format!("Saving '{display_name}'…")
    } else {
        format!("Adding '{display_name}'…")
    });

    vec![AppEffect::SaveConnection { profile, is_edit }]
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
        Some(Overlay::ConfirmWriteback { .. }) => apply_confirm_writeback_action(state, action),
        Some(Overlay::ConfirmShellAction { .. }) => apply_confirm_shell_action(state, action),
        Some(Overlay::ConfirmDeleteConnection { .. }) => {
            apply_confirm_delete_connection_action(state, action)
        }
        Some(Overlay::AiPlan { .. }) => apply_ai_plan_action(state, action),
        Some(Overlay::Connections { .. }) => apply_connections_action(state, action),
        Some(Overlay::TransferQueue { .. }) => apply_transfer_queue_action(state, action),
        Some(Overlay::LogViewer { .. }) => apply_log_viewer_action(state, action),
        Some(Overlay::Pager { .. }) => apply_pager_action(state, action),
        Some(Overlay::ExecPane { .. }) => apply_exec_pane_action(state, action),
        Some(Overlay::PortForwardStatus { .. }) => apply_port_forward_status_action(state, action),
        Some(Overlay::ConnectionForm { .. }) => apply_connection_form_action(state, action),
        // Text prompts and the vault-unlock field capture keystrokes as `Msg::Text`; non-quit
        // actions don't reach them. VaultCreate is also primarily text-driven, but `ToggleRemember`
        // needs an action path (Ctrl-R is intercepted in `map_input` before `capturing_text` is
        // checked), so it has its own action handler.
        Some(Overlay::VaultCreate { .. }) => apply_vault_create_action(state, action),
        Some(Overlay::Prompt { .. } | Overlay::VaultUnlock { .. }) | None => Vec::new(),
    }
}

/// Handle the one non-text action available while the VaultCreate overlay is open:
/// `ToggleRemember` (Ctrl-R) flips the OS-keychain opt-in flag. All other actions are no-ops
/// because the overlay captures keystrokes as text — they never reach this path.
fn apply_vault_create_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    if action == Action::ToggleRemember {
        if let Some(Overlay::VaultCreate { remember, .. }) = &mut state.overlay {
            *remember = !*remember;
        }
    }
    Vec::new()
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

/// Drive [`Overlay::ConfirmWriteback`] (RFC-0012 P3): `CursorUp`/`CursorDown` move the selection,
/// `Confirm`/`Enter` acts on the highlighted [`WritebackChoice`]. `Cancel` (Esc) is deliberately
/// **not** treated as "dismiss and do nothing" the way it is elsewhere: doing nothing here would
/// abandon the temp file with no way back, contradicting "never silently clobber or discard" — so
/// Esc is mapped to the same effect as [`WritebackChoice::KeepEditing`], the least destructive
/// option (nothing is written, nothing is deleted, the temp file is simply edited again).
fn apply_confirm_writeback_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    match action {
        Action::CursorUp => {
            if let Some(Overlay::ConfirmWriteback { cursor, .. }) = &mut state.overlay {
                *cursor = cursor.saturating_sub(1);
            }
            Vec::new()
        }
        Action::CursorDown => {
            if let Some(Overlay::ConfirmWriteback { cursor, .. }) = &mut state.overlay {
                if *cursor + 1 < WritebackChoice::ALL.len() {
                    *cursor += 1;
                }
            }
            Vec::new()
        }
        Action::Confirm | Action::Enter => {
            let Some(Overlay::ConfirmWriteback {
                id,
                conn,
                path,
                temp_path,
                v0,
                orig_size,
                orig_perms,
                download_hash,
                hash,
                cursor,
                ..
            }) = state.overlay.take()
            else {
                return Vec::new();
            };
            resolve_writeback_choice(
                state,
                WritebackChoice::ALL[cursor.min(WritebackChoice::ALL.len() - 1)],
                id,
                conn,
                path,
                temp_path,
                v0,
                orig_size,
                orig_perms,
                download_hash,
                hash,
            )
        }
        Action::Cancel => {
            let Some(Overlay::ConfirmWriteback {
                id,
                conn,
                path,
                temp_path,
                v0,
                orig_size,
                orig_perms,
                download_hash,
                hash,
                ..
            }) = state.overlay.take()
            else {
                return Vec::new();
            };
            resolve_writeback_choice(
                state,
                WritebackChoice::KeepEditing,
                id,
                conn,
                path,
                temp_path,
                v0,
                orig_size,
                orig_perms,
                download_hash,
                hash,
            )
        }
        _ => Vec::new(),
    }
}

/// Turn a resolved [`WritebackChoice`] into the effect that carries it out; sets a matching status
/// message. Shared by both the explicit `Confirm`/`Enter` path and the `Cancel`-as-`KeepEditing`
/// fallback in [`apply_confirm_writeback_action`].
///
/// `KeepEditing` forwards `download_hash` **unchanged** — it is the stable, whole-session no-op
/// baseline (see `AppEffect::EditRemoteTemp`'s doc) — and `hash` (the pre-round baseline, i.e. the
/// content this same conflict was raised over) as the new invocation's starting point.
#[allow(clippy::too_many_arguments)]
fn resolve_writeback_choice(
    state: &mut AppState,
    choice: WritebackChoice,
    id: crate::state::RemoteEditId,
    conn: cairn_types::ConnectionId,
    path: VfsPath,
    temp_path: std::path::PathBuf,
    v0: crate::state::RemoteVersion,
    orig_size: u64,
    orig_perms: Option<cairn_types::UnixPerms>,
    download_hash: crate::state::ContentHash,
    hash: crate::state::ContentHash,
) -> Vec<AppEffect> {
    match choice {
        WritebackChoice::Overwrite => vec![AppEffect::WriteBack {
            id,
            conn,
            path,
            temp_path,
            v0,
            orig_size,
            orig_perms,
            download_hash,
            mode: WriteBackMode::ForceOverwrite,
        }],
        WritebackChoice::SaveAs => vec![AppEffect::WriteBack {
            id,
            conn,
            path,
            temp_path,
            v0,
            orig_size,
            orig_perms,
            download_hash,
            mode: WriteBackMode::SaveAsSibling,
        }],
        WritebackChoice::KeepEditing => vec![AppEffect::EditRemoteTemp {
            id,
            conn,
            path,
            temp_path,
            v0,
            orig_size,
            orig_perms,
            download_hash,
            hash,
        }],
        WritebackChoice::Discard => {
            state.status = Some("Edit discarded — nothing written back".to_owned());
            vec![AppEffect::CancelRemoteEdit { id }]
        }
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

/// Drive the confirm-delete-connection dialog: `Confirm`/`Enter` fires `DeleteConnection`;
/// `Cancel`/`Esc` returns to the connections overlay (or closes if there is none).
fn apply_confirm_delete_connection_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    let Some(Overlay::ConfirmDeleteConnection {
        id,
        display_name: _,
    }) = &state.overlay
    else {
        return Vec::new();
    };
    let id = *id;
    match action {
        Action::Confirm | Action::Enter => {
            if state.connection_saving {
                return Vec::new();
            }
            // Look up the vault credential id before the profile is removed from state.
            let secret_ref = state.saved_profiles.get(&id).and_then(|p| p.secret_ref);
            state.connection_saving = true;
            state.status = Some("Deleting connection…".to_owned());
            state.overlay = None;
            vec![AppEffect::DeleteConnection { id, secret_ref }]
        }
        Action::Cancel => {
            state.overlay = None;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Drive the connection switcher: navigate the choice list and open the selected connection in the
/// active pane.
///
/// `cursor` indexes the *currently visible* subset of `state.connections` — see
/// [`visible_connection_indices`] — not the raw list, so a hidden entry (RFC-0011 P6) never
/// participates in navigation/selection unless `show_hidden` has revealed it.
fn apply_connections_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    let Some(Overlay::Connections {
        cursor,
        show_hidden,
    }) = &mut state.overlay
    else {
        return Vec::new();
    };
    let show_hidden_val = *show_hidden;
    let visible = crate::state::visible_connection_indices(&state.connections, show_hidden_val);
    let n = visible.len();
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
        Action::ToggleShowHidden => {
            *show_hidden = !show_hidden_val;
            // Re-clamp the cursor into the freshly (un)filtered list so it never points past the
            // end when revealing/re-hiding shrinks or grows the visible set.
            let new_n =
                crate::state::visible_connection_indices(&state.connections, !show_hidden_val)
                    .len();
            *cursor = (*cursor).min(new_n.saturating_sub(1));
            Vec::new()
        }
        Action::Confirm | Action::Enter => {
            let Some(Overlay::Connections { cursor, .. }) = state.overlay.take() else {
                return Vec::new();
            };
            // Cheap insurance: `cursor` is bounded by `n` (clamped on CursorDown), but
            // P4 will allow mid-switcher removal. Get instead of indexing so any TOCTOU
            // between the cursor bound-check and the take() is handled cleanly.
            let Some(&real_idx) = visible.get(cursor) else {
                return Vec::new();
            };
            let Some(choice) = state.connections.get(real_idx).cloned() else {
                return Vec::new();
            };
            let side = state.focus;
            match choice.status {
                ChoiceStatus::Ready => {
                    // A direct-navigate to a Ready connection supersedes any in-flight lazy open
                    // for this same pane. Clear the per-side slot so a delayed ConnectionOpened
                    // for the prior in-flight connection cannot later repoint this pane.
                    state.pending_conn_open[side.index()] = None;
                    state.status = Some(format!("Opened {}", choice.label));
                    navigate_to_conn(state, side, choice.conn)
                }
                ChoiceStatus::NeedsOpen => {
                    // Record pending intent in the per-side slot so ConnectionOpened knows
                    // where to navigate after the open completes.
                    state.status = Some(format!("Opening {}…", choice.label));
                    state.pending_conn_open[side.index()] = Some(choice.conn);
                    vec![AppEffect::OpenConnection { conn: choice.conn }]
                }
                ChoiceStatus::NeedsVault => {
                    // Record which conn + pane triggered the overlay so we can auto-open after.
                    state.pending_conn_open[side.index()] = Some(choice.conn);
                    if state.vault_file_exists {
                        // Vault is on disk but locked — open the unlock overlay.
                        state.overlay = Some(Overlay::VaultUnlock {
                            input: MaskedInput::new(),
                            error: None,
                            pending_conn: Some(choice.conn),
                            pending_save: None,
                        });
                    } else {
                        // No vault file yet — guide through first-run creation first.
                        state.overlay = Some(Overlay::VaultCreate {
                            passphrase: MaskedInput::new(),
                            confirm: MaskedInput::new(),
                            focus: 0,
                            remember: false,
                            error: None,
                            creating: false,
                            pending_conn: Some(choice.conn),
                            pending_save: None,
                        });
                    }
                    Vec::new()
                }
                ChoiceStatus::Unreachable => {
                    // Retry: treat exactly like NeedsOpen — the descriptor is still in the
                    // side-map. The transient failure (e.g. network blip) may have resolved.
                    state.status = Some(format!("Retrying {}…", choice.label));
                    state.pending_conn_open[side.index()] = Some(choice.conn);
                    vec![AppEffect::OpenConnection { conn: choice.conn }]
                }
            }
        }
        Action::Cancel => {
            state.overlay = None;
            Vec::new()
        }
        Action::NewConnection => {
            // Open the scheme picker to start adding a new connection.
            // `cursor` (the &mut into state.overlay) is not used after this point, so
            // NLL ends the borrow before we replace state.overlay.
            open_scheme_picker(state);
            Vec::new()
        }
        Action::EditConnection => {
            // Copy the cursor value so the mutable borrow on state.overlay ends before we read
            // state.connections and then replace state.overlay.
            let cursor_val = *cursor;
            let Some(&real_idx) = visible.get(cursor_val) else {
                return Vec::new();
            };
            let Some(choice) = state.connections.get(real_idx).cloned() else {
                return Vec::new();
            };
            let profile_id = match &choice.kind {
                ConnectionKind::Profile { id } => *id,
                ConnectionKind::AutoDiscovered => {
                    state.status = Some(
                        "Built-in and auto-discovered connections are not editable".to_owned(),
                    );
                    return Vec::new();
                }
            };
            let Some(profile_data) = state.saved_profiles.get(&profile_id).cloned() else {
                state.status = Some("Connection data not found — try restarting Cairn".to_owned());
                return Vec::new();
            };
            // Pre-populate the form with the existing profile data.
            let mut values = std::collections::HashMap::new();
            values.insert("display_name".to_owned(), profile_data.display_name.clone());
            for (k, v) in &profile_data.endpoint {
                values.insert(k.clone(), v.clone());
            }
            let is_edit = true;
            let scheme = profile_data.scheme.clone();
            let cred_cursor =
                crate::forms::default_credential_cursor(&scheme, &state.os_sources, is_edit);
            state.overlay = Some(Overlay::ConnectionForm {
                stage: ConnectionFormStage::Fields,
                scheme,
                values,
                focus: 0,
                field_errors: std::collections::HashMap::new(),
                editing_id: Some(profile_id),
                existing_secret_ref: profile_data.secret_ref,
                cred_method_cursor: cred_cursor,
                cred_method: None,
                cred_fields: std::collections::HashMap::new(),
                cred_focus: 0,
            });
            Vec::new()
        }
        // `Action::Delete` is the key 'd' maps to; `Action::DeleteConnection` is the named action
        // available via keybinding override. Both trigger the same delete logic here: open a
        // confirmation dialog rather than immediately deleting (destructive, no undo).
        Action::Delete | Action::DeleteConnection => {
            let cursor_val = *cursor;
            let Some(&real_idx) = visible.get(cursor_val) else {
                return Vec::new();
            };
            let Some(choice) = state.connections.get(real_idx).cloned() else {
                return Vec::new();
            };
            let profile_id = match &choice.kind {
                ConnectionKind::Profile { id } => *id,
                ConnectionKind::AutoDiscovered => {
                    state.status = Some(
                        "Built-in and auto-discovered connections cannot be deleted".to_owned(),
                    );
                    return Vec::new();
                }
            };
            // Show a confirmation prompt instead of deleting immediately — this is destructive.
            state.overlay = Some(Overlay::ConfirmDeleteConnection {
                id: profile_id,
                display_name: choice.label.clone(),
            });
            Vec::new()
        }
        // Probe reachability without opening a pane (RFC-0011 P6). `Ready` and `NeedsVault` are
        // resolved purely from state — no I/O:
        // - `Ready` means the backend is already mounted; re-probing would be wasted work (the
        //   "debounce/cache" tuning the RFC asks for), so just confirm it.
        // - `NeedsVault` is reported as "needs unlock" WITHOUT opening the vault-unlock/create
        //   overlay — testing must never force a vault flow on the user.
        Action::TestConnection => {
            let cursor_val = *cursor;
            let Some(&real_idx) = visible.get(cursor_val) else {
                return Vec::new();
            };
            let Some(choice) = state.connections.get(real_idx).cloned() else {
                return Vec::new();
            };
            match choice.status {
                ChoiceStatus::Ready => {
                    state.status = Some(format!("{} — already connected", choice.label));
                    Vec::new()
                }
                ChoiceStatus::NeedsVault => {
                    state.status = Some(format!(
                        "{} — needs the vault unlocked to test (Ctrl-U)",
                        choice.label
                    ));
                    Vec::new()
                }
                ChoiceStatus::NeedsOpen | ChoiceStatus::Unreachable => {
                    state.status = Some(format!("Testing {}…", choice.label));
                    vec![AppEffect::TestConnection { conn: choice.conn }]
                }
            }
        }
        // Pin/hide apply to any switcher entry (builtin, saved, or discovered) — matching the
        // `[discovery]` overlay's own scope, which is not restricted to `AutoDiscovered` kinds.
        Action::PinConnection => {
            let cursor_val = *cursor;
            let Some(&real_idx) = visible.get(cursor_val) else {
                return Vec::new();
            };
            let Some(choice) = state.connections.get(real_idx) else {
                return Vec::new();
            };
            let conn = choice.conn;
            let label = choice.label.clone();
            let new_pinned = !choice.pinned;
            state.status = Some(if new_pinned {
                format!("Pinning {label}…")
            } else {
                format!("Unpinning {label}…")
            });
            vec![AppEffect::SetConnectionPinned {
                conn,
                pinned: new_pinned,
            }]
        }
        Action::HideConnection => {
            let cursor_val = *cursor;
            let Some(&real_idx) = visible.get(cursor_val) else {
                return Vec::new();
            };
            let Some(choice) = state.connections.get(real_idx) else {
                return Vec::new();
            };
            let conn = choice.conn;
            let label = choice.label.clone();
            let new_hidden = !choice.hidden;
            state.status = Some(if new_hidden {
                format!("Hiding {label}… (press S to show hidden entries)")
            } else {
                format!("Unhiding {label}…")
            });
            vec![AppEffect::SetConnectionHidden {
                conn,
                hidden: new_hidden,
            }]
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
            let e = entries[i];
            // Explicit guard so the exclusion of `..` is auditable here, not solely
            // reliant on `VfsPath::join` continuing to reject traversal (a future
            // symlink-following backend might relax that).
            if e.is_dotdot_sentinel() {
                return None;
            }
            let name = e.name.to_string();
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
                | Overlay::ConfirmWriteback { .. }
                | Overlay::ConfirmShellAction { .. }
                | Overlay::ConfirmDeleteConnection { .. }
                | Overlay::AiPlan { .. }
                | Overlay::Connections { .. }
                | Overlay::TransferQueue { .. }
                | Overlay::Prompt { .. }
                | Overlay::VaultUnlock { .. }
                | Overlay::ConnectionForm { .. }
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

/// Append decoded text into a ring-buffered line store.
///
/// Splits `text` on `'\n'`, accumulating complete lines in `lines` and the final
/// unterminated fragment in `partial`. Both `lines` and `partial` contribute to
/// `byte_size`; the caller must initialise all three consistently.
///
/// Returns the number of complete lines evicted from the front to enforce the caps.
/// The caller should subtract this from any stored scroll offset to prevent view drift.
///
/// # OOM safety
///
/// When a chunk arrives with **no newline** (binary output, `\r`-only progress bars,
/// `yes | tr -d '\n'`), the `partial` buffer absorbs all bytes without the eviction loop
/// ever running — breaking the documented byte cap. This function detects that case and
/// force-flushes `partial` as a synthetic complete line so eviction can proceed.
fn append_to_ring(
    lines: &mut VecDeque<String>,
    partial: &mut String,
    byte_size: &mut usize,
    text: &str,
    max_lines: usize,
    max_bytes: usize,
) -> usize {
    let mut evicted = 0usize;
    let mut segments = text.split('\n');

    // The first segment continues the current partial line.
    if let Some(first) = segments.next() {
        *byte_size += first.len();
        partial.push_str(first);
    }

    for seg in segments {
        // A newline was found: flush partial into a complete line.
        let complete = std::mem::take(partial);
        *byte_size += 1; // the '\n' itself
        lines.push_back(complete);
        *byte_size += seg.len();
        partial.push_str(seg);

        // Evict oldest complete lines until within both caps.
        while (lines.len() > max_lines || *byte_size > max_bytes) && !lines.is_empty() {
            if let Some(e) = lines.pop_front() {
                *byte_size = byte_size.saturating_sub(e.len() + 1);
                evicted += 1;
            }
        }
    }

    // OOM guard: if `partial` has grown past the byte cap (no `\n` in the entire chunk),
    // force-flush it as a synthetic complete line so the eviction loop above can bound it.
    // Without this, a single large no-newline chunk bypasses the cap entirely.
    if *byte_size > max_bytes && !partial.is_empty() {
        let complete = std::mem::take(partial);
        *byte_size += 1; // synthetic newline
        lines.push_back(complete);
        while (*byte_size > max_bytes || lines.len() > max_lines) && !lines.is_empty() {
            if let Some(e) = lines.pop_front() {
                *byte_size = byte_size.saturating_sub(e.len() + 1);
                evicted += 1;
            }
        }
        // If lines is now empty but byte_size is still > 0 (the evicted line was larger
        // than max_bytes), byte_size is now the residual of the synthetic newline. Clear it.
        if lines.is_empty() {
            *byte_size = 0;
        }
    }

    evicted
}

/// Append a decoded text chunk from a log stream into the log-viewer ring buffer.
///
/// Delegates to [`append_to_ring`] with [`LOG_VIEWER_MAX_LINES`] / [`LOG_VIEWER_MAX_BYTES`].
/// Returns the number of complete lines evicted; the log-viewer scroll handler can use this
/// to adjust the stored scroll offset (scroll drift prevention).
fn append_log_chunk(
    lines: &mut VecDeque<String>,
    partial: &mut String,
    byte_size: &mut usize,
    text: &str,
) -> usize {
    use crate::state::{LOG_VIEWER_MAX_BYTES, LOG_VIEWER_MAX_LINES};
    append_to_ring(
        lines,
        partial,
        byte_size,
        text,
        LOG_VIEWER_MAX_LINES,
        LOG_VIEWER_MAX_BYTES,
    )
}

/// Append a decoded text chunk from an exec session into the session output ring buffer.
///
/// Delegates to [`append_to_ring`] with [`SESSION_OUTPUT_MAX_LINES`] / [`SESSION_OUTPUT_MAX_BYTES`].
/// Returns the number of complete lines evicted from the front; the [`AppEvent::SessionOutput`]
/// handler subtracts this from the stored scroll offset to prevent view drift.
fn append_session_output(rec: &mut SessionRecord, text: &str) -> usize {
    append_to_ring(
        &mut rec.output_lines,
        &mut rec.output_partial,
        &mut rec.output_byte_size,
        text,
        SESSION_OUTPUT_MAX_LINES,
        SESSION_OUTPUT_MAX_BYTES,
    )
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
    // The synthetic `..` sentinel is a navigation affordance: pressing Enter on it is
    // identical to pressing Backspace/h/Left (leave_dir). We must NOT call `join("..")` —
    // that is rejected by `VfsPath::join`. Route through `leave_dir`; the cursor lands on
    // the `..` row of the new listing (index 0), which is the MC-consistent behaviour.
    if state
        .pane(side)
        .current()
        .is_some_and(|e| e.is_dotdot_sentinel())
    {
        return leave_dir(state);
    }
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
        // Not a directory: kick off the async sniff so `Enter` opens the read-only pager in the
        // right mode (RFC-0012 P1). `sniff_current_file` no-ops on entry kinds that have their
        // own dedicated flow (Stream/Special) rather than a plain byte stream.
        None => sniff_current_file(state),
    }
}

/// `Enter` on a non-directory entry: read a bounded prefix and classify it (text vs binary). A
/// `Text` result routes to the in-place editor (RFC-0012 P2, [`AppEffect::SuspendAndEdit`]); a
/// `Binary` result opens the read-only hex pager (unchanged from P1) — so a binary file never
/// flashes garbled text or gets loaded into an editor Cairn is not built to handle. Only
/// [`EntryKind::File`] and [`EntryKind::Symlink`] are sniffed — `Stream` entries (e.g.
/// container/pod logs) already have a dedicated live-tail flow ([`Action::OpenLogViewer`]), and
/// `Special` nodes (sockets, devices, FIFOs) are not something a byte-stream pager/editor can
/// meaningfully show. See the `AppEvent::FileSniffed` handler for the mode routing.
fn sniff_current_file(state: &mut AppState) -> Vec<AppEffect> {
    let side = state.focus;
    let pane = state.active();
    let Some(entry) = pane.current() else {
        return Vec::new();
    };
    if !matches!(entry.kind, EntryKind::File | EntryKind::Symlink) {
        return Vec::new();
    }
    let Ok(path) = pane.cwd.join(entry.name.as_ref()) else {
        state.status = Some("Cannot open this entry".to_owned());
        return Vec::new();
    };
    let conn = pane.conn;
    vec![AppEffect::SniffFile {
        pane: side,
        conn,
        path,
    }]
}

/// Open the pager directly via `F3` (`Action::View`) — skips the sniff that `Enter` does, since
/// F3 always means "view this". Starts in `Text` mode with no prefetch; the reducer flips to
/// `Hex` if the first streamed chunk contains a NUL byte (see the `AppEvent::PagerChunk` handler).
fn open_pager_for_view(state: &mut AppState) -> Vec<AppEffect> {
    let pane = state.active();
    let Some(entry) = pane.current() else {
        state.status = Some("Nothing selected".to_owned());
        return Vec::new();
    };
    if entry.is_dotdot_sentinel() || entry.is_dir() {
        state.status = Some("Cannot view a directory".to_owned());
        return Vec::new();
    }
    // Restrict to the same kinds the Enter sniff accepts (`File`/`Symlink`). Opening a `Special`
    // node (FIFO, device, socket) for read on the local backend blocks indefinitely until a writer
    // connects, and that `open_read().await` runs before the pager's cancellation token is polled —
    // so without this guard, F3 on a named pipe spawns an unkillable, forever-hung stream. `Stream`
    // entries (container/pod logs) have their own live-tail flow and aren't byte-pager content.
    if !matches!(entry.kind, EntryKind::File | EntryKind::Symlink) {
        state.status = Some("Cannot view this entry".to_owned());
        return Vec::new();
    }
    let Ok(path) = pane.cwd.join(entry.name.as_ref()) else {
        state.status = Some("Cannot open this entry".to_owned());
        return Vec::new();
    };
    let conn = pane.conn;
    let title = format!("{} — view", entry.name);
    let total_size = entry.size;
    open_pager(
        state,
        conn,
        path,
        title,
        PagerMode::Text,
        total_size,
        Bytes::new(),
    )
}

/// Open the external editor directly via `F4` (`Action::Edit`) — MC-faithful: no text/binary
/// sniff, `F4` always means "edit this" regardless of content. Guards to the same entry kinds as
/// the pager (`File`/`Symlink`) and emits [`AppEffect::SuspendAndEdit`]; the local-path resolution,
/// terminal suspend/resume, and editor spawn all happen in the runtime (`crates/cairn/src/app.rs`),
/// never in this pure reducer.
fn start_edit(state: &mut AppState) -> Vec<AppEffect> {
    let pane = state.active();
    let Some(entry) = pane.current() else {
        state.status = Some("Nothing to edit".to_owned());
        return Vec::new();
    };
    if entry.is_dotdot_sentinel() || entry.is_dir() {
        state.status = Some("Cannot edit a directory".to_owned());
        return Vec::new();
    }
    // Same restriction as the pager/sniff: only a plain byte stream is editable. `Stream` entries
    // (container/pod logs) have their own live-tail flow; `Special` nodes (FIFO/device/socket)
    // aren't meaningfully editable and opening one for write could block indefinitely.
    if !matches!(entry.kind, EntryKind::File | EntryKind::Symlink) {
        state.status = Some("Cannot edit this entry".to_owned());
        return Vec::new();
    }
    let Ok(path) = pane.cwd.join(entry.name.as_ref()) else {
        state.status = Some("Cannot open this entry".to_owned());
        return Vec::new();
    };
    let conn = pane.conn;
    vec![AppEffect::SuspendAndEdit { conn, path }]
}

/// Mint a fresh pager id, install [`Overlay::Pager`] in `mode` (seeded with `prefetch`, if any, so
/// the first frame renders instantly), and emit [`AppEffect::OpenPager`] to stream the rest.
///
/// The effect's `skip` is derived from `prefetch.len()` directly — not from the post-decode
/// `byte_size` the seeding leaves behind, which can differ from the raw input length (`Text`
/// mode's lossy UTF-8 decoding can replace invalid sequences with a wider placeholder; `Hex`
/// mode's byte↔char encoding can take 1–2 UTF-8 bytes per raw byte). Using the raw length keeps
/// the "how many bytes has the runner already shown" accounting exact regardless of how the
/// reducer chooses to store them.
fn open_pager(
    state: &mut AppState,
    conn: ConnectionId,
    path: VfsPath,
    title: String,
    mode: PagerMode,
    total_size: Option<u64>,
    prefetch: Bytes,
) -> Vec<AppEffect> {
    let id = state.next_pager_id;
    state.next_pager_id += 1;

    // If a pager is already open (e.g. F3 opened one, then an in-flight Enter sniff resolves, or
    // two quick Enters both resolve), its stream must be cancelled before we replace the overlay —
    // otherwise its `PagerId` becomes unreachable: no `ClosePager` would ever be emitted for it, so
    // its runner keeps reading the whole file to EOF (uncapped, uncancellable) with every chunk
    // silently dropped by the id guard. Capture the old id here and close it alongside the open.
    let superseded = match &state.overlay {
        Some(Overlay::Pager { id: old, .. }) => Some(*old),
        _ => None,
    };

    let mut lines = VecDeque::new();
    let mut partial = String::new();
    let mut byte_size = 0usize;
    if !prefetch.is_empty() {
        append_pager_bytes(mode, &mut lines, &mut partial, &mut byte_size, &prefetch);
    }
    let skip = prefetch.len() as u64;

    state.overlay = Some(Overlay::Pager {
        id,
        title,
        mode,
        lines,
        partial,
        byte_size,
        total_size,
        scroll: 0,
        status: PagerStatus::Loading,
        wrap: true,
    });

    let mut effects = Vec::with_capacity(2);
    if let Some(old) = superseded {
        effects.push(AppEffect::ClosePager { id: old });
    }
    effects.push(AppEffect::OpenPager {
        id,
        conn,
        path,
        skip,
    });
    effects
}

/// Accumulate raw bytes into the pager's stored lines/rows, dispatching on `mode`. Returns `true`
/// once [`PAGER_MAX_BYTES`] is reached — the caller stops accepting further bytes for this stream.
fn append_pager_bytes(
    mode: PagerMode,
    lines: &mut VecDeque<String>,
    partial: &mut String,
    byte_size: &mut usize,
    chunk: &[u8],
) -> bool {
    match mode {
        PagerMode::Text => append_pager_text(lines, partial, byte_size, chunk),
        PagerMode::Hex => append_pager_hex(lines, partial, byte_size, chunk),
    }
}

/// Split raw bytes into decoded text lines for the pager (mirrors the log viewer's line-splitting
/// convention, decoding each chunk lossily) — but, unlike the log viewer's live-tail ring buffer,
/// never evicts from the front: a file pager shows the *first* N bytes up to the cap, not the most
/// recent ones. Returns `true` once [`PAGER_MAX_BYTES`] is reached.
fn append_pager_text(
    lines: &mut VecDeque<String>,
    partial: &mut String,
    byte_size: &mut usize,
    chunk: &[u8],
) -> bool {
    if *byte_size >= PAGER_MAX_BYTES {
        return true;
    }
    let text = String::from_utf8_lossy(chunk);
    let mut segments = text.split('\n');
    if let Some(first) = segments.next() {
        if !push_within_pager_budget(partial, byte_size, first) {
            return true;
        }
    }
    for seg in segments {
        let complete = std::mem::take(partial);
        lines.push_back(complete);
        *byte_size += 1; // the '\n' itself
        if *byte_size >= PAGER_MAX_BYTES {
            return true;
        }
        if !push_within_pager_budget(partial, byte_size, seg) {
            return true;
        }
    }
    *byte_size >= PAGER_MAX_BYTES
}

/// Append as much of `s` as fits within the remaining [`PAGER_MAX_BYTES`] budget onto `partial`,
/// updating `byte_size`. Returns `false` once the budget is exhausted (the caller stops after this
/// call either way, so the exhausted-write is the last thing appended).
///
/// Lossy UTF-8 decoding can *expand* a chunk (an invalid byte becomes the 3-byte replacement
/// character `U+FFFD`), so clamping the raw input bytes before decoding would not be sufficient to
/// bound the decoded string — this truncates the already-decoded `&str` instead, backing off to a
/// `str::is_char_boundary` so a multi-byte character is never split (which would panic on
/// indexing) even for maliciously or incidentally malformed input.
fn push_within_pager_budget(partial: &mut String, byte_size: &mut usize, s: &str) -> bool {
    let remaining = PAGER_MAX_BYTES.saturating_sub(*byte_size);
    if s.len() <= remaining {
        *byte_size += s.len();
        partial.push_str(s);
        true
    } else {
        let mut cut = remaining;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        *byte_size += cut;
        partial.push_str(&s[..cut]);
        false
    }
}

/// Accumulate raw bytes into fixed [`PAGER_HEX_ROW_BYTES`]-byte "rows" for the hex-mode pager.
/// Each row is encoded as a `String` via a lossless byte↔char mapping (`char::from(byte)`, always
/// a valid Unicode scalar value for `0..=255`) so the raw bytes round-trip exactly — unlike `Text`
/// mode, hex content must never be corrupted by lossy UTF-8 decoding. The render layer decodes
/// each row back to bytes and formats the `offset | hex | ascii` display row; storage stays raw
/// here. Like [`append_pager_text`], never evicts — returns `true` once [`PAGER_MAX_BYTES`] bytes
/// have been retained.
fn append_pager_hex(
    lines: &mut VecDeque<String>,
    partial: &mut String,
    byte_size: &mut usize,
    chunk: &[u8],
) -> bool {
    if *byte_size >= PAGER_MAX_BYTES {
        return true;
    }
    for &b in chunk {
        partial.push(char::from(b));
        *byte_size += 1;
        if partial.chars().count() == PAGER_HEX_ROW_BYTES {
            lines.push_back(std::mem::take(partial));
        }
        if *byte_size >= PAGER_MAX_BYTES {
            return true;
        }
    }
    false
}

/// Page size for the pager's scroll actions (mirrors the log viewer's `LOG_VIEWER_PAGE`).
const PAGER_PAGE: usize = 20;

/// Handle actions while the pager overlay is open. Unlike the log viewer there is no `follow`
/// state — a static file pager doesn't auto-tail — so this is a simpler, symmetric set of
/// cursor/page moves over whatever has streamed in so far.
fn apply_pager_action(state: &mut AppState, action: Action) -> Vec<AppEffect> {
    let Some(Overlay::Pager {
        id, lines, scroll, ..
    }) = &mut state.overlay
    else {
        return Vec::new();
    };
    let total = lines.len();
    match action {
        Action::CursorUp => {
            *scroll = scroll.saturating_sub(1);
            Vec::new()
        }
        Action::PageUp => {
            *scroll = scroll.saturating_sub(PAGER_PAGE);
            Vec::new()
        }
        Action::CursorDown => {
            if *scroll + 1 < total {
                *scroll += 1;
            }
            Vec::new()
        }
        Action::PageDown => {
            *scroll = (*scroll + PAGER_PAGE).min(total.saturating_sub(1));
            Vec::new()
        }
        Action::CursorTop => {
            *scroll = 0;
            Vec::new()
        }
        Action::CursorBottom => {
            *scroll = total.saturating_sub(1);
            Vec::new()
        }
        Action::Cancel => {
            let pager_id = *id;
            state.overlay = None;
            vec![AppEffect::ClosePager { id: pager_id }]
        }
        _ => Vec::new(),
    }
}

fn leave_dir(state: &mut AppState) -> Vec<AppEffect> {
    let side = state.focus;
    let parent = state.pane(side).cwd.parent();
    match parent {
        Some(dir) => navigate(state, side, dir),
        // At the VFS root: if this pane descended into a mounted archive (RFC-0013), pop back to
        // the connection/directory it came from instead of the previous no-op. Generalizes to
        // nested archives — each mount pushed its own frame, so popping always returns exactly one
        // level up. A pane that never mounted anything has an empty stack and this is still a no-op.
        None => match state.pane_mut(side).mount_stack.pop() {
            Some(frame) => {
                state.pane_mut(side).conn = frame.conn;
                navigate(state, side, frame.cwd)
            }
            None => Vec::new(),
        },
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
                    // Strip any literal `..` or `.` entries a misbehaving (or mock) backend
                    // might return — they would collide with the synthetic sentinel we inject
                    // below and confuse the guard logic throughout the reducer.
                    entries.retain(|e| e.name.as_str() != ".." && e.name.as_str() != ".");
                    sort_entries(&mut entries, p.sort);
                    // Inject a synthetic `..` entry at position 0 when not at the VFS root.
                    // This is a pure UI affordance (MC convention): the entry name is `..` and
                    // kind is `Dir`, but it never becomes a real `VfsPath`. `enter_dir` detects
                    // it via `is_dotdot_sentinel()` and delegates to `leave_dir` rather than
                    // calling `join("..")`.
                    if !dir.is_root() {
                        entries.insert(
                            0,
                            cairn_types::Entry::new("..", cairn_types::EntryKind::Dir),
                        );
                    }
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
                    let evicted = append_log_chunk(lines, partial, byte_size, &text);
                    // Adjust scroll for front-evictions to prevent view drift.
                    *scroll = scroll.saturating_sub(evicted);
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
        AppEvent::FileSniffed {
            pane,
            conn,
            path,
            kind,
            prefetch,
        } => {
            // Guard against a stale result: the user may have navigated away, or pressed
            // Enter/F3 on a different entry, while the classification was in flight. Only open
            // the pager if the *issuing* pane is still positioned on exactly this file (the same
            // pane-scoped staleness convention as the guard on `AppEvent::Listed`).
            let still_current = {
                let p = state.pane(pane);
                p.conn == conn
                    && p.current()
                        .and_then(|e| p.cwd.join(&e.name).ok())
                        .is_some_and(|joined| joined == path)
            };
            if !still_current {
                return Vec::new();
            }
            match kind {
                // RFC-0012 P2: a text file routes to the in-place editor, not the read-only
                // pager. The runtime resolves the local path, suspends the TUI, and launches
                // $VISUAL/$EDITOR/vi; see `AppEffect::SuspendAndEdit`.
                FileKind::Text => vec![AppEffect::SuspendAndEdit { conn, path }],
                FileKind::Binary => {
                    // Re-fetch rather than holding the borrow above across the mutation below;
                    // guarded by `still_current`, so this is expected to be `Some`, but we still
                    // pattern-match instead of `.expect()`ing (no panics on a reachable path).
                    let Some(entry) = state.pane(pane).current() else {
                        return Vec::new();
                    };
                    let title = format!("{} — view", entry.name);
                    let total_size = entry.size;
                    open_pager(
                        state,
                        conn,
                        path,
                        title,
                        PagerMode::Hex,
                        total_size,
                        prefetch,
                    )
                }
                // RFC-0013: `Enter` on a recognized tar/zip archive mounts it as a fresh,
                // ephemeral connection rather than opening the pager/editor — see
                // `AppEffect::MountArchive` and its runner in `crates/cairn/src/app.rs`.
                FileKind::Archive(_format) => vec![AppEffect::MountArchive { pane, conn, path }],
            }
        }
        AppEvent::SniffFailed { message } => {
            state.status = Some(format!("error: {message}"));
            Vec::new()
        }
        AppEvent::EditFinished { status, error } => {
            state.status = Some(status);
            if error {
                Vec::new()
            } else {
                // The file may have changed size/mtime under the editor — refresh the active
                // pane's listing (same refresh path as `Action::Refresh`). Safe to assume the
                // active pane is still the one that requested the edit: the whole runtime is
                // suspended (input reader parked, event_loop parked on the editor's `.await`)
                // for the entire duration, so no user action can change `state.focus` meanwhile.
                reload(state, state.focus)
            }
        }
        AppEvent::RemoteEditNeedsDownload {
            conn,
            path,
            v0,
            size,
            orig_perms,
        } => {
            let id = state.next_remote_edit_id;
            state.next_remote_edit_id += 1;
            let name = path.file_name().unwrap_or("file").to_owned();
            state.status = Some(format!("Downloading {name} ({size} bytes) for editing…"));
            vec![AppEffect::DownloadForEdit {
                id,
                conn,
                path,
                v0,
                size,
                orig_perms,
            }]
        }
        AppEvent::RemoteEditDownloaded {
            id,
            conn,
            path,
            temp_path,
            v0,
            orig_size,
            orig_perms,
            download_hash,
        } => vec![AppEffect::EditRemoteTemp {
            id,
            conn,
            path,
            temp_path,
            v0,
            orig_size,
            orig_perms,
            download_hash,
            // The first edit round's pre-round baseline is the download itself — the two
            // coincide exactly once, at session start.
            hash: download_hash,
        }],
        AppEvent::RemoteEditNoChange { id: _, name } => {
            state.status = Some(format!("No changes to {name} — nothing written back"));
            Vec::new()
        }
        AppEvent::RemoteEditModified {
            id,
            conn,
            path,
            temp_path,
            v0,
            orig_size,
            orig_perms,
            download_hash,
            hash: _,
        } => {
            state.status = Some("Checking for remote changes before writing back…".to_owned());
            vec![AppEffect::WriteBack {
                id,
                conn,
                path,
                temp_path,
                v0,
                orig_size,
                orig_perms,
                download_hash,
                mode: WriteBackMode::CheckThenWrite,
            }]
        }
        AppEvent::RemoteEditFailed { id: _, status } => {
            state.status = Some(status);
            Vec::new()
        }
        AppEvent::WriteBackConflict {
            id,
            conn,
            path,
            temp_path,
            v0,
            orig_size,
            orig_perms,
            download_hash,
            hash,
            reason,
        } => {
            state.status = None;
            state.overlay = Some(Overlay::ConfirmWriteback {
                id,
                conn,
                path,
                temp_path,
                v0,
                orig_size,
                orig_perms,
                download_hash,
                hash,
                reason,
                cursor: 0,
            });
            Vec::new()
        }
        AppEvent::WriteBackDone { id: _, name } => {
            state.status = Some(format!("Wrote back {name}"));
            reload(state, state.focus)
        }
        AppEvent::ArchiveMounted { pane, conn, root } => {
            // Push the pane's pre-mount origin so a later `..`-out (see `leave_dir`) can restore
            // it. Nested mounts stack correctly since this is a `Vec`, not a single slot.
            let frame = MountFrame {
                conn: state.pane(pane).conn,
                cwd: state.pane(pane).cwd.clone(),
            };
            state.pane_mut(pane).mount_stack.push(frame);
            state.status = Some("Mounted archive — Leave at the top to unmount".to_owned());
            state.pane_mut(pane).conn = conn;
            navigate(state, pane, root)
        }
        AppEvent::ArchiveMountFailed { pane: _, message } => {
            state.status = Some(format!("error: {message}"));
            Vec::new()
        }
        AppEvent::PagerChunk { id, bytes } => {
            if let Some(Overlay::Pager {
                id: ov_id,
                mode,
                lines,
                partial,
                byte_size,
                status,
                ..
            }) = &mut state.overlay
            {
                if *ov_id == id && *status == PagerStatus::Loading {
                    // F3 opens in `Text` optimistically; flip to `Hex` if the *first* chunk of a
                    // directly-viewed file contains a NUL byte (the same binary heuristic the Enter
                    // sniff uses). Gate on `byte_size == 0` so a NUL appearing only in a later chunk
                    // of an otherwise-text file can't reshuffle already-displayed content.
                    if *byte_size == 0 && *mode == PagerMode::Text && bytes.contains(&0) {
                        *mode = PagerMode::Hex;
                    }
                    let cap_hit = append_pager_bytes(*mode, lines, partial, byte_size, &bytes);
                    if cap_hit {
                        // Flush the trailing incomplete line/row so the last (partial) content at
                        // the cap boundary is still shown — the runner is about to be cancelled, so
                        // the `PagerDone` flush would otherwise never run for a truncated view.
                        if !partial.is_empty() {
                            lines.push_back(std::mem::take(partial));
                        }
                        *status = PagerStatus::Truncated;
                        // Tell the runner to stop reading — the reducer already has all the
                        // bytes it will ever show for this session.
                        return vec![AppEffect::ClosePager { id }];
                    }
                }
            }
            Vec::new()
        }
        AppEvent::PagerDone {
            id,
            error,
            truncated,
        } => {
            if let Some(Overlay::Pager {
                id: ov_id,
                lines,
                partial,
                status,
                ..
            }) = &mut state.overlay
            {
                // Only act while still `Loading`: the cap path (`PagerChunk`) may already have set
                // `Truncated` and cancelled the runner, but for a file sized exactly at the cap the
                // runner can still have observed EOF and enqueued a `PagerDone` first — without this
                // guard that stray event would clobber `Truncated` back to `Ready`.
                if *ov_id == id && *status == PagerStatus::Loading {
                    // Flush any incomplete trailing line/row so it is visible without the
                    // renderer having to special-case `partial` (unlike the log viewer's
                    // live-tail convention, where a growing partial is expected mid-stream).
                    if !partial.is_empty() {
                        lines.push_back(std::mem::take(partial));
                    }
                    *status = match error {
                        Some(msg) => PagerStatus::Error(msg),
                        None if truncated => PagerStatus::Truncated,
                        None => PagerStatus::Ready,
                    };
                }
            }
            Vec::new()
        }
        AppEvent::VaultUnlocked { result } => {
            // The attempt is no longer in flight, whatever the outcome.
            state.vault_unlocking = false;
            match result {
                Ok(()) => {
                    state.vault_unlocked = true;
                    state.has_locked_connections = false;
                    // P2: flip every NeedsVault entry to NeedsOpen — they are now openable.
                    // (Previously P1 added new ConnectionChoices from the deferred list;
                    //  in P2 they are already in the switcher since enumeration time.)
                    // Collect the flipped ids so we can clean stale pending slots below.
                    // Count only the newly-flipped entries so the message is accurate even when
                    // credential-free NeedsOpen entries already exist in the switcher.
                    let mut n_flipped: usize = 0;
                    let mut flipped_ids: std::collections::HashSet<cairn_types::ConnectionId> =
                        std::collections::HashSet::new();
                    for choice in &mut state.connections {
                        if choice.status == ChoiceStatus::NeedsVault {
                            choice.status = ChoiceStatus::NeedsOpen;
                            n_flipped += 1;
                            flipped_ids.insert(choice.conn);
                        }
                    }
                    // Extract the pending connection AND pending_save from the overlay BEFORE
                    // closing it. Both are consumed (moved out) before we close the overlay.
                    let (pending_conn, pending_save) = match state.overlay.take() {
                        Some(Overlay::VaultUnlock {
                            pending_conn,
                            pending_save,
                            ..
                        }) => (pending_conn, pending_save),
                        other => {
                            // Overlay was replaced while unlock ran — restore it.
                            state.overlay = other;
                            (None, None)
                        }
                    };
                    if pending_conn.is_none() && pending_save.is_none() {
                        for slot in &mut state.pending_conn_open {
                            if let Some(id) = *slot {
                                if flipped_ids.contains(&id) {
                                    *slot = None;
                                }
                            }
                        }
                    }
                    state.status = Some(if n_flipped == 0 {
                        "Vault unlocked".to_owned()
                    } else {
                        format!("Vault unlocked — {n_flipped} connection(s) now openable")
                    });
                    // P5: if a deferred credential save was waiting for the vault, emit it now.
                    let mut effects = Vec::new();
                    if let Some(ps) = pending_save {
                        state.connection_saving = true;
                        effects.push(AppEffect::ProvisionAndSaveConnection {
                            profile: ps.profile,
                            draft: ps.draft,
                            is_edit: ps.is_edit,
                        });
                    } else if let Some(conn) = pending_conn {
                        // Auto-open the connection that triggered the unlock (if any).
                        effects.push(AppEffect::OpenConnection { conn });
                    }
                    effects
                }
                Err(msg) => {
                    // Keep the unlock overlay open with a retryable error and a wiped field; if the
                    // user has since closed it, fall back to the status line. The message is
                    // secret-free.
                    if let Some(Overlay::VaultUnlock { input, error, .. }) = &mut state.overlay {
                        input.clear();
                        *error = Some(msg);
                    } else {
                        state.status = Some(format!("Vault unlock failed: {msg}"));
                    }
                    Vec::new()
                }
            }
        }
        AppEvent::VaultCreated {
            result,
            already_exists,
        } => {
            // The Argon2id task is no longer running; clear the in-flight flag regardless of outcome.
            if let Some(Overlay::VaultCreate { creating, .. }) = &mut state.overlay {
                *creating = false;
            }
            match result {
                Ok(()) => {
                    // The vault file now exists and the broker is unlocked.
                    state.vault_file_exists = true;
                    state.vault_unlocked = true;
                    state.has_locked_connections = false;
                    // Flip every NeedsVault entry to NeedsOpen (same logic as VaultUnlocked).
                    let mut n_flipped: usize = 0;
                    let mut flipped_ids: std::collections::HashSet<cairn_types::ConnectionId> =
                        std::collections::HashSet::new();
                    for choice in &mut state.connections {
                        if choice.status == ChoiceStatus::NeedsVault {
                            choice.status = ChoiceStatus::NeedsOpen;
                            n_flipped += 1;
                            flipped_ids.insert(choice.conn);
                        }
                    }
                    // Extract the pending connection AND pending_save from the overlay before
                    // closing it. Both are consumed before the overlay is closed.
                    let (pending_conn, pending_save) = match state.overlay.take() {
                        Some(Overlay::VaultCreate {
                            pending_conn,
                            pending_save,
                            ..
                        }) => (pending_conn, pending_save),
                        other => {
                            state.overlay = other;
                            (None, None)
                        }
                    };
                    if pending_conn.is_none() && pending_save.is_none() {
                        // Overlay was replaced while Argon2id ran; clean stale pending slots.
                        for slot in &mut state.pending_conn_open {
                            if let Some(id) = *slot {
                                if flipped_ids.contains(&id) {
                                    *slot = None;
                                }
                            }
                        }
                    }
                    state.status = Some(if n_flipped == 0 {
                        "Vault created and unlocked".to_owned()
                    } else {
                        format!("Vault created — {n_flipped} connection(s) now openable")
                    });
                    // P5: if a deferred credential save was waiting for the vault, emit it now.
                    let mut effects = Vec::new();
                    if let Some(ps) = pending_save {
                        state.connection_saving = true;
                        effects.push(AppEffect::ProvisionAndSaveConnection {
                            profile: ps.profile,
                            draft: ps.draft,
                            is_edit: ps.is_edit,
                        });
                    } else if let Some(conn) = pending_conn {
                        effects.push(AppEffect::OpenConnection { conn });
                    }
                    effects
                }
                Err(msg) => {
                    // Keep the create overlay open with a retryable error. The error message is
                    // secret-free and path-free (the effect runner ensures this).
                    //
                    // Special case: if the vault was created out-of-band (another process or
                    // terminal) between startup and now, `already_exists == true`. We flip
                    // `vault_file_exists` so subsequent Ctrl-U opens the Unlock overlay instead
                    // of looping on "already exists".
                    if already_exists {
                        state.vault_file_exists = true;
                    }
                    let display = if already_exists {
                        "A vault already exists — press Esc, then Ctrl-U to unlock it.".to_owned()
                    } else {
                        msg
                    };
                    if let Some(Overlay::VaultCreate { error, .. }) = &mut state.overlay {
                        *error = Some(display);
                    } else {
                        state.status = Some(format!("Vault creation failed: {display}"));
                    }
                    Vec::new()
                }
            }
        }
        AppEvent::SessionOutput { id, text } => {
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
        AppEvent::ConnectionSaved {
            id,
            display_name,
            label,
            is_edit,
            profile,
        } => {
            state.connection_saving = false;
            state.saved_profiles.insert(id, profile);
            if is_edit {
                // Update the label of the matching choice in-place (no re-enumeration needed).
                // Use `label` (computed by the effect runner) rather than `display_name` so that
                // local profiles get the "local: {path}" prefix the provider uses, not just the name.
                if let Some(choice) = state
                    .connections
                    .iter_mut()
                    .find(|c| matches!(&c.kind, ConnectionKind::Profile { id: cid } if *cid == id))
                {
                    choice.label = label.clone();
                }
                state.status = Some(format!(
                    "Updated '{display_name}' — endpoint changes apply on next open/restart"
                ));
            } else {
                // New connection: won't appear in the switcher until restart (descriptor_map is
                // immutable for this session and re-enumeration would alias IDs — P5 will fix this).
                state.status = Some(format!(
                    "Connection '{display_name}' saved — restart Cairn to use it in the switcher"
                ));
            }
            if matches!(state.overlay, Some(Overlay::ConnectionForm { .. })) {
                state.overlay = None;
            }
            Vec::new()
        }
        AppEvent::ConnectionDeleted { id } => {
            state.connection_saving = false;
            state.saved_profiles.remove(&id);
            // Remove the matching choice by UUID, keeping ConnectionIds of all other entries stable.
            state
                .connections
                .retain(|c| !matches!(&c.kind, ConnectionKind::Profile { id: cid } if *cid == id));
            state.status = Some("Connection deleted".to_owned());
            Vec::new()
        }
        AppEvent::ConnectionOpFailed { message } => {
            state.connection_saving = false;
            state.status = Some(message);
            Vec::new()
        }
        AppEvent::ConnectionOpened { conn, result } => {
            // Flip the choice's reachability status based on the open outcome (always
            // unconditional — the badge must reflect reality even for superseded opens).
            if let Some(choice) = state.connections.iter_mut().find(|c| c.conn == conn) {
                match &result {
                    Ok(()) => choice.status = ChoiceStatus::Ready,
                    Err(_) => choice.status = ChoiceStatus::Unreachable,
                }
            }
            match result {
                Ok(()) => {
                    // Find the per-side slot that was waiting for this conn, navigate that side,
                    // and clear only that slot. The other side's slot (if any) is untouched,
                    // allowing simultaneous in-flight opens on both panes.
                    if let Some(idx) = state
                        .pending_conn_open
                        .iter()
                        .position(|s| *s == Some(conn))
                    {
                        let side = if idx == 0 { Side::Left } else { Side::Right };
                        state.pending_conn_open[idx] = None;
                        let label = state
                            .connections
                            .iter()
                            .find(|c| c.conn == conn)
                            .map(|c| c.label.clone())
                            .unwrap_or_default();
                        state.status = Some(format!("Opened {label}"));
                        return navigate_to_conn(state, side, conn);
                    }
                    Vec::new()
                }
                Err(msg) => {
                    // Only update the status line when this conn matches an active pending slot.
                    // A superseded background open must not overwrite the status the user sees.
                    let is_active = state.pending_conn_open.iter_mut().any(|slot| {
                        if *slot == Some(conn) {
                            *slot = None;
                            true
                        } else {
                            false
                        }
                    });
                    if is_active {
                        state.status = Some(format!("Failed to open connection: {msg}"));
                    }
                    Vec::new()
                }
            }
        }
        AppEvent::ConnectionTested { conn, result } => {
            // Unconditional, like `ConnectionOpened`'s badge flip — a probe's outcome must
            // reflect reality even if the switcher has since closed. Never touches
            // `pending_conn_open`/navigation: a test never switches a pane.
            if let Some(choice) = state.connections.iter_mut().find(|c| c.conn == conn) {
                match &result {
                    Ok(()) => choice.status = ChoiceStatus::Ready,
                    Err(_) => choice.status = ChoiceStatus::Unreachable,
                }
            }
            let label = state
                .connections
                .iter()
                .find(|c| c.conn == conn)
                .map(|c| c.label.clone())
                .unwrap_or_default();
            state.status = Some(match result {
                Ok(()) => format!("{label} — reachable"),
                Err(msg) => format!("{label} — unreachable: {msg}"),
            });
            Vec::new()
        }
        AppEvent::ConnectionPinSet {
            conn,
            pinned,
            result,
        } => {
            match result {
                Ok(()) => {
                    if let Some(choice) = state.connections.iter_mut().find(|c| c.conn == conn) {
                        choice.pinned = pinned;
                    }
                    // Keep pinned entries floated to the front (stable — ties keep their
                    // existing relative order), mirroring the coordinator's own pinned-overlay
                    // ordering at enumeration time.
                    state
                        .connections
                        .sort_by_key(|c| std::cmp::Reverse(c.pinned));
                    let label = state
                        .connections
                        .iter()
                        .find(|c| c.conn == conn)
                        .map(|c| c.label.clone())
                        .unwrap_or_default();
                    state.status = Some(if pinned {
                        format!("Pinned {label}")
                    } else {
                        format!("Unpinned {label}")
                    });
                }
                Err(msg) => {
                    state.status = Some(format!("Failed to update pin: {msg}"));
                }
            }
            Vec::new()
        }
        AppEvent::ConnectionHideSet {
            conn,
            hidden,
            result,
        } => {
            match result {
                Ok(()) => {
                    let label = state
                        .connections
                        .iter()
                        .find(|c| c.conn == conn)
                        .map(|c| c.label.clone())
                        .unwrap_or_default();
                    if let Some(choice) = state.connections.iter_mut().find(|c| c.conn == conn) {
                        choice.hidden = hidden;
                    }
                    state.status = Some(if hidden {
                        format!("Hid {label} (press S in the switcher to show hidden entries)")
                    } else {
                        format!("Unhid {label}")
                    });
                    // A newly-hidden entry may vanish from the visible subset; re-clamp the
                    // switcher cursor so it never points past the end of the shrunk list.
                    if let Some(Overlay::Connections {
                        cursor,
                        show_hidden,
                    }) = &mut state.overlay
                    {
                        let visible_len = crate::state::visible_connection_indices(
                            &state.connections,
                            *show_hidden,
                        )
                        .len();
                        *cursor = (*cursor).min(visible_len.saturating_sub(1));
                    }
                }
                Err(msg) => {
                    state.status = Some(format!("Failed to update visibility: {msg}"));
                }
            }
            Vec::new()
        }
        AppEvent::OsSourcesDetected { os_sources } => {
            // Update the default credential method cursor if the picker is currently open AND
            // the user has not manually moved it. We detect "user moved" by comparing the
            // current cursor against what the open-time default would have been (using the
            // pre-detection `state.os_sources`). If they match, the cursor is still at the
            // auto-default and it is safe to advance it to the post-detection default.
            if let Some(Overlay::ConnectionForm {
                stage: ConnectionFormStage::CredentialMethodPicker,
                scheme,
                editing_id,
                cred_method_cursor,
                ..
            }) = &mut state.overlay
            {
                let is_edit = editing_id.is_some();
                let open_time_default =
                    crate::forms::default_credential_cursor(scheme, &state.os_sources, is_edit);
                if *cred_method_cursor == open_time_default {
                    *cred_method_cursor =
                        crate::forms::default_credential_cursor(scheme, &os_sources, is_edit);
                }
            }
            state.os_sources = os_sources;
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
        // P5: initial_effects now emits List×2 + DetectOsSources.
        assert_eq!(
            fx.len(),
            3,
            "expected 3 initial effects (List×2 + DetectOsSources), got {fx:?}"
        );
        assert!(
            fx.iter()
                .filter(|e| matches!(e, AppEffect::List { .. }))
                .count()
                == 2,
            "must have exactly 2 List effects"
        );
        assert!(
            fx.iter().any(|e| matches!(e, AppEffect::DetectOsSources)),
            "must have a DetectOsSources effect"
        );
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
    fn enter_file_emits_sniff_effect() {
        // RFC-0012 P1: `Enter` on a file no longer no-ops — it kicks off the async sniff so the
        // pager can open in the right mode. The cwd doesn't change (only a directory navigates).
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("file", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::Enter));
        assert_eq!(fx.len(), 1);
        assert!(matches!(
            fx[0],
            AppEffect::SniffFile {
                conn: ConnectionId(1),
                ..
            }
        ));
        assert_eq!(s.active().cwd.as_str(), "/");
    }

    #[test]
    fn enter_on_stream_entry_does_nothing() {
        // Stream entries (container/pod logs) have their own dedicated flow
        // (`Action::OpenLogViewer`) — `Enter` must not try to sniff/page them.
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("web-1", EntryKind::Stream)],
        );
        let fx = update(&mut s, Msg::Action(Action::Enter));
        assert!(fx.is_empty());
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
            ..Default::default()
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
                ..Default::default()
            },
            ConnectionChoice {
                conn: ConnectionId(7),
                label: "local: /b".into(),
                ..Default::default()
            },
        ];
        // Open the switcher.
        let fx = update(&mut s, Msg::Action(Action::OpenConnections));
        assert!(fx.is_empty());
        assert!(matches!(
            s.overlay,
            Some(Overlay::Connections {
                cursor: 0,
                show_hidden: false
            })
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
                ..Default::default()
            },
            ConnectionChoice {
                conn: ConnectionId(4),
                label: "b".into(),
                ..Default::default()
            },
        ];
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        // Up at the top clamps; down past the end clamps.
        let _ = update(&mut s, Msg::Action(Action::CursorUp));
        assert!(matches!(
            s.overlay,
            Some(Overlay::Connections {
                cursor: 0,
                show_hidden: false
            })
        ));
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        assert!(matches!(
            s.overlay,
            Some(Overlay::Connections {
                cursor: 1,
                show_hidden: false
            })
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
            ..Default::default()
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
        s.vault_file_exists = true; // existing vault on disk → unlock flow
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
        s.vault_file_exists = true; // existing vault → unlock flow
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
            Some(Overlay::VaultUnlock { input, error, .. }) => {
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
        s.vault_file_exists = true; // existing vault → unlock flow
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
        s.vault_file_exists = true; // existing vault → unlock flow
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
            Some(Overlay::VaultUnlock { input, error, .. }) => {
                assert!(input.is_empty(), "the field is wiped for a retry");
                assert!(error
                    .as_deref()
                    .is_some_and(|e| e.contains("wrong passphrase")));
            }
            other => panic!("overlay should stay open for a retry, got {other:?}"),
        }
    }

    #[test]
    fn vault_unlock_success_closes_overlay_and_flips_needs_vault_to_needs_open() {
        // P2: connections are pre-populated in the switcher as NeedsVault at startup.
        // A successful unlock flips them to NeedsOpen; the count stays the same.
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.vault_file_exists = true; // existing vault → unlock flow
        s.has_locked_connections = true;
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(7),
            label: "ssh: bastion".to_owned(),
            status: ChoiceStatus::NeedsVault,
            ..Default::default()
        }];
        let _ = update(&mut s, Msg::Action(Action::VaultUnlock));
        type_text(&mut s, "right-pass");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        // The effect runner sends Ok(()) — no Vec<ConnectionChoice>, just success.
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::VaultUnlocked { result: Ok(()) }),
        );
        // No pending_conn in the overlay (opened via Action::VaultUnlock, not via switcher),
        // so no OpenConnection effect is emitted.
        assert!(fx.is_empty());
        assert!(s.overlay.is_none(), "overlay closes on success");
        assert!(s.vault_unlocked);
        assert!(!s.vault_unlocking, "the in-flight flag is cleared");
        assert!(!s.has_locked_connections);
        // Connection count unchanged; the entry flipped from NeedsVault to NeedsOpen.
        assert_eq!(s.connections.len(), 1);
        assert_eq!(s.connections[0].label, "ssh: bastion");
        assert_eq!(s.connections[0].status, ChoiceStatus::NeedsOpen);
        assert_eq!(
            s.status.as_deref(),
            Some("Vault unlocked — 1 connection(s) now openable")
        );
    }

    #[test]
    fn vault_unlock_double_submit_does_not_spawn_a_second_effect() {
        let mut s = state();
        s.vault_file_exists = true; // existing vault → unlock flow
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
        s.vault_file_exists = true; // existing vault → unlock flow
        s.connections = vec![crate::ConnectionChoice {
            conn: cairn_types::ConnectionId(3),
            label: "local: /".to_owned(),
            ..Default::default()
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
            Msg::Event(AppEvent::VaultUnlocked { result: Ok(()) }),
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

    // ── P4.5: vault-create-from-UI ───────────────────────────────────────────────────

    /// Helper: open the VaultCreate overlay (no vault file on disk).
    fn open_vault_create(s: &mut AppState) {
        s.vault_file_exists = false;
        let _ = update(s, Msg::Action(Action::VaultUnlock));
        assert!(
            matches!(s.overlay, Some(Overlay::VaultCreate { .. })),
            "expected VaultCreate overlay to be open"
        );
    }

    #[test]
    fn ctrl_u_opens_create_overlay_when_no_vault_file_exists() {
        let mut s = state();
        s.vault_file_exists = false;
        let fx = update(&mut s, Msg::Action(Action::VaultUnlock));
        assert!(fx.is_empty());
        assert!(
            matches!(
                s.overlay,
                Some(Overlay::VaultCreate {
                    focus: 0,
                    remember: false,
                    ..
                })
            ),
            "Ctrl-U with no vault file opens the create overlay"
        );
        assert!(s.capturing_text(), "create overlay captures keystrokes");
    }

    #[test]
    fn ctrl_u_opens_unlock_overlay_when_vault_file_exists() {
        let mut s = state();
        s.vault_file_exists = true;
        let fx = update(&mut s, Msg::Action(Action::VaultUnlock));
        assert!(fx.is_empty());
        assert!(
            matches!(s.overlay, Some(Overlay::VaultUnlock { .. })),
            "Ctrl-U with an existing vault file opens the unlock overlay"
        );
    }

    #[test]
    fn vault_create_cancel_closes_overlay() {
        let mut s = state();
        open_vault_create(&mut s);
        let fx = update(&mut s, Msg::Text(TextEdit::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none(), "Esc/Cancel closes the create overlay");
    }

    #[test]
    fn vault_create_cancel_clears_pending_conn_open_slot() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.vault_file_exists = false;
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(7),
            label: "ssh: host".to_owned(),
            status: ChoiceStatus::NeedsVault,
            ..Default::default()
        }];
        // Selecting the NeedsVault entry opens VaultCreate with a pending_conn.
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let _ = update(&mut s, Msg::Action(Action::Confirm));
        assert!(
            s.pending_conn_open.iter().any(|s| s.is_some()),
            "pending_conn_open must be set after NeedsVault selection"
        );
        // Cancel must clear the pending slot so a stale ConnectionOpened can't navigate the pane.
        let _ = update(&mut s, Msg::Text(TextEdit::Cancel));
        assert!(
            s.pending_conn_open.iter().all(|s| s.is_none()),
            "cancel must clear the pending_conn_open slot"
        );
    }

    #[test]
    fn vault_create_tab_cycles_focus_between_fields() {
        let mut s = state();
        open_vault_create(&mut s);
        // Initially focused on field 0 (passphrase).
        assert!(matches!(
            s.overlay,
            Some(Overlay::VaultCreate { focus: 0, .. })
        ));
        // Tab moves to confirm.
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        assert!(matches!(
            s.overlay,
            Some(Overlay::VaultCreate { focus: 1, .. })
        ));
        // Shift-Tab moves back.
        let _ = update(&mut s, Msg::Text(TextEdit::PrevField));
        assert!(matches!(
            s.overlay,
            Some(Overlay::VaultCreate { focus: 0, .. })
        ));
    }

    #[test]
    fn vault_create_toggle_remember_flips_the_flag() {
        let mut s = state();
        open_vault_create(&mut s);
        assert!(matches!(
            s.overlay,
            Some(Overlay::VaultCreate {
                remember: false,
                ..
            })
        ));
        // ToggleRemember (Ctrl-R bypass in map_input) flips the flag.
        let _ = update(&mut s, Msg::Action(Action::ToggleRemember));
        assert!(matches!(
            s.overlay,
            Some(Overlay::VaultCreate { remember: true, .. })
        ));
        let _ = update(&mut s, Msg::Action(Action::ToggleRemember));
        assert!(matches!(
            s.overlay,
            Some(Overlay::VaultCreate {
                remember: false,
                ..
            })
        ));
    }

    #[test]
    fn vault_create_empty_passphrase_rejected_before_compare() {
        let mut s = state();
        open_vault_create(&mut s);
        // Submit with both fields empty.
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty(), "empty passphrase emits no effect");
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::VaultCreate { error: Some(_), .. })
            ),
            "empty passphrase sets an error"
        );
        // The error must not mention any secret value.
        if let Some(Overlay::VaultCreate {
            error: Some(err), ..
        }) = &s.overlay
        {
            assert!(!err.is_empty());
        }
    }

    #[test]
    fn vault_create_too_short_passphrase_rejected_with_length_error() {
        let mut s = state();
        open_vault_create(&mut s);
        // 11 chars = VAULT_PASSPHRASE_MIN_LEN - 1 → must be rejected.
        type_text(&mut s, "short_passw"); // exactly 11 chars
        assert_eq!(
            "short_passw".chars().count(),
            VAULT_PASSPHRASE_MIN_LEN - 1,
            "precondition: string must be one char below the minimum"
        );
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty(), "11-char passphrase emits no effect");
        match &s.overlay {
            Some(Overlay::VaultCreate {
                error: Some(err), ..
            }) => {
                assert!(
                    err.contains("least"),
                    "error must mention the minimum length, got: {err}"
                );
            }
            other => panic!("expected error in overlay, got {other:?}"),
        }
    }

    #[test]
    fn vault_create_exactly_min_len_passphrase_accepted() {
        let mut s = state();
        open_vault_create(&mut s);
        // 12 chars = VAULT_PASSPHRASE_MIN_LEN → must be accepted (no length error).
        let pp = "a".repeat(VAULT_PASSPHRASE_MIN_LEN);
        type_text(&mut s, &pp);
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        type_text(&mut s, &pp);
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        // Length is valid; a CreateVault effect is emitted.
        assert!(
            fx.iter()
                .any(|e| matches!(e, AppEffect::CreateVault { .. })),
            "12-char passphrase must emit CreateVault effect, got: {fx:?}"
        );
    }

    /// Fix 7: typing any character after a validation error clears the error.
    #[test]
    fn vault_create_insert_clears_stale_error() {
        let mut s = state();
        open_vault_create(&mut s);
        // Trigger an error: submit with an empty passphrase.
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::VaultCreate { error: Some(_), .. })
            ),
            "precondition: submit on empty must set an error"
        );
        // Any new character must clear it.
        let _ = update(&mut s, Msg::Text(TextEdit::Insert('a')));
        match &s.overlay {
            Some(Overlay::VaultCreate { error, .. }) => {
                assert!(
                    error.is_none(),
                    "Insert must clear the stale error, got: {error:?}"
                );
            }
            other => panic!("overlay must still be open, got {other:?}"),
        }
    }

    #[test]
    fn vault_create_empty_confirm_rejected_before_content_compare() {
        let mut s = state();
        open_vault_create(&mut s);
        // Type a valid passphrase but leave confirm empty.
        type_text(&mut s, "correct horse battery");
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty(), "empty confirm emits no effect");
        match &s.overlay {
            Some(Overlay::VaultCreate {
                error: Some(err), ..
            }) => {
                assert!(
                    err.to_lowercase().contains("confirm"),
                    "error must mention confirm, got: {err}"
                );
            }
            other => panic!("expected error in overlay, got {other:?}"),
        }
    }

    #[test]
    fn vault_create_length_mismatch_clears_confirm_and_leaves_passphrase_intact() {
        let mut s = state();
        open_vault_create(&mut s);
        // Type passphrase, switch to confirm, type a shorter value.
        type_text(&mut s, "correct horse battery staple");
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        type_text(&mut s, "correct horse"); // shorter — length mismatch
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty(), "length mismatch emits no effect");
        match &s.overlay {
            Some(Overlay::VaultCreate {
                passphrase,
                confirm,
                error,
                ..
            }) => {
                // Passphrase field is intact for a quick retry.
                assert!(
                    !passphrase.is_empty(),
                    "passphrase must be intact on a length mismatch"
                );
                // Confirm is wiped so the user re-types only the confirm field.
                assert!(
                    confirm.is_empty(),
                    "confirm must be wiped on a length mismatch"
                );
                assert!(
                    error.as_deref().is_some_and(|e| e.contains("match")),
                    "error must say passphrases don't match"
                );
            }
            other => panic!("expected VaultCreate overlay, got {other:?}"),
        }
    }

    #[test]
    fn vault_create_content_mismatch_wipes_both_fields_and_resets_focus() {
        let mut s = state();
        open_vault_create(&mut s);
        // Same length (≥ 12) but different content — triggers the secret-comparison path.
        type_text(&mut s, "aaaaaaaaaaaa"); // 12 chars (VAULT_PASSPHRASE_MIN_LEN)
        let _ = update(&mut s, Msg::Text(TextEdit::NextField)); // switch to confirm (focus = 1)
        type_text(&mut s, "bbbbbbbbbbbb"); // 12 chars, different content
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(fx.is_empty(), "content mismatch emits no effect");
        match &s.overlay {
            Some(Overlay::VaultCreate {
                passphrase,
                confirm,
                error,
                focus,
                ..
            }) => {
                // Both fields are wiped on content mismatch (the comparison consumed both secrets).
                assert!(
                    passphrase.is_empty(),
                    "passphrase must be wiped on content mismatch"
                );
                assert!(
                    confirm.is_empty(),
                    "confirm must be wiped on content mismatch"
                );
                assert!(
                    error.as_deref().is_some_and(|e| e.contains("match")),
                    "error must say passphrases don't match"
                );
                // Fix 6: focus resets to 0 so the user knows to re-enter both from the start.
                assert_eq!(
                    *focus, 0,
                    "focus must reset to passphrase field on content mismatch"
                );
            }
            other => panic!("expected VaultCreate overlay, got {other:?}"),
        }
    }

    #[test]
    fn vault_create_matching_passphrase_emits_create_vault_effect_and_redacts_in_debug() {
        use cairn_secrets::ExposeSecret;
        const PP: &str = "correct horse battery staple";
        let mut s = state();
        open_vault_create(&mut s);
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        type_text(&mut s, PP);
        // Verify the passphrase is NOT visible in Debug before submit.
        assert!(
            !format!("{s:?}").contains("staple"),
            "AppState Debug must not reveal the passphrase before submit"
        );
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        // Exactly one CreateVault effect.
        let (passphrase, remember) = match &fx[..] {
            [AppEffect::CreateVault {
                passphrase,
                remember,
            }] => (passphrase, *remember),
            other => panic!("expected CreateVault effect, got {other:?}"),
        };
        // The effect carries the right secret.
        assert_eq!(passphrase.expose_secret(), PP);
        // The effect's own Debug must not reveal it (SecretString redacts).
        assert!(
            !format!("{:?}", fx[0]).contains("staple"),
            "effect Debug must not reveal the passphrase"
        );
        assert!(!remember, "remember defaults to false");
        // Both fields are wiped after take_secret.
        match &s.overlay {
            Some(Overlay::VaultCreate {
                passphrase,
                confirm,
                creating,
                ..
            }) => {
                assert!(passphrase.is_empty(), "passphrase field must be wiped");
                assert!(confirm.is_empty(), "confirm field must be wiped");
                assert!(*creating, "creating flag must be set while Argon2id runs");
            }
            other => panic!("overlay must stay open during create, got {other:?}"),
        }
        assert_eq!(s.status.as_deref(), Some("Creating vault…"));
    }

    #[test]
    fn vault_create_remember_flag_carried_in_effect() {
        const PP: &str = "hunter2hunter2";
        let mut s = state();
        open_vault_create(&mut s);
        // Toggle remember on.
        let _ = update(&mut s, Msg::Action(Action::ToggleRemember));
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        type_text(&mut s, PP);
        let fx = update(&mut s, Msg::Text(TextEdit::Submit));
        match &fx[..] {
            [AppEffect::CreateVault { remember: true, .. }] => {}
            other => panic!("expected CreateVault{{remember:true}}, got {other:?}"),
        }
    }

    #[test]
    fn vault_create_double_submit_suppressed_by_creating_flag() {
        const PP: &str = "longEnoughPassphrase";
        let mut s = state();
        open_vault_create(&mut s);
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        type_text(&mut s, PP);
        let fx1 = update(&mut s, Msg::Text(TextEdit::Submit));
        assert_eq!(fx1.len(), 1, "first submit emits the CreateVault effect");
        // The creating flag is now set; a second submit must be suppressed.
        let fx2 = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(
            fx2.is_empty(),
            "second submit must be suppressed while creating"
        );
    }

    #[test]
    fn vault_created_ok_flips_state_and_auto_opens_pending_conn() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.vault_file_exists = false;
        s.has_locked_connections = true;
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(9),
            label: "ssh: remote".to_owned(),
            status: ChoiceStatus::NeedsVault,
            ..Default::default()
        }];
        // Simulate the user selecting the NeedsVault entry, which opens VaultCreate.
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let _ = update(&mut s, Msg::Action(Action::Confirm));
        assert!(matches!(s.overlay, Some(Overlay::VaultCreate { .. })));
        // Submit matching passphrases to emit the effect, setting creating=true.
        const PP: &str = "vaultpassword1";
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        // The "effect runner" reports success.
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::VaultCreated {
                result: Ok(()),
                already_exists: false,
            }),
        );
        // Must emit OpenConnection for the pending conn.
        assert!(
            matches!(fx[..], [AppEffect::OpenConnection { conn }] if conn == cairn_types::ConnectionId(9)),
            "VaultCreated success must auto-open the pending connection, got {fx:?}"
        );
        assert!(s.vault_file_exists, "vault_file_exists must flip to true");
        assert!(s.vault_unlocked, "vault_unlocked must flip to true");
        assert!(!s.has_locked_connections);
        assert_eq!(s.connections[0].status, ChoiceStatus::NeedsOpen);
        assert!(s.overlay.is_none(), "overlay closes on success");
    }

    #[test]
    fn vault_created_err_keeps_overlay_open_with_error() {
        const PP: &str = "correcthorsebattery";
        let mut s = state();
        open_vault_create(&mut s);
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        // Simulate failure (e.g. an I/O error — NOT an AlreadyExists).
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::VaultCreated {
                result: Err("io error".to_owned()),
                already_exists: false,
            }),
        );
        assert!(fx.is_empty(), "failure emits no effect");
        assert!(
            !s.vault_file_exists,
            "vault_file_exists must stay false on non-AlreadyExists failure"
        );
        assert!(
            !s.vault_unlocked,
            "vault_unlocked must stay false on failure"
        );
        match &s.overlay {
            Some(Overlay::VaultCreate {
                creating, error, ..
            }) => {
                assert!(!creating, "creating flag must be cleared on failure");
                assert!(
                    error.as_deref().is_some_and(|e| e.contains("io error")),
                    "non-AlreadyExists error must be shown verbatim in the overlay, got {error:?}"
                );
            }
            other => panic!("overlay must stay open for retry, got {other:?}"),
        }
    }

    /// Fix 1: when `already_exists == true` the reducer sets `vault_file_exists = true` so the
    /// user can press Esc → Ctrl-U to unlock instead of being stuck on "already exists".
    #[test]
    fn vault_created_already_exists_sets_vault_file_exists_and_allows_unlock() {
        const PP: &str = "correcthorsebattery1";
        let mut s = state();
        s.vault_file_exists = false; // app started before vault file existed
        open_vault_create(&mut s);
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        // The effect runner found VaultError::AlreadyExists (vault appeared out-of-band).
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::VaultCreated {
                result: Err("vault already exists".to_owned()),
                already_exists: true,
            }),
        );
        assert!(fx.is_empty(), "already_exists failure emits no effect");
        // vault_file_exists must now be true so Ctrl-U opens VaultUnlock.
        assert!(
            s.vault_file_exists,
            "vault_file_exists must flip to true when already_exists"
        );
        assert!(
            !s.vault_unlocked,
            "vault must not be marked unlocked (we didn't actually unlock it)"
        );
        // Overlay must show the recovery hint, not the raw error.
        match &s.overlay {
            Some(Overlay::VaultCreate { error, .. }) => {
                let msg = error.as_deref().expect("error must be set");
                assert!(
                    msg.to_lowercase().contains("unlock"),
                    "error must tell the user to unlock, got: {msg}"
                );
            }
            other => panic!("overlay must stay open, got {other:?}"),
        }
        // Pressing Esc then Ctrl-U must open VaultUnlock (vault_file_exists == true).
        let _ = update(&mut s, Msg::Text(TextEdit::Cancel));
        let _ = update(&mut s, Msg::Action(Action::VaultUnlock));
        assert!(
            matches!(s.overlay, Some(Overlay::VaultUnlock { .. })),
            "Ctrl-U must open VaultUnlock after already_exists recovery, got {:?}",
            s.overlay
        );
    }

    // Note (shared structural limitation): a late `VaultCreated { Ok(()) }` arriving after the
    // user has closed the VaultCreate overlay and opened a different one will still set
    // `vault_file_exists` and `vault_unlocked` (correct) but will NOT close the new overlay
    // (also correct — we guard with `if matches!(state.overlay, Some(Overlay::VaultCreate { .. }))`).
    // This is the same race the `VaultUnlock` path has. A request-id mechanism would fix both
    // but is out of scope for P4.5. Covered by `vault_created_ok_does_not_clobber_unrelated_overlay`.
    //
    // Note (AI plan suppression): `Action::VaultUnlock` (which opens both VaultCreate and
    // VaultUnlock) is already blocked while `state.ai_pending` or `state.ai_executing` is true
    // (see the guards at the top of `update()`). No additional gate is needed.

    #[test]
    fn vault_created_ok_does_not_clobber_unrelated_overlay() {
        use crate::state::ConnectionChoice;
        const PP: &str = "somepassword1234";
        let mut s = state();
        s.vault_file_exists = false;
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(1),
            label: "local: /".to_owned(),
            ..Default::default()
        }];
        open_vault_create(&mut s);
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        type_text(&mut s, PP);
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // spawns create task
                                                             // User closes create overlay and opens connections before the result arrives.
        let _ = update(&mut s, Msg::Text(TextEdit::Cancel));
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        assert!(matches!(s.overlay, Some(Overlay::Connections { .. })));
        // Late success must not clobber the unrelated overlay.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::VaultCreated {
                result: Ok(()),
                already_exists: false,
            }),
        );
        assert!(
            matches!(s.overlay, Some(Overlay::Connections { .. })),
            "VaultCreated success must not close an unrelated overlay"
        );
        assert!(s.vault_file_exists);
        assert!(s.vault_unlocked);
    }

    #[test]
    fn needs_vault_selection_opens_create_overlay_when_no_vault_file() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.vault_file_exists = false;
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(5),
            label: "s3: my-bucket".to_owned(),
            status: ChoiceStatus::NeedsVault,
            ..Default::default()
        }];
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(fx.is_empty(), "NeedsVault must emit no effects (UI only)");
        match &s.overlay {
            Some(Overlay::VaultCreate { pending_conn, .. }) => {
                assert_eq!(
                    *pending_conn,
                    Some(cairn_types::ConnectionId(5)),
                    "pending_conn must record which connection triggered the create"
                );
            }
            other => panic!("expected VaultCreate overlay, got {other:?}"),
        }
        // per-side slot must be set so ConnectionOpened knows where to navigate.
        assert!(s
            .pending_conn_open
            .contains(&Some(cairn_types::ConnectionId(5))));
    }

    #[test]
    fn needs_vault_selection_opens_unlock_overlay_when_vault_file_exists() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.vault_file_exists = true;
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(5),
            label: "s3: my-bucket".to_owned(),
            status: ChoiceStatus::NeedsVault,
            ..Default::default()
        }];
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(fx.is_empty());
        assert!(
            matches!(s.overlay, Some(Overlay::VaultUnlock { .. })),
            "NeedsVault selection with existing vault file must open the unlock overlay"
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

    // ── RFC-0012 P1: read-only file pager ─────────────────────────────────────────────────────

    /// `Action::View` (F3) opens the pager immediately — no sniff — starting `Loading`/`Text`.
    #[test]
    fn view_action_opens_pager_immediately_in_text_mode() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::View));
        assert!(
            matches!(&fx[..], [AppEffect::OpenPager { skip: 0, .. }]),
            "F3 must emit OpenPager with no skip (no prefetch), got {fx:?}"
        );
        let Some(Overlay::Pager { mode, status, .. }) = &s.overlay else {
            panic!("pager overlay must be open");
        };
        assert_eq!(*mode, PagerMode::Text);
        assert_eq!(*status, PagerStatus::Loading);
    }

    /// F3 on a directory (or `..`) is a no-op — the pager only makes sense on file-like entries.
    #[test]
    fn view_action_on_directory_does_nothing() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("dir", EntryKind::Dir)]);
        let fx = update(&mut s, Msg::Action(Action::View));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
    }

    /// A stale `FileSniffed` for a file the cursor has since moved away from must not pop open a
    /// pager the user didn't ask to see anymore.
    #[test]
    fn stale_file_sniffed_is_ignored() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![
                Entry::new("a.txt", EntryKind::File),
                Entry::new("b.txt", EntryKind::File),
            ],
        );
        let fx = update(&mut s, Msg::Action(Action::Enter)); // sniffs a.txt
        let AppEffect::SniffFile { pane, conn, path } = fx.into_iter().next().unwrap() else {
            panic!("expected SniffFile");
        };
        // The user moves on to b.txt before a.txt's sniff result arrives.
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::FileSniffed {
                pane,
                conn,
                path,
                kind: FileKind::Text,
                prefetch: Bytes::from_static(b"hello"),
            }),
        );
        assert!(fx.is_empty());
        assert!(s.overlay.is_none(), "stale sniff must not open the pager");
    }

    /// RFC-0012 P2: a matching `FileSniffed { kind: Text }` now routes to the in-place editor
    /// (`SuspendAndEdit`) instead of opening the read-only pager — no overlay is opened, the
    /// prefetch bytes are simply discarded (the editor reloads the file itself).
    #[test]
    fn file_sniffed_text_routes_to_edit_not_pager() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::Enter));
        let AppEffect::SniffFile { pane, conn, path } = fx.into_iter().next().unwrap() else {
            panic!("expected SniffFile");
        };
        let expected_path = path.clone();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::FileSniffed {
                pane,
                conn,
                path,
                kind: FileKind::Text,
                prefetch: Bytes::from_static(b"line one\nline two"),
            }),
        );
        assert!(
            matches!(
                &fx[..],
                [AppEffect::SuspendAndEdit { conn: c, path: p }]
                    if *c == conn && *p == expected_path
            ),
            "Text FileSniffed must emit SuspendAndEdit, got {fx:?}"
        );
        assert!(
            s.overlay.is_none(),
            "no pager overlay should open for a text file in P2"
        );
    }

    /// A `Binary` classification opens the pager in `Hex` mode.
    #[test]
    fn file_sniffed_binary_opens_hex_mode() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.bin", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::Enter));
        let AppEffect::SniffFile { pane, conn, path } = fx.into_iter().next().unwrap() else {
            panic!("expected SniffFile");
        };
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::FileSniffed {
                pane,
                conn,
                path,
                kind: FileKind::Binary,
                prefetch: Bytes::from_static(b"\x89PNG\x00\x00"),
            }),
        );
        let Some(Overlay::Pager { mode, .. }) = &s.overlay else {
            panic!("pager overlay must be open");
        };
        assert_eq!(*mode, PagerMode::Hex);
    }

    // ── RFC-0013: archive mount + `..`-out ────────────────────────────────────────────────────

    /// `Enter` on an entry the sniff classifies as an archive mounts it (`MountArchive`) instead
    /// of routing to the editor/pager — no overlay opens here; the mount effect's runner drives
    /// the rest via `AppEvent::ArchiveMounted`.
    #[test]
    fn file_sniffed_archive_emits_mount_archive() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("bundle.zip", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::Enter));
        let AppEffect::SniffFile { pane, conn, path } = fx.into_iter().next().unwrap() else {
            panic!("expected SniffFile");
        };
        let expected_path = path.clone();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::FileSniffed {
                pane,
                conn,
                path,
                kind: FileKind::Archive(crate::state::ArchiveFormat::Zip),
                prefetch: Bytes::from_static(b"PK\x03\x04"),
            }),
        );
        assert!(
            matches!(
                &fx[..],
                [AppEffect::MountArchive { pane: p, conn: c, path: pa }]
                    if *p == pane && *c == conn && *pa == expected_path
            ),
            "Archive FileSniffed must emit MountArchive, got {fx:?}"
        );
        assert!(s.overlay.is_none());
    }

    /// A successful mount pushes the pane's pre-mount `(conn, cwd)` onto `mount_stack` and
    /// navigates the pane into the new connection at its root.
    #[test]
    fn archive_mounted_pushes_frame_and_navigates_into_it() {
        let mut s = state();
        let origin_conn = s.pane(Side::Left).conn;
        let origin_cwd = s.pane(Side::Left).cwd.clone();
        let new_conn = ConnectionId(42);
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::ArchiveMounted {
                pane: Side::Left,
                conn: new_conn,
                root: VfsPath::root(),
            }),
        );
        assert!(matches!(&fx[..], [AppEffect::List { conn, .. }] if *conn == new_conn));
        assert_eq!(s.pane(Side::Left).conn, new_conn);
        assert_eq!(s.pane(Side::Left).cwd, VfsPath::root());
        assert_eq!(s.pane(Side::Left).mount_stack.len(), 1);
        let frame = &s.pane(Side::Left).mount_stack[0];
        assert_eq!(frame.conn, origin_conn);
        assert_eq!(frame.cwd, origin_cwd);
    }

    /// Leaving from the mounted archive's root pops the frame and restores the origin connection
    /// and directory — the end-to-end `..`-out described in RFC-0013.
    #[test]
    fn leave_dir_at_archive_root_pops_mount_frame_and_restores_origin() {
        let mut s = AppState::new(
            ConnectionId(1),
            ConnectionId(2),
            VfsPath::parse("/work").unwrap(),
        );
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("bundle.zip", EntryKind::File)],
        );
        let origin_conn = s.pane(Side::Left).conn;

        let new_conn = ConnectionId(77);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::ArchiveMounted {
                pane: Side::Left,
                conn: new_conn,
                root: VfsPath::root(),
            }),
        );
        assert_eq!(s.pane(Side::Left).conn, new_conn);

        // At the archive's root, Leave must pop the frame rather than no-op.
        let fx = update(&mut s, Msg::Action(Action::Leave));
        assert!(
            !fx.is_empty(),
            "leaving a mounted archive's root must re-list the origin"
        );
        assert_eq!(s.pane(Side::Left).conn, origin_conn);
        assert_eq!(s.pane(Side::Left).cwd.as_str(), "/work");
        assert!(
            s.pane(Side::Left).mount_stack.is_empty(),
            "the frame must be consumed, not left dangling"
        );
    }

    /// A failed mount (no local path, unrecognized archive, or a security cap) leaves the pane
    /// exactly where it was and only updates the status line.
    #[test]
    fn archive_mount_failed_sets_status_without_navigating() {
        let mut s = state();
        let before_conn = s.pane(Side::Left).conn;
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::ArchiveMountFailed {
                pane: Side::Left,
                message: "Copy the archive to a local pane to browse it".to_owned(),
            }),
        );
        assert!(fx.is_empty());
        assert_eq!(s.pane(Side::Left).conn, before_conn);
        assert!(s.pane(Side::Left).mount_stack.is_empty());
        assert!(s.status.unwrap().contains("Copy the archive"));
    }

    /// `SniffFailed` shows a redacted status message and opens no overlay.
    #[test]
    fn sniff_failed_sets_status_without_opening_overlay() {
        let mut s = state();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::SniffFailed {
                message: "permission denied".to_owned(),
            }),
        );
        assert!(fx.is_empty());
        assert!(s.overlay.is_none());
        assert_eq!(s.status.as_deref(), Some("error: permission denied"));
    }

    fn state_with_pager(id: crate::state::PagerId, mode: PagerMode) -> AppState {
        let mut s = state();
        s.overlay = Some(Overlay::Pager {
            id,
            title: "f.txt — view".to_owned(),
            mode,
            lines: std::collections::VecDeque::new(),
            partial: String::new(),
            byte_size: 0,
            total_size: None,
            scroll: 0,
            status: PagerStatus::Loading,
            wrap: true,
        });
        s
    }

    #[test]
    fn pager_chunk_appends_text_lines() {
        let mut s = state_with_pager(1, PagerMode::Text);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 1,
                bytes: Bytes::from_static(b"alpha\nbeta\ngam"),
            }),
        );
        let Some(Overlay::Pager { lines, partial, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(
            lines.iter().map(String::as_str).collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert_eq!(partial, "gam");
    }

    /// A chunk for a different (stale) pager id is ignored.
    #[test]
    fn pager_chunk_with_mismatched_id_is_ignored() {
        let mut s = state_with_pager(1, PagerMode::Text);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 99,
                bytes: Bytes::from_static(b"nope"),
            }),
        );
        let Some(Overlay::Pager { lines, partial, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert!(lines.is_empty());
        assert!(partial.is_empty());
    }

    /// Hitting `PAGER_MAX_BYTES` marks the view `Truncated` and emits `ClosePager` to stop the
    /// stream — no more bytes will ever be shown for this session.
    #[test]
    fn pager_chunk_hitting_cap_truncates_and_closes() {
        let mut s = state_with_pager(3, PagerMode::Text);
        let big = vec![b'x'; PAGER_MAX_BYTES + 10];
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 3,
                bytes: Bytes::from(big),
            }),
        );
        assert!(matches!(&fx[..], [AppEffect::ClosePager { id: 3 }]));
        let Some(Overlay::Pager {
            status, byte_size, ..
        }) = &s.overlay
        else {
            panic!("pager overlay gone");
        };
        assert_eq!(*status, PagerStatus::Truncated);
        assert_eq!(
            *byte_size, PAGER_MAX_BYTES,
            "a single newline-free chunk must be clamped exactly to the cap, not overshoot it"
        );
    }

    /// A single chunk whose cap-crossing point lands in the middle of a multi-byte UTF-8
    /// character must truncate at the preceding char boundary rather than panicking (slicing a
    /// `&str` at a non-boundary index panics) or producing invalid UTF-8.
    #[test]
    fn pager_chunk_cap_truncation_backs_off_to_char_boundary() {
        let mut s = state_with_pager(11, PagerMode::Text);
        // Fill to exactly one byte short of the cap, then send a chunk starting with a 3-byte
        // character ('€', U+20AC) — the cap lands inside its second byte.
        if let Some(Overlay::Pager { byte_size, .. }) = &mut s.overlay {
            *byte_size = PAGER_MAX_BYTES - 1;
        }
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 11,
                bytes: Bytes::from_static("€uro".as_bytes()),
            }),
        );
        assert!(matches!(&fx[..], [AppEffect::ClosePager { id: 11 }]));
        let Some(Overlay::Pager { byte_size, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        // Only 1 byte of budget remained, and '€' needs 3 — the whole character must be dropped
        // rather than splitting it, so byte_size stays at the pre-chunk value.
        assert_eq!(*byte_size, PAGER_MAX_BYTES - 1);
    }

    /// Truncating on the cap must still flush the trailing (newline-free) partial into `lines` —
    /// the runner is cancelled by the `ClosePager`, so the `PagerDone` flush never runs and the
    /// renderer only shows `lines`. Regression: the last screenful of a truncated file was dropped.
    #[test]
    fn pager_chunk_cap_truncation_flushes_partial_line() {
        let mut s = state_with_pager(21, PagerMode::Text);
        let big = vec![b'x'; PAGER_MAX_BYTES + 10];
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 21,
                bytes: Bytes::from(big),
            }),
        );
        assert!(matches!(&fx[..], [AppEffect::ClosePager { id: 21 }]));
        let Some(Overlay::Pager {
            lines,
            partial,
            status,
            ..
        }) = &s.overlay
        else {
            panic!("pager overlay gone");
        };
        assert_eq!(*status, PagerStatus::Truncated);
        assert!(partial.is_empty(), "partial must be flushed on truncation");
        assert_eq!(
            lines.back().map(String::len),
            Some(PAGER_MAX_BYTES),
            "the truncated (newline-free) content must be visible as a flushed line"
        );
    }

    /// F3 opens optimistically in `Text`; a NUL byte in the *first* streamed chunk flips it to
    /// `Hex` so a binary file directly viewed with F3 renders as a hex dump, not garbled `U+FFFD`
    /// text. Documented in the `AppEvent::PagerChunk` contract; regression: the flip was missing.
    #[test]
    fn view_first_chunk_with_nul_flips_text_to_hex() {
        let mut s = state_with_pager(31, PagerMode::Text);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 31,
                bytes: Bytes::from_static(b"\x89PNG\x00\x0d"),
            }),
        );
        let Some(Overlay::Pager { mode, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(*mode, PagerMode::Hex, "first-chunk NUL must flip to Hex");
    }

    /// A NUL appearing only in a *later* chunk of an otherwise-text file must NOT flip the mode —
    /// that would reshuffle already-displayed content. The flip is gated on `byte_size == 0`.
    #[test]
    fn view_later_chunk_nul_does_not_flip_mode() {
        let mut s = state_with_pager(32, PagerMode::Text);
        // First chunk: plain text (advances byte_size past 0).
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 32,
                bytes: Bytes::from_static(b"hello world\n"),
            }),
        );
        // Second chunk carries a NUL — too late to flip.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 32,
                bytes: Bytes::from_static(b"tail\x00"),
            }),
        );
        let Some(Overlay::Pager { mode, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(*mode, PagerMode::Text, "a late NUL must not flip the mode");
    }

    /// A late `PagerDone` (clean EOF for a file sized exactly at the cap) must not clobber a
    /// `Truncated` status already set by the cap path back to `Ready`.
    #[test]
    fn pager_done_does_not_clobber_truncated() {
        let mut s = state_with_pager(33, PagerMode::Text);
        // Drive it to `Truncated` via the cap path.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 33,
                bytes: Bytes::from(vec![b'x'; PAGER_MAX_BYTES + 1]),
            }),
        );
        // A stray clean-EOF PagerDone arrives afterwards.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::PagerDone {
                id: 33,
                error: None,
                truncated: false,
            }),
        );
        let Some(Overlay::Pager { status, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(*status, PagerStatus::Truncated, "Truncated must stick");
    }

    /// Opening a pager while one is already open (e.g. F3 opened one, then an in-flight Enter sniff
    /// on a *binary* file resolves) must emit `ClosePager` for the superseded id so its stream
    /// can't be orphaned into an uncapped, uncancellable read. (Since RFC-0012 P2 a `Text` result
    /// no longer opens a pager at all — see `file_sniffed_text_routes_to_edit_not_pager` — so this
    /// exercises the supersede path with `FileKind::Binary`, the only kind that still does.)
    #[test]
    fn opening_pager_over_existing_closes_the_old() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        let fx = update(&mut s, Msg::Action(Action::Enter));
        let AppEffect::SniffFile { pane, conn, path } = fx.into_iter().next().unwrap() else {
            panic!("expected SniffFile");
        };
        // Simulate a pager already open (id 7) — e.g. the user pressed F3 first.
        s.overlay = Some(Overlay::Pager {
            id: 7,
            title: "f.txt — view".to_owned(),
            mode: PagerMode::Text,
            lines: std::collections::VecDeque::new(),
            partial: String::new(),
            byte_size: 0,
            total_size: None,
            scroll: 0,
            status: PagerStatus::Loading,
            wrap: true,
        });
        s.next_pager_id = 8;
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::FileSniffed {
                pane,
                conn,
                path,
                kind: FileKind::Binary,
                prefetch: Bytes::new(),
            }),
        );
        assert!(
            matches!(
                &fx[..],
                [
                    AppEffect::ClosePager { id: 7 },
                    AppEffect::OpenPager { id: 8, .. }
                ]
            ),
            "superseding a pager must close the old stream, got {fx:?}"
        );
        let Some(Overlay::Pager { id, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(*id, 8, "overlay must be the new pager");
    }

    /// F3 on a `Special` entry (FIFO/device/socket) must not open a pager — reading it can block
    /// forever before the cancellation token is polled. Mirrors the Enter sniff's kind guard.
    #[test]
    fn view_action_on_special_entry_does_nothing() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("pipe", EntryKind::Special)],
        );
        let fx = update(&mut s, Msg::Action(Action::View));
        assert!(fx.is_empty());
        assert!(
            s.overlay.is_none(),
            "F3 on a special node must not open a pager"
        );
    }

    // ── RFC-0012 P2: in-place editor launch ───────────────────────────────────────────────────

    /// `Action::Edit` (F4) on a file emits `SuspendAndEdit` with the entry's connection and path —
    /// no sniff, no overlay (the runtime owns the terminal suspend/resume and editor spawn).
    #[test]
    fn edit_action_emits_suspend_and_edit() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        let expected_conn = s.active().conn;
        let fx = update(&mut s, Msg::Action(Action::Edit));
        assert!(
            matches!(
                &fx[..],
                [AppEffect::SuspendAndEdit { conn, path }]
                    if *conn == expected_conn && path.as_str().ends_with("f.txt")
            ),
            "F4 must emit SuspendAndEdit, got {fx:?}"
        );
        assert!(s.overlay.is_none(), "F4 opens no overlay");
    }

    /// F4 on a directory (or `..`) is a no-op — mirrors F3's directory guard.
    #[test]
    fn edit_action_on_directory_does_nothing() {
        let mut s = state();
        deliver(&mut s, Side::Left, vec![Entry::new("dir", EntryKind::Dir)]);
        let fx = update(&mut s, Msg::Action(Action::Edit));
        assert!(fx.is_empty());
        assert_eq!(s.status.as_deref(), Some("Cannot edit a directory"));
    }

    /// F4 on a `Special` entry (FIFO/device/socket) is refused, mirroring the pager/sniff guard.
    #[test]
    fn edit_action_on_special_entry_does_nothing() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("pipe", EntryKind::Special)],
        );
        let fx = update(&mut s, Msg::Action(Action::Edit));
        assert!(fx.is_empty());
        assert_eq!(s.status.as_deref(), Some("Cannot edit this entry"));
    }

    /// A successful `EditFinished` sets the status and refreshes the active pane's listing.
    #[test]
    fn edit_finished_success_sets_status_and_refreshes() {
        let mut s = state();
        deliver(
            &mut s,
            Side::Left,
            vec![Entry::new("f.txt", EntryKind::File)],
        );
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::EditFinished {
                status: "edited f.txt".to_owned(),
                error: false,
            }),
        );
        assert_eq!(s.status.as_deref(), Some("edited f.txt"));
        assert!(
            matches!(&fx[..], [AppEffect::List { .. }]),
            "a successful edit must refresh the active pane, got {fx:?}"
        );
    }

    /// A failed `EditFinished` (e.g. no `$EDITOR`, connection unavailable) sets the status but does
    /// NOT refresh — nothing changed on disk.
    #[test]
    fn edit_finished_failure_sets_status_without_refresh() {
        let mut s = state();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::EditFinished {
                status: "connection unavailable".to_owned(),
                error: true,
            }),
        );
        assert_eq!(s.status.as_deref(), Some("connection unavailable"));
        assert!(fx.is_empty(), "a failed edit must not refresh, got {fx:?}");
    }

    // ── RFC-0012 P3: remote file edit — download / conflict-check / write-back ───────────────

    fn remote_version_etag(tag: &str) -> crate::RemoteVersion {
        crate::RemoteVersion::ETag(tag.to_owned())
    }

    /// `RemoteEditNeedsDownload` mints a fresh `RemoteEditId` (starting at 1) and emits
    /// `DownloadForEdit` carrying the same connection/path/version/size.
    #[test]
    fn remote_edit_needs_download_mints_id_and_emits_download() {
        let mut s = state();
        let path = VfsPath::parse("/notes.txt").unwrap();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::RemoteEditNeedsDownload {
                conn: ConnectionId(1),
                path: path.clone(),
                v0: remote_version_etag("v1"),
                size: 128,
                orig_perms: None,
            }),
        );
        assert!(
            matches!(
                &fx[..],
                [AppEffect::DownloadForEdit { id: 1, conn: ConnectionId(1), path: p, v0, size: 128, .. }]
                    if *p == path && *v0 == remote_version_etag("v1")
            ),
            "got {fx:?}"
        );
        assert_eq!(s.next_remote_edit_id, 2, "the id counter must advance");
        assert!(s.status.as_deref().unwrap_or("").contains("notes.txt"));
    }

    /// A second `RemoteEditNeedsDownload` mints a distinct, higher id — sessions never collide.
    #[test]
    fn remote_edit_needs_download_ids_are_distinct() {
        let mut s = state();
        let path = VfsPath::parse("/a.txt").unwrap();
        let first = update(
            &mut s,
            Msg::Event(AppEvent::RemoteEditNeedsDownload {
                conn: ConnectionId(1),
                path: path.clone(),
                v0: remote_version_etag("v1"),
                size: 10,
                orig_perms: None,
            }),
        );
        let second = update(
            &mut s,
            Msg::Event(AppEvent::RemoteEditNeedsDownload {
                conn: ConnectionId(1),
                path,
                v0: remote_version_etag("v2"),
                size: 20,
                orig_perms: None,
            }),
        );
        let AppEffect::DownloadForEdit { id: id1, .. } = &first[0] else {
            panic!()
        };
        let AppEffect::DownloadForEdit { id: id2, .. } = &second[0] else {
            panic!()
        };
        assert_ne!(id1, id2);
    }

    /// `RemoteEditDownloaded` immediately hands off to the editor via `EditRemoteTemp`.
    #[test]
    fn remote_edit_downloaded_opens_editor() {
        let mut s = state();
        let path = VfsPath::parse("/notes.txt").unwrap();
        let temp_path = std::path::PathBuf::from("/tmp/.cairn-edit-x/notes.txt");
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::RemoteEditDownloaded {
                id: 5,
                conn: ConnectionId(1),
                path: path.clone(),
                temp_path: temp_path.clone(),
                v0: remote_version_etag("v1"),
                orig_size: 64,
                orig_perms: None,
                download_hash: [1u8; 32],
            }),
        );
        assert!(
            matches!(
                &fx[..],
                [AppEffect::EditRemoteTemp {
                    id: 5, path: p, temp_path: t, orig_size: 64, download_hash, hash, ..
                }]
                    if *p == path && *t == temp_path && *download_hash == [1u8; 32] && *hash == [1u8; 32]
            ),
            "the first edit round's pre-round hash must equal the download hash; got {fx:?}"
        );
    }

    /// A no-op edit sets a status message and emits no effects (nothing to write back, no refresh).
    #[test]
    fn remote_edit_no_change_sets_status_only() {
        let mut s = state();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::RemoteEditNoChange {
                id: 5,
                name: "notes.txt".to_owned(),
            }),
        );
        assert!(fx.is_empty());
        assert!(s.status.as_deref().unwrap_or("").contains("No changes"));
    }

    /// A modified edit routes into `WriteBack` in `CheckThenWrite` mode — the conflict re-check
    /// always runs first for a fresh (non-user-confirmed) write-back attempt.
    #[test]
    fn remote_edit_modified_emits_check_then_write() {
        let mut s = state();
        let path = VfsPath::parse("/notes.txt").unwrap();
        let temp_path = std::path::PathBuf::from("/tmp/.cairn-edit-x/notes.txt");
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::RemoteEditModified {
                id: 5,
                conn: ConnectionId(1),
                path: path.clone(),
                temp_path: temp_path.clone(),
                v0: remote_version_etag("v1"),
                orig_size: 64,
                orig_perms: None,
                download_hash: [1u8; 32],
                hash: [2u8; 32],
            }),
        );
        assert!(
            matches!(
                &fx[..],
                [AppEffect::WriteBack { id: 5, path: p, temp_path: t, orig_size: 64, download_hash, mode: WriteBackMode::CheckThenWrite, .. }]
                    if *p == path && *t == temp_path && *download_hash == [1u8; 32]
            ),
            "got {fx:?}"
        );
    }

    /// `RemoteEditFailed` just sets the status — no effects, no overlay.
    #[test]
    fn remote_edit_failed_sets_status_only() {
        let mut s = state();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::RemoteEditFailed {
                id: 5,
                status: "download failed: timed out".to_owned(),
            }),
        );
        assert!(fx.is_empty());
        assert_eq!(s.status.as_deref(), Some("download failed: timed out"));
    }

    /// `WriteBackDone` sets a status message and refreshes the active pane.
    #[test]
    fn writeback_done_refreshes_active_pane() {
        let mut s = state();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::WriteBackDone {
                id: 5,
                name: "notes.txt".to_owned(),
            }),
        );
        assert!(s.status.as_deref().unwrap_or("").contains("notes.txt"));
        assert!(matches!(&fx[..], [AppEffect::List { .. }]));
    }

    fn confirm_writeback_overlay(
        cursor: usize,
        reason: crate::WritebackConflictReason,
    ) -> AppState {
        let mut s = state();
        s.overlay = Some(Overlay::ConfirmWriteback {
            id: 5,
            conn: ConnectionId(1),
            path: VfsPath::parse("/notes.txt").unwrap(),
            temp_path: std::path::PathBuf::from("/tmp/.cairn-edit-x/notes.txt"),
            v0: remote_version_etag("v1"),
            orig_size: 64,
            orig_perms: None,
            download_hash: [1u8; 32],
            hash: [3u8; 32],
            reason,
            cursor,
        });
        s
    }

    /// `WriteBackConflict` opens `Overlay::ConfirmWriteback` with the cursor at 0 (Overwrite).
    #[test]
    fn writeback_conflict_opens_overlay() {
        let mut s = state();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::WriteBackConflict {
                id: 5,
                conn: ConnectionId(1),
                path: VfsPath::parse("/notes.txt").unwrap(),
                temp_path: std::path::PathBuf::from("/tmp/.cairn-edit-x/notes.txt"),
                v0: remote_version_etag("v1"),
                orig_size: 64,
                orig_perms: None,
                download_hash: [1u8; 32],
                hash: [3u8; 32],
                reason: crate::WritebackConflictReason::RemoteChanged,
            }),
        );
        assert!(fx.is_empty());
        assert!(matches!(
            s.overlay,
            Some(Overlay::ConfirmWriteback { cursor: 0, .. })
        ));
    }

    /// Cursor navigation clamps at both ends of `WritebackChoice::ALL`.
    #[test]
    fn confirm_writeback_cursor_clamps() {
        let mut s = confirm_writeback_overlay(0, crate::WritebackConflictReason::RemoteChanged);
        let _ = update(&mut s, Msg::Action(Action::CursorUp));
        assert!(matches!(
            s.overlay,
            Some(Overlay::ConfirmWriteback { cursor: 0, .. })
        ));
        for _ in 0..10 {
            let _ = update(&mut s, Msg::Action(Action::CursorDown));
        }
        assert!(matches!(
            s.overlay,
            Some(Overlay::ConfirmWriteback { cursor: 3, .. })
        ));
    }

    /// Confirming "Overwrite" emits `WriteBack` in `ForceOverwrite` mode and closes the overlay.
    #[test]
    fn confirm_writeback_overwrite_forces_write() {
        let mut s = confirm_writeback_overlay(0, crate::WritebackConflictReason::RemoteChanged);
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(s.overlay.is_none());
        assert!(matches!(
            &fx[..],
            [AppEffect::WriteBack {
                mode: WriteBackMode::ForceOverwrite,
                ..
            }]
        ));
    }

    /// Confirming "Save as a new file" emits `WriteBack` in `SaveAsSibling` mode.
    #[test]
    fn confirm_writeback_save_as_emits_sibling_mode() {
        let mut s = confirm_writeback_overlay(1, crate::WritebackConflictReason::RemoteChanged);
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(s.overlay.is_none());
        assert!(matches!(
            &fx[..],
            [AppEffect::WriteBack {
                mode: WriteBackMode::SaveAsSibling,
                ..
            }]
        ));
    }

    /// Confirming "Keep editing" re-emits `EditRemoteTemp` on the same temp file, carrying the
    /// overlay's `hash` forward as the new pre-round baseline — and, critically, `download_hash`
    /// forward **unchanged** as the stable whole-session no-op baseline (the bug this regression
    /// guards: an earlier implementation used the conflict-time hash as the no-op baseline, which
    /// silently discarded an edit that stayed unchanged across a `KeepEditing` re-open — see
    /// `crates/cairn/src/app.rs`'s `keep_editing_after_conflict_does_not_discard_the_edit`).
    #[test]
    fn confirm_writeback_keep_editing_reopens_editor() {
        let mut s = confirm_writeback_overlay(2, crate::WritebackConflictReason::RemoteChanged);
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(s.overlay.is_none());
        assert!(matches!(
            &fx[..],
            [AppEffect::EditRemoteTemp { download_hash, hash, .. }]
                if *download_hash == [1u8; 32] && *hash == [3u8; 32]
        ));
    }

    /// Confirming "Discard" emits `CancelRemoteEdit` and sets a status message — nothing written.
    #[test]
    fn confirm_writeback_discard_cancels() {
        let mut s = confirm_writeback_overlay(3, crate::WritebackConflictReason::RemoteChanged);
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(s.overlay.is_none());
        assert!(matches!(&fx[..], [AppEffect::CancelRemoteEdit { id: 5 }]));
        assert!(s.status.as_deref().unwrap_or("").contains("discarded"));
    }

    /// Esc (`Action::Cancel`) is deliberately mapped to the same effect as "Keep editing" — never
    /// silently discarding or overwriting.
    #[test]
    fn confirm_writeback_escape_keeps_editing_not_discards() {
        let mut s = confirm_writeback_overlay(3, crate::WritebackConflictReason::ZeroLengthGuard);
        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(s.overlay.is_none());
        assert!(
            matches!(&fx[..], [AppEffect::EditRemoteTemp { .. }]),
            "Esc must behave like KeepEditing, got {fx:?}"
        );
    }

    /// The zero-length-guard conflict reason renders through the same overlay/flow as a remote
    /// version change — a pure data-shape check (rendering is covered by the cairn-tui snapshot).
    #[test]
    fn zero_length_guard_reason_also_opens_overlay() {
        let mut s = state();
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::WriteBackConflict {
                id: 9,
                conn: ConnectionId(1),
                path: VfsPath::parse("/big.bin").unwrap(),
                temp_path: std::path::PathBuf::from("/tmp/.cairn-edit-y/big.bin"),
                v0: remote_version_etag("v1"),
                orig_size: 1024,
                orig_perms: None,
                download_hash: [1u8; 32],
                hash: [0u8; 32],
                reason: crate::WritebackConflictReason::ZeroLengthGuard,
            }),
        );
        assert!(fx.is_empty());
        assert!(matches!(
            s.overlay,
            Some(Overlay::ConfirmWriteback {
                reason: crate::WritebackConflictReason::ZeroLengthGuard,
                ..
            })
        ));
    }

    /// `advance_queue` must not start a queued transfer while `ConfirmWriteback` is open — it's a
    /// modal demanding immediate attention, like `ConfirmOverwrite`.
    #[test]
    fn confirm_writeback_blocks_queued_transfers() {
        let mut s = confirm_writeback_overlay(0, crate::WritebackConflictReason::RemoteChanged);
        s.concurrency_limit = 1;
        s.transfer_queue.push_back(QueuedTransfer {
            src_conn: ConnectionId(1),
            dst_conn: ConnectionId(2),
            items: vec![(VfsPath::parse("/a").unwrap(), VfsPath::parse("/b").unwrap())],
            is_move: false,
        });
        let fx = update(&mut s, Msg::Tick);
        assert!(
            fx.is_empty(),
            "queue must stay blocked while ConfirmWriteback is open, got {fx:?}"
        );
    }

    /// `PagerDone` flushes a trailing (incomplete) line into `lines` and sets `Ready`.
    #[test]
    fn pager_done_flushes_partial_and_sets_ready() {
        let mut s = state_with_pager(2, PagerMode::Text);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::PagerChunk {
                id: 2,
                bytes: Bytes::from_static(b"only line, no trailing newline"),
            }),
        );
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::PagerDone {
                id: 2,
                error: None,
                truncated: false,
            }),
        );
        assert!(fx.is_empty());
        let Some(Overlay::Pager {
            lines,
            partial,
            status,
            ..
        }) = &s.overlay
        else {
            panic!("pager overlay gone");
        };
        assert_eq!(*status, PagerStatus::Ready);
        assert!(partial.is_empty(), "partial must be flushed into lines");
        assert_eq!(
            lines.back().map(String::as_str),
            Some("only line, no trailing newline")
        );
    }

    /// `PagerDone` with an error sets `PagerStatus::Error` with the redacted message.
    #[test]
    fn pager_done_with_error_sets_error_status() {
        let mut s = state_with_pager(4, PagerMode::Text);
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::PagerDone {
                id: 4,
                error: Some("connection reset".to_owned()),
                truncated: false,
            }),
        );
        let Some(Overlay::Pager { status, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(*status, PagerStatus::Error("connection reset".to_owned()));
    }

    /// Cursor/page scroll actions move `scroll` within `[0, lines.len())`, and `Cancel` closes the
    /// overlay while emitting `ClosePager` to stop the stream.
    #[test]
    fn pager_scroll_and_cancel() {
        let mut s = state_with_pager(9, PagerMode::Text);
        if let Some(Overlay::Pager { lines, .. }) = &mut s.overlay {
            for i in 0..50 {
                lines.push_back(format!("line {i}"));
            }
        }
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let Some(Overlay::Pager { scroll, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(*scroll, 1);

        let _ = update(&mut s, Msg::Action(Action::PageDown));
        let Some(Overlay::Pager { scroll, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(*scroll, 21);

        let _ = update(&mut s, Msg::Action(Action::CursorTop));
        let Some(Overlay::Pager { scroll, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(*scroll, 0);

        let _ = update(&mut s, Msg::Action(Action::CursorBottom));
        let Some(Overlay::Pager { scroll, .. }) = &s.overlay else {
            panic!("pager overlay gone");
        };
        assert_eq!(*scroll, 49);

        let fx = update(&mut s, Msg::Action(Action::Cancel));
        assert!(matches!(&fx[..], [AppEffect::ClosePager { id: 9 }]));
        assert!(s.overlay.is_none());
    }

    // ── P2: lazy connection-open routing ──────────────────────────────────────────────────────

    /// Selecting a NeedsOpen connection emits OpenConnection and records the pending intent.
    #[test]
    fn needs_open_selection_emits_open_connection() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(5),
            label: "ssh: dev".to_owned(),
            status: ChoiceStatus::NeedsOpen,
            ..Default::default()
        }];
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(
            matches!(&fx[..], [AppEffect::OpenConnection { conn }] if *conn == cairn_types::ConnectionId(5)),
            "NeedsOpen selection must emit OpenConnection, got {fx:?}"
        );
        assert!(s.overlay.is_none(), "overlay is closed after selection");
        assert_eq!(
            s.pending_conn_open[Side::Left.index()],
            Some(cairn_types::ConnectionId(5))
        );
    }

    /// Selecting a NeedsVault connection opens the vault-unlock overlay with the conn recorded
    /// (only when the vault file already exists; otherwise VaultCreate opens instead).
    #[test]
    fn needs_vault_selection_opens_vault_unlock_overlay_with_pending_conn() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.vault_file_exists = true; // existing vault → unlock flow
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(6),
            label: "s3: prod".to_owned(),
            status: ChoiceStatus::NeedsVault,
            ..Default::default()
        }];
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(fx.is_empty(), "NeedsVault must emit no effects");
        match &s.overlay {
            Some(Overlay::VaultUnlock { pending_conn, .. }) => {
                assert_eq!(
                    *pending_conn,
                    Some(cairn_types::ConnectionId(6)),
                    "pending_conn must record the triggering connection"
                );
            }
            other => panic!("expected VaultUnlock overlay, got {other:?}"),
        }
        assert_eq!(
            s.pending_conn_open[Side::Left.index()],
            Some(cairn_types::ConnectionId(6))
        );
    }

    /// Selecting an Unreachable connection retries the open (emits OpenConnection), since the
    /// failure may have been transient and the descriptor is still available.
    #[test]
    fn unreachable_selection_emits_retry_open_connection() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(7),
            label: "docker: local".to_owned(),
            status: ChoiceStatus::Unreachable,
            ..Default::default()
        }];
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(
            matches!(
                &fx[..],
                [AppEffect::OpenConnection { conn }] if *conn == cairn_types::ConnectionId(7)
            ),
            "Unreachable retry must emit OpenConnection, got {fx:?}"
        );
        assert!(s.overlay.is_none(), "connections overlay is closed");
        assert!(
            s.status.as_deref().is_some_and(|m| m.contains("Retry")),
            "status must mention retry"
        );
        assert_eq!(
            s.pending_conn_open[Side::Left.index()],
            Some(cairn_types::ConnectionId(7)),
            "pending_conn_open[Left] is set for the retry"
        );
    }

    /// ConnectionOpened Ok: flips the choice to Ready and navigates the pending pane.
    #[test]
    fn connection_opened_ok_flips_ready_and_navigates() {
        use crate::state::{ConnectionChoice, Listing};
        let mut s = state();
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(5),
            label: "ssh: dev".to_owned(),
            status: ChoiceStatus::NeedsOpen,
            ..Default::default()
        }];
        // Simulate having selected the connection (pending set by the reducer on NeedsOpen select).
        s.pending_conn_open[Side::Left.index()] = Some(cairn_types::ConnectionId(5));
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionOpened {
                conn: cairn_types::ConnectionId(5),
                result: Ok(()),
            }),
        );
        assert_eq!(s.connections[0].status, ChoiceStatus::Ready);
        assert!(
            s.pending_conn_open.iter().all(|s| s.is_none()),
            "pending is cleared"
        );
        assert_eq!(s.active().conn, cairn_types::ConnectionId(5));
        assert!(
            matches!(s.active().listing, Listing::Loading),
            "pane is loading after navigate"
        );
        assert!(
            matches!(&fx[..], [AppEffect::List { conn, .. }] if *conn == cairn_types::ConnectionId(5)),
            "must emit List effect for the new connection"
        );
    }

    /// ConnectionOpened Err: flips the choice to Unreachable and shows an error status.
    #[test]
    fn connection_opened_err_sets_unreachable() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(5),
            label: "ssh: dev".to_owned(),
            status: ChoiceStatus::NeedsOpen,
            ..Default::default()
        }];
        s.pending_conn_open[Side::Left.index()] = Some(cairn_types::ConnectionId(5));
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionOpened {
                conn: cairn_types::ConnectionId(5),
                result: Err("connection refused".to_owned()),
            }),
        );
        assert!(fx.is_empty());
        assert_eq!(s.connections[0].status, ChoiceStatus::Unreachable);
        assert!(
            s.pending_conn_open.iter().all(|s| s.is_none()),
            "pending is cleared on error"
        );
        assert!(
            s.status
                .as_deref()
                .is_some_and(|m| m.contains("Failed to open connection")),
            "status must mention the failure"
        );
    }

    /// After vault unlock, the connection that triggered the unlock is auto-opened.
    #[test]
    fn vault_unlock_success_auto_opens_triggering_connection() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.has_locked_connections = true;
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(8),
            label: "s3: prod".to_owned(),
            status: ChoiceStatus::NeedsVault,
            ..Default::default()
        }];
        // Simulate: user selected the s3 conn, was redirected to VaultUnlock overlay.
        s.pending_conn_open[Side::Left.index()] = Some(cairn_types::ConnectionId(8));
        s.overlay = Some(Overlay::VaultUnlock {
            input: MaskedInput::new(),
            error: None,
            pending_conn: Some(cairn_types::ConnectionId(8)),
            pending_save: None,
        });
        s.vault_unlocking = true;
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::VaultUnlocked { result: Ok(()) }),
        );
        // Must emit OpenConnection for the triggering connection.
        assert!(
            matches!(&fx[..], [AppEffect::OpenConnection { conn }] if *conn == cairn_types::ConnectionId(8)),
            "unlock must auto-open the triggering conn, got {fx:?}"
        );
        assert!(s.overlay.is_none(), "unlock overlay is closed");
        assert_eq!(s.connections[0].status, ChoiceStatus::NeedsOpen);
    }

    /// Regression for Finding 1: selecting a Ready connection on the same pane as an in-flight
    /// NeedsOpen must clear `pending_conn_open` so the delayed ConnectionOpened does not hijack
    /// the pane after the user has already navigated elsewhere.
    #[test]
    fn ready_selection_clears_pending_open_for_same_side() {
        use crate::state::{ConnectionChoice, Listing};
        let mut s = state();
        s.connections = vec![
            ConnectionChoice {
                conn: cairn_types::ConnectionId(5),
                label: "ssh: dev".to_owned(),
                status: ChoiceStatus::NeedsOpen,
                ..Default::default()
            },
            ConnectionChoice {
                conn: cairn_types::ConnectionId(6),
                label: "/ (local)".to_owned(),
                status: ChoiceStatus::Ready,
                ..Default::default()
            },
        ];
        // Simulate: user previously selected NeedsOpen conn 5 — pending was set for Left.
        s.pending_conn_open[Side::Left.index()] = Some(cairn_types::ConnectionId(5));
        // User now selects Ready conn 6 on the same Left pane.
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let _ = update(&mut s, Msg::Action(Action::CursorDown)); // cursor → index 1 (conn 6)
        let nav_fx = update(&mut s, Msg::Action(Action::Confirm));
        // The per-side slot for Left (conn 5) must be cleared.
        assert!(
            s.pending_conn_open[Side::Left.index()].is_none(),
            "Ready navigation on same side must clear the stale Left pending_conn_open slot"
        );
        // A List for conn 6 must have been emitted (navigate happened).
        assert!(
            matches!(&nav_fx[..], [AppEffect::List { conn, .. }] if *conn == cairn_types::ConnectionId(6)),
            "Ready navigation must emit List for the new conn, got {nav_fx:?}"
        );
        // Now the delayed ConnectionOpened{5, Ok} arrives — it must be a no-op for navigation.
        let late_fx = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionOpened {
                conn: cairn_types::ConnectionId(5),
                result: Ok(()),
            }),
        );
        assert!(
            late_fx.is_empty(),
            "stale ConnectionOpened must not emit navigate effects"
        );
        assert!(
            matches!(s.active().listing, Listing::Loading),
            "pane is still loading for conn 6"
        );
        assert_eq!(
            s.active().conn,
            cairn_types::ConnectionId(6),
            "pane must remain on conn 6, not be hijacked to conn 5"
        );
    }

    /// Regression for Finding 6: cancelling the vault-unlock overlay (opened via a NeedsVault
    /// selection) must clear `pending_conn_open` — no OpenConnection was emitted, so no event
    /// will arrive to consume the slot.
    #[test]
    fn vault_unlock_cancel_clears_pending_conn_open() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(9),
            label: "ssh: bastion".to_owned(),
            status: ChoiceStatus::NeedsVault,
            ..Default::default()
        }];
        let _ = update(&mut s, Msg::Action(Action::OpenConnections));
        let fx = update(&mut s, Msg::Action(Action::Confirm));
        assert!(fx.is_empty(), "NeedsVault emits no effect");
        assert!(
            s.pending_conn_open.iter().any(|s| s.is_some()),
            "pending_conn_open must be set after NeedsVault selection"
        );
        // User presses Esc to cancel the vault-unlock overlay.
        let fx = update(&mut s, Msg::Text(TextEdit::Cancel));
        assert!(fx.is_empty());
        assert!(s.overlay.is_none(), "overlay must be closed");
        assert!(
            s.pending_conn_open.iter().all(|s| s.is_none()),
            "pending_conn_open must be cleared when vault-unlock is cancelled"
        );
    }

    /// Regression for Finding 5: a ConnectionOpened error for a superseded (stale) open must
    /// not overwrite the status line the user is currently looking at.
    #[test]
    fn superseded_connection_error_does_not_pollute_status() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.connections = vec![ConnectionChoice {
            conn: cairn_types::ConnectionId(5),
            label: "ssh: dev".to_owned(),
            status: ChoiceStatus::NeedsOpen,
            ..Default::default()
        }];
        // pending_conn_open cleared — the user navigated elsewhere (e.g. via the per-side fix).
        s.pending_conn_open = [None; 2];
        s.status = Some("Opened / (local)".to_owned());
        // A delayed error from the background open of conn 5 arrives.
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionOpened {
                conn: cairn_types::ConnectionId(5),
                result: Err("network timeout".to_owned()),
            }),
        );
        assert!(fx.is_empty());
        // The badge is still flipped to Unreachable (always unconditional).
        assert_eq!(s.connections[0].status, ChoiceStatus::Unreachable);
        // The status line must NOT be overwritten by the stale background error.
        assert_eq!(
            s.status.as_deref(),
            Some("Opened / (local)"),
            "superseded background error must not overwrite the active status"
        );
    }

    /// Regression for CR-5: ConnectionOpened Ok for a *different* conn than `pending_conn_open`
    /// must restore the slot and emit no navigate effects.
    #[test]
    fn connection_opened_ok_for_different_pending_does_not_navigate() {
        use crate::state::ConnectionChoice;
        let mut s = state();
        s.connections = vec![
            ConnectionChoice {
                conn: cairn_types::ConnectionId(5),
                label: "ssh: dev".to_owned(),
                status: ChoiceStatus::NeedsOpen,
                ..Default::default()
            },
            ConnectionChoice {
                conn: cairn_types::ConnectionId(6),
                label: "sftp: prod".to_owned(),
                status: ChoiceStatus::NeedsOpen,
                ..Default::default()
            },
        ];
        // pending_conn_open: Left=conn6, Right=empty.
        s.pending_conn_open[Side::Left.index()] = Some(cairn_types::ConnectionId(6));
        // ConnectionOpened arrives for conn 5 (a different in-flight open with no pending slot).
        let fx = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionOpened {
                conn: cairn_types::ConnectionId(5),
                result: Ok(()),
            }),
        );
        // No navigate effects for conn 5 (no pending slot matched).
        assert!(
            fx.is_empty(),
            "different-conn Ok must emit no navigate effects, got {fx:?}"
        );
        // Conn 5 is now Ready in the switcher.
        assert_eq!(s.connections[0].status, ChoiceStatus::Ready);
        // The pending slot for conn 6 on Left must be preserved.
        assert_eq!(
            s.pending_conn_open[Side::Left.index()],
            Some(cairn_types::ConnectionId(6)),
            "pending slot for conn 6 must survive an unrelated ConnectionOpened"
        );
        assert_eq!(
            s.pending_conn_open[Side::Right.index()],
            None,
            "Right slot was never set"
        );
    }

    /// Validates the per-side pending slots: simultaneous in-flight opens on both panes each
    /// navigate the correct side and clear only their own slot.
    #[test]
    fn per_side_pending_both_open_navigate_correct_sides() {
        use crate::state::{ConnectionChoice, Listing};
        let mut s = state();
        s.connections = vec![
            ConnectionChoice {
                conn: cairn_types::ConnectionId(10),
                label: "ssh: left-server".to_owned(),
                status: ChoiceStatus::NeedsOpen,
                ..Default::default()
            },
            ConnectionChoice {
                conn: cairn_types::ConnectionId(11),
                label: "ssh: right-server".to_owned(),
                status: ChoiceStatus::NeedsOpen,
                ..Default::default()
            },
        ];
        // Two concurrent in-flight opens — one per pane.
        s.pending_conn_open[Side::Left.index()] = Some(cairn_types::ConnectionId(10));
        s.pending_conn_open[Side::Right.index()] = Some(cairn_types::ConnectionId(11));

        // Left open completes first.
        let fx_a = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionOpened {
                conn: cairn_types::ConnectionId(10),
                result: Ok(()),
            }),
        );
        assert!(
            matches!(&fx_a[..], [AppEffect::List { conn, pane: Side::Left, .. }] if *conn == cairn_types::ConnectionId(10)),
            "Left open must navigate Left pane, got {fx_a:?}"
        );
        // Left slot is cleared; Right slot survives.
        assert_eq!(s.pending_conn_open[Side::Left.index()], None);
        assert_eq!(
            s.pending_conn_open[Side::Right.index()],
            Some(cairn_types::ConnectionId(11)),
            "Right slot must not be disturbed by Left's ConnectionOpened"
        );

        // Right open completes next.
        let fx_b = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionOpened {
                conn: cairn_types::ConnectionId(11),
                result: Ok(()),
            }),
        );
        assert!(
            matches!(&fx_b[..], [AppEffect::List { conn, pane: Side::Right, .. }] if *conn == cairn_types::ConnectionId(11)),
            "Right open must navigate Right pane, got {fx_b:?}"
        );
        // Both slots are now cleared.
        assert!(
            s.pending_conn_open.iter().all(|s| s.is_none()),
            "both slots must be cleared after both opens complete"
        );
        // Pane listing states: both should be Loading (navigate_to_conn resets to Loading).
        assert!(matches!(s.pane(Side::Left).listing, Listing::Loading));
        assert!(matches!(s.pane(Side::Right).listing, Listing::Loading));
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
                text: "hello\nworld\n".to_owned(),
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
                text: "hel".to_owned(),
            }),
        );
        let _ = update(
            &mut state,
            Msg::Event(AppEvent::SessionOutput {
                id,
                text: "lo\n".to_owned(),
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
                    text: chunk.clone(),
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

    #[test]
    fn append_to_ring_oom_guard_caps_partial() {
        use crate::state::{SESSION_OUTPUT_MAX_BYTES, SESSION_OUTPUT_MAX_LINES};
        let mut lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        let mut partial = String::new();
        let mut byte_size: usize = 0;
        // One chunk of max_bytes + 1 bytes with no newline — must not let partial grow unbounded.
        let big = "x".repeat(SESSION_OUTPUT_MAX_BYTES + 1);
        append_to_ring(
            &mut lines,
            &mut partial,
            &mut byte_size,
            &big,
            SESSION_OUTPUT_MAX_LINES,
            SESSION_OUTPUT_MAX_BYTES,
        );
        // After the OOM guard fires, the total retained bytes must be within the cap.
        let retained = lines.iter().map(|l| l.len() + 1).sum::<usize>() + partial.len();
        assert!(
            retained <= SESSION_OUTPUT_MAX_BYTES,
            "retained {retained} bytes exceeds cap {SESSION_OUTPUT_MAX_BYTES}"
        );
    }

    #[test]
    fn append_to_ring_scroll_drift_corrected() {
        let mut lines: std::collections::VecDeque<String> =
            (0..10).map(|i| format!("line {i}")).collect();
        let mut partial = String::new();
        let mut byte_size: usize = lines.iter().map(|l| l.len() + 1).sum();
        let mut scroll = 5usize;

        // Deliver a chunk that forces 3 evictions (max_lines = 10, so adding 3 new lines evicts 3).
        let evicted = append_to_ring(
            &mut lines,
            &mut partial,
            &mut byte_size,
            "a\nb\nc\n",
            10,         // max_lines
            usize::MAX, // no byte cap
        );

        assert_eq!(evicted, 3, "should evict 3 lines");
        scroll = scroll.saturating_sub(evicted);
        assert_eq!(scroll, 2, "scroll should decrease by 3");
    }
}

#[cfg(test)]
mod navigation_dotdot_tests {
    use super::*;
    use cairn_types::{ConnectionId, Entry, EntryKind, VfsPath};
    use cairn_vfs::ListPage;

    fn page(entries: Vec<Entry>) -> ListPage {
        ListPage {
            entries,
            cursor: None,
            done: true,
        }
    }

    fn state_at(cwd: VfsPath) -> AppState {
        AppState::new(ConnectionId(1), ConnectionId(2), cwd)
    }

    fn state_root() -> AppState {
        AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root())
    }

    fn deliver_to_left(s: &mut AppState, entries: Vec<Entry>) {
        let dir = s.pane(Side::Left).cwd.clone();
        let conn = s.pane(Side::Left).conn;
        let _ = update(
            s,
            Msg::Event(AppEvent::Listed {
                pane: Side::Left,
                conn,
                dir,
                result: Ok(page(entries)),
            }),
        );
    }

    fn visible_names(s: &AppState) -> Vec<String> {
        s.pane(Side::Left)
            .visible()
            .iter()
            .map(|e| e.name.to_string())
            .collect()
    }

    fn type_text(s: &mut AppState, text: &str) {
        for c in text.chars() {
            let _ = update(s, Msg::Text(TextEdit::Insert(c)));
        }
    }

    // --- Change 1: default-root pane navigation ---

    #[test]
    fn leave_dir_navigates_up_from_deep_cwd_to_root_and_stops() {
        // Panes rooted at `/` with cwd `/a/b`; Leave should walk /a/b → /a → / and no further.
        let mut s = state_at(VfsPath::parse("/a/b").unwrap());
        // Deliver a listing so the pane is in Ready state.
        deliver_to_left(&mut s, vec![Entry::new("child", EntryKind::Dir)]);

        // Leave: /a/b → /a
        let fx = update(&mut s, Msg::Action(Action::Leave));
        assert!(!fx.is_empty(), "Leave should emit a List effect");
        assert_eq!(s.pane(Side::Left).cwd.as_str(), "/a");

        // Deliver listing for /a.
        deliver_to_left(&mut s, vec![Entry::new("child", EntryKind::Dir)]);

        // Leave: /a → /
        let fx = update(&mut s, Msg::Action(Action::Leave));
        assert!(!fx.is_empty(), "Leave from /a should emit a List effect");
        assert_eq!(s.pane(Side::Left).cwd.as_str(), "/");

        // Deliver listing for root.
        deliver_to_left(&mut s, vec![Entry::new("child", EntryKind::Dir)]);

        // Leave at /: no-op (root has no parent).
        let fx = update(&mut s, Msg::Action(Action::Leave));
        assert!(fx.is_empty(), "Leave at root must be a no-op");
        assert_eq!(s.pane(Side::Left).cwd.as_str(), "/", "cwd stays at /");
    }

    // --- Change 2: synthetic `..` entry tests ---

    /// Deliver a listing to the left pane and return the visible entry names.
    fn deliver_and_visible_left(s: &mut AppState, entries: Vec<Entry>) -> Vec<String> {
        deliver_to_left(s, entries);
        visible_names(s)
    }

    #[test]
    fn dotdot_injected_first_when_not_at_root() {
        let mut s = state_at(VfsPath::parse("/a/b").unwrap());
        let visible = deliver_and_visible_left(
            &mut s,
            vec![
                Entry::new("dir1", EntryKind::Dir),
                Entry::new("file.txt", EntryKind::File),
            ],
        );
        assert_eq!(visible[0], "..", "first visible entry must be `..`");
        assert_eq!(visible.len(), 3, "total: .. + dir1 + file.txt");
    }

    #[test]
    fn dotdot_not_injected_at_root() {
        let mut s = state_root();
        let visible = deliver_and_visible_left(&mut s, vec![Entry::new("dir1", EntryKind::Dir)]);
        assert_eq!(visible[0], "dir1", "no `..` at root");
        assert_eq!(visible.len(), 1);
    }

    #[test]
    fn dotdot_survives_cycle_sort() {
        // `..` must stay at position 0 after cycling through all sort modes.
        let mut s = state_at(VfsPath::parse("/x").unwrap());
        let _ = deliver_and_visible_left(
            &mut s,
            vec![
                Entry::new("adir", EntryKind::Dir),
                Entry::new("file.txt", EntryKind::File),
            ],
        );
        for _ in 0..4 {
            let _ = update(&mut s, Msg::Action(Action::CycleSort));
            let first = s
                .pane(Side::Left)
                .listing
                .entries()
                .first()
                .map(|e| e.name.to_string());
            assert_eq!(
                first.as_deref(),
                Some(".."),
                ".. must remain first after sort cycle"
            );
        }
    }

    #[test]
    fn dotdot_visible_regardless_of_filter() {
        let mut s = state_at(VfsPath::parse("/a").unwrap());
        let _ = deliver_and_visible_left(
            &mut s,
            vec![
                Entry::new("alpha", EntryKind::File),
                Entry::new("beta", EntryKind::File),
            ],
        );
        // Apply a filter that matches neither `..` nor any real entry.
        let _ = update(&mut s, Msg::Action(Action::Filter));
        // Type a filter that matches no real file but `..` should still appear.
        type_text(&mut s, "zzz");
        let visible = s
            .pane(Side::Left)
            .visible()
            .iter()
            .map(|e| e.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            visible,
            vec![".."],
            "`..` visible even when filter matches nothing"
        );
    }

    #[test]
    fn enter_on_dotdot_navigates_up() {
        let mut s = state_at(VfsPath::parse("/a/b").unwrap());
        let _ = deliver_and_visible_left(&mut s, vec![Entry::new("sub", EntryKind::Dir)]);
        // cursor is 0: on the `..` entry.
        assert_eq!(s.pane(Side::Left).cursor, 0);
        assert_eq!(
            s.pane(Side::Left).current().map(|e| e.name.as_str()),
            Some("..")
        );
        let fx = update(&mut s, Msg::Action(Action::Enter));
        // Must navigate up to /a.
        assert!(!fx.is_empty(), "Enter on `..` must emit a List effect");
        assert_eq!(s.pane(Side::Left).cwd.as_str(), "/a");
    }

    #[test]
    fn toggle_mark_on_dotdot_is_a_no_op() {
        let mut s = state_at(VfsPath::parse("/a").unwrap());
        let _ = deliver_and_visible_left(&mut s, vec![Entry::new("file", EntryKind::File)]);
        // cursor 0 = `..`; ToggleMark must not add it to the mark set.
        assert_eq!(s.pane(Side::Left).cursor, 0);
        let _ = update(&mut s, Msg::Action(Action::ToggleMark));
        assert!(
            s.pane(Side::Left).marked.is_empty(),
            "marking `..` must be a no-op"
        );
    }

    #[test]
    fn op_targets_excludes_dotdot() {
        // `op_targets` is private; test it indirectly: Copy with cursor on `..` emits no Transfer.
        let mut s = state_at(VfsPath::parse("/a").unwrap());
        let _ = deliver_and_visible_left(&mut s, vec![Entry::new("sub", EntryKind::Dir)]);
        // Cursor is on `..` (index 0).
        assert_eq!(s.pane(Side::Left).cursor, 0);
        let fx = update(&mut s, Msg::Action(Action::Copy));
        // Copy with no valid target must produce no Transfer effect.
        assert!(
            fx.is_empty(),
            "Copy with cursor on `..` must produce no Transfer effect"
        );
    }

    #[test]
    fn delete_with_cursor_on_dotdot_is_a_no_op() {
        let mut s = state_at(VfsPath::parse("/a").unwrap());
        let _ = deliver_and_visible_left(&mut s, vec![Entry::new("sub", EntryKind::Dir)]);
        // cursor 0 = `..`; Delete (confirm-delete overlay) should not open for `..`.
        let fx = update(&mut s, Msg::Action(Action::Delete));
        assert!(
            fx.is_empty() && s.overlay.is_none(),
            "Delete with cursor on `..` must not open ConfirmDelete"
        );
    }

    #[test]
    fn rename_on_dotdot_is_a_no_op() {
        // Rename bypasses op_targets and uses `current()` directly, so it needs its own guard.
        // Pressing Rename with cursor on `..` must silently return with no overlay and no
        // error status — consistent with ToggleMark/Copy/Delete on `..`.
        let mut s = state_at(VfsPath::parse("/a").unwrap());
        let _ = deliver_and_visible_left(&mut s, vec![Entry::new("file.txt", EntryKind::File)]);
        // cursor 0 = `..`.
        assert_eq!(s.pane(Side::Left).cursor, 0);
        let _ = update(&mut s, Msg::Action(Action::Rename));
        assert!(
            s.overlay.is_none(),
            "Rename on `..` must not open a Prompt overlay"
        );
        assert!(
            s.status.is_none(),
            "Rename on `..` must not set an error status"
        );
    }

    #[test]
    fn marks_offset_correctness() {
        // With visible ["..","f1","f2"] in /a, marking visible index 2 (f2) and then issuing
        // Copy must target /a/f2 — proves the `..`-at-index-0 offset doesn't shift the
        // resolved path to the wrong file.
        let mut s = state_at(VfsPath::parse("/a").unwrap());
        let _ = deliver_and_visible_left(
            &mut s,
            vec![
                Entry::new("f1", EntryKind::File),
                Entry::new("f2", EntryKind::File),
            ],
        );
        // visible: ["..", "f1", "f2"] — move cursor to index 2 (f2).
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        assert_eq!(
            s.pane(Side::Left).current().map(|e| e.name.as_str()),
            Some("f2")
        );
        // Mark f2.
        let _ = update(&mut s, Msg::Action(Action::ToggleMark));
        // Copy must target /a/f2.
        let fx = update(&mut s, Msg::Action(Action::Copy));
        let has_f2_target = fx.iter().any(|e| {
            if let AppEffect::Transfer { items, .. } = e {
                items.iter().any(|(src, _)| src.as_str() == "/a/f2")
            } else {
                false
            }
        });
        assert!(
            has_f2_target,
            "Copy of marked f2 must target /a/f2, not /a/f1 (off-by-one from .. offset)"
        );
    }

    #[test]
    fn empty_non_root_dir_only_dotdot() {
        // An empty listing in a non-root directory produces exactly one entry (`..`).
        // The pane must report current()="..", len()==1, is_empty()==false, and neither
        // CycleSort nor CursorDown must panic.
        let mut s = state_at(VfsPath::parse("/a").unwrap());
        let _ = deliver_and_visible_left(&mut s, vec![]);
        {
            let pane = s.pane(Side::Left);
            assert_eq!(
                pane.current().map(|e| e.name.as_str()),
                Some(".."),
                "only entry must be `..`"
            );
            assert_eq!(pane.len(), 1, "len must be 1 (just `..`)");
            assert!(
                !pane.is_empty(),
                "is_empty must be false when `..` is present"
            );
        }
        // Neither action must panic.
        let _ = update(&mut s, Msg::Action(Action::CycleSort));
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
    }

    #[test]
    fn filter_collapses_to_only_dotdot() {
        // When a filter matches no real entries, visible() == [".."], cursor clamps to 0,
        // current() is `..`, and op_targets is empty (Copy produces no Transfer effect).
        let mut s = state_at(VfsPath::parse("/a").unwrap());
        let _ = deliver_and_visible_left(
            &mut s,
            vec![
                Entry::new("alpha", EntryKind::File),
                Entry::new("beta", EntryKind::File),
            ],
        );
        // Move cursor to a real entry so the clamp is meaningful.
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        // Activate filter.
        let _ = update(&mut s, Msg::Action(Action::Filter));
        type_text(&mut s, "zzz"); // matches nothing
        {
            let pane = s.pane(Side::Left);
            assert_eq!(pane.cursor, 0, "cursor must clamp to 0 after filter");
            assert_eq!(
                pane.current().map(|e| e.name.as_str()),
                Some(".."),
                "current must be `..` when filter matches nothing"
            );
        }
        let fx = update(&mut s, Msg::Action(Action::Copy));
        assert!(
            fx.is_empty(),
            "op_targets must be empty when only `..` is visible"
        );
    }
}

#[cfg(test)]
mod p4_form_tests {
    use super::*;
    use crate::state::{ChoiceProvenance, ConnectionChoice, DiscoverySource};
    use cairn_types::{ConnectionId, VfsPath};

    fn base() -> AppState {
        AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root())
    }

    fn open_form(state: &mut AppState) {
        let effects = update(state, Msg::Action(Action::NewConnection));
        assert!(
            effects.is_empty(),
            "NewConnection should not emit effects: {effects:?}"
        );
    }

    #[test]
    fn new_connection_opens_scheme_picker() {
        let mut s = base();
        open_form(&mut s);
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: ConnectionFormStage::SchemePicker,
                    ..
                })
            ),
            "overlay should be SchemePicker, got {:?}",
            s.overlay
        );
    }

    #[test]
    fn scheme_picker_enter_advances_to_fields() {
        let mut s = base();
        open_form(&mut s);
        let effects = update(&mut s, Msg::Action(Action::Enter));
        assert!(
            effects.is_empty(),
            "Enter in picker should not emit effects"
        );
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: ConnectionFormStage::Fields,
                    ..
                })
            ),
            "overlay should advance to Fields, got {:?}",
            s.overlay
        );
    }

    #[test]
    fn scheme_picker_cancel_closes_overlay() {
        let mut s = base();
        open_form(&mut s);
        let effects = update(&mut s, Msg::Action(Action::Cancel));
        assert!(effects.is_empty());
        assert!(s.overlay.is_none(), "overlay must close on Cancel");
    }

    #[test]
    fn fields_text_insert_and_backspace() {
        let mut s = base();
        open_form(&mut s);
        // Advance past scheme picker.
        let _ = update(&mut s, Msg::Action(Action::Enter));

        // Type "test" into the display_name field (first field, index 0).
        for c in "test".chars() {
            let _ = update(&mut s, Msg::Text(TextEdit::Insert(c)));
        }
        let val = match &s.overlay {
            Some(Overlay::ConnectionForm { values, .. }) => {
                values.get("display_name").cloned().unwrap_or_default()
            }
            _ => String::new(),
        };
        assert_eq!(val, "test");

        // Backspace removes the last character.
        let _ = update(&mut s, Msg::Text(TextEdit::Backspace));
        let val = match &s.overlay {
            Some(Overlay::ConnectionForm { values, .. }) => {
                values.get("display_name").cloned().unwrap_or_default()
            }
            _ => String::new(),
        };
        assert_eq!(val, "tes");
    }

    #[test]
    fn tab_and_shift_tab_cycle_focus() {
        let mut s = base();
        open_form(&mut s);
        let _ = update(&mut s, Msg::Action(Action::Enter));

        let focus_before = match &s.overlay {
            Some(Overlay::ConnectionForm { focus, .. }) => *focus,
            _ => 0,
        };
        assert_eq!(focus_before, 0);

        // Tab moves to next field.
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        let focus_after = match &s.overlay {
            Some(Overlay::ConnectionForm { focus, .. }) => *focus,
            _ => 0,
        };
        assert_eq!(focus_after, 1);

        // Shift-Tab moves back.
        let _ = update(&mut s, Msg::Text(TextEdit::PrevField));
        let focus_back = match &s.overlay {
            Some(Overlay::ConnectionForm { focus, .. }) => *focus,
            _ => 0,
        };
        assert_eq!(focus_back, 0);
    }

    #[test]
    fn submit_with_missing_required_field_shows_error() {
        let mut s = base();
        open_form(&mut s);
        let _ = update(&mut s, Msg::Action(Action::Enter)); // advance to Fields (scheme = first = ssh)

        // Submit without filling any fields — should stay open with errors.
        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(
            effects.is_empty(),
            "submit with errors should not emit effects"
        );
        assert!(
            s.overlay.is_some(),
            "overlay must stay open on validation failure"
        );
        let errors_empty = match &s.overlay {
            Some(Overlay::ConnectionForm { field_errors, .. }) => field_errors.is_empty(),
            _ => true,
        };
        assert!(
            !errors_empty,
            "field_errors must be populated on validation failure"
        );
    }

    #[test]
    fn submit_valid_ssh_form_emits_save_effect() {
        // P5: submitting SSH endpoint fields no longer immediately emits SaveConnection.
        // Instead the form transitions to the credential method picker so the user can
        // choose how to authenticate. The effect is emitted after the credential stage.
        //
        // All credential methods (including delegation like SshAgent) require the vault to
        // store a non-secret marker. Set vault_unlocked = true so the form proceeds to emit
        // ProvisionAndSaveConnection rather than opening the vault-unlock overlay.
        let mut s = base();
        s.vault_unlocked = true;
        open_form(&mut s);
        let _ = update(&mut s, Msg::Action(Action::Enter)); // → Fields, scheme = ssh (first in list)

        // Fill required fields: display_name, host, user.
        let required = [
            ("display_name", "My SSH"),
            ("host", "ssh.example.com"),
            ("user", "alice"),
        ];
        for (key, val) in required {
            // Advance focus to this field.
            let target_focus = crate::forms::scheme_fields("ssh")
                .iter()
                .position(|f| f.key == key)
                .unwrap_or(0);
            if let Some(Overlay::ConnectionForm { focus, .. }) = &mut s.overlay {
                *focus = target_focus;
            }
            for c in val.chars() {
                let _ = update(&mut s, Msg::Text(TextEdit::Insert(c)));
            }
        }

        // Submit endpoint fields: P5 transitions to CredentialMethodPicker, no effects yet.
        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(
            effects.is_empty(),
            "advancing to credential stage must emit no effects: {effects:?}"
        );
        assert!(
            !s.connection_saving,
            "connection_saving must not be set until credential draft is submitted"
        );
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: ConnectionFormStage::CredentialMethodPicker,
                    ..
                })
            ),
            "SSH endpoint submit must advance to CredentialMethodPicker, got {:?}",
            s.overlay
        );

        // Move cursor to SshAgent (index 0) and confirm — delegation method submits immediately.
        // Default cursor may be at SshPrivateKeyFile (index 1) when no agent is detected.
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 0; // SshAgent
        }
        let effects = update(&mut s, Msg::Action(Action::Enter));
        assert_eq!(
            effects.len(),
            1,
            "expected one ProvisionAndSaveConnection effect"
        );
        assert!(
            matches!(
                &effects[0],
                AppEffect::ProvisionAndSaveConnection { is_edit: false, .. }
            ),
            "expected ProvisionAndSaveConnection(is_edit: false): {effects:?}"
        );
        assert!(
            s.connection_saving,
            "connection_saving must be set after credential submit"
        );
    }

    #[test]
    fn connection_saved_event_updates_state() {
        let mut s = base();
        let id = uuid::Uuid::new_v4();
        let profile = crate::forms::ProfileData {
            id,
            scheme: "ssh".to_owned(),
            display_name: "Test".to_owned(),
            endpoint: std::collections::BTreeMap::new(),
            secret_ref: None,
        };
        s.connection_saving = true;

        let effects = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionSaved {
                id,
                display_name: "Test".to_owned(),
                label: "Test".to_owned(),
                is_edit: false,
                profile: profile.clone(),
            }),
        );

        assert!(!s.connection_saving, "connection_saving must clear");
        assert!(
            s.saved_profiles.contains_key(&id),
            "saved_profiles must contain the new id"
        );
        assert!(
            effects.is_empty()
                || effects
                    .iter()
                    .all(|e| !matches!(e, AppEffect::SaveConnection { .. }))
        );
    }

    #[test]
    fn connection_op_failed_clears_saving_flag() {
        let mut s = base();
        s.connection_saving = true;
        let effects = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionOpFailed {
                message: "disk full".to_owned(),
            }),
        );
        assert!(effects.is_empty());
        assert!(
            !s.connection_saving,
            "ConnectionOpFailed must clear connection_saving"
        );
        assert_eq!(s.status.as_deref(), Some("disk full"));
    }

    /// Build a state with one saved SSH profile and the Connections overlay open at cursor 0.
    fn state_with_connections() -> AppState {
        let mut s = AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root());
        let prof_id = uuid::Uuid::new_v4();
        let mut ep = std::collections::BTreeMap::new();
        ep.insert("host".to_owned(), "example.com".to_owned());
        ep.insert("user".to_owned(), "ubuntu".to_owned());
        let profile = crate::forms::ProfileData {
            id: prof_id,
            scheme: "ssh".to_owned(),
            display_name: "test-ssh".to_owned(),
            endpoint: ep,
            secret_ref: None,
        };
        s.saved_profiles.insert(prof_id, profile);
        s.connections = vec![ConnectionChoice {
            conn: ConnectionId(3),
            label: "test-ssh".to_owned(),
            provenance: ChoiceProvenance::Saved,
            status: ChoiceStatus::NeedsOpen,
            kind: ConnectionKind::Profile { id: prof_id },
            ..Default::default()
        }];
        s.overlay = Some(Overlay::Connections {
            cursor: 0,
            show_hidden: false,
        });
        s
    }

    #[test]
    fn scheme_change_prunes_stale_values() {
        let mut s = AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root());
        // Open scheme picker (new connection).
        let _ = update(&mut s, Msg::Action(Action::NewConnection));
        // Select SSH (focus=0) → advance to fields.
        let _ = update(&mut s, Msg::Action(Action::Confirm));
        // Move focus to the host field (index 1) and type a value.
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        for c in "example.com".chars() {
            let _ = update(&mut s, Msg::Text(TextEdit::Insert(c)));
        }
        // Esc → back to SchemePicker (new connection so Esc goes back, not closes).
        let _ = update(&mut s, Msg::Text(TextEdit::Cancel));
        assert!(
            matches!(
                s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: ConnectionFormStage::SchemePicker,
                    ..
                })
            ),
            "Esc in new-connection Fields should return to SchemePicker"
        );
        // Move to S3 (index 1) and confirm.
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let _ = update(&mut s, Msg::Action(Action::Confirm));
        // The stale "host" value must be gone; scheme must be s3.
        match &s.overlay {
            Some(Overlay::ConnectionForm { values, scheme, .. }) => {
                assert_eq!(scheme, "s3", "scheme must be s3 after picker advance");
                assert!(
                    !values.contains_key("host"),
                    "host value must be pruned on scheme change"
                );
            }
            _ => panic!("expected ConnectionForm after scheme selection"),
        }
    }

    #[test]
    fn failed_save_keeps_form_open_with_values() {
        // Use local scheme (no credential stage) to test the "retry on failure" guarantee.
        // P5: credential-requiring schemes (ssh/s3/…) close the form after draining secrets;
        // local retains P4 behavior — the overlay stays open so the user can retry or cancel.
        let mut s = AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root());
        let _ = update(&mut s, Msg::Action(Action::NewConnection));
        // KNOWN_SCHEMES order: ssh=0, s3=1, gcs=2, azure=3, local=4.
        for _ in 0..4 {
            let _ = update(&mut s, Msg::Action(Action::CursorDown));
        }
        let _ = update(&mut s, Msg::Action(Action::Confirm)); // pick local (focus=4)

        // Fill required fields for local: display_name and path.
        for c in "myserver".chars() {
            let _ = update(&mut s, Msg::Text(TextEdit::Insert(c)));
        }
        let _ = update(&mut s, Msg::Text(TextEdit::NextField));
        for c in "/srv/data".chars() {
            let _ = update(&mut s, Msg::Text(TextEdit::Insert(c)));
        }

        // Submit — local scheme emits SaveConnection immediately and keeps the form open (Fix 3).
        let _ = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(
            s.connection_saving,
            "connection_saving must be set after submit"
        );
        assert!(
            matches!(s.overlay, Some(Overlay::ConnectionForm { .. })),
            "form overlay must stay open while saving"
        );

        // Simulate a disk-full error.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionOpFailed {
                message: "disk full".to_owned(),
            }),
        );
        assert!(
            !s.connection_saving,
            "connection_saving must be cleared on failure"
        );
        assert!(
            matches!(s.overlay, Some(Overlay::ConnectionForm { .. })),
            "form overlay must survive a save failure so user can retry or cancel"
        );
        // User's values must still be there.
        match &s.overlay {
            Some(Overlay::ConnectionForm { values, .. }) => {
                assert_eq!(
                    values.get("display_name").map(String::as_str),
                    Some("myserver")
                );
            }
            _ => panic!("overlay lost on failed save"),
        }
    }

    #[test]
    fn delete_connection_opens_confirm_overlay() {
        let mut s = state_with_connections();
        let effects = update(&mut s, Msg::Action(Action::DeleteConnection));
        assert!(
            effects.is_empty(),
            "must not immediately delete — show confirm first"
        );
        assert!(
            matches!(s.overlay, Some(Overlay::ConfirmDeleteConnection { .. })),
            "overlay must be ConfirmDeleteConnection after pressing delete"
        );
        // Confirm delete — must emit DeleteConnection effect.
        let effects = update(&mut s, Msg::Action(Action::Confirm));
        assert_eq!(effects.len(), 1, "expected one DeleteConnection effect");
        assert!(matches!(effects[0], AppEffect::DeleteConnection { .. }));
    }

    #[test]
    fn delete_confirm_cancel_restores_no_overlay() {
        let mut s = state_with_connections();
        let _ = update(&mut s, Msg::Action(Action::DeleteConnection));
        let effects = update(&mut s, Msg::Action(Action::Cancel));
        assert!(effects.is_empty());
        assert!(s.overlay.is_none(), "cancelling confirm must close overlay");
    }

    #[test]
    fn edit_prefills_all_endpoint_fields() {
        let mut s = state_with_connections();
        let prof_id = match &s.connections[0].kind {
            ConnectionKind::Profile { id } => *id,
            _ => panic!("expected profile"),
        };
        let effects = update(&mut s, Msg::Action(Action::EditConnection));
        assert!(effects.is_empty());
        match &s.overlay {
            Some(Overlay::ConnectionForm {
                stage,
                scheme,
                values,
                editing_id,
                existing_secret_ref,
                ..
            }) => {
                assert_eq!(*stage, ConnectionFormStage::Fields);
                assert_eq!(scheme, "ssh");
                assert_eq!(values.get("host").map(String::as_str), Some("example.com"));
                assert_eq!(values.get("user").map(String::as_str), Some("ubuntu"));
                assert_eq!(*editing_id, Some(prof_id));
                assert!(
                    existing_secret_ref.is_none(),
                    "test profile has no secret_ref"
                );
            }
            _ => panic!("expected ConnectionForm in Fields stage after EditConnection"),
        }
    }

    #[test]
    fn secret_ref_preserved_through_submit() {
        let mut s = AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root());
        let prof_id = uuid::Uuid::new_v4();
        let vault_ref = uuid::Uuid::new_v4();
        let mut ep = std::collections::BTreeMap::new();
        ep.insert("host".to_owned(), "host".to_owned());
        ep.insert("user".to_owned(), "u".to_owned());
        let profile = crate::forms::ProfileData {
            id: prof_id,
            scheme: "ssh".to_owned(),
            display_name: "secured".to_owned(),
            endpoint: ep,
            secret_ref: Some(vault_ref),
        };
        s.connections = vec![ConnectionChoice {
            conn: ConnectionId(3),
            label: "ssh: secured".to_owned(),
            provenance: ChoiceProvenance::Saved,
            status: ChoiceStatus::NeedsOpen,
            kind: ConnectionKind::Profile { id: prof_id },
            ..Default::default()
        }];
        s.saved_profiles.insert(prof_id, profile);
        s.overlay = Some(Overlay::Connections {
            cursor: 0,
            show_hidden: false,
        });

        // Open the edit form.
        let _ = update(&mut s, Msg::Action(Action::EditConnection));
        match &s.overlay {
            Some(Overlay::ConnectionForm {
                existing_secret_ref,
                ..
            }) => {
                assert_eq!(
                    *existing_secret_ref,
                    Some(vault_ref),
                    "secret_ref must be pre-loaded from the profile"
                );
            }
            _ => panic!("expected ConnectionForm"),
        }

        // Submit endpoint fields → advances to CredentialMethodPicker (no effect yet).
        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(
            effects.is_empty(),
            "endpoint submit in edit mode must emit no effects"
        );
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: ConnectionFormStage::CredentialMethodPicker,
                    ..
                })
            ),
            "must advance to CredentialMethodPicker, got {:?}",
            s.overlay
        );

        // In edit mode cursor=0 = KeepExisting (prepended by credential_methods when is_edit).
        // Confirm: KeepExisting is delegation → submits immediately via submit_credential_draft.
        let effects = update(&mut s, Msg::Action(Action::Enter));
        // Should emit ProvisionAndSaveConnection with the original secret_ref preserved.
        assert_eq!(
            effects.len(),
            1,
            "KeepExisting must emit one ProvisionAndSaveConnection effect"
        );
        match &effects[0] {
            AppEffect::ProvisionAndSaveConnection { profile, .. } => {
                assert_eq!(
                    profile.secret_ref,
                    Some(vault_ref),
                    "ProvisionAndSaveConnection must preserve secret_ref via KeepExisting"
                );
            }
            other => panic!(
                "expected ProvisionAndSaveConnection effect, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn auto_discovered_edit_is_rejected_with_hint() {
        let mut s = AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root());
        s.connections = vec![ConnectionChoice {
            conn: ConnectionId(3),
            label: "docker: /".to_owned(),
            provenance: ChoiceProvenance::Discovered {
                source: DiscoverySource::Docker,
            },
            status: ChoiceStatus::Ready,
            kind: ConnectionKind::AutoDiscovered,
            ..Default::default()
        }];
        s.overlay = Some(Overlay::Connections {
            cursor: 0,
            show_hidden: false,
        });
        let effects = update(&mut s, Msg::Action(Action::EditConnection));
        assert!(effects.is_empty());
        let status = s.status.as_deref().unwrap_or("").to_lowercase();
        assert!(
            status.contains("not editable") || status.contains("cannot"),
            "edit of auto-discovered must show rejection hint: {:?}",
            s.status
        );
        // Overlay should remain on Connections (not switch to ConnectionForm).
        assert!(
            matches!(s.overlay, Some(Overlay::Connections { .. })),
            "overlay must stay as Connections after rejecting edit of auto-discovered"
        );
    }

    #[test]
    fn auto_discovered_delete_is_rejected_with_hint() {
        let mut s = AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root());
        s.connections = vec![ConnectionChoice {
            conn: ConnectionId(3),
            label: "docker: /".to_owned(),
            provenance: ChoiceProvenance::Discovered {
                source: DiscoverySource::Docker,
            },
            status: ChoiceStatus::Ready,
            kind: ConnectionKind::AutoDiscovered,
            ..Default::default()
        }];
        s.overlay = Some(Overlay::Connections {
            cursor: 0,
            show_hidden: false,
        });
        let effects = update(&mut s, Msg::Action(Action::DeleteConnection));
        assert!(effects.is_empty());
        assert!(
            s.status
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("cannot"),
            "delete of auto-discovered must show rejection hint: {:?}",
            s.status
        );
    }

    #[test]
    fn edit_updates_switcher_label_with_scheme_prefix() {
        let mut s = state_with_connections();
        // Simulate the save completing with a prefixed label.
        let prof_id = match &s.connections[0].kind {
            ConnectionKind::Profile { id } => *id,
            _ => panic!("expected profile"),
        };
        let saved_profile = s.saved_profiles[&prof_id].clone();
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionSaved {
                id: prof_id,
                display_name: "my-server".to_owned(),
                label: "ssh: my-server".to_owned(),
                is_edit: true,
                profile: saved_profile,
            }),
        );
        assert_eq!(
            s.connections[0].label, "ssh: my-server",
            "edited connection label must carry the scheme prefix"
        );
    }

    #[test]
    fn connection_deleted_event_removes_from_saved_profiles() {
        let mut s = base();
        let id = uuid::Uuid::new_v4();
        let profile = crate::forms::ProfileData {
            id,
            scheme: "local".to_owned(),
            display_name: "Old".to_owned(),
            endpoint: std::collections::BTreeMap::new(),
            secret_ref: None,
        };
        s.saved_profiles.insert(id, profile);
        s.connection_saving = true;

        let effects = update(&mut s, Msg::Event(AppEvent::ConnectionDeleted { id }));

        assert!(!s.connection_saving, "connection_saving must clear");
        assert!(
            !s.saved_profiles.contains_key(&id),
            "saved_profiles must not contain the deleted id"
        );
        assert!(
            effects.is_empty()
                || effects
                    .iter()
                    .all(|e| !matches!(e, AppEffect::DeleteConnection { .. }))
        );
    }
}

// ─────────────────────────── P6 polish tests (test/pin/hide/show-hidden) ─────────────────────────

#[cfg(test)]
mod p6_connection_polish_tests {
    //! Hermetic unit tests for RFC-0011 P6: test-connection, pin/hide, and the switcher's
    //! "show hidden" toggle. No I/O — effects are asserted as data, never executed.

    use super::*;
    use crate::state::ConnectionChoice;

    fn base() -> AppState {
        AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root())
    }

    /// Three switcher entries: `Ready`, `NeedsOpen`, `NeedsVault`. Overlay open, cursor at 0.
    fn state_with_three_choices() -> AppState {
        let mut s = base();
        s.connections = vec![
            ConnectionChoice {
                conn: ConnectionId(3),
                label: "local: /".to_owned(),
                status: ChoiceStatus::Ready,
                ..Default::default()
            },
            ConnectionChoice {
                conn: ConnectionId(4),
                label: "ssh: bastion".to_owned(),
                status: ChoiceStatus::NeedsOpen,
                ..Default::default()
            },
            ConnectionChoice {
                conn: ConnectionId(5),
                label: "s3: prod".to_owned(),
                status: ChoiceStatus::NeedsVault,
                ..Default::default()
            },
        ];
        s.overlay = Some(Overlay::Connections {
            cursor: 0,
            show_hidden: false,
        });
        s
    }

    fn set_cursor(s: &mut AppState, cursor: usize) {
        if let Some(Overlay::Connections { cursor: c, .. }) = &mut s.overlay {
            *c = cursor;
        } else {
            panic!("expected Overlay::Connections");
        }
    }

    // ── TestConnection ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_connection_on_ready_entry_is_a_status_only_noop() {
        let mut s = state_with_three_choices(); // cursor 0 = Ready
        let effects = update(&mut s, Msg::Action(Action::TestConnection));
        assert!(
            effects.is_empty(),
            "an already-Ready entry must not be re-probed (debounce): {effects:?}"
        );
        assert!(s.status.as_deref().is_some_and(|m| m.contains("already")));
    }

    #[test]
    fn test_connection_on_needs_vault_reports_without_opening_any_overlay() {
        let mut s = state_with_three_choices();
        set_cursor(&mut s, 2); // NeedsVault
        let effects = update(&mut s, Msg::Action(Action::TestConnection));
        assert!(
            effects.is_empty(),
            "a NeedsVault entry must never trigger I/O for a test: {effects:?}"
        );
        // Testing must NEVER force the vault-unlock/create overlay on the user.
        assert!(
            matches!(s.overlay, Some(Overlay::Connections { .. })),
            "the switcher must stay open, not be replaced by a vault overlay"
        );
        assert!(
            s.status
                .as_deref()
                .is_some_and(|m| m.contains("unlock") || m.contains("vault")),
            "status must explain the vault is the blocker: {:?}",
            s.status
        );
    }

    #[test]
    fn test_connection_on_needs_open_emits_the_probe_effect() {
        let mut s = state_with_three_choices();
        set_cursor(&mut s, 1); // NeedsOpen
        let effects = update(&mut s, Msg::Action(Action::TestConnection));
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0],
            AppEffect::TestConnection {
                conn: ConnectionId(4)
            }
        ));
        assert!(s.status.as_deref().is_some_and(|m| m.contains("Testing")));
    }

    #[test]
    fn connection_tested_ok_flips_ready_without_navigating() {
        let mut s = state_with_three_choices();
        let prior_side_slots = s.pending_conn_open;
        let effects = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionTested {
                conn: ConnectionId(4),
                result: Ok(()),
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(
            s.connections
                .iter()
                .find(|c| c.conn == ConnectionId(4))
                .unwrap()
                .status,
            ChoiceStatus::Ready
        );
        assert_eq!(
            s.pending_conn_open, prior_side_slots,
            "a test result must never touch pane-navigation state"
        );
        assert!(s.status.as_deref().is_some_and(|m| m.contains("reachable")));
    }

    #[test]
    fn connection_tested_err_flips_unreachable_and_redacted_message_surfaces() {
        let mut s = state_with_three_choices();
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionTested {
                conn: ConnectionId(4),
                result: Err("ssh: connection failed".to_owned()),
            }),
        );
        assert_eq!(
            s.connections
                .iter()
                .find(|c| c.conn == ConnectionId(4))
                .unwrap()
                .status,
            ChoiceStatus::Unreachable
        );
        assert!(s
            .status
            .as_deref()
            .is_some_and(|m| m.contains("unreachable")));
    }

    // ── PinConnection ────────────────────────────────────────────────────────────────────────────

    #[test]
    fn pin_connection_emits_effect_with_the_toggled_state() {
        let mut s = state_with_three_choices(); // cursor 0, not pinned
        let effects = update(&mut s, Msg::Action(Action::PinConnection));
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0],
            AppEffect::SetConnectionPinned {
                conn: ConnectionId(3),
                pinned: true,
            }
        ));
        // Not applied to the display yet — only once ConnectionPinSet confirms the write.
        assert!(!s.connections[0].pinned);
    }

    #[test]
    fn connection_pin_set_ok_applies_and_floats_to_front() {
        let mut s = state_with_three_choices();
        // Pin the *last* entry (s3: prod) and confirm it floats to the front on success.
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionPinSet {
                conn: ConnectionId(5),
                pinned: true,
                result: Ok(()),
            }),
        );
        assert!(s.connections[0].pinned);
        assert_eq!(s.connections[0].conn, ConnectionId(5));
        assert!(s.status.as_deref().is_some_and(|m| m.contains("Pinned")));
    }

    #[test]
    fn connection_pin_set_err_leaves_display_unchanged() {
        let mut s = state_with_three_choices();
        let before: Vec<ConnectionId> = s.connections.iter().map(|c| c.conn).collect();
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionPinSet {
                conn: ConnectionId(5),
                pinned: true,
                result: Err("disk full".to_owned()),
            }),
        );
        assert!(!s.connections.iter().any(|c| c.pinned));
        let after: Vec<ConnectionId> = s.connections.iter().map(|c| c.conn).collect();
        assert_eq!(before, after, "a failed pin must not reorder the list");
        assert!(s
            .status
            .as_deref()
            .is_some_and(|m| m.to_lowercase().contains("failed")));
    }

    // ── HideConnection / ToggleShowHidden ────────────────────────────────────────────────────────

    #[test]
    fn hide_connection_emits_effect_with_the_toggled_state() {
        let mut s = state_with_three_choices();
        let effects = update(&mut s, Msg::Action(Action::HideConnection));
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0],
            AppEffect::SetConnectionHidden {
                conn: ConnectionId(3),
                hidden: true,
            }
        ));
    }

    #[test]
    fn connection_hide_set_ok_removes_entry_from_default_view() {
        let mut s = state_with_three_choices();
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionHideSet {
                conn: ConnectionId(3),
                hidden: true,
                result: Ok(()),
            }),
        );
        assert!(s.connections[0].hidden);
        // The entry is still enumerated (recoverable)...
        assert_eq!(s.connections.len(), 3);
        // ...but absent from the default (show_hidden = false) visible set.
        let visible = crate::state::visible_connection_indices(&s.connections, false);
        assert!(
            !visible.contains(&0),
            "hidden entry must not be visible by default"
        );
        // ...and present once hidden entries are revealed.
        let visible_all = crate::state::visible_connection_indices(&s.connections, true);
        assert!(visible_all.contains(&0));
    }

    #[test]
    fn hiding_the_cursor_entry_reclamps_the_switcher_cursor() {
        let mut s = state_with_three_choices();
        set_cursor(&mut s, 2); // last entry
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::ConnectionHideSet {
                conn: ConnectionId(5), // the entry the cursor is on
                hidden: true,
                result: Ok(()),
            }),
        );
        let Some(Overlay::Connections { cursor, .. }) = s.overlay else {
            panic!("overlay must still be Connections");
        };
        // Only 2 entries are now visible (indices 0,1); cursor must not point past the end.
        assert!(cursor < 2, "cursor {cursor} must be re-clamped");
    }

    #[test]
    fn hidden_entry_never_traps_the_user_toggle_show_hidden_reveals_it_for_unhiding() {
        let mut s = state_with_three_choices();
        s.connections[1].hidden = true; // pre-hidden "ssh: bastion"

        // Default view: only 2 of 3 are navigable.
        let visible = crate::state::visible_connection_indices(&s.connections, false);
        assert_eq!(visible, vec![0, 2]);

        // Toggle "show hidden" on.
        let _ = update(&mut s, Msg::Action(Action::ToggleShowHidden));
        let Some(Overlay::Connections { show_hidden, .. }) = s.overlay else {
            panic!("expected Connections overlay");
        };
        assert!(show_hidden, "ToggleShowHidden must flip show_hidden on");

        // Now the hidden entry can be reached and un-hidden.
        set_cursor(&mut s, 1); // second visible-in-full-list row = the hidden ssh entry
        let effects = update(&mut s, Msg::Action(Action::HideConnection));
        assert!(matches!(
            effects.as_slice(),
            [AppEffect::SetConnectionHidden {
                conn: ConnectionId(4),
                hidden: false,
            }]
        ));
    }

    #[test]
    fn cursor_navigation_skips_hidden_entries_by_default() {
        let mut s = state_with_three_choices();
        s.connections[1].hidden = true; // hide the middle entry ("ssh: bastion")
                                        // Rebuild the overlay so the cursor starts fresh at the top of the visible set.
        s.overlay = Some(Overlay::Connections {
            cursor: 0,
            show_hidden: false,
        });
        let _ = update(&mut s, Msg::Action(Action::CursorDown));
        let Some(Overlay::Connections { cursor, .. }) = s.overlay else {
            panic!("expected Connections overlay");
        };
        // Only 2 visible entries (indices 0 and 2 in the raw list) — CursorDown must land on
        // the second *visible* row, not stop early or skip past the end.
        assert_eq!(cursor, 1);
        // Confirming from here must act on the raw entry at index 2 (s3: prod), not the hidden
        // one at index 1 — proving selection uses the same visible-index mapping as navigation.
        // `PinConnection` (unlike `TestConnection`) always emits regardless of `ChoiceStatus`, so
        // it isolates the index-mapping behavior from status-dependent branching.
        let effects = update(&mut s, Msg::Action(Action::PinConnection));
        assert!(matches!(
            effects.as_slice(),
            [AppEffect::SetConnectionPinned {
                conn: ConnectionId(5),
                pinned: true,
            }]
        ));
    }
}

// ─────────────────────────── P5 credential-flow tests ────────────────────────────────────────────

#[cfg(test)]
mod p5_cred_tests {
    //! Hermetic unit tests for the P5 credential provisioning flow.
    //!
    //! All tests run offline: no vault I/O, no network calls, no env-var dependencies.
    //! The OS-sources detection (`DetectOsSources` effect) is not triggered here; `os_sources`
    //! stays at its zero-value default (no SSH agent, no AWS profiles, no ADC, no Azure AD).

    use super::*;
    use crate::state::{
        ChoiceProvenance, ChoiceStatus, ConnectionChoice, ConnectionFormStage as Stage,
        ConnectionKind, Overlay,
    };
    use cairn_types::{ConnectionId, VfsPath};

    fn base() -> AppState {
        AppState::new(ConnectionId(1), ConnectionId(2), VfsPath::root())
    }

    // ── Helpers ──────────────────────────────────────────────────────────────────────────────────

    /// Open the scheme picker and pick the given scheme by index in KNOWN_SCHEMES.
    fn pick_scheme(s: &mut AppState, index: usize) {
        let _ = update(s, Msg::Action(Action::NewConnection));
        for _ in 0..index {
            let _ = update(s, Msg::Action(Action::CursorDown));
        }
        let _ = update(s, Msg::Action(Action::Enter));
    }

    /// Advance the focus to the field at `target_index` and type `text`.
    fn fill_field(s: &mut AppState, target_index: usize, text: &str) {
        if let Some(Overlay::ConnectionForm { focus, .. }) = &mut s.overlay {
            *focus = target_index;
        }
        for c in text.chars() {
            let _ = update(s, Msg::Text(TextEdit::Insert(c)));
        }
    }

    // ── Credential method picker tests ────────────────────────────────────────────────────────────

    #[test]
    fn ssh_endpoint_submit_transitions_to_credential_method_picker() {
        let mut s = base();
        pick_scheme(&mut s, 0); // ssh

        fill_field(&mut s, 0, "MyServer"); // display_name
        fill_field(&mut s, 1, "host.example.com"); // host
        fill_field(&mut s, 2, "alice"); // user

        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(
            effects.is_empty(),
            "endpoint submit must not emit effects: {effects:?}"
        );
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: Stage::CredentialMethodPicker,
                    ..
                })
            ),
            "SSH endpoint submit must advance to CredentialMethodPicker, got {:?}",
            s.overlay
        );
        assert!(!s.connection_saving);
    }

    #[test]
    fn credential_method_picker_cursor_up_down() {
        let mut s = base();
        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "S");
        fill_field(&mut s, 1, "h");
        fill_field(&mut s, 2, "u");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // SSH new-form methods: [SshAgent, SshPrivateKeyFile, SshInlinePem, SshPassword] (4 items).
        // Default cursor (no SSH agent) = 1 (SshPrivateKeyFile).
        let cursor_before = match &s.overlay {
            Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) => *cred_method_cursor,
            _ => panic!("expected ConnectionForm"),
        };
        assert_eq!(
            cursor_before, 1,
            "default cursor should be SshPrivateKeyFile (1) when no agent"
        );

        let _ = update(&mut s, Msg::Action(Action::CursorDown)); // → 2
        let c = match &s.overlay {
            Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) => *cred_method_cursor,
            _ => panic!(),
        };
        assert_eq!(c, 2);

        let _ = update(&mut s, Msg::Action(Action::CursorUp));
        let c = match &s.overlay {
            Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) => *cred_method_cursor,
            _ => panic!(),
        };
        assert_eq!(c, 1);
    }

    #[test]
    fn picking_ssh_agent_emits_provision_and_save_immediately() {
        // SshAgent is a delegation method — it has no fields and submits the draft on Enter.
        // All methods (including delegation) require the vault to store a non-secret marker,
        // so set vault_unlocked = true to bypass the vault gate and emit the effect directly.
        let mut s = base();
        s.vault_unlocked = true;
        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "AgentConn");
        fill_field(&mut s, 1, "bastion.internal");
        fill_field(&mut s, 2, "deploy");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // Force cursor to SshAgent (index 0).
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 0;
        }

        let effects = update(&mut s, Msg::Action(Action::Enter));
        assert_eq!(
            effects.len(),
            1,
            "SshAgent must emit one effect: {effects:?}"
        );
        assert!(
            matches!(
                &effects[0],
                AppEffect::ProvisionAndSaveConnection { is_edit: false, .. }
            ),
            "expected ProvisionAndSaveConnection: {effects:?}"
        );
        assert!(s.connection_saving, "connection_saving must be set");
        // Form closes after credential draft is submitted.
        assert!(s.overlay.is_none(), "overlay must close after provision");
    }

    #[test]
    fn picking_ssh_private_key_file_advances_to_credential_fields() {
        // SshPrivateKeyFile is field-bearing — picking it advances to CredentialFields.
        let mut s = base();
        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "KeyConn");
        fill_field(&mut s, 1, "host.example.com");
        fill_field(&mut s, 2, "bob");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // Default cursor = 1 (SshPrivateKeyFile) when no agent detected.
        assert_eq!(
            match &s.overlay {
                Some(Overlay::ConnectionForm {
                    cred_method_cursor, ..
                }) => *cred_method_cursor,
                _ => panic!(),
            },
            1
        );

        let effects = update(&mut s, Msg::Action(Action::Enter)); // pick SshPrivateKeyFile
        assert!(
            effects.is_empty(),
            "advancing to CredentialFields must emit no effects"
        );
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: Stage::CredentialFields,
                    cred_method: Some(crate::forms::CredentialMethod::SshPrivateKeyFile),
                    ..
                })
            ),
            "must be in CredentialFields with SshPrivateKeyFile, got {:?}",
            s.overlay
        );
    }

    #[test]
    fn credential_fields_cancel_returns_to_method_picker() {
        let mut s = base();
        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "K");
        fill_field(&mut s, 1, "h");
        fill_field(&mut s, 2, "u");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker
                                                             // Force to SshPrivateKeyFile
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 1;
        }
        let _ = update(&mut s, Msg::Action(Action::Enter)); // → CredentialFields

        // Cancel from CredentialFields must return to CredentialMethodPicker.
        let effects = update(&mut s, Msg::Action(Action::Cancel));
        assert!(effects.is_empty());
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: Stage::CredentialMethodPicker,
                    ..
                })
            ),
            "Cancel from CredentialFields must return to CredentialMethodPicker"
        );
    }

    #[test]
    fn credential_method_picker_cancel_returns_to_fields() {
        let mut s = base();
        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "K");
        fill_field(&mut s, 1, "h");
        fill_field(&mut s, 2, "u");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // Cancel from CredentialMethodPicker must return to Fields.
        let effects = update(&mut s, Msg::Action(Action::Cancel));
        assert!(effects.is_empty());
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: Stage::Fields,
                    ..
                })
            ),
            "Cancel from CredentialMethodPicker must return to Fields"
        );
    }

    #[test]
    fn submitting_key_path_field_emits_provision_and_save() {
        // Full flow: pick SshPrivateKeyFile, fill path, submit credential → ProvisionAndSaveConnection.
        // The vault must be unlocked because SshPrivateKeyFile is non-delegation (vault stores passphrase).
        let mut s = base();
        s.vault_unlocked = true;
        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "KeyConn");
        fill_field(&mut s, 1, "host.example.com");
        fill_field(&mut s, 2, "alice");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // Force SshPrivateKeyFile (index 1).
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 1;
        }
        let _ = update(&mut s, Msg::Action(Action::Enter)); // → CredentialFields

        // Fill the path field (plain, index 0) and submit.
        if let Some(Overlay::ConnectionForm { cred_focus, .. }) = &mut s.overlay {
            *cred_focus = 0;
        }
        for c in "/home/alice/.ssh/id_ed25519".chars() {
            let _ = update(&mut s, Msg::Text(TextEdit::Insert(c)));
        }

        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert_eq!(effects.len(), 1, "must emit one effect: {effects:?}");
        assert!(
            matches!(
                &effects[0],
                AppEffect::ProvisionAndSaveConnection { is_edit: false, .. }
            ),
            "expected ProvisionAndSaveConnection: {effects:?}"
        );
        assert!(s.connection_saving);
    }

    #[test]
    fn empty_required_cred_field_is_rejected() {
        // Submitting credential fields with an empty required field must be rejected.
        let mut s = base();
        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "K");
        fill_field(&mut s, 1, "h");
        fill_field(&mut s, 2, "u");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // SshPrivateKeyFile: path is required.
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 1;
        }
        let _ = update(&mut s, Msg::Action(Action::Enter)); // → CredentialFields (path empty)

        // Submit without filling the path.
        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(
            effects.is_empty(),
            "empty required credential field must not emit effects"
        );
        assert!(
            !s.connection_saving,
            "connection_saving must not be set on validation failure"
        );
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: Stage::CredentialFields,
                    ..
                })
            ),
            "form must stay in CredentialFields on validation failure"
        );
    }

    // ── Vault gating tests ────────────────────────────────────────────────────────────────────────

    #[test]
    fn non_delegation_method_gates_on_vault_when_locked() {
        // SshPassword stores a secret; if the vault is locked/absent, submitting must
        // open the VaultCreate/VaultUnlock overlay instead of emitting ProvisionAndSaveConnection.
        let mut s = base();
        // vault_unlocked defaults to false, vault_file_exists defaults to false.
        assert!(!s.vault_unlocked);
        assert!(!s.vault_file_exists);

        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "PwConn");
        fill_field(&mut s, 1, "secure.example.com");
        fill_field(&mut s, 2, "root");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // SshPassword is at index 3.
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 3; // SshPassword
        }
        let _ = update(&mut s, Msg::Action(Action::Enter)); // → CredentialFields

        // Fill the password field (cred_focus=0 → "cred_password", secret).
        // Use the update path so routing is exercised correctly.
        for c in "hunter2".chars() {
            let _ = update(&mut s, Msg::Text(TextEdit::Insert(c)));
        }

        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        // No ProvisionAndSaveConnection — vault gate triggers.
        assert!(
            effects.is_empty(),
            "vault-gated submit must emit no effects: {effects:?}"
        );
        assert!(!s.connection_saving);
        // Must open VaultCreate overlay (no vault file exists) with pending_save set.
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::VaultCreate {
                    pending_save: Some(_),
                    ..
                })
            ),
            "must show VaultCreate with pending_save, got {:?}",
            s.overlay
        );
    }

    #[test]
    fn delegation_method_gates_on_vault_when_locked() {
        // SshAgent (delegation) stores a non-secret marker in the vault so the connect layer
        // can determine which OS delegation source to use. Even delegation methods must wait
        // for the vault to be open before saving.
        let mut s = base();
        assert!(!s.vault_unlocked);
        assert!(!s.vault_file_exists);

        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "AgentConn");
        fill_field(&mut s, 1, "host.example.com");
        fill_field(&mut s, 2, "user");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // Force SshAgent (index 0).
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 0;
        }
        let effects = update(&mut s, Msg::Action(Action::Enter));
        // Vault is locked/absent — must open VaultCreate overlay with pending_save,
        // NOT emit ProvisionAndSaveConnection directly.
        assert!(
            effects.is_empty(),
            "vault-gated delegation submit must emit no effects: {effects:?}"
        );
        assert!(!s.connection_saving);
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::VaultCreate {
                    pending_save: Some(_),
                    ..
                })
            ),
            "must show VaultCreate with pending_save, got {:?}",
            s.overlay
        );
    }

    // ── Edit / KeepExisting tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn edit_form_defaults_to_keep_existing_when_secret_ref_present() {
        // In edit mode with an existing secret_ref, KeepExisting is the default (cursor=0).
        let mut s = base();
        let prof_id = uuid::Uuid::new_v4();
        let vault_ref = uuid::Uuid::new_v4();
        let mut ep = std::collections::BTreeMap::new();
        ep.insert("host".to_owned(), "h".to_owned());
        ep.insert("user".to_owned(), "u".to_owned());
        let profile = crate::forms::ProfileData {
            id: prof_id,
            scheme: "ssh".to_owned(),
            display_name: "MyConn".to_owned(),
            endpoint: ep,
            secret_ref: Some(vault_ref),
        };
        s.connections = vec![ConnectionChoice {
            conn: ConnectionId(3),
            label: "ssh: MyConn".to_owned(),
            provenance: ChoiceProvenance::Saved,
            status: ChoiceStatus::NeedsOpen,
            kind: ConnectionKind::Profile { id: prof_id },
            ..Default::default()
        }];
        s.saved_profiles.insert(prof_id, profile);
        s.overlay = Some(Overlay::Connections {
            cursor: 0,
            show_hidden: false,
        });

        let _ = update(&mut s, Msg::Action(Action::EditConnection));

        // Submit endpoint fields → CredentialMethodPicker.
        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(effects.is_empty());

        // KeepExisting must be at cursor=0 in edit mode.
        let cursor = match &s.overlay {
            Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) => *cred_method_cursor,
            _ => panic!("expected ConnectionForm"),
        };
        assert_eq!(
            cursor, 0,
            "edit mode must default to KeepExisting (cursor 0)"
        );
    }

    #[test]
    fn keep_existing_preserves_secret_ref_in_provision_effect() {
        // KeepExisting (edit mode) emits ProvisionAndSaveConnection with the original vault ref.
        let mut s = base();
        let prof_id = uuid::Uuid::new_v4();
        let vault_ref = uuid::Uuid::new_v4();
        let mut ep = std::collections::BTreeMap::new();
        ep.insert("host".to_owned(), "example.com".to_owned());
        ep.insert("user".to_owned(), "carol".to_owned());
        let profile = crate::forms::ProfileData {
            id: prof_id,
            scheme: "ssh".to_owned(),
            display_name: "CarolConn".to_owned(),
            endpoint: ep,
            secret_ref: Some(vault_ref),
        };
        s.connections = vec![ConnectionChoice {
            conn: ConnectionId(3),
            label: "ssh: CarolConn".to_owned(),
            provenance: ChoiceProvenance::Saved,
            status: ChoiceStatus::NeedsOpen,
            kind: ConnectionKind::Profile { id: prof_id },
            ..Default::default()
        }];
        s.saved_profiles.insert(prof_id, profile);
        s.overlay = Some(Overlay::Connections {
            cursor: 0,
            show_hidden: false,
        });

        let _ = update(&mut s, Msg::Action(Action::EditConnection));
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // cursor=0 = KeepExisting in edit mode.
        let effects = update(&mut s, Msg::Action(Action::Enter));
        assert_eq!(effects.len(), 1, "KeepExisting must emit one effect");
        match &effects[0] {
            AppEffect::ProvisionAndSaveConnection {
                profile, is_edit, ..
            } => {
                assert_eq!(
                    profile.secret_ref,
                    Some(vault_ref),
                    "secret_ref must be preserved via KeepExisting"
                );
                assert!(*is_edit, "is_edit must be true");
            }
            other => panic!("expected ProvisionAndSaveConnection: {other:?}"),
        }
    }

    // ── AwsProfile field flow ─────────────────────────────────────────────────────────────────────

    #[test]
    fn aws_profile_requires_field_entry_not_delegation_fast_path() {
        // AwsProfile is NOT a delegation method — it must advance to CredentialFields so the
        // user can enter the profile name. Picking it must NOT skip to submit immediately.
        let mut s = base();
        pick_scheme(&mut s, 1); // s3 (index 1 in KNOWN_SCHEMES)
        fill_field(&mut s, 0, "ProdBucket");
        fill_field(&mut s, 1, "my-bucket");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // S3 method list: [AwsDefaultChain=0, AwsProfile=1, AwsStatic=2]
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 1; // AwsProfile
        }

        let effects = update(&mut s, Msg::Action(Action::Enter));
        // Must advance to CredentialFields (no delegation fast-path), not emit an effect.
        assert!(
            effects.is_empty(),
            "AwsProfile must advance to CredentialFields, not emit effects: {effects:?}"
        );
        assert!(
            matches!(
                &s.overlay,
                Some(Overlay::ConnectionForm {
                    stage: Stage::CredentialFields,
                    cred_method: Some(crate::forms::CredentialMethod::AwsProfile),
                    ..
                })
            ),
            "must be in CredentialFields with AwsProfile, got {:?}",
            s.overlay
        );
    }

    #[test]
    fn aws_profile_empty_field_is_rejected() {
        // Submitting with an empty cred_profile_name must be rejected by validation.
        let mut s = base();
        pick_scheme(&mut s, 1); // s3
        fill_field(&mut s, 0, "B");
        fill_field(&mut s, 1, "bucket");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 1; // AwsProfile
        }
        let _ = update(&mut s, Msg::Action(Action::Enter)); // → CredentialFields (empty)

        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert!(
            effects.is_empty(),
            "empty profile_name must be rejected: {effects:?}"
        );
        assert!(!s.connection_saving);
    }

    #[test]
    fn aws_profile_with_name_emits_provision_and_save() {
        // Full AwsProfile flow: fill the profile name, vault open, submit → ProvisionAndSaveConnection.
        let mut s = base();
        s.vault_unlocked = true;
        pick_scheme(&mut s, 1); // s3
        fill_field(&mut s, 0, "ProdBucket");
        fill_field(&mut s, 1, "my-bucket");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 1; // AwsProfile
        }
        let _ = update(&mut s, Msg::Action(Action::Enter)); // → CredentialFields

        // Fill the profile name (plain, cred_focus=0).
        for c in "production".chars() {
            let _ = update(&mut s, Msg::Text(TextEdit::Insert(c)));
        }
        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert_eq!(effects.len(), 1, "must emit one effect: {effects:?}");
        match &effects[0] {
            AppEffect::ProvisionAndSaveConnection { draft, .. } => {
                assert!(
                    matches!(
                        draft,
                        crate::forms::CredentialDraft::AwsProfile {
                            profile_name
                        } if profile_name == "production"
                    ),
                    "draft must carry the profile name: {draft:?}"
                );
            }
            other => panic!("expected ProvisionAndSaveConnection: {other:?}"),
        }
    }

    // ── Deferred method tests ─────────────────────────────────────────────────────────────────────

    #[test]
    fn deferred_method_skips_vault_gate_and_emits_correct_draft() {
        // GcpServiceAccountJson is deferred-P5: no field entry, no vault gate even if vault
        // is locked. The emitted draft must be `GcpServiceAccountJson` (not `GcpApplicationDefault`).
        let mut s = base();
        assert!(!s.vault_unlocked);

        pick_scheme(&mut s, 2); // gcs (index 2 in KNOWN_SCHEMES)
        fill_field(&mut s, 0, "ProdGCS");
        fill_field(&mut s, 1, "my-gcs-bucket");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // GCS methods: [GcpApplicationDefault=0, GcpServiceAccountJson=1]
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 1; // GcpServiceAccountJson (deferred)
        }

        let effects = update(&mut s, Msg::Action(Action::Enter));
        // Deferred method skips vault gate — must emit ProvisionAndSaveConnection directly.
        assert_eq!(
            effects.len(),
            1,
            "deferred method must emit one effect: {effects:?}"
        );
        match &effects[0] {
            AppEffect::ProvisionAndSaveConnection { draft, .. } => {
                assert!(
                    matches!(draft, crate::forms::CredentialDraft::GcpServiceAccountJson { .. }),
                    "deferred draft must be GcpServiceAccountJson, not GcpApplicationDefault: {draft:?}"
                );
            }
            other => panic!("expected ProvisionAndSaveConnection: {other:?}"),
        }
    }

    // ── OS sources detection ──────────────────────────────────────────────────────────────────────

    #[test]
    fn os_sources_detected_event_updates_state() {
        let mut s = base();
        assert!(!s.os_sources.ssh_agent);

        let os = crate::forms::OsSources {
            ssh_agent: true,
            aws_profiles: vec!["default".to_owned()],
            gcp_adc: false,
            azure_ad_likely: false,
        };
        let effects = update(
            &mut s,
            Msg::Event(AppEvent::OsSourcesDetected {
                os_sources: os.clone(),
            }),
        );
        assert!(effects.is_empty());
        assert!(s.os_sources.ssh_agent, "ssh_agent must be updated");
        assert_eq!(s.os_sources.aws_profiles, vec!["default"]);
    }

    #[test]
    fn os_sources_updates_open_picker_cursor() {
        // If the credential picker is open when OsSourcesDetected arrives, the cursor must
        // be updated to the newly preferred default for the current scheme.
        let mut s = base();
        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "S");
        fill_field(&mut s, 1, "h");
        fill_field(&mut s, 2, "u");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // Before: no agent → cursor 1 (SshPrivateKeyFile).
        let c_before = match &s.overlay {
            Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) => *cred_method_cursor,
            _ => panic!(),
        };
        assert_eq!(c_before, 1);

        // Agent detected → cursor should move to 0 (SshAgent).
        let os = crate::forms::OsSources {
            ssh_agent: true,
            ..Default::default()
        };
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::OsSourcesDetected { os_sources: os }),
        );

        let c_after = match &s.overlay {
            Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) => *cred_method_cursor,
            _ => panic!(),
        };
        assert_eq!(
            c_after, 0,
            "cursor must update to SshAgent after OS detection"
        );
    }

    #[test]
    fn os_sources_detected_does_not_clobber_manually_moved_cursor() {
        // If the user has moved the cursor away from the open-time default, OsSourcesDetected
        // must NOT overwrite it.
        let mut s = base();
        pick_scheme(&mut s, 0); // ssh (no ssh_agent in default os_sources)
        fill_field(&mut s, 0, "S");
        fill_field(&mut s, 1, "h");
        fill_field(&mut s, 2, "u");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // Default cursor (no agent) = 1; user moves it to 3 (SshPassword).
        let _ = update(&mut s, Msg::Action(Action::CursorDown)); // → 2
        let _ = update(&mut s, Msg::Action(Action::CursorDown)); // → 3
        let cursor_after_move = match &s.overlay {
            Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) => *cred_method_cursor,
            _ => panic!(),
        };
        assert_eq!(cursor_after_move, 3, "user moved cursor to SshPassword");

        // Now detection arrives with ssh_agent=true (would set default to 0 if cursor not moved).
        let os = crate::forms::OsSources {
            ssh_agent: true,
            ..Default::default()
        };
        let _ = update(
            &mut s,
            Msg::Event(AppEvent::OsSourcesDetected { os_sources: os }),
        );

        let cursor_after_detection = match &s.overlay {
            Some(Overlay::ConnectionForm {
                cred_method_cursor, ..
            }) => *cred_method_cursor,
            _ => panic!(),
        };
        assert_eq!(
            cursor_after_detection, 3,
            "user-moved cursor must not be clobbered by OsSourcesDetected"
        );
    }

    // ── SshInlinePem and AwsStatic assemble_draft paths ──────────────────────────────────────────

    /// Set the `cred_focus` in the ConnectionForm overlay (test helper).
    fn set_cred_focus(s: &mut AppState, idx: usize) {
        if let Some(Overlay::ConnectionForm { cred_focus, .. }) = &mut s.overlay {
            *cred_focus = idx;
        }
    }

    /// Type `text` into the current `cred_focus` credential field.
    fn type_cred(s: &mut AppState, text: &str) {
        for c in text.chars() {
            let _ = update(s, Msg::Text(TextEdit::Insert(c)));
        }
    }

    #[test]
    fn ssh_inline_pem_assemble_draft_carries_pem_and_emits_provision() {
        // Full flow for SshInlinePem: endpoint → picker (index 2) → CredentialFields → submit.
        // Checks that the emitted draft is SshInlinePem (not SshAgent or SshPrivateKeyFile).
        let mut s = base();
        s.vault_unlocked = true;

        pick_scheme(&mut s, 0); // ssh
        fill_field(&mut s, 0, "PemServer");
        fill_field(&mut s, 1, "pem.example.com");
        fill_field(&mut s, 2, "admin");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // SSH methods: [SshAgent=0, SshPrivateKeyFile=1, SshInlinePem=2, SshPassword=3]
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 2; // SshInlinePem
        }
        let _ = update(&mut s, Msg::Action(Action::Enter)); // → CredentialFields

        // cred_focus 0 = cred_key_pem (required, secret)
        set_cred_focus(&mut s, 0);
        type_cred(&mut s, "-----BEGIN OPENSSH PRIVATE KEY-----");

        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert_eq!(effects.len(), 1, "must emit one effect: {effects:?}");
        match &effects[0] {
            AppEffect::ProvisionAndSaveConnection { draft, .. } => {
                assert!(
                    matches!(draft, crate::forms::CredentialDraft::SshInlinePem { .. }),
                    "draft must be SshInlinePem: {draft:?}"
                );
                // The debug output must not contain the PEM material.
                let dbg = format!("{draft:?}");
                assert!(
                    !dbg.contains("BEGIN OPENSSH"),
                    "debug must not contain PEM key material: {dbg}"
                );
            }
            other => panic!("expected ProvisionAndSaveConnection: {other:?}"),
        }
    }

    #[test]
    fn aws_static_assemble_draft_carries_access_key_and_emits_provision() {
        // Full flow for AwsStatic: fill access_key_id (plain) and secret_access_key (secret).
        let mut s = base();
        s.vault_unlocked = true;

        pick_scheme(&mut s, 1); // s3
        fill_field(&mut s, 0, "StaticBucket");
        fill_field(&mut s, 1, "my-static-bucket");
        let _ = update(&mut s, Msg::Text(TextEdit::Submit)); // → CredentialMethodPicker

        // S3 methods: [AwsDefaultChain=0, AwsProfile=1, AwsStatic=2]
        if let Some(Overlay::ConnectionForm {
            cred_method_cursor, ..
        }) = &mut s.overlay
        {
            *cred_method_cursor = 2; // AwsStatic
        }
        let _ = update(&mut s, Msg::Action(Action::Enter)); // → CredentialFields

        // cred_focus 0 = cred_access_key_id (required, plain)
        set_cred_focus(&mut s, 0);
        type_cred(&mut s, "AKIAIOSFODNN7EXAMPLE");

        // cred_focus 1 = cred_secret_access_key (required, secret)
        set_cred_focus(&mut s, 1);
        type_cred(&mut s, "wJalrXUtnFEMI");

        let effects = update(&mut s, Msg::Text(TextEdit::Submit));
        assert_eq!(effects.len(), 1, "must emit one effect: {effects:?}");
        match &effects[0] {
            AppEffect::ProvisionAndSaveConnection { draft, .. } => {
                assert!(
                    matches!(
                        draft,
                        crate::forms::CredentialDraft::AwsStatic {
                            access_key_id,
                            ..
                        } if access_key_id == "AKIAIOSFODNN7EXAMPLE"
                    ),
                    "draft must be AwsStatic with correct access_key_id: {draft:?}"
                );
                // The secret key must not appear in the debug output.
                let dbg = format!("{draft:?}");
                assert!(
                    !dbg.contains("wJalrXUtnFEMI"),
                    "debug must not contain the secret key: {dbg}"
                );
            }
            other => panic!("expected ProvisionAndSaveConnection: {other:?}"),
        }
    }
}
