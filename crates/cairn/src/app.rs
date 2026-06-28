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
use cairn_core::{initial_effects, update, AppEffect, AppEvent, AppState, ConnectionChoice, Msg};
use cairn_transfer::{ConflictPolicy, TransferOp, TransferSpec, VerifyPolicy};
use cairn_tui::Keymap;
use cairn_types::{ConnectionId, VfsPath};
use cairn_vfs::{ListOpts, ListPage, Recurse, Vfs, VfsError, VfsRegistry};
use futures::StreamExt;
use ratatui::crossterm::event::{self, Event};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const LEFT: ConnectionId = ConnectionId(1);
const RIGHT: ConnectionId = ConnectionId(2);

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

    let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(256);
    let (input_tx, mut input_rx) = mpsc::channel::<Event>(256);
    spawn_input_reader(input_tx);

    let mut terminal = ratatui::init();
    install_terminal_panic_hook();

    for effect in initial_effects(&state) {
        dispatch(effect, &registry, &event_tx);
    }
    terminal.draw(|f| cairn_tui::render(f, &state))?;

    let result = event_loop(
        &mut terminal,
        &mut state,
        &registry,
        &keymap,
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
    keymap: &Keymap,
    event_tx: &mpsc::Sender<AppEvent>,
    event_rx: &mut mpsc::Receiver<AppEvent>,
    input_rx: &mut mpsc::Receiver<Event>,
) -> anyhow::Result<()> {
    loop {
        let msg = tokio::select! {
            Some(ev) = event_rx.recv() => Some(Msg::Event(ev)),
            Some(input) = input_rx.recv() => map_input(input, keymap),
            else => break,
        };
        let Some(msg) = msg else { continue };

        let effects = update(state, msg);
        if state.should_quit {
            break;
        }
        terminal.draw(|f| cairn_tui::render(f, state))?;
        for effect in effects {
            dispatch(effect, registry, event_tx);
        }
    }
    Ok(())
}

/// Translate a terminal event into a reducer message (or `None` to ignore).
fn map_input(input: Event, keymap: &Keymap) -> Option<Msg> {
    match input {
        Event::Key(key) => keymap.action_for(key).map(Msg::Action),
        // A resize triggers a redraw via the no-op tick.
        Event::Resize(_, _) => Some(Msg::Tick),
        _ => None,
    }
}

/// Execute an effect on the tokio runtime; results flow back as [`AppEvent`]s.
fn dispatch(effect: AppEffect, registry: &VfsRegistry, event_tx: &mpsc::Sender<AppEvent>) {
    match effect {
        AppEffect::List { pane, conn, dir } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let result = list_dir(&registry, conn, &dir).await;
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
        } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ev = run_transfer_effect(&registry, src_conn, dst_conn, items, is_move).await;
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
            tokio::spawn(async move {
                // The assistant may only act on the two pane connections it can see in its
                // WorldSnapshot — not the switcher backends (which include a root-mounted `/`).
                // TODO(RFC-0007): thread a plan-level CancellationToken so the UI can abort a
                // long-running execution.
                let exec = crate::executor::BinaryStepExecutor::new(registry, vec![LEFT, RIGHT]);
                let n = plan.steps.len();
                let ev = match plan.execute(&exec).await {
                    Ok(()) if plan.state == cairn_ai::PlanState::Done => AppEvent::OpDone {
                        status: format!("Plan complete: {n} step(s) executed"),
                        error: false,
                    },
                    Ok(()) => {
                        // A step failed; surface its redacted reason (the executor already redacts).
                        let why = plan
                            .steps
                            .iter()
                            .find_map(|s| s.error.clone())
                            .unwrap_or_else(|| "a step failed".to_owned());
                        AppEvent::OpDone {
                            status: format!("Plan stopped: {why}"),
                            error: true,
                        }
                    }
                    Err(e) => AppEvent::OpDone {
                        status: format!("Plan not executed: {e}"),
                        error: true,
                    },
                };
                let _ = event_tx.send(ev).await;
            });
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

async fn run_transfer_effect(
    registry: &VfsRegistry,
    src_conn: ConnectionId,
    dst_conn: ConnectionId,
    items: Vec<(VfsPath, VfsPath)>,
    is_move: bool,
) -> AppEvent {
    let (Some(src), Some(dst)) = (registry.get(src_conn).await, registry.get(dst_conn).await)
    else {
        return AppEvent::OpDone {
            status: "connection unavailable".to_owned(),
            error: true,
        };
    };
    let spec = TransferSpec {
        op: if is_move {
            TransferOp::Move
        } else {
            TransferOp::Copy
        },
        conflict: ConflictPolicy::Overwrite,
        verify: VerifyPolicy::Size,
    };
    let cancel = CancellationToken::new();
    let mut bytes = 0u64;
    let verb = if is_move { "Moved" } else { "Copied" };
    match cairn_transfer::run_transfer(&src, &dst, &items, spec, &cancel, &mut |b| bytes += b).await
    {
        Ok(out) => AppEvent::OpDone {
            status: format!("{verb} {} file(s), {} dir(s)", out.files, out.dirs),
            error: false,
        },
        Err(e) => AppEvent::OpDone {
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

async fn list_dir(
    registry: &VfsRegistry,
    conn: ConnectionId,
    dir: &VfsPath,
) -> Result<ListPage, VfsError> {
    let Some(vfs) = registry.get(conn).await else {
        return Err(VfsError::NotFound(dir.clone()));
    };
    collect_pages(vfs, dir).await
}

/// Drain a backend's listing stream into a single page (sufficient for backends that paginate; the
/// UI virtualizes either way).
async fn collect_pages(vfs: Arc<dyn Vfs>, dir: &VfsPath) -> Result<ListPage, VfsError> {
    let mut entries = Vec::new();
    let mut stream = vfs.list(dir, ListOpts::default());
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
        let res = list_dir(&registry, ConnectionId(99), &VfsPath::root()).await;
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
        let page = list_dir(&registry, LEFT, &VfsPath::root()).await.unwrap();
        assert!(page.entries.iter().any(|e| e.name == "hello.txt"));
    }

    #[test]
    fn map_input_translates_keys_and_resize() {
        let km = Keymap::default();
        let q = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(map_input(q, &km), Some(Msg::Action(_))));
        assert!(matches!(
            map_input(Event::Resize(80, 24), &km),
            Some(Msg::Tick)
        ));
    }

    fn tempfile_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
}
