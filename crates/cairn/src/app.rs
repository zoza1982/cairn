//! The application event loop and effect runner.
//!
//! Ties together `cairn-core` (state + reducer), `cairn-tui` (render + keymap), and the VFS
//! backends. The render path is synchronous; all I/O runs as tokio tasks whose results return as
//! [`AppEvent`]s over a bounded channel — see `docs/LLD.md` §4–§6.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::connect::coordinator::{ConnectionCoordinator, DeferredConnection};
use crate::connect::descriptor::ConnectionDescriptor;
use crate::connect::provider::DiscoveryCtx;
use cairn_backend_local::LocalVfs;
use cairn_broker::{Actor, Broker};
use cairn_config::Config;
use cairn_core::{
    initial_effects, update, Action, AppEffect, AppEvent, AppState, ChoiceProvenance, ChoiceStatus,
    ConnectionChoice, ConnectionKind, LogViewerId, Msg, Overlay, ShellActionMeta, TransferId,
};
use cairn_transfer::{ConflictPolicy, TransferOp, TransferSpec, VerifyPolicy};
use cairn_tui::{text_edit_for, Keymap, Theme};
use cairn_types::SessionId;
use cairn_types::{ConnectionId, VfsPath};
use cairn_vault::Vault;
use cairn_vfs::{ListOpts, ListPage, Recurse, Vfs, VfsError, VfsRegistry};
use futures::StreamExt;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
use tokio::sync::{mpsc, watch};
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

/// Runtime-side state for the vault-unlock flow (M3-7): the shared credential [`Broker`] (kept alive
/// so the unlock overlay can install a decrypted vault into it), the resolved vault file path, and
/// the connections deferred at startup while the vault was locked. Lives in the effect layer, never
/// in [`AppState`] — it holds no secret, but it holds the live broker handle.
struct VaultContext {
    broker: Arc<Broker>,
    vault_path: Option<PathBuf>,
    deferred: Arc<[DeferredConnection]>,
}

/// Split an absolute OS path into its root prefix (the `LocalVfs` base) and the remaining
/// segments as a [`VfsPath`].
///
/// On Unix `/home/user/projects` → `(PathBuf("/"), VfsPath("/home/user/projects"))`.
/// On Windows `C:\Users\me` → `(PathBuf("C:\"), VfsPath("/Users/me"))`.
///
/// Falls back to `(PathBuf("/"), VfsPath::root())` when:
/// - the path is relative (no root component),
/// - a segment is not valid UTF-8,
/// - a segment contains a `/` or control character (rejected by `VfsPath::join`), or
/// - a `..` component appears (should never happen from `current_dir()` but guarded).
fn split_cwd_root(cwd: &std::path::Path) -> (std::path::PathBuf, VfsPath) {
    use std::path::Component;

    // Any exit that hasn't yet accumulated a root component (Prefix or RootDir) must
    // fall back to "/" — otherwise the empty PathBuf makes `LocalVfs` fail closed and
    // both panes show nothing. This applies to relative-path inputs and any in-loop
    // early return that fires before the first root component is seen.
    let or_default = |r: std::path::PathBuf| -> std::path::PathBuf {
        if r.as_os_str().is_empty() {
            std::path::PathBuf::from("/")
        } else {
            r
        }
    };

    let mut root = std::path::PathBuf::new();
    let mut vfs = VfsPath::root();

    for component in cwd.components() {
        match component {
            // Prefix (e.g. `C:` on Windows) and RootDir (`/` on Unix, `\` on Windows)
            // together form the LocalVfs base directory.
            Component::Prefix(_) | Component::RootDir => {
                root.push(component.as_os_str());
            }
            // Each Normal segment becomes one VfsPath level.
            Component::Normal(name) => {
                let Some(s) = name.to_str() else {
                    // Non-UTF-8 path component: fall back so the UI still opens.
                    return (or_default(root), VfsPath::root());
                };
                match vfs.join(s) {
                    Ok(p) => vfs = p,
                    // Segment contains a control character or other disallowed byte (e.g. the
                    // OS accepted it but VfsPath cannot represent it). Fall back gracefully.
                    Err(_) => return (or_default(root), VfsPath::root()),
                }
            }
            // `.` is a no-op in an absolute path.
            Component::CurDir => {}
            // `..` should never appear in a canonical `current_dir()` result, but guard it.
            Component::ParentDir => return (or_default(root), VfsPath::root()),
        }
    }

    if root.as_os_str().is_empty() {
        // Relative path or empty input: no Prefix/RootDir was ever pushed.
        // Fall back entirely so neither the base nor the VfsPath is stale.
        return (std::path::PathBuf::from("/"), VfsPath::root());
    }

    (root, vfs)
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

    // Root both default panes at the OS filesystem root so the user can navigate all the way up
    // to `/` (Unix) or the drive root (Windows). `split_cwd_root` splits the launch directory
    // into the root prefix (the `LocalVfs` base) and the remaining path segments (the initial
    // `VfsPath`), so Cairn opens at the launch directory but `..` navigation is unrestricted.
    let (fs_root, initial_cwd) = {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        split_cwd_root(&cwd)
    };
    let registry = VfsRegistry::new();
    registry
        .insert(LEFT, Arc::new(LocalVfs::new(LEFT, fs_root.clone())))
        .await;
    registry
        .insert(RIGHT, Arc::new(LocalVfs::new(RIGHT, fs_root)))
        .await;

    let mut state = AppState::new(LEFT, RIGHT, initial_cwd);

    // Load user config (keybinding overrides, connection profiles, …); fall back to defaults.
    let config = load_config();
    // How many transfers run at once (clamped to >= 1 so a stray `0` can't wedge the queue).
    state.concurrency_limit = config.transfers.effective_concurrency();

    // Validate the (file-trusted) shell actions, dropping any malformed entry with a warning. The
    // surviving list, in order, is the single source of index alignment shared by the keymap,
    // `AppState::shell_actions`, and the runtime's `shell_action_defs`.
    let mut shell_action_defs = Vec::new();
    for def in &config.shell_actions {
        match def.validate() {
            Ok(()) => shell_action_defs.push(def.clone()),
            Err(reason) => tracing::warn!("ignoring shell action: {reason}"),
        }
    }
    state.shell_actions = shell_action_defs
        .iter()
        .map(|d| ShellActionMeta {
            name: d.name.clone(),
            confirm: d.confirm,
        })
        .collect();
    let shell_action_defs: Arc<[cairn_config::ShellActionDef]> = shell_action_defs.into();

    let (keymap, warnings) = Keymap::with_shell_actions(
        &config.ui.keybindings,
        shell_action_defs
            .iter()
            .enumerate()
            .map(|(i, d)| (i, d.key.clone())),
    );
    for w in warnings {
        tracing::warn!("{w}");
    }

    // The capability broker mediates credential resolution for credential-bearing (remote)
    // connections. It starts *locked*, so credential-bearing profiles can't be opened yet — the
    // opener resolves cleanly to a locked-vault error rather than connecting. The `Arc<Broker>` is
    // kept alive in the runtime-side `VaultContext` (below) so the vault-unlock overlay (M3-7) can
    // `.unlock(vault)` it and re-open the connections deferred here.
    let broker = Arc::new(Broker::locked());
    let opener = crate::connect::ConnectionOpener::new(broker.clone());

    // Register the switchable connections (Ctrl-O) and record their labels: built-in local roots
    // plus the config profiles. Local profiles mount immediately; credential-bearing profiles (ssh/
    // s3/gcs/azure) are opened via the broker-backed opener. A profile that can't open *because the
    // vault is locked* is returned as a `deferred` connection (retried after unlock); other failures
    // (missing field, backend not built) are skipped with a warning.
    let (choices, deferred, descriptors) = register_connections(&registry, &config, &opener).await;
    state.connections = choices;
    state.vault_unlocked = broker.is_unlocked(); // false at startup; flips on unlock
    state.has_locked_connections = !deferred.is_empty();
    if state.has_locked_connections {
        state.status = Some(format!(
            "{} connection(s) need the vault — press Ctrl-U to unlock",
            deferred.len()
        ));
    }

    // Runtime-side vault context: the shared broker, the resolved vault file path, and the profiles
    // deferred above. The unlock effect reads these to open the vault and retry those connections.
    let vault_ctx = VaultContext {
        broker,
        vault_path: config.vault_path(),
        deferred: deferred.into(),
    };

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
    let mut startup_controls = HashMap::new();
    let mut startup_log_controls: HashMap<LogViewerId, CancellationToken> = HashMap::new();
    let mut startup_session_controls: HashMap<SessionId, SessionControls> = HashMap::new();
    for effect in initial {
        dispatch(
            effect,
            &registry,
            &event_tx,
            &mut startup_controls,
            &mut None,
            &mut startup_log_controls,
            &mut startup_session_controls,
            &shell_action_defs,
            &vault_ctx,
        );
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
        &shell_action_defs,
        &vault_ctx,
        descriptors,
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
    let mut cfg = match Config::load(&path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load config; using defaults");
            Config::default()
        }
    };
    // Gate the executable shell-actions section on file trust (drops them from an untrusted file).
    if let Some(warning) = cfg.secure_shell_actions(&path) {
        tracing::warn!("{warning}");
    }
    cfg
}

