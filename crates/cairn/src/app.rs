//! The application event loop and effect runner.
//!
//! Ties together `cairn-core` (state + reducer), `cairn-tui` (render + keymap), and the VFS
//! backends. The render path is synchronous; all I/O runs as tokio tasks whose results return as
//! [`AppEvent`]s over a bounded channel — see `docs/LLD.md` §4–§6.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cairn_backend_local::LocalVfs;
use cairn_config::Config;
use cairn_core::{
    initial_effects, update, Action, AppEffect, AppEvent, AppState, ConnectionChoice, Msg,
};
use cairn_transfer::{ConflictPolicy, TransferOp, TransferSpec, VerifyPolicy};
use cairn_tui::{text_edit_for, Keymap, Theme};
use cairn_types::{ConnectionId, VfsPath};
use cairn_vfs::{ListOpts, ListPage, Recurse, Vfs, VfsError, VfsRegistry};
use futures::StreamExt;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const LEFT: ConnectionId = ConnectionId(1);
const RIGHT: ConnectionId = ConnectionId(2);

/// UI progress granularity: the transfer callback notifies the status bar at most every this many
/// bytes. 256 KiB balances update frequency against channel pressure; progress is sent best-effort
/// (`try_send`, dropped when the channel is full), so there is no back-pressure on the transfer.
const TRANSFER_PROGRESS_STEP: u64 = 256 * 1024;

/// The resolved UI configuration threaded through the event loop (input mapping + colors).
struct Ui {
    keymap: Keymap,
    theme: Theme,
}

/// Build the runtime and run the application to completion.
pub(crate) fn run() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_async())
}

async fn run_async() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("cairn requires an interactive terminal (stdout is not a TTY)");
    }

    // Both panes browse the local filesystem rooted at the current directory.
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let registry = VfsRegistry::new();
    registry
        .insert(LEFT, Arc::new(LocalVfs::new(LEFT, root.clone())))
        .await;
    registry
        .insert(RIGHT, Arc::new(LocalVfs::new(RIGHT, root)))
        .await;

    let mut state = AppState::new(LEFT, RIGHT, VfsPath::root());

    // Load user config (keybinding overrides, connection profiles, …); fall back to defaults.
    let config = load_config();
    let (keymap, warnings) = Keymap::from_overrides(&config.ui.keybindings);
    for w in warnings {
        tracing::warn!("{w}");
    }

    // Register the switchable connections (Ctrl-O) and record their labels: built-in local roots
    // plus any `scheme = "local"` profiles from config. Non-local profiles need live transport and
    // are not yet connectable.
    state.connections = register_connections(&registry, &config).await;

    // Resolve the color theme from the preset + per-role config overrides.
    let (theme, theme_warnings) = Theme::resolve(&config.ui.theme, &config.ui.colors);
    for w in theme_warnings {
        tracing::warn!("{w}");
    }
    let ui = Ui { keymap, theme };

    let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(256);
    let (input_tx, mut input_rx) = mpsc::channel::<Event>(256);
    spawn_input_reader(input_tx);

    let mut terminal = ratatui::init();
    install_terminal_panic_hook();

    // Initial effects are only directory listings — no transfer, so no token slot needed.
    let initial = initial_effects(&state);
    debug_assert!(
        initial.iter().all(|e| matches!(e, AppEffect::List { .. })),
        "initial_effects must emit only List effects; a transfer here would be uncancellable"
    );
    for effect in initial {
        dispatch(effect, &registry, &event_tx, &mut None, &mut None);
    }
    terminal.draw(|f| cairn_tui::render(f, &state, &ui.theme))?;

    let result = event_loop(
        &mut terminal,
        &mut state,
        &registry,
        &ui,
        &event_tx,
        &mut event_rx,
        &mut input_rx,
    )
    .await;

    ratatui::restore();
    result
}

/// Load the user config from the platform config path, returning defaults if it is missing or
/// unreadable (a broken config must never prevent the app from starting).
fn load_config() -> Config {
    let Some(path) = cairn_config::default_config_path() else {
        tracing::debug!("no platform config directory; using default config");
        return Config::default();
    };
    match Config::load(&path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load config; using defaults");
            Config::default()
        }
    }
}

