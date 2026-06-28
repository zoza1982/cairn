//! The application event loop and effect runner.
//!
//! Ties together `cairn-core` (state + reducer), `cairn-tui` (render + keymap), and the VFS
//! backends. The render path is synchronous; all I/O runs as tokio tasks whose results return as
//! [`AppEvent`]s over a bounded channel — see `docs/LLD.md` §4–§6.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cairn_backend_local::LocalVfs;
use cairn_core::{initial_effects, update, AppEffect, AppEvent, AppState, Msg};
use cairn_types::{ConnectionId, VfsPath};
use cairn_vfs::{ListOpts, ListPage, Vfs, VfsError, VfsRegistry};
use futures::StreamExt;
use ratatui::crossterm::event::{self, Event};
use tokio::sync::mpsc;

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
        &event_tx,
        &mut event_rx,
        &mut input_rx,
    )
    .await;

    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut AppState,
    registry: &VfsRegistry,
    event_tx: &mpsc::Sender<AppEvent>,
    event_rx: &mut mpsc::Receiver<AppEvent>,
    input_rx: &mut mpsc::Receiver<Event>,
) -> anyhow::Result<()> {
    loop {
        let msg = tokio::select! {
            Some(ev) = event_rx.recv() => Some(Msg::Event(ev)),
            Some(input) = input_rx.recv() => map_input(input),
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
fn map_input(input: Event) -> Option<Msg> {
    match input {
        Event::Key(key) => cairn_tui::action_for(key).map(Msg::Action),
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
                let _ = event_tx.send(AppEvent::Listed { pane, dir, result }).await;
            });
        }
        // `AppEffect` is non-exhaustive; future variants are wired up in later milestones.
        other => tracing::warn!(effect = ?other, "unhandled effect"),
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
        let q = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(map_input(q), Some(Msg::Action(_))));
        assert!(matches!(map_input(Event::Resize(80, 24)), Some(Msg::Tick)));
    }

    fn tempfile_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
}