/// Register the connections offered by the switcher and return their UI choices, the connections
/// deferred while the vault is locked, and the runtime descriptor side-map.
///
/// Delegates to [`ConnectionCoordinator`] (RFC-0011 P1). Built-in local roots (`/` and `$HOME`
/// when set) and each `scheme = "local"` config profile mount immediately; credential-bearing
/// profiles (`ssh`/`s3`/`gcs`/`azure`) are opened through the broker-backed opener. A profile
/// that fails **specifically because the vault is locked** is returned in the deferred vec so the
/// vault-unlock flow can retry it; any other failure is skipped with a warning.
///
/// The descriptor map is established here for P2 use (lazy open on selection); the values are
/// unused by reducer logic in P1.
async fn register_connections(
    registry: &VfsRegistry,
    config: &Config,
    opener: &crate::connect::ConnectionOpener,
) -> (
    Vec<ConnectionChoice>,
    Vec<DeferredConnection>,
    HashMap<ConnectionId, ConnectionDescriptor>,
) {
    let coordinator = ConnectionCoordinator::new(opener.clone(), RIGHT.0 + 1);
    // Derive vault_locked from the live broker state so this call site and future P2
    // re-enumeration calls automatically reflect the current lock status.
    let ctx = DiscoveryCtx {
        config,
        vault_locked: opener.vault_locked(),
    };
    coordinator.run(registry, &ctx).await
}

#[allow(clippy::too_many_arguments)]
async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut AppState,
    registry: &VfsRegistry,
    ui: &Ui,
    event_tx: &mpsc::Sender<AppEvent>,
    event_rx: &mut mpsc::Receiver<AppEvent>,
    input_rx: &mut mpsc::Receiver<Event>,
    shell_action_defs: &Arc<[cairn_config::ShellActionDef]>,
    vault_ctx: &VaultContext,
    descriptor_map: HashMap<ConnectionId, ConnectionDescriptor>,
) -> anyhow::Result<()> {
    // P2 will bind this map to resolve a ConnectionId to its descriptor on selection.
    let _p2_descriptor_map = descriptor_map;
    // Control channels of the in-flight transfer / AI plan (if any), held runtime-side so the
    // matching effect can signal them. Each is cleared when its Done event arrives.
    // Per-transfer control, keyed by `TransferId`: the cancel token + pause sender form a *control
    // pair* created together (in `AppEffect::Transfer`) and removed together (on that transfer's
    // `TransferDone`/`TransferConflict`). Multiple transfers run concurrently, so this is a map.
    let mut transfer_controls: HashMap<TransferId, TransferControls> = HashMap::new();
    let mut ai_cancel: Option<CancellationToken> = None;
    let mut log_viewer_controls: HashMap<LogViewerId, CancellationToken> = HashMap::new();
    let mut session_controls: HashMap<SessionId, SessionControls> = HashMap::new();
    loop {
        let msg = tokio::select! {
            Some(ev) = event_rx.recv() => Some(Msg::Event(ev)),
            Some(input) = input_rx.recv() => map_input(input, &ui.keymap, state),
            else => break,
        };
        let Some(msg) = msg else { continue };

        // Clear before `update`: a transfer's Done/Conflict releases its control entry *before*
        // `update` (which may start a queued transfer via the tail-drain) so the fresh entry the new
        // transfer's dispatch inserts isn't wiped.
        if let Msg::Event(
            AppEvent::TransferDone { id, .. } | AppEvent::TransferConflict { id, .. },
        ) = &msg
        {
            transfer_controls.remove(id);
        }
        if matches!(msg, Msg::Event(AppEvent::AiPlanExecuted { .. })) {
            ai_cancel = None;
        }
        if let Msg::Event(AppEvent::LogStreamEnded { id, .. }) = &msg {
            log_viewer_controls.remove(id);
        }
        // Session cleanup: remove the controls entry when the session ends so the oneshot/mpsc
        // senders are dropped (closing stdin and signalling the relay task) if they haven't been
        // consumed already. The session record in `AppState::sessions` is cleaned up by the reducer.
        if let Msg::Event(AppEvent::SessionEnded { id, .. }) = &msg {
            session_controls.remove(id);
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
                &mut transfer_controls,
                &mut ai_cancel,
                &mut log_viewer_controls,
                &mut session_controls,
                shell_action_defs,
                vault_ctx,
            );
        }
    }
    Ok(())
}

/// The per-transfer control pair held runtime-side, keyed by [`TransferId`].
struct TransferControls {
    cancel: CancellationToken,
    pause: watch::Sender<bool>,
}

/// Drop-guard that emits a synthetic [`AppEvent::TransferDone`] if the transfer task ends without
/// sending its own (a panic or an early drop). This keeps the reducer's `active_transfers` and the
/// runtime's control map from leaking a slot. Disarmed once the task produces its real outcome event.
struct TransferDoneGuard {
    id: TransferId,
    event_tx: mpsc::Sender<AppEvent>,
    armed: bool,
}

impl Drop for TransferDoneGuard {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort (`try_send`, no `.await` allowed in Drop). If the bounded event channel is
            // momentarily full the synthetic event is dropped and this transfer's slot leaks for the
            // process lifetime — but the channel (256 deep) is ~never near full at concurrency ≤ a
            // few while a task panics, so the race is negligible.
            let _ = self.event_tx.try_send(AppEvent::TransferDone {
                id: self.id,
                status: "Transfer interrupted".to_owned(),
                error: true,
            });
        }
    }
}

/// Emits a synthetic [`AppEvent::SessionEnded`] if dropped without being disarmed.
///
/// Guards `run_exec_session_effect` and `run_port_forward_effect` against task panics:
/// if the spawned task exits abnormally (panic, cancelled future), the guard fires so the
/// UI overlay does not freeze in a permanently un-closeable "Running" state.
struct SessionDoneGuard {
    id: SessionId,
    event_tx: mpsc::Sender<AppEvent>,
    armed: bool,
}

impl SessionDoneGuard {
    fn new(id: SessionId, event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self {
            id,
            event_tx,
            armed: true,
        }
    }

    /// Disarm before normal completion — the caller will emit `SessionEnded` itself.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionDoneGuard {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort: if the event channel is full, the UI is probably shutting down.
            let _ = self.event_tx.try_send(AppEvent::SessionEnded {
                id: self.id,
                exit_code: None,
                error: Some("session relay interrupted unexpectedly".to_owned()),
            });
        }
    }
}

/// Runtime-side handles for an active exec or port-forward session, keyed by [`SessionId`].
///
/// Held by the effect runner for the session's lifetime. [`AppEffect::CloseSession`] cancels the
/// token (which the relay task watches) and drops the stdin sender (EOF to the remote process).
/// [`AppEvent::SessionEnded`] removes the entry, dropping remaining senders.
struct SessionControls {
    /// Fires to signal the relay task to cancel the backend session.
    cancel: CancellationToken,
    /// Stdin relay channel: the event loop forwards `SendSessionInput` bytes here; the relay task
    /// sends them on to the backend's stdin pipe. `None` for port-forward sessions.
    stdin: Option<tokio::sync::mpsc::Sender<bytes::Bytes>>,
    /// TTY resize sink; `None` for v1 non-TTY exec and all port-forward sessions.
    resize: Option<tokio::sync::mpsc::Sender<(u16, u16)>>,
}