/// Register the connections offered by the switcher and return their UI choices. Built-in local
/// roots (`/` and `$HOME` when set) plus each `scheme = "local"` config profile (using its
/// `endpoint.path`). Non-local profiles require live transport and are skipped for now.
async fn register_connections(registry: &VfsRegistry, config: &Config) -> Vec<ConnectionChoice> {
    // Build the (path, label) targets: built-in roots first, then local config profiles.
    let mut targets: Vec<(PathBuf, String)> = vec![(PathBuf::from("/"), "local: /".to_owned())];
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        targets.push((
            PathBuf::from(&home),
            format!("local: {}", home.to_string_lossy()),
        ));
    }
    for prof in &config.connections {
        if prof.scheme == "local" {
            if let Some(path) = prof.endpoint.get("path") {
                targets.push((PathBuf::from(path), format!("local: {}", prof.display_name)));
            }
        }
    }

    // Switcher connection ids follow the startup panes (LEFT/RIGHT) so they never collide.
    let base = RIGHT.0 + 1;
    let mut choices = Vec::with_capacity(targets.len());
    for (i, (path, label)) in targets.into_iter().enumerate() {
        let id = ConnectionId(base + i as u64);
        registry.insert(id, Arc::new(LocalVfs::new(id, path))).await;
        choices.push(ConnectionChoice { conn: id, label });
    }
    choices
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut AppState,
    registry: &VfsRegistry,
    ui: &Ui,
    event_tx: &mpsc::Sender<AppEvent>,
    event_rx: &mut mpsc::Receiver<AppEvent>,
    input_rx: &mut mpsc::Receiver<Event>,
) -> anyhow::Result<()> {
    // Cancellation tokens of the in-flight transfer / AI plan (if any), held runtime-side so the
    // matching Cancel effect can signal them. Each is cleared when its Done event arrives.
    // NOTE: if a third independently-cancellable operation appears, extract a shared cancel-slot
    // abstraction rather than repeating this pattern again.
    let mut transfer_cancel: Option<CancellationToken> = None;
    let mut ai_cancel: Option<CancellationToken> = None;
    loop {
        let msg = tokio::select! {
            Some(ev) = event_rx.recv() => Some(Msg::Event(ev)),
            Some(input) = input_rx.recv() => map_input(input, &ui.keymap, state),
            else => break,
        };
        let Some(msg) = msg else { continue };

        // Clear before `update`: if `update(TransferDone)` ever emitted a new Transfer, it would get
        // a fresh slot rather than one cleared right after dispatch. A conflict means no transfer ran.
        if matches!(
            msg,
            Msg::Event(AppEvent::TransferDone { .. } | AppEvent::TransferConflict { .. })
        ) {
            transfer_cancel = None;
        }
        if matches!(msg, Msg::Event(AppEvent::AiPlanExecuted { .. })) {
            ai_cancel = None;
        }
        let effects = update(state, msg);
        if state.should_quit {
            break;
        }
        terminal.draw(|f| cairn_tui::render(f, state, &ui.theme))?;
        for effect in effects {
            dispatch(
                effect,
                registry,
                event_tx,
                &mut transfer_cancel,
                &mut ai_cancel,
            );
        }
    }
    Ok(())
}

/// Translate a terminal event into a reducer message (or `None` to ignore).
///
/// While a text prompt is capturing input, keys are routed to the field as [`Msg::Text`] rather than
/// resolved to actions — except `Ctrl-C`, which always quits so the user is never trapped in a field.
fn map_input(input: Event, keymap: &Keymap, state: &AppState) -> Option<Msg> {
    match input {
        Event::Key(key) if state.capturing_text() => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                return Some(Msg::Action(Action::Quit));
            }
            text_edit_for(key).map(Msg::Text)
        }
        Event::Key(key) => keymap.action_for(key).map(Msg::Action),
        // A resize triggers a redraw via the no-op tick.
        Event::Resize(_, _) => Some(Msg::Tick),
        _ => None,
    }
}