/// Translate a terminal event into a reducer message (or `None` to ignore).
///
/// While a text prompt is capturing input, keys are routed to the field as [`Msg::Text`] rather than
/// resolved to actions — except `Ctrl-C`, which always quits so the user is never trapped in a field.
fn map_input(input: Event, keymap: &Keymap, state: &AppState) -> Option<Msg> {
    use cairn_core::TextEdit;
    match input {
        Event::Key(key) if state.capturing_text() => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // Ctrl-C always quits — the user must never be trapped in any capturing field.
            if ctrl && key.code == KeyCode::Char('c') {
                return Some(Msg::Action(Action::Quit));
            }
            // Inside an ExecPane:
            //   Ctrl-] → detach (close overlay, leave remote process running) → Action::Cancel
            //   Ctrl-D → close stdin (EOF signal to the remote process) → TextEdit::CloseStdin
            if matches!(&state.overlay, Some(Overlay::ExecPane { .. })) {
                if ctrl && key.code == KeyCode::Char(']') {
                    return Some(Msg::Action(Action::Cancel));
                }
                if ctrl && key.code == KeyCode::Char('d') {
                    return Some(Msg::Text(TextEdit::CloseStdin));
                }
            }
            text_edit_for(key).map(Msg::Text)
        }
        Event::Key(key) => keymap.action_for(key).map(Msg::Action),
        // A resize triggers a redraw via the no-op tick.
        Event::Resize(_, _) => Some(Msg::Tick),
        _ => None,
    }
}

/// Execute an effect on the tokio runtime; results flow back as [`AppEvent`]s. `transfer_controls`
/// maps each [`TransferId`] to its cancel token + pause sender, so [`AppEffect::CancelTransfer`] and
/// [`AppEffect::SetTransferPaused`] can target the right transfer task.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    effect: AppEffect,
    registry: &VfsRegistry,
    event_tx: &mpsc::Sender<AppEvent>,
    transfer_controls: &mut HashMap<TransferId, TransferControls>,
    ai_cancel: &mut Option<CancellationToken>,
    log_viewer_controls: &mut HashMap<LogViewerId, CancellationToken>,
    session_controls: &mut HashMap<SessionId, SessionControls>,
    shell_action_defs: &Arc<[cairn_config::ShellActionDef]>,
    vault_ctx: &VaultContext,
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
            id,
            src_conn,
            dst_conn,
            items,
            is_move,
            overwrite,
        } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            // Keep this transfer's control pair (cancel token clone + pause sender) keyed by id, so
            // `CancelTransfer`/`SetTransferPaused` can target exactly this transfer; the task gets the
            // cancel token and the pause receiver. Starts unpaused.
            let cancel = CancellationToken::new();
            let (pause_tx, paused) = watch::channel(false);
            transfer_controls.insert(
                id,
                TransferControls {
                    cancel: cancel.clone(),
                    pause: pause_tx,
                },
            );
            tokio::spawn(async move {
                // A drop-guard guarantees a terminal event even if the transfer task panics or is
                // dropped mid-run, so its control entry + active-transfer slot are always reclaimed
                // (otherwise a panicked task at concurrency > 1 would slowly leak slots).
                let mut guard = TransferDoneGuard {
                    id,
                    event_tx: event_tx.clone(),
                    armed: true,
                };
                let ev = run_transfer_effect(
                    &registry, id, src_conn, dst_conn, items, is_move, overwrite, &event_tx,
                    cancel, paused,
                )
                .await;
                guard.armed = false; // completed normally; the real event below reports the outcome
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::CancelTransfer { id } => {
            // Remove + fire. Its Done event also removes the entry (idempotent).
            if let Some(ctrl) = transfer_controls.remove(&id) {
                ctrl.cancel.cancel();
            }
        }
        AppEffect::SetTransferPaused { id, paused } => {
            // Ignore if that transfer isn't running. A send error means its task already finished and
            // dropped the receiver — also safely ignored.
            if let Some(ctrl) = transfer_controls.get(&id) {
                let _ = ctrl.pause.send(paused);
            }
        }
        AppEffect::RunShellAction {
            index,
            conn,
            target,
        } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            let defs = shell_action_defs.clone();
            tokio::spawn(async move {
                let ev = run_shell_action_effect(&registry, &defs, index, conn, target).await;
                let _ = event_tx.send(ev).await;
            });
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
        AppEffect::UnlockVault { passphrase } => {
            let broker = vault_ctx.broker.clone();
            let vault_path = vault_ctx.vault_path.clone();
            let deferred = vault_ctx.deferred.clone();
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let result =
                    run_vault_unlock_effect(&broker, vault_path, &deferred, &registry, passphrase)
                        .await;
                let _ = event_tx.send(AppEvent::VaultUnlocked { result }).await;
            });
        }
        AppEffect::OpenLogViewer {
            id,
            conn,
            path,
            title: _,
        } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            let cancel = CancellationToken::new();
            log_viewer_controls.insert(id, cancel.clone());
            tokio::spawn(async move {
                run_log_viewer_effect(registry, id, conn, path, event_tx, cancel).await;
            });
        }
        AppEffect::CloseLogViewer { id } => {
            if let Some(token) = log_viewer_controls.remove(&id) {
                token.cancel();
            }
        }
        AppEffect::OpenExecSession {
            id,
            conn,
            path,
            argv,
            tty,
            title: _,
        } => {
            let cancel = CancellationToken::new();
            // v1: cooked mode (non-TTY), so resize is not wired.
            let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(64);
            session_controls.insert(
                id,
                SessionControls {
                    cancel: cancel.clone(),
                    stdin: Some(stdin_tx),
                    resize: None,
                },
            );
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                run_exec_session_effect(
                    registry, id, conn, path, argv, tty, stdin_rx, cancel, event_tx,
                )
                .await;
            });
        }
        AppEffect::OpenPortForward {
            id,
            conn,
            path,
            local_port,
            remote_port,
            title: _,
        } => {
            let cancel = CancellationToken::new();
            session_controls.insert(
                id,
                SessionControls {
                    cancel: cancel.clone(),
                    stdin: None,
                    resize: None,
                },
            );
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                run_port_forward_effect(
                    registry,
                    id,
                    conn,
                    path,
                    local_port,
                    remote_port,
                    cancel,
                    event_tx,
                )
                .await;
            });
        }
        AppEffect::CloseSession { id } => {
            // Cancel the relay task and drop the stdin/resize senders (closing those channels).
            if let Some(ctrl) = session_controls.remove(&id) {
                ctrl.cancel.cancel();
                // `ctrl.stdin` and `ctrl.resize` are dropped here, closing the relay channels.
            }
        }
        AppEffect::SendSessionInput { id, bytes } => {
            if let Some(ctrl) = session_controls.get(&id) {
                if let Some(stdin) = &ctrl.stdin {
                    if stdin.try_send(bytes::Bytes::from(bytes)).is_err() {
                        // Channel full or relay task exited. Log so the operator can diagnose;
                        // we cannot block the event loop here with send().await.
                        tracing::warn!(
                            session = %id,
                            "SendSessionInput dropped — stdin relay channel full or closed"
                        );
                    }
                }
            }
        }
        AppEffect::CloseStdin { id } => {
            // Drop only the stdin sender; the cancel token stays live so the relay task keeps
            // draining stdout until the process exits.
            if let Some(ctrl) = session_controls.get_mut(&id) {
                ctrl.stdin = None;
            }
        }
        AppEffect::ResizeSession { id, rows, cols } => {
            if let Some(ctrl) = session_controls.get(&id) {
                if let Some(resize) = &ctrl.resize {
                    let _ = resize.try_send((rows, cols));
                }
            }
        }
        // `AppEffect` is non-exhaustive; future variants are wired up in later milestones.
        other => tracing::warn!(effect = ?other, "unhandled effect"),
    }
}

/// Stream logs for a container/pod entry and forward each decoded chunk as an [`AppEvent::LogChunk`].
///
/// Invokes the backend's `"logs"` action ([`cairn_vfs::ActionCtx::Logs`] in follow mode), reads the
/// resulting [`cairn_vfs::ActionOutcome::Stream`], and decodes each chunk lossily before forwarding
/// it. The loop runs until the stream ends (clean EOF → `LogStreamEnded { error: None }`), errors (a
/// redacted message), or the [`CancellationToken`] fires (Esc closed the overlay — no terminal event
/// needed). A backend without log streaming returns `VfsError::Unsupported`, surfaced as an error.
async fn run_log_viewer_effect(
    registry: VfsRegistry,
    id: LogViewerId,
    conn: cairn_types::ConnectionId,
    path: cairn_types::VfsPath,
    event_tx: mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
) {
    use cairn_vfs::{ActionCtx, ActionId, ActionOutcome};

    let Some(vfs) = registry.get(conn).await else {
        let _ = event_tx
            .send(AppEvent::LogStreamEnded {
                id,
                error: Some("connection unavailable".to_owned()),
            })
            .await;
        return;
    };
    let outcome = vfs
        .invoke(
            &path,
            ActionId::new("logs"),
            ActionCtx::Logs {
                follow: true,
                since: None,
                container: None,
            },
        )
        .await;
    let stream = match outcome {
        Ok(ActionOutcome::Stream(s)) => s,
        Ok(_) => {
            let _ = event_tx
                .send(AppEvent::LogStreamEnded {
                    id,
                    error: Some("logs action returned unexpected outcome".to_owned()),
                })
                .await;
            return;
        }
        Err(e) => {
            let _ = event_tx
                .send(AppEvent::LogStreamEnded {
                    id,
                    error: Some(e.redacted().to_string()),
                })
                .await;
            return;
        }
    };
    futures::pin_mut!(stream);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            chunk = stream.next() => match chunk {
                Some(Ok(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = event_tx.send(AppEvent::LogChunk { id, text }).await;
                }
                Some(Err(e)) => {
                    let _ = event_tx
                        .send(AppEvent::LogStreamEnded {
                            id,
                            error: Some(e.redacted().to_string()),
                        })
                        .await;
                    return;
                }
                None => {
                    let _ = event_tx
                        .send(AppEvent::LogStreamEnded { id, error: None })
                        .await;
                    return;
                }
            },
        }
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

/// Unlock the secrets vault with `passphrase`, install it into the shared broker, and retry the
/// connections deferred at startup while the vault was locked (M3-7).
///
/// Returns `Ok(opened)` with the now-reachable connections (possibly empty) to add to the switcher,
/// or `Err(message)` with a secret-free, retryable reason (missing vault / wrong passphrase). The
/// passphrase is consumed here and zeroized on drop; it is never logged.
///
/// `Vault::open` runs Argon2id key derivation (CPU-bound) plus a file read, so it is offloaded to a
/// blocking thread to keep the async runtime — and the render path — responsive.
async fn run_vault_unlock_effect(
    broker: &Arc<Broker>,
    vault_path: Option<PathBuf>,
    deferred: &[DeferredConnection],
    registry: &VfsRegistry,
    passphrase: cairn_secrets::SecretString,
) -> Result<Vec<ConnectionChoice>, String> {
    let Some(path) = vault_path else {
        return Err("no vault path configured".to_owned());
    };
    // Open + decrypt off the async runtime: `Vault::open` runs Argon2id (CPU-bound) plus a file read,
    // and the existence check is itself a blocking `stat`, so *all* filesystem I/O is isolated here.
    // The owned `SecretString` lives in the closure and is zeroized when it returns. Vault CREATION
    // from the TUI is a follow-up — for now an absent file is a clear message, not a prompt.
    let vault = match tokio::task::spawn_blocking(move || {
        if !path.exists() {
            return Err(
                "no vault found (creating a vault from the UI is not yet available)".to_owned(),
            );
        }
        // `VaultError`'s Display is secret-free by construction (see its docs), so it is safe to show.
        Vault::open(&path, &passphrase).map_err(|e| e.to_string())
    })
    .await
    {
        Ok(Ok(vault)) => vault,
        Ok(Err(msg)) => return Err(msg),
        Err(_) => return Err("unlock task failed".to_owned()),
    };
    broker.unlock(vault);

    // Retry the deferred profiles now that the broker is unlocked; the opener shares this broker.
    let opener = crate::connect::ConnectionOpener::new(broker.clone());
    let mut opened = Vec::new();
    for d in deferred {
        match opener.open(Actor::User, d.id, &d.profile).await {
            Ok(vfs) => {
                registry.insert(d.id, vfs).await;
                opened.push(ConnectionChoice {
                    conn: d.id,
                    label: format!("{}: {}", d.profile.scheme, d.profile.display_name),
                    provenance: ChoiceProvenance::Saved,
                    status: ChoiceStatus::Ready,
                    kind: ConnectionKind::Profile { id: d.profile.id },
                });
            }
            Err(e) => {
                // The vault is unlocked but this one still won't open (e.g. bad field); log and skip.
                tracing::warn!(
                    scheme = %d.profile.scheme,
                    name = %d.profile.display_name,
                    error = %e,
                    "deferred connection still failed after unlock"
                );
            }
        }
    }
    Ok(opened)
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

/// Subtract accumulated paused wall-time from the total elapsed to get the effective (active)
/// elapsed for throughput rate calculation. Saturates to [`Duration::ZERO`] when `paused`
/// exceeds `total` (e.g., the accumulator raced ahead of the clock); [`avg_rate`]'s own 0.05 s
/// floor then bounds the reported rate.
fn effective_elapsed(total: Duration, paused: Duration) -> Duration {
    total.saturating_sub(paused)
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
    id: TransferId,
    src_conn: ConnectionId,
    dst_conn: ConnectionId,
    items: Vec<(VfsPath, VfsPath)>,
    is_move: bool,
    overwrite: bool,
    event_tx: &mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
    // Drives the engine's pause once the byte copy starts. NOTE: the read-only pre-flight phases
    // below (conflict pre-check + size pre-scan) honor only `cancel`, not `paused`, so pressing `p`
    // during a long pre-scan shows "paused" while the scan finishes; the pause takes hold at
    // `run_transfer`. Harmless (read-only) and `Esc` still aborts both phases.
    paused: watch::Receiver<bool>,
) -> AppEvent {
    let (Some(src), Some(dst)) = (registry.get(src_conn).await, registry.get(dst_conn).await)
    else {
        return AppEvent::TransferDone {
            id,
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
                    id,
                    status: "Transfer cancelled".to_owned(),
                    error: false,
                };
            }
            match dst.stat(to).await {
                Ok(_) => conflicts += 1,
                Err(VfsError::NotFound(_)) => {}
                Err(e) => {
                    return AppEvent::TransferDone {
                        id,
                        status: format!("Transfer aborted: {}", e.redacted()),
                        error: true,
                    };
                }
            }
        }
        if conflicts > 0 {
            return AppEvent::TransferConflict {
                id,
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
    // reported rate is the average throughput over **effective** (non-paused) elapsed time.

    // Track wall-time spent paused so the throughput rate is not skewed by idle clock cycles.
    // A lightweight accumulator task subscribes to the pause watch: each true→false transition adds
    // the pause interval (in milliseconds) to a shared `AtomicU64`. The drop-guard cancels the task
    // when `run_transfer_effect` returns via any path (normal, early-exit, or cancelled).
    let paused_ms_acc = Arc::new(AtomicU64::new(0));
    let _accu_guard = {
        let token = CancellationToken::new();
        let guard = token.clone().drop_guard();
        tokio::spawn({
            let paused_ms = paused_ms_acc.clone();
            let cancel = token;
            let mut paused_rx = paused.clone();
            async move {
                let mut pause_start: Option<std::time::Instant> = None;
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        result = paused_rx.changed() => {
                            match result {
                                Ok(()) => {
                                    if *paused_rx.borrow() {
                                        pause_start = Some(std::time::Instant::now());
                                    } else if let Some(s) = pause_start.take() {
                                        // Clamp to u64::MAX (≈ 584 million years) to avoid a
                                        // near-impossible overflow; in practice pauses are seconds.
                                        let ms = u64::try_from(s.elapsed().as_millis())
                                            .unwrap_or(u64::MAX);
                                        paused_ms.fetch_add(ms, Ordering::Relaxed);
                                    }
                                }
                                Err(_) => break, // watch sender dropped; transfer has ended
                            }
                        }
                    }
                }
            }
        });
        guard
    };
    let started = std::time::Instant::now();
    let rate_bps = |bytes: u64| -> u64 {
        let paused_dur = Duration::from_millis(paused_ms_acc.load(Ordering::Relaxed));
        avg_rate(
            bytes,
            effective_elapsed(started.elapsed(), paused_dur).as_secs_f64(),
        )
    };
    let mut bytes = 0u64;
    let mut last_sent = 0u64;
    let mut on_progress = |b: u64| {
        bytes += b;
        debug_assert!(bytes >= last_sent, "progress bytes must be cumulative");
        if bytes - last_sent >= TRANSFER_PROGRESS_STEP {
            last_sent = bytes;
            let _ = event_tx.try_send(AppEvent::TransferProgress {
                id,
                bytes,
                rate_bps: rate_bps(bytes),
                total,
            });
        }
    };
    match cairn_transfer::run_transfer(&src, &dst, &items, spec, &cancel, &paused, &mut on_progress)
        .await
    {
        Ok(out) => {
            // Flush the exact final total for one frame before `TransferDone` clears the indicator
            // (so a transfer smaller than the coalescing step doesn't only ever show "0 B").
            let _ = event_tx.try_send(AppEvent::TransferProgress {
                id,
                bytes: out.bytes,
                rate_bps: rate_bps(out.bytes),
                total,
            });
            AppEvent::TransferDone {
                id,
                status: format!("{verb} {}", outcome_summary(&out)),
                error: false,
            }
        }
        Err(cairn_transfer::TransferError::Cancelled(done)) => AppEvent::TransferDone {
            id,
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
            id,
            status: format!("{} failed: {}", verb.trim_end_matches('d'), e.redacted()),
            error: true,
        },
    }
}

/// Wall-clock limit for a shell action before it is killed.
const SHELL_ACTION_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-stream output cap captured from a shell action (the rest is dropped, marked truncated).
const SHELL_ACTION_OUTPUT_CAP: usize = 64 * 1024;

/// Run a user-defined shell action (M8-7) against `target`, returning a redacted [`AppEvent`].
///
/// SECURITY (see `docs/adr/0005-shell-command-actions.md`): the program is run with **no shell**
/// (argv vector, never `sh -c`), only against a **local** backend (via [`Vfs::local_path`], which
/// canonicalizes and confines the path), with a **scrubbed environment** (no secrets/vault material),
/// an explicit cwd, stdin closed, output captured (not inherited) and capped, and a wall-clock timeout
/// after which the whole process group is killed.
async fn run_shell_action_effect(
    registry: &VfsRegistry,
    defs: &[cairn_config::ShellActionDef],
    index: usize,
    conn: ConnectionId,
    target: VfsPath,
) -> AppEvent {
    let Some(def) = defs.get(index) else {
        // Index/keymap drift — a wiring bug, surfaced rather than panicking.
        return shell_done("shell action unavailable", true);
    };
    let name = def.name.clone();
    // Defensive re-validation (already validated at startup; cheap and keeps the boundary local).
    if let Err(reason) = def.validate() {
        return shell_done(&format!("'{name}': {reason}"), true);
    }
    let Some(vfs) = registry.get(conn).await else {
        return shell_done(&format!("'{name}': connection unavailable"), true);
    };
    // Resolve the real, confined OS path off the async runtime (local_path does blocking canonicalize).
    // `None` ⇒ a non-local backend or a path escaping the root ⇒ refuse.
    let real = {
        let (vfs, target) = (vfs.clone(), target.clone());
        match tokio::task::spawn_blocking(move || vfs.local_path(&target)).await {
            Ok(Some(p)) => p,
            _ => return shell_done(&format!("'{name}': requires a local file"), true),
        }
    };
    let dir = real
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| real.clone());
    let file_name = real
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let path_str = real.to_string_lossy();
    let dir_str = dir.to_string_lossy();
    // Expand each arg into exactly one argv element — placeholders only, never re-split or shell-parsed.
    let argv: Vec<String> = def
        .args
        .iter()
        .map(|a| expand_placeholders(a, &path_str, &dir_str, &file_name))
        .collect();

    match spawn_shell_action(&def.command, &argv, &dir, &name).await {
        Ok(summary) => shell_done(&format!("'{name}': {summary}"), false),
        Err(reason) => shell_done(&format!("'{name}': {reason}"), true),
    }
}