/// Execute an effect on the tokio runtime; results flow back as [`AppEvent`]s. `transfer_cancel`
/// holds the in-flight transfer's cancellation token so a [`AppEffect::CancelTransfer`] can fire it.
fn dispatch(
    effect: AppEffect,
    registry: &VfsRegistry,
    event_tx: &mpsc::Sender<AppEvent>,
    transfer_cancel: &mut Option<CancellationToken>,
    ai_cancel: &mut Option<CancellationToken>,
) {
    match effect {
        AppEffect::List {
            pane,
            conn,
            dir,
            all,
        } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let result = list_dir(&registry, conn, &dir, all).await;
                let _ = event_tx
                    .send(AppEvent::Listed {
                        pane,
                        conn,
                        dir,
                        result,
                    })
                    .await;
            });
        }
        AppEffect::Transfer {
            src_conn,
            dst_conn,
            items,
            is_move,
            overwrite,
        } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            // Hand the task a token whose clone we keep, so `CancelTransfer` can abort it.
            let cancel = CancellationToken::new();
            *transfer_cancel = Some(cancel.clone());
            tokio::spawn(async move {
                let ev = run_transfer_effect(
                    &registry, src_conn, dst_conn, items, is_move, overwrite, &event_tx, cancel,
                )
                .await;
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::CancelTransfer => {
            if let Some(token) = transfer_cancel.take() {
                token.cancel();
            }
        }
        AppEffect::Delete { conn, paths } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ev = run_delete_effect(&registry, conn, paths).await;
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::CreateDir { conn, path } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ev = run_create_dir_effect(&registry, conn, path).await;
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::Rename { conn, from, to } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ev = run_rename_effect(&registry, conn, from, to).await;
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::RequestAiPlan { prompt } => {
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let _ = event_tx
                    .send(AppEvent::AiPlanProposed(propose_plan(&prompt).await))
                    .await;
            });
        }
        AppEffect::ExecutePlan { mut plan } => {
            // Run the approved plan's steps against the registered backends (RFC-0007). Safe/local
            // tools execute now; exec/logs/port-forward report not-yet-available until the live
            // invoke path lands.
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            // Hand the execution a token (cancellation is checked between steps) and keep a clone so
            // a `CancelAiPlan` effect can abort it. Only one plan executes at a time (the reducer
            // refuses a second while `ai_executing`), so the slot is never live here.
            debug_assert!(
                ai_cancel.is_none(),
                "overwriting a live AI-plan cancel token"
            );
            let cancel = CancellationToken::new();
            *ai_cancel = Some(cancel.clone());
            tokio::spawn(async move {
                // The assistant may only act on the two pane connections it can see in its
                // WorldSnapshot — not the switcher backends (which include a root-mounted `/`).
                let exec = crate::executor::BinaryStepExecutor::new(registry, vec![LEFT, RIGHT]);
                let n = plan.steps.len();
                let ev = match plan.execute(&exec, &|| cancel.is_cancelled()).await {
                    Ok(()) if plan.state == cairn_ai::PlanState::Done => {
                        // Surface the steps' secret-free output summaries (RFC-0007 Gap 1).
                        let outputs: Vec<String> = plan
                            .steps
                            .iter()
                            .filter_map(|s| s.output.as_ref().map(|o| format!("{}→{o}", s.tool)))
                            .collect();
                        let detail = if outputs.is_empty() {
                            format!("{n} step(s) executed")
                        } else {
                            outputs.join("; ")
                        };
                        AppEvent::AiPlanExecuted {
                            status: format!("Plan complete: {detail}"),
                            error: false,
                        }
                    }
                    Ok(()) if plan.state == cairn_ai::PlanState::Aborted => {
                        let done = plan
                            .steps
                            .iter()
                            .filter(|s| s.status == cairn_ai::StepStatus::Done)
                            .count();
                        AppEvent::AiPlanExecuted {
                            status: format!("Plan cancelled after {done} step(s)"),
                            error: false,
                        }
                    }
                    Ok(()) => {
                        // The only remaining Ok state is Failed; surface the step's redacted reason.
                        debug_assert_eq!(plan.state, cairn_ai::PlanState::Failed);
                        let why = plan
                            .steps
                            .iter()
                            .find_map(|s| s.error.clone())
                            .unwrap_or_else(|| "a step failed".to_owned());
                        AppEvent::AiPlanExecuted {
                            status: format!("Plan stopped: {why}"),
                            error: true,
                        }
                    }
                    Err(e) => AppEvent::AiPlanExecuted {
                        status: format!("Plan not executed: {e}"),
                        error: true,
                    },
                };
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::CancelAiPlan => {
            if let Some(token) = ai_cancel.take() {
                token.cancel();
            }
        }
        // `AppEffect` is non-exhaustive; future variants are wired up in later milestones.
        other => tracing::warn!(effect = ?other, "unhandled effect"),
    }
}

/// Ask the assistant to propose a plan. Until the HTTP providers (M7-2) land, this uses an offline
/// `MockProvider`; the plan → confirm flow it drives in the UI is the real thing.
async fn propose_plan(prompt: &str) -> Result<cairn_ai::Plan, String> {
    use cairn_ai::{request_plan, LlmRequest, Message, MockProvider, Role};
    // A representative, executable proposal (safe/local tools) that exercises the full
    // plan → confirm → execute loop against the left pane's connection (RFC-0007 input schema).
    let provider = MockProvider::proposing(serde_json::json!({
        "summary": format!("Plan for: {prompt}"),
        "steps": [
            {"tool": "list", "input": {"conn": "conn:1", "path": "/"},
             "description": "list the current directory"},
            {"tool": "stat", "input": {"conn": "conn:1", "path": "/"},
             "description": "confirm the directory exists"}
        ]
    }));
    let req = LlmRequest {
        system: None,
        messages: vec![Message {
            role: Role::User,
            text: prompt.to_owned(),
        }],
        tools: Vec::new(),
    };
    // `AgentError`'s Display is our own, secret-free enum. When the HTTP providers (M7-2) land,
    // map their transport errors to categorized, redacted messages here before they reach the UI.
    request_plan(&provider, req)
        .await
        .map_err(|e| e.to_string())
}

/// A compact "N file(s), M dir(s)" summary shared by the success and cancelled status messages.
fn outcome_summary(o: &cairn_transfer::TransferOutcome) -> String {
    format!("{} file(s), {} dir(s)", o.files, o.dirs)
}

/// Average throughput in bytes/sec over `secs` elapsed. The elapsed time is floored at a small
/// epsilon so a near-instant transfer reports a sane number rather than an absurd one-frame spike;
/// the `f64`→`u64` cast saturates (never wraps/panics).
fn avg_rate(bytes: u64, secs: f64) -> u64 {
    let secs = secs.max(0.05);
    (bytes as f64 / secs) as u64
}

/// Sum the source file sizes of a transfer's `items` (recursively for directories), for a
/// percentage/ETA. Best-effort: returns `None` if any stat/listing fails or a size is unknown, so
/// the caller falls back to a byte+rate display rather than a misleading total.
async fn scan_total_bytes(src: &Arc<dyn Vfs>, items: &[(VfsPath, VfsPath)]) -> Option<u64> {
    let mut total: u64 = 0;
    for (from, _) in items {
        let mut stack = vec![from.clone()];
        while let Some(path) = stack.pop() {
            let meta = src.stat(&path).await.ok()?;
            if meta.is_dir() {
                let mut stream = src.list(&path, ListOpts { all: true });
                while let Some(page) = stream.next().await {
                    for e in page.ok()?.entries {
                        stack.push(path.join(&e.name).ok()?);
                    }
                }
            } else if let Some(sz) = meta.size {
                total = total.saturating_add(sz);
            }
            // Entries with no known size (symlinks, special files) contribute nothing rather than
            // disabling the whole estimate — the total is a lower bound; the % display clamps to 100.
        }
    }
    Some(total)
}

#[allow(clippy::too_many_arguments)]
async fn run_transfer_effect(
    registry: &VfsRegistry,
    src_conn: ConnectionId,
    dst_conn: ConnectionId,
    items: Vec<(VfsPath, VfsPath)>,
    is_move: bool,
    overwrite: bool,
    event_tx: &mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
) -> AppEvent {
    let (Some(src), Some(dst)) = (registry.get(src_conn).await, registry.get(dst_conn).await)
    else {
        return AppEvent::TransferDone {
            status: "connection unavailable".to_owned(),
            error: true,
        };
    };
    // Unless the user already confirmed, refuse to clobber: count existing destinations and bounce
    // back a `TransferConflict` so the UI can ask first (data-safety — no silent overwrite). Only a
    // definite `NotFound` is "safe to write"; any other stat error aborts rather than risk an
    // overwrite (mirrors `run_rename_effect`). NOTE: this is check-then-act — a destination that
    // appears in the TOCTOU window before the write is still overwritten.
    if !overwrite {
        let mut conflicts = 0usize;
        for (_, to) in &items {
            if cancel.is_cancelled() {
                return AppEvent::TransferDone {
                    status: "Transfer cancelled".to_owned(),
                    error: false,
                };
            }
            match dst.stat(to).await {
                Ok(_) => conflicts += 1,
                Err(VfsError::NotFound(_)) => {}
                Err(e) => {
                    return AppEvent::TransferDone {
                        status: format!("Transfer aborted: {}", e.redacted()),
                        error: true,
                    };
                }
            }
        }
        if conflicts > 0 {
            return AppEvent::TransferConflict {
                src_conn,
                dst_conn,
                items,
                is_move,
                conflicts,
            };
        }
    }
    let spec = TransferSpec {
        op: if is_move {
            TransferOp::Move
        } else {
            TransferOp::Copy
        },
        conflict: ConflictPolicy::Overwrite,
        verify: VerifyPolicy::Size,
    };
    let verb = if is_move { "Moved" } else { "Copied" };
    // Pre-scan the source size for a percentage/ETA. Best-effort: `None` (a backend that can't be
    // walked, an error, or cancellation) degrades to byte+rate display. Skipped for a same-connection
    // move — that takes the engine's instant rename fast-path which writes no bytes, so a scan would
    // be wasted work and the bar would sit at 0%. Cancellable so a big-tree scan doesn't block Esc.
    let total = if is_move && src_conn == dst_conn {
        None
    } else {
        tokio::select! {
            t = scan_total_bytes(&src, &items) => t,
            () = cancel.cancelled() => None,
        }
    };
    // Emit coalesced, non-blocking progress: accumulate bytes and notify the UI at most every
    // `TRANSFER_PROGRESS_STEP` bytes via `try_send`, which drops the update if the bounded channel is
    // full rather than stalling the transfer task (the render path must never be blocked here). The
    // reported rate is the average throughput since the transfer started.
    let started = std::time::Instant::now();
    let rate_bps = |bytes: u64| -> u64 { avg_rate(bytes, started.elapsed().as_secs_f64()) };
    let mut bytes = 0u64;
    let mut last_sent = 0u64;
    let mut on_progress = |b: u64| {
        bytes += b;
        debug_assert!(bytes >= last_sent, "progress bytes must be cumulative");
        if bytes - last_sent >= TRANSFER_PROGRESS_STEP {
            last_sent = bytes;
            let _ = event_tx.try_send(AppEvent::TransferProgress {
                bytes,
                rate_bps: rate_bps(bytes),
                total,
            });
        }
    };
    match cairn_transfer::run_transfer(&src, &dst, &items, spec, &cancel, &mut on_progress).await {
        Ok(out) => {
            // Flush the exact final total for one frame before `TransferDone` clears the indicator
            // (so a transfer smaller than the coalescing step doesn't only ever show "0 B").
            let _ = event_tx.try_send(AppEvent::TransferProgress {
                bytes: out.bytes,
                rate_bps: rate_bps(out.bytes),
                total,
            });
            AppEvent::TransferDone {
                status: format!("{verb} {}", outcome_summary(&out)),
                error: false,
            }
        }
        Err(cairn_transfer::TransferError::Cancelled(done)) => AppEvent::TransferDone {
            // Cancellation is cooperative and mid-flight; report what already completed (a move's
            // earlier sources are already deleted) so the user knows partial changes remain.
            status: if done == cairn_transfer::TransferOutcome::default() {
                "Transfer cancelled".to_owned()
            } else {
                format!(
                    "Transfer cancelled after {}; partial changes may remain",
                    outcome_summary(&done)
                )
            },
            error: false,
        },
        Err(e) => AppEvent::TransferDone {
            status: format!("{} failed: {}", verb.trim_end_matches('d'), e.redacted()),
            error: true,
        },
    }
}

async fn run_delete_effect(
    registry: &VfsRegistry,
    conn: ConnectionId,
    paths: Vec<VfsPath>,
) -> AppEvent {
    let Some(vfs) = registry.get(conn).await else {
        return AppEvent::OpDone {
            status: "connection unavailable".to_owned(),
            error: true,
        };
    };
    let mut deleted = 0u64;
    for path in &paths {
        if let Err(e) = vfs.remove(path, Recurse::Yes).await {
            return AppEvent::OpDone {
                status: format!("Delete failed: {}", e.redacted()),
                error: true,
            };
        }
        deleted += 1;
    }
    AppEvent::OpDone {
        status: format!("Deleted {deleted} item(s)"),
        error: false,
    }
}

async fn run_create_dir_effect(
    registry: &VfsRegistry,
    conn: ConnectionId,
    path: VfsPath,
) -> AppEvent {
    let Some(vfs) = registry.get(conn).await else {
        return AppEvent::OpDone {
            status: "connection unavailable".to_owned(),
            error: true,
        };
    };
    match vfs.create_dir(&path).await {
        Ok(()) => AppEvent::OpDone {
            status: "Directory created".to_owned(),
            error: false,
        },
        Err(e) => AppEvent::OpDone {
            status: format!("Create failed: {}", e.redacted()),
            error: true,
        },
    }
}

async fn run_rename_effect(
    registry: &VfsRegistry,
    conn: ConnectionId,
    from: VfsPath,
    to: VfsPath,
) -> AppEvent {
    let Some(vfs) = registry.get(conn).await else {
        return AppEvent::OpDone {
            status: "connection unavailable".to_owned(),
            error: true,
        };
    };
    // Refuse to clobber an existing target — a rename must not silently destroy data (local
    // `fs::rename` overwrites atomically, so the backend won't stop us). Only a definite "not found"
    // is safe to proceed on; any other stat error (forbidden, transport) aborts rather than risking
    // an overwrite.
    match vfs.stat(&to).await {
        Ok(_) => {
            return AppEvent::OpDone {
                status: "Rename failed: destination already exists".to_owned(),
                error: true,
            };
        }
        Err(VfsError::NotFound(_)) => {}
        Err(e) => {
            return AppEvent::OpDone {
                status: format!("Rename failed: {}", e.redacted()),
                error: true,
            };
        }
    }
    match vfs.rename(&from, &to).await {
        Ok(()) => AppEvent::OpDone {
            status: "Renamed".to_owned(),
            error: false,
        },
        Err(e) => AppEvent::OpDone {
            status: format!("Rename failed: {}", e.redacted()),
            error: true,
        },
    }
}

async fn list_dir(
    registry: &VfsRegistry,
    conn: ConnectionId,
    dir: &VfsPath,
    all: bool,
) -> Result<ListPage, VfsError> {
    let Some(vfs) = registry.get(conn).await else {
        return Err(VfsError::NotFound(dir.clone()));
    };
    collect_pages(vfs, dir, all).await
}

/// Drain a backend's listing stream into a single page (sufficient for backends that paginate; the
/// UI virtualizes either way).
async fn collect_pages(vfs: Arc<dyn Vfs>, dir: &VfsPath, all: bool) -> Result<ListPage, VfsError> {
    let mut entries = Vec::new();
    let mut stream = vfs.list(dir, ListOpts { all });
    while let Some(page) = stream.next().await {
        let page = page?;
        entries.extend(page.entries);
        if page.done {
            break;
        }
    }
    Ok(ListPage {
        entries,
        cursor: None,
        done: true,
    })
}

/// Spawn a blocking OS thread that reads terminal events and forwards them over the channel. Reading
/// input off the async runtime keeps a slow render from starving input and vice versa.
fn spawn_input_reader(tx: mpsc::Sender<Event>) {
    std::thread::spawn(move || loop {
        match event::poll(Duration::from_millis(200)) {
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if tx.blocking_send(ev).is_err() {
                        break; // receiver dropped; app is shutting down
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });
}

/// Ensure a panic restores the terminal before the message is printed, so it is never left in raw
/// mode / the alternate screen.
fn install_terminal_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[tokio::test]
    async fn list_dir_unknown_connection_is_not_found() {
        let registry = VfsRegistry::new();
        let res = list_dir(&registry, ConnectionId(99), &VfsPath::root(), false).await;
        assert!(matches!(res, Err(VfsError::NotFound(_))));
    }

    #[tokio::test]
    async fn list_dir_reads_a_registered_backend() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        let page = list_dir(&registry, LEFT, &VfsPath::root(), false)
            .await
            .unwrap();
        assert!(page.entries.iter().any(|e| e.name == "hello.txt"));
    }

    #[tokio::test]
    async fn list_dir_all_flag_controls_hidden_entries() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("visible.txt"), b"hi").unwrap();
        std::fs::write(dir.path().join(".secret"), b"shh").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        // all = false hides dotfiles.
        let hidden = list_dir(&registry, LEFT, &VfsPath::root(), false)
            .await
            .unwrap();
        assert!(hidden.entries.iter().all(|e| e.name != ".secret"));
        // all = true reveals them.
        let shown = list_dir(&registry, LEFT, &VfsPath::root(), true)
            .await
            .unwrap();
        assert!(shown.entries.iter().any(|e| e.name == ".secret"));
    }

    #[test]
    fn map_input_translates_keys_and_resize() {
        let km = Keymap::default();
        let st = AppState::new(LEFT, RIGHT, VfsPath::root());
        let q = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(map_input(q, &km, &st), Some(Msg::Action(_))));
        assert!(matches!(
            map_input(Event::Resize(80, 24), &km, &st),
            Some(Msg::Tick)
        ));
    }

    #[test]
    fn map_input_routes_keys_to_text_while_a_prompt_captures() {
        use cairn_core::{Overlay, PromptKind};
        let km = Keymap::default();
        let mut st = AppState::new(LEFT, RIGHT, VfsPath::root());
        st.overlay = Some(Overlay::Prompt {
            kind: PromptKind::MakeDir,
            input: String::new(),
        });
        // 'q' types a 'q' instead of quitting.
        let q = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(
            map_input(q, &km, &st),
            Some(Msg::Text(cairn_core::TextEdit::Insert('q')))
        ));
        // Ctrl-C still quits even while capturing.
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(
            map_input(ctrl_c, &km, &st),
            Some(Msg::Action(Action::Quit))
        ));
    }

    #[tokio::test]
    async fn rename_refuses_to_overwrite_an_existing_destination() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"keep").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        let ev = run_rename_effect(
            &registry,
            LEFT,
            VfsPath::parse("/a.txt").unwrap(),
            VfsPath::parse("/b.txt").unwrap(),
        )
        .await;
        assert!(matches!(ev, AppEvent::OpDone { error: true, .. }));
        // The destination is untouched and the source still exists.
        assert_eq!(std::fs::read(dir.path().join("b.txt")).unwrap(), b"keep");
        assert!(dir.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn rename_moves_to_a_free_destination() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("a.txt"), b"data").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        let ev = run_rename_effect(
            &registry,
            LEFT,
            VfsPath::parse("/a.txt").unwrap(),
            VfsPath::parse("/c.txt").unwrap(),
        )
        .await;
        assert!(matches!(ev, AppEvent::OpDone { error: false, .. }));
        assert!(!dir.path().join("a.txt").exists());
        assert_eq!(std::fs::read(dir.path().join("c.txt")).unwrap(), b"data");
    }

    #[tokio::test]
    async fn cancelled_transfer_reports_a_non_error_completion() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("src.txt"), b"data").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        registry
            .insert(RIGHT, Arc::new(LocalVfs::new(RIGHT, dir.path())))
            .await;
        // A token already fired before the first cancellation check short-circuits the transfer.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (tx, _rx) = mpsc::channel(8);
        let ev = run_transfer_effect(
            &registry,
            LEFT,
            RIGHT,
            vec![(
                VfsPath::parse("/src.txt").unwrap(),
                VfsPath::parse("/dst.txt").unwrap(),
            )],
            false,
            true, // overwrite: skip the conflict pre-check in this cancellation test
            &tx,
            cancel,
        )
        .await;
        match ev {
            AppEvent::TransferDone { status, error } => {
                assert!(!error, "cancellation is user-initiated, not a failure");
                assert!(status.contains("cancelled"), "got: {status}");
            }
            _ => panic!("expected TransferDone"),
        }
        // The destination was never written.
        assert!(!dir.path().join("dst.txt").exists());
    }

    #[tokio::test]
    async fn transfer_pre_check_reports_a_conflict_for_an_existing_destination() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("src.txt"), b"new").unwrap();
        std::fs::write(dir.path().join("dst.txt"), b"existing").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        let (tx, _rx) = mpsc::channel(8);
        // overwrite = false → the existing /dst.txt is detected and reported, not clobbered.
        let ev = run_transfer_effect(
            &registry,
            LEFT,
            LEFT,
            vec![(
                VfsPath::parse("/src.txt").unwrap(),
                VfsPath::parse("/dst.txt").unwrap(),
            )],
            false,
            false,
            &tx,
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            ev,
            AppEvent::TransferConflict { conflicts: 1, .. }
        ));
        // The existing destination is untouched.
        assert_eq!(
            std::fs::read(dir.path().join("dst.txt")).unwrap(),
            b"existing"
        );
    }

    #[tokio::test]
    async fn create_dir_makes_a_directory() {
        let dir = tempfile_dir();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        let ev = run_create_dir_effect(&registry, LEFT, VfsPath::parse("/sub").unwrap()).await;
        assert!(matches!(ev, AppEvent::OpDone { error: false, .. }));
        assert!(dir.path().join("sub").is_dir());
    }

    #[tokio::test]
    async fn scan_total_bytes_sums_files_recursively() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap(); // 5
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("d/b.txt"), b"world!").unwrap(); // 6
        let src: Arc<dyn Vfs> = Arc::new(LocalVfs::new(LEFT, dir.path()));
        let items = vec![
            (VfsPath::parse("/a.txt").unwrap(), VfsPath::root()),
            (VfsPath::parse("/d").unwrap(), VfsPath::root()),
        ];
        assert_eq!(scan_total_bytes(&src, &items).await, Some(11));
    }

    #[tokio::test]
    async fn scan_total_bytes_skips_unsized_entries_and_degrades_on_error() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap(); // 5
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", dir.path().join("link")).unwrap();
        let src: Arc<dyn Vfs> = Arc::new(LocalVfs::new(LEFT, dir.path()));
        // A symlink (no known size) is skipped, not fatal: the directory total is just the file.
        assert_eq!(
            scan_total_bytes(&src, &[(VfsPath::root(), VfsPath::root())]).await,
            Some(5)
        );
        // A missing source degrades to None (best-effort) rather than panicking.
        let missing = vec![(VfsPath::parse("/nope").unwrap(), VfsPath::root())];
        assert_eq!(scan_total_bytes(&src, &missing).await, None);
    }

    #[test]
    fn avg_rate_floors_elapsed_and_saturates() {
        // 1 MiB over 1s → 1 MiB/s.
        assert_eq!(avg_rate(1024 * 1024, 1.0), 1024 * 1024);
        // Near-instant transfer: elapsed is floored at 0.05s, so the rate is bounded, not absurd.
        assert_eq!(avg_rate(1024 * 1024, 0.0), 1024 * 1024 * 20);
        assert_eq!(avg_rate(1024 * 1024, -1.0), 1024 * 1024 * 20); // negative also floored
                                                                   // No panic / wrap at the extreme.
        let _ = avg_rate(u64::MAX, 0.000_001);
    }

    fn tempfile_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
}