/// Drain an optional child stream to EOF, returning the total byte count. Output is **discarded**
/// (read into a small reusable buffer, never retained) — it may contain secrets and is never surfaced
/// beyond a summary. We drain past [`SHELL_ACTION_OUTPUT_CAP`] rather than stopping at it so a
/// well-behaved verbose program isn't killed by `EPIPE`; memory stays O(chunk) regardless of size,
/// and total runtime is bounded by the caller's timeout. The returned count lets the caller report
/// truncation (`>= cap`).
async fn drain_capped<R>(stream: Option<R>) -> usize
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut s) = stream else { return 0 };
    let mut total = 0usize;
    let mut buf = [0u8; 8192];
    loop {
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => total = total.saturating_add(n),
        }
    }
    total
}

/// Substitute `{path}`/`{dir}`/`{name}` in `arg` in a **single left-to-right pass**, so a value
/// inserted for one placeholder is never rescanned for another (a filename literally containing
/// `{name}` cannot corrupt an already-expanded `{path}`). Unknown `{...}` tokens are left verbatim
/// (config validation already rejected them at startup).
fn expand_placeholders(arg: &str, path: &str, dir: &str, name: &str) -> String {
    let mut out = String::with_capacity(arg.len());
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        if let Some(close) = after.find('}') {
            match &after[1..close] {
                "path" => out.push_str(path),
                "dir" => out.push_str(dir),
                "name" => out.push_str(name),
                _ => out.push_str(&after[..=close]), // unknown token: emit verbatim
            }
            rest = &after[close + 1..];
        } else {
            // No closing brace: emit the rest verbatim and stop.
            out.push_str(after);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Build an [`AppEvent::ShellActionDone`]. `status` is already secret-free (counts/exit codes only).
fn shell_done(status: &str, error: bool) -> AppEvent {
    AppEvent::ShellActionDone {
        status: status.to_owned(),
        error,
    }
}

/// Spawn the hardened child and return a short status (`"exit 0"`, `"exit 2"`, `"timed out"`). On
/// success/failure the captured output is summarized to a byte count — never echoed (it may contain
/// secrets), and never forwarded to the AI layer.
async fn spawn_shell_action(
    program: &str,
    argv: &[String],
    cwd: &std::path::Path,
    action_name: &str,
) -> Result<String, String> {
    let mut std_cmd = std::process::Command::new(program);
    std_cmd
        .args(argv)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Scrub the environment: start empty, then re-add a minimal, non-secret allow-list with a
    // sanitized PATH (no `.`/empty entries). This keeps tokens like AWS_*/GITHUB_TOKEN out of the
    // child — secrets must never reach a shell action.
    std_cmd.env_clear();
    if let Some(path) = sanitized_path() {
        std_cmd.env("PATH", path);
    }
    for key in ["HOME", "USER", "LOGNAME", "LANG", "TZ", "TMPDIR"] {
        if let Some(v) = std::env::var_os(key) {
            std_cmd.env(key, v);
        }
    }
    for (k, v) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("LC_") {
            std_cmd.env(k, v);
        }
    }
    // Let the program know it was launched as a Cairn action (non-secret, analogous to `$EDITOR`).
    std_cmd.env("CAIRN_ACTION", action_name);
    // Unix: put the child in its own process group so a timeout can kill it *and* its descendants.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        std_cmd.process_group(0);
    }

    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("could not start: {e}"))?;
    let pid = child.id();

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Drain both pipes *concurrently* with the wait. Reading stdout fully and only then stderr would
    // deadlock: a child that fills the (~64 KiB) stderr pipe blocks on write while we block reading
    // stdout. Each stream is independently capped; we track whether either hit the cap.
    let capture = async {
        let (out_len, err_len, status) =
            tokio::join!(drain_capped(stdout), drain_capped(stderr), child.wait());
        let truncated = out_len >= SHELL_ACTION_OUTPUT_CAP || err_len >= SHELL_ACTION_OUTPUT_CAP;
        (status, truncated)
    };

    match tokio::time::timeout(SHELL_ACTION_TIMEOUT, capture).await {
        Ok((Ok(status), truncated)) => {
            let out = if truncated { " (output truncated)" } else { "" };
            match status.code() {
                Some(0) => Ok(format!("done{out}")),
                Some(c) => Err(format!("exit {c}{out}")),
                None => Err(format!("killed by signal{out}")),
            }
        }
        Ok((Err(e), _)) => Err(format!("wait failed: {e}")),
        Err(_) => {
            // Timed out: kill the whole process group (Unix) or just the child (otherwise), then reap.
            kill_process_tree(pid);
            Err("timed out".to_owned())
        }
    }
}

/// Best-effort kill of a timed-out child and its descendants. On Unix the child leads its own process
/// group (`process_group(0)`), so signalling the group reaps grandchildren too; elsewhere only the
/// child is killed (the `kill_on_drop` guard still applies when the handle drops).
#[cfg(unix)]
fn kill_process_tree(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        if let Some(pid) = rustix::process::Pid::from_raw(pid) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_tree(_pid: Option<u32>) {
    // The `kill_on_drop(true)` guard kills the child when its handle is dropped.
}

/// The current `PATH` with `.` and empty entries removed, so a shell action can't resolve a bare
/// program name against the directory being browsed. `None` if `PATH` is unset.
fn sanitized_path() -> Option<std::ffi::OsString> {
    let path = std::env::var_os("PATH")?;
    let kept: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|p| !p.as_os_str().is_empty() && p != std::path::Path::new("."))
        .collect();
    std::env::join_paths(kept).ok()
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

/// Decode as much valid UTF-8 as possible from `carry ++ bytes`, returning the decoded
/// text. Any trailing incomplete codepoint bytes (1–3) are left in `carry` for the next call.
/// This prevents the `U+FFFD` mojibake that `from_utf8_lossy` produces when a multi-byte
/// character straddles a chunk boundary.
fn decode_utf8_chunk(carry: &mut Vec<u8>, bytes: &[u8]) -> String {
    carry.extend_from_slice(bytes);
    match std::str::from_utf8(carry) {
        Ok(s) => {
            let text = s.to_owned();
            carry.clear();
            text
        }
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            let text = String::from_utf8_lossy(&carry[..valid_up_to]).into_owned();
            carry.drain(..valid_up_to);
            // carry now holds only the 1–3 trailing incomplete bytes.
            text
        }
    }
}

/// Run an interactive cooked-mode exec session.
///
/// Calls `Vfs::invoke(path, "exec", ActionCtx::Exec { argv, tty })`, receives the
/// `ActionOutcome::Session(SessionHandle)`, and runs three relay loops concurrently:
///
/// 1. **stdout relay** — reads `handle.stdout` chunks and emits [`AppEvent::SessionOutput`].
/// 2. **stdin relay** — forwards bytes from `stdin_rx` (fed by `SendSessionInput` effects) to
///    `handle.stdin`. Ends when the sender is dropped (i.e., after [`AppEffect::CloseSession`]).
/// 3. **done sentinel** — awaits `handle.done` and emits [`AppEvent::SessionEnded`].
///
/// Any of the three terminating, or the `cancel` token firing, shuts down all of them cleanly.
#[allow(clippy::too_many_arguments)]
async fn run_exec_session_effect(
    registry: VfsRegistry,
    id: SessionId,
    conn: cairn_types::ConnectionId,
    path: cairn_types::VfsPath,
    argv: Vec<String>,
    tty: bool,
    mut stdin_rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    cancel: CancellationToken,
    event_tx: mpsc::Sender<AppEvent>,
) {
    use cairn_vfs::{ActionCtx, ActionId, ActionOutcome};

    // RAII guard: if this task panics or is dropped before it reaches the normal completion
    // path, the guard's Drop impl emits a synthetic SessionEnded so the reducer and UI are
    // never left waiting for an event that will never arrive.
    let mut guard = SessionDoneGuard::new(id, event_tx.clone());

    let Some(vfs) = registry.get(conn).await else {
        let _ = event_tx
            .send(AppEvent::SessionEnded {
                id,
                exit_code: None,
                error: Some("connection unavailable".to_owned()),
            })
            .await;
        return;
    };
    let outcome = vfs
        .invoke(&path, ActionId::new("exec"), ActionCtx::Exec { argv, tty })
        .await;
    let handle = match outcome {
        Ok(ActionOutcome::Session(h)) => h,
        Ok(_) => {
            let _ = event_tx
                .send(AppEvent::SessionEnded {
                    id,
                    exit_code: None,
                    error: Some("exec action returned unexpected outcome".to_owned()),
                })
                .await;
            return;
        }
        Err(e) => {
            let _ = event_tx
                .send(AppEvent::SessionEnded {
                    id,
                    exit_code: None,
                    error: Some(e.redacted().to_string()),
                })
                .await;
            return;
        }
    };

    // Destructure the handle; the backend's cancel sender fires when dropped, so we must not drop
    // it until we are ready to tear down.
    let cairn_vfs::SessionHandle {
        cancel: backend_cancel,
        done,
        stdout,
        stdin: backend_stdin,
        ..
    } = handle;

    // Stdin relay: forward bytes from the event loop to the backend's stdin channel.
    // Spawned as an independent task so it can run concurrently with stdout draining.
    if let Some(backend_stdin_tx) = backend_stdin {
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel_clone.cancelled() => break,
                    maybe = stdin_rx.recv() => match maybe {
                        Some(bytes) => {
                            // Best-effort: if the backend dropped its receiver, stop relaying.
                            if backend_stdin_tx.send(bytes).await.is_err() {
                                break;
                            }
                        }
                        None => break, // sender dropped (CloseSession or process exit)
                    },
                }
            }
        });
    }

    // Stdout + done relay on the current task.
    let mut stdout = stdout;
    let mut done = done;
    let mut exit_code_result: Option<Result<i32, cairn_vfs::VfsError>> = None;
    // Carry buffer for cross-chunk UTF-8 stitching: avoids U+FFFD replacement characters when a
    // multibyte codepoint is split across two consecutive stdout chunks.
    let mut utf8_carry: Vec<u8> = Vec::new();

    loop {
        // Poll stdout chunks (if present) and the done receiver concurrently, with cancel.
        match &mut stdout {
            Some(rx) => {
                tokio::select! {
                    () = cancel.cancelled() => {
                        // Fire the backend cancel signal; backend cleans up.
                        let _ = backend_cancel.send(());
                        break;
                    }
                    maybe_bytes = rx.recv() => match maybe_bytes {
                        Some(bytes) => {
                            let text = decode_utf8_chunk(&mut utf8_carry, &bytes);
                            if !text.is_empty() {
                                let _ = event_tx
                                    .send(AppEvent::SessionOutput { id, text })
                                    .await;
                            }
                        }
                        None => {
                            // stdout channel closed; drain done.
                            break;
                        }
                    },
                    result = &mut done => {
                        exit_code_result = Some(result.unwrap_or(Err(cairn_vfs::VfsError::Backend {
                            code: "done-channel-closed".to_owned(),
                            msg: "session done channel closed unexpectedly".to_owned(),
                            retryable: false,
                        })));
                        break;
                    }
                }
            }
            None => {
                // No stdout (unusual for exec, but handle it): just wait for done or cancel.
                tokio::select! {
                    () = cancel.cancelled() => {
                        let _ = backend_cancel.send(());
                        break;
                    }
                    result = &mut done => {
                        exit_code_result = Some(result.unwrap_or(Err(cairn_vfs::VfsError::Backend {
                            code: "done-channel-closed".to_owned(),
                            msg: "session done channel closed unexpectedly".to_owned(),
                            retryable: false,
                        })));
                        break;
                    }
                }
            }
        }
    }

    // Drain any remaining stdout that arrived before or concurrently with the done signal.
    // Uses an async recv loop (not try_recv) so in-flight chunks are not dropped; bounded by a
    // 5-second timeout so a stuck stdout producer cannot block the task indefinitely.
    if let Some(rx) = &mut stdout {
        let drain = async {
            while let Some(bytes) = rx.recv().await {
                let text = decode_utf8_chunk(&mut utf8_carry, &bytes);
                if !text.is_empty() {
                    let _ = event_tx.send(AppEvent::SessionOutput { id, text }).await;
                }
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), drain).await;
    }
    // Flush any incomplete multibyte sequence left in the carry buffer (e.g. when the
    // stdout channel was closed mid-codepoint).
    if !utf8_carry.is_empty() {
        let text = String::from_utf8_lossy(&utf8_carry).into_owned();
        let _ = event_tx.send(AppEvent::SessionOutput { id, text }).await;
        utf8_carry.clear();
    }

    // Wait for the done receiver if we broke out of the stdout drain before it resolved.
    let (exit_code, error) = match exit_code_result {
        Some(Ok(code)) => (Some(code), None),
        Some(Err(e)) => (None, Some(e.redacted().to_string())),
        None => match done.await {
            Ok(Ok(code)) => (Some(code), None),
            Ok(Err(e)) => (None, Some(e.redacted().to_string())),
            Err(_) => (None, None),
        },
    };
    guard.disarm();
    let _ = event_tx
        .send(AppEvent::SessionEnded {
            id,
            exit_code,
            error,
        })
        .await;
}

/// Run a port-forward session.
///
/// Calls `Vfs::invoke(path, "port-forward", ActionCtx::PortForward { local, remote })`, receives
/// the `ActionOutcome::Session(SessionHandle)`, emits [`AppEvent::PortForwardBound`] (using
/// `handle.local_port`), and then waits for the session to end (via [`AppEvent::SessionEnded`]).
#[allow(clippy::too_many_arguments)]
async fn run_port_forward_effect(
    registry: VfsRegistry,
    id: SessionId,
    conn: cairn_types::ConnectionId,
    path: cairn_types::VfsPath,
    local_port: u16,
    remote_port: u16,
    cancel: CancellationToken,
    event_tx: mpsc::Sender<AppEvent>,
) {
    use cairn_vfs::{ActionCtx, ActionId, ActionOutcome};

    // RAII guard: emits a synthetic SessionEnded if this task panics or is dropped early.
    let mut guard = SessionDoneGuard::new(id, event_tx.clone());

    let Some(vfs) = registry.get(conn).await else {
        let _ = event_tx
            .send(AppEvent::SessionEnded {
                id,
                exit_code: None,
                error: Some("connection unavailable".to_owned()),
            })
            .await;
        return;
    };
    let outcome = vfs
        .invoke(
            &path,
            ActionId::new("port-forward"),
            ActionCtx::PortForward {
                local: local_port,
                remote: remote_port,
            },
        )
        .await;
    let handle = match outcome {
        Ok(ActionOutcome::Session(h)) => h,
        Ok(_) => {
            let _ = event_tx
                .send(AppEvent::SessionEnded {
                    id,
                    exit_code: None,
                    error: Some("port-forward action returned unexpected outcome".to_owned()),
                })
                .await;
            return;
        }
        Err(e) => {
            let _ = event_tx
                .send(AppEvent::SessionEnded {
                    id,
                    exit_code: None,
                    error: Some(e.redacted().to_string()),
                })
                .await;
            return;
        }
    };

    let cairn_vfs::SessionHandle {
        cancel: backend_cancel,
        mut done,
        local_port: bound_port,
        ..
    } = handle;

    // Emit PortForwardBound so the UI can display the actual local port (especially when 0 was
    // requested and the OS assigned an ephemeral one).
    let actual_port = bound_port.unwrap_or(local_port);
    let _ = event_tx
        .send(AppEvent::PortForwardBound {
            id,
            local_port: actual_port,
        })
        .await;

    // Wait for the session to end or the cancel token to fire.
    let (exit_code, error) = tokio::select! {
        () = cancel.cancelled() => {
            let _ = backend_cancel.send(());
            (Some(0), None)
        }
        result = &mut done => match result {
            Ok(Ok(code)) => (Some(code), None),
            Ok(Err(e)) => (None, Some(e.redacted().to_string())),
            Err(_) => (None, None),
        },
    };
    guard.disarm();
    let _ = event_tx
        .send(AppEvent::SessionEnded {
            id,
            exit_code,
            error,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::ConnectionProfile;
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
            1, // transfer id
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
            watch::channel(false).1, // never paused
        )
        .await;
        match ev {
            AppEvent::TransferDone { status, error, .. } => {
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
            1, // transfer id
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
            watch::channel(false).1, // never paused
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

    /// Pure unit test for [`effective_elapsed`] — no async, no sleeps.
    /// Verifies that accumulated paused time is correctly subtracted, with saturation to zero when
    /// paused >= total (the `avg_rate` floor then bounds the reported rate).
    #[test]
    fn effective_elapsed_saturates_at_zero_and_subtracts_correctly() {
        // Normal: 5 s elapsed, 2 s paused → 3 s effective.
        assert_eq!(
            effective_elapsed(Duration::from_secs(5), Duration::from_secs(2)),
            Duration::from_secs(3)
        );
        // Saturates to zero when paused exceeds total (avg_rate's floor then kicks in).
        assert_eq!(
            effective_elapsed(Duration::from_secs(1), Duration::from_secs(2)),
            Duration::ZERO
        );
        // Exact match: paused == total → zero.
        assert_eq!(
            effective_elapsed(Duration::from_secs(3), Duration::from_secs(3)),
            Duration::ZERO
        );
        // No pause → full elapsed is effective.
        assert_eq!(
            effective_elapsed(Duration::from_secs(4), Duration::ZERO),
            Duration::from_secs(4)
        );
        // Both zero.
        assert_eq!(
            effective_elapsed(Duration::ZERO, Duration::ZERO),
            Duration::ZERO
        );
    }

    /// Verifies that Ctrl-] and Ctrl-D are routed correctly when an exec-pane overlay is active:
    /// Ctrl-] → `Action::Cancel` (detach/close overlay), Ctrl-D → `TextEdit::CloseStdin` (EOF).
    #[test]
    fn map_input_routes_ctrl_bracket_and_ctrl_d_when_exec_pane_active() {
        use cairn_core::{Overlay, TextEdit};
        let km = Keymap::default();
        let mut st = AppState::new(LEFT, RIGHT, VfsPath::root());
        st.overlay = Some(Overlay::ExecPane {
            id: SessionId(1),
            input: String::new(),
            scroll: 0,
            follow: true,
        });

        // Ctrl-] → detach (Action::Cancel), not Quit
        let ctrl_bracket = Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        assert!(
            matches!(
                map_input(ctrl_bracket, &km, &st),
                Some(Msg::Action(Action::Cancel))
            ),
            "Ctrl-] must cancel/detach the exec pane"
        );

        // Ctrl-D → CloseStdin (EOF to remote process), not Quit
        let ctrl_d = Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(
            matches!(
                map_input(ctrl_d, &km, &st),
                Some(Msg::Text(TextEdit::CloseStdin))
            ),
            "Ctrl-D must send CloseStdin when exec pane is active"
        );
    }

    fn tempfile_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn shell_action(command: &str, args: &[&str]) -> cairn_config::ShellActionDef {
        cairn_config::ShellActionDef {
            name: "Test".to_owned(),
            key: "f9".to_owned(),
            command: command.to_owned(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            confirm: false,
        }
    }

    #[tokio::test]
    async fn shell_action_unknown_index_is_an_error() {
        let registry = VfsRegistry::new();
        let ev = run_shell_action_effect(&registry, &[], 0, LEFT, VfsPath::root()).await;
        assert!(matches!(ev, AppEvent::ShellActionDone { error: true, .. }));
    }

    #[tokio::test]
    async fn transfer_done_guard_emits_synthetic_done_when_dropped_armed() {
        // Dropping an armed guard (the task panicked / was dropped before completing) must emit a
        // terminal TransferDone so the reducer + control map release the slot.
        let (tx, mut rx) = mpsc::channel(8);
        let guard = TransferDoneGuard {
            id: 7,
            event_tx: tx,
            armed: true,
        };
        drop(guard);
        let ev = rx.try_recv().ok();
        assert!(
            matches!(
                ev,
                Some(AppEvent::TransferDone {
                    id: 7,
                    error: true,
                    ..
                })
            ),
            "expected a synthetic TransferDone{{id:7,error:true}}"
        );
    }

    #[tokio::test]
    async fn transfer_done_guard_is_silent_when_disarmed() {
        // Normal completion disarms the guard before sending the real event → no synthetic duplicate.
        let (tx, mut rx) = mpsc::channel(8);
        let guard = TransferDoneGuard {
            id: 7,
            event_tx: tx,
            armed: false,
        };
        drop(guard);
        assert!(rx.try_recv().is_err(), "disarmed guard must emit nothing");
    }

    #[tokio::test]
    async fn shell_action_refuses_a_non_local_or_missing_target() {
        // A LocalVfs whose target does not exist → `local_path` returns None → refusal (same branch a
        // non-local backend hits). Proves the local-only gate without spawning anything.
        let dir = tempfile_dir();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        let defs = [shell_action("true", &[])];
        let ev = run_shell_action_effect(
            &registry,
            &defs,
            0,
            LEFT,
            VfsPath::parse("/nope.txt").unwrap(),
        )
        .await;
        match ev {
            AppEvent::ShellActionDone { status, error } => {
                assert!(error);
                assert!(status.contains("requires a local file"), "got: {status}");
            }
            _ => panic!("expected ShellActionDone"),
        }
    }

    #[cfg(unix)]
    async fn run_one(
        dir: &std::path::Path,
        def: cairn_config::ShellActionDef,
        target: &str,
    ) -> AppEvent {
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir)))
            .await;
        run_shell_action_effect(&registry, &[def], 0, LEFT, VfsPath::parse(target).unwrap()).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_action_true_succeeds() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let ev = run_one(dir.path(), shell_action("true", &["{path}"]), "/f.txt").await;
        assert!(matches!(ev, AppEvent::ShellActionDone { error: false, .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_action_false_reports_nonzero_exit() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let ev = run_one(dir.path(), shell_action("false", &[]), "/f.txt").await;
        match ev {
            AppEvent::ShellActionDone { status, error } => {
                assert!(error);
                assert!(status.contains("exit 1"), "got: {status}");
            }
            _ => panic!("expected ShellActionDone"),
        }
    }

    #[test]
    fn expand_placeholders_is_single_pass() {
        // A path that itself contains the literal `{name}` must NOT be re-expanded by the later
        // `{name}` substitution — argv must receive exactly the resolved path.
        let path = "/home/u/{name}/report.txt";
        assert_eq!(
            expand_placeholders("{path}", path, "/home/u/{name}", "report.txt"),
            path
        );
        // Embedded placeholders in surrounding text still work; unknown tokens pass through verbatim.
        assert_eq!(expand_placeholders("{name}.tgz", "/p", "/d", "f"), "f.tgz");
        assert_eq!(
            expand_placeholders("{unknown}", "/p", "/d", "f"),
            "{unknown}"
        );
        assert_eq!(
            expand_placeholders("a{path}b{dir}", "/p", "/d", "f"),
            "a/pb/d"
        );
        // Unbalanced brace is emitted verbatim (config validation rejects it upstream anyway).
        assert_eq!(expand_placeholders("{path", "/p", "/d", "f"), "{path");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_action_with_large_stderr_does_not_deadlock() {
        // A child that writes far more than the pipe buffer to stderr while stdout stays open must
        // not deadlock the sequential-then-concurrent capture; it should complete well under the
        // 10s timeout. `head -c` bounds the write so the test is fast and hermetic.
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        // /bin/sh is fine *as the program here* (we're testing our capture, not shell-injection):
        // it writes 200 KiB to stderr (no pipeline, so no SIGPIPE) and exits 0. Our runner still
        // execs argv with no shell of its own.
        let def = shell_action("/bin/sh", &["-c", "head -c 200000 /dev/zero 1>&2"]);
        let ev = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_one(dir.path(), def, "/f.txt"),
        )
        .await
        .expect("must finish well under the action timeout");
        // Exit 0; output exceeded the cap so it's marked truncated.
        match ev {
            AppEvent::ShellActionDone { status, error } => {
                assert!(!error, "got: {status}");
                assert!(status.contains("truncated"), "got: {status}");
            }
            _ => panic!("expected ShellActionDone"),
        }
    }

    // ---- M3-7: vault-unlock effect ---------------------------------------------------------------

    #[tokio::test]
    async fn vault_unlock_effect_opens_a_vault_and_unlocks_the_broker() {
        use cairn_secrets::SecretString;
        use cairn_vault::{KdfParams, Vault};
        let dir = tempfile_dir();
        let path = dir.path().join("vault.cvlt");
        Vault::create_with_params(
            &path,
            &SecretString::from("open-sesame".to_owned()),
            KdfParams::fast_for_tests(),
        )
        .unwrap();
        let broker = Arc::new(Broker::locked());
        let registry = VfsRegistry::new();
        let res = run_vault_unlock_effect(
            &broker,
            Some(path),
            &[],
            &registry,
            SecretString::from("open-sesame".to_owned()),
        )
        .await;
        assert!(res.is_ok(), "unlock should succeed");
        assert_eq!(res.unwrap().len(), 0, "no deferred connections to open");
        assert!(broker.is_unlocked(), "the broker must now be unlocked");
    }

    #[tokio::test]
    async fn vault_unlock_effect_wrong_passphrase_keeps_the_broker_locked() {
        use cairn_secrets::SecretString;
        use cairn_vault::{KdfParams, Vault};
        let dir = tempfile_dir();
        let path = dir.path().join("vault.cvlt");
        Vault::create_with_params(
            &path,
            &SecretString::from("right".to_owned()),
            KdfParams::fast_for_tests(),
        )
        .unwrap();
        let broker = Arc::new(Broker::locked());
        let registry = VfsRegistry::new();
        let err = run_vault_unlock_effect(
            &broker,
            Some(path),
            &[],
            &registry,
            SecretString::from("wrong".to_owned()),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_lowercase().contains("passphrase") || err.to_lowercase().contains("decryption"),
            "expected a wrong-passphrase message, got: {err}"
        );
        assert!(
            !broker.is_unlocked(),
            "a failed unlock must leave the broker locked"
        );
    }

    #[tokio::test]
    async fn vault_unlock_effect_no_path_configured_returns_a_clear_error() {
        use cairn_secrets::SecretString;
        let broker = Arc::new(Broker::locked());
        let registry = VfsRegistry::new();
        let err = run_vault_unlock_effect(
            &broker,
            None,
            &[],
            &registry,
            SecretString::from("x".to_owned()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("no vault path"), "got: {err}");
        assert!(!broker.is_unlocked());
    }

    #[tokio::test]
    async fn vault_unlock_effect_missing_file_is_a_clear_message() {
        use cairn_secrets::SecretString;
        let broker = Arc::new(Broker::locked());
        let registry = VfsRegistry::new();
        let err = run_vault_unlock_effect(
            &broker,
            Some(PathBuf::from("/no/such/dir/vault.cvlt")),
            &[],
            &registry,
            SecretString::from("x".to_owned()),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("no vault"),
            "expected a friendly message, got: {err}"
        );
        assert!(!broker.is_unlocked());
    }

    // A locked vault defers a credential-bearing profile; unlocking retries it. Hermetic: the S3
    // profile references a wrong-family (SSH) credential, which every connector rejects with `Auth`
    // *before any network*, so the retry exercises the full path without a live server.
    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn locked_vault_defers_credentialed_profiles_then_unlock_retries_them() {
        use cairn_secrets::SecretString;
        use cairn_vault::{CredentialSecret, KdfParams, SshCredential, Vault};
        let dir = tempfile_dir();
        let path = dir.path().join("vault.cvlt");
        let mut vault = Vault::create_with_params(
            &path,
            &SecretString::from("pw".to_owned()),
            KdfParams::fast_for_tests(),
        )
        .unwrap();
        let cred_id = vault.add(
            "c",
            CredentialSecret::Ssh(SshCredential::Password(SecretString::from("k".to_owned()))),
        );
        vault.save().unwrap();

        let mut cfg = Config::default();
        let mut prof = ConnectionProfile::new("s3", "prod");
        prof.endpoint.insert("bucket".to_owned(), "b".to_owned());
        prof.secret_ref = Some(cred_id);
        cfg.connections.push(prof);

        let broker = Arc::new(Broker::locked());
        let opener = crate::connect::ConnectionOpener::new(broker.clone());
        let registry = VfsRegistry::new();
        let (_choices, deferred, _descriptors) =
            register_connections(&registry, &cfg, &opener).await;
        assert_eq!(
            deferred.len(),
            1,
            "a locked credentialed profile must be deferred"
        );

        let opened = run_vault_unlock_effect(
            &broker,
            Some(path),
            &deferred,
            &registry,
            SecretString::from("pw".to_owned()),
        )
        .await
        .unwrap();
        assert!(
            broker.is_unlocked(),
            "the broker is unlocked even if a retry fails"
        );
        assert!(
            opened.is_empty(),
            "the wrong-family credential is rejected pre-network, so nothing connects"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_action_does_not_invoke_a_shell() {
        // Args that a shell would treat as `; touch pwned` must reach `true` as inert literal argv,
        // so no `pwned` file is created. Proves there is no shell interpretation layer.
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let ev = run_one(
            dir.path(),
            shell_action("true", &["{path}", ";", "touch", "pwned"]),
            "/f.txt",
        )
        .await;
        assert!(matches!(ev, AppEvent::ShellActionDone { error: false, .. }));
        assert!(
            !dir.path().join("pwned").exists(),
            "a shell would have created 'pwned'; argv exec must not"
        );
    }

    // --- split_cwd_root ---

    #[test]
    fn split_cwd_root_unix_absolute_path() {
        // /a/b/c → base "/" + VfsPath "/a/b/c"
        let (base, vfs) = split_cwd_root(std::path::Path::new("/a/b/c"));
        assert_eq!(base, std::path::PathBuf::from("/"));
        assert_eq!(vfs, VfsPath::parse("/a/b/c").unwrap());
    }

    #[test]
    fn split_cwd_root_unix_filesystem_root() {
        // "/" → base "/" + VfsPath "/" (root)
        let (base, vfs) = split_cwd_root(std::path::Path::new("/"));
        assert_eq!(base, std::path::PathBuf::from("/"));
        assert!(vfs.is_root());
    }

    #[test]
    fn split_cwd_root_relative_path_falls_back() {
        // A relative path has no root component → fall back to ("/" , root).
        let (base, vfs) = split_cwd_root(std::path::Path::new("a/b"));
        assert_eq!(base, std::path::PathBuf::from("/"));
        assert!(vfs.is_root());
    }

    #[test]
    fn split_cwd_root_empty_path_falls_back() {
        let (base, vfs) = split_cwd_root(std::path::Path::new(""));
        assert_eq!(base, std::path::PathBuf::from("/"));
        assert!(vfs.is_root());
    }

    #[test]
    fn split_cwd_root_in_loop_early_return_normalizes_empty_root() {
        // A relative path that triggers an in-loop early return (via a control character in a
        // segment that VfsPath::join rejects) must still produce "/" as the base, not an
        // empty PathBuf.  Without the fix, the in-loop `return (root, …)` fired before any
        // Prefix/RootDir component was pushed, leaving `root` empty and breaking LocalVfs.
        let (base, vfs) = split_cwd_root(std::path::Path::new("a/\u{1}b"));
        assert_eq!(
            base,
            std::path::PathBuf::from("/"),
            "in-loop early return must normalize empty base to /"
        );
        assert!(vfs.is_root());
    }
}
