//! The application event loop and effect runner.
//!
//! Ties together `cairn-core` (state + reducer), `cairn-tui` (render + keymap), and the VFS
//! backends. The render path is synchronous; all I/O runs as tokio tasks whose results return as
//! [`AppEvent`]s over a bounded channel — see `docs/LLD.md` §4–§6.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::connect::coordinator::ConnectionCoordinator;
use crate::connect::descriptor::{ConnectionDescriptor, OpenTarget};
use crate::connect::provider::DiscoveryCtx;
use cairn_backend_local::LocalVfs;
use cairn_broker::{Actor, Broker};
use cairn_config::Config;
use cairn_core::{
    initial_effects, update, Action, AppEffect, AppEvent, AppState, ChoiceStatus, ConnectionChoice,
    ContentHash, LogViewerId, Msg, Overlay, PagerId, RemoteEditId, RemoteVersion, ShellActionMeta,
    Side, TransferId, WriteBackMode, REMOTE_EDIT_MAX_BYTES,
};
use cairn_transfer::{ConflictPolicy, TransferOp, TransferSpec, VerifyPolicy};
use cairn_tui::{text_edit_for, Keymap, Theme};
use cairn_types::SessionId;
use cairn_types::{Caps, ConnectionId, UnixPerms, VfsPath};
use cairn_vault::Vault;
use cairn_vfs::{ByteRange, ListOpts, ListPage, Recurse, Vfs, VfsError, VfsRegistry};
use futures::StreamExt;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

const LEFT: ConnectionId = ConnectionId(1);
const RIGHT: ConnectionId = ConnectionId(2);

/// First [`ConnectionId`] minted for an ephemeral archive mount (RFC-0013,
/// `docs/adr/0012-archive-mount-model.md`). Deliberately far above anything
/// [`ConnectionCoordinator`] could plausibly assign (it starts at `RIGHT.0 + 1 = 3` and grows by
/// one per real connection discovered/saved) — archive mounts live in a disjoint, monotonically
/// increasing id space of their own for the process lifetime, so the two counters can never
/// collide without needing to consult each other's claimed-id sets.
const ARCHIVE_CONN_ID_BASE: u64 = 1_000_000_000;

/// A private sentinel [`ConnectionId`] for the ephemeral [`LocalVfs`] the RFC-0012 P3 remote-edit
/// download/write-back effects build over a temp file. Never inserted into the [`VfsRegistry`] and
/// never compared against a real connection's id for anything other than the transfer engine's
/// same-connection fast-path check (`src.connection() == dst.connection()`), which must be
/// `false` here so a remote→temp / temp→remote copy always takes the stream-through path rather
/// than the same-backend rename fast-path. `u64::MAX` can never collide with a real connection id
/// (assigned sequentially from 1) or an archive-mount id (assigned sequentially from
/// [`ARCHIVE_CONN_ID_BASE`]).
const REMOTE_EDIT_TEMP_CONN_ID: ConnectionId = ConnectionId(u64::MAX);

/// UI progress granularity: the transfer callback notifies the status bar at most every this many
/// bytes. 256 KiB balances update frequency against channel pressure; progress is sent best-effort
/// (`try_send`, dropped when the channel is full), so there is no back-pressure on the transfer.
const TRANSFER_PROGRESS_STEP: u64 = 256 * 1024;

/// Minimum wall-clock gap (milliseconds) between coalesced transfer UI updates — both the pre-flight
/// scan (`TransferScanning`) and the time-driven byte-progress tick. Throttled by time, not
/// entry/byte count, because stat/list/copy rates span orders of magnitude across backends (local vs
/// an SFTP round-trip vs a cloud API): a fixed count threshold would flood a fast local operation yet
/// go silent for seconds on a slow remote one. ~120 ms keeps things visibly moving without flooding.
const TRANSFER_UI_MIN_INTERVAL_MS: u64 = 120;
/// [`TRANSFER_UI_MIN_INTERVAL_MS`] as a [`Duration`], for the scan's `Instant`-based throttle.
const TRANSFER_UI_MIN_INTERVAL: Duration = Duration::from_millis(TRANSFER_UI_MIN_INTERVAL_MS);

/// Whether a byte-progress UI update should be emitted now: on either enough new bytes
/// ([`TRANSFER_PROGRESS_STEP`]) *or* enough elapsed wall-clock ([`TRANSFER_UI_MIN_INTERVAL_MS`]) since
/// the last emit — the latter so a small/slow copy (total under one step) still ticks instead of
/// sitting at 0%. A time-only tick never fires without real progress (`bytes > last_sent`), so a
/// stalled/zero-byte transfer emits no redundant frames.
fn progress_emit_due(bytes: u64, last_sent: u64, now_ms: u64, last_sent_ms: u64) -> bool {
    let step_due = bytes - last_sent >= TRANSFER_PROGRESS_STEP;
    let time_due = now_ms.saturating_sub(last_sent_ms) >= TRANSFER_UI_MIN_INTERVAL_MS;
    step_due || (time_due && bytes > last_sent)
}

/// The resolved UI configuration threaded through the event loop (input mapping + colors).
struct Ui {
    keymap: Keymap,
    /// The `[ui.colors]` role overrides, applied on top of whichever preset `AppState::theme_name`
    /// currently names. Kept (rather than a pre-resolved `Theme`) so the palette is resolved fresh
    /// each frame — a live theme switch (Shift-T) then takes effect immediately.
    colors: std::collections::BTreeMap<String, String>,
}

impl Ui {
    /// Resolve the current palette from the live theme name + the config color overrides. Cheap
    /// (a match + a small map walk); `Theme` is `Copy`. Startup already logged any warnings, so they
    /// are dropped here.
    fn theme(&self, state: &AppState) -> Theme {
        Theme::resolve(&state.theme_name, &self.colors).0
    }
}

/// Runtime-side state for the vault-unlock and lazy-open flows. Lives in the effect layer, never
/// in [`AppState`] — it holds no secrets, but it holds the live broker handle and the opener.
///
/// - `broker`: shared credential broker; `run_vault_unlock_effect` installs the decrypted vault.
/// - `vault_path`: resolved vault file path (from config).
/// - `opener`: cloneable opener used by `run_open_connection_effect` to open Profile targets.
struct VaultContext {
    broker: Arc<Broker>,
    vault_path: Option<PathBuf>,
    opener: crate::connect::ConnectionOpener,
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
    // kept alive in the runtime-side `VaultContext` (below) so the vault-unlock overlay (M3-7)
    // can `.unlock(vault)` it; after unlock the reducer flips NeedsVault entries to NeedsOpen and
    // the trigger connection opens automatically via the P2 lazy-open path.
    let broker = Arc::new(Broker::locked());
    let opener = crate::connect::ConnectionOpener::new(broker.clone());

    // Register the switchable connections (Ctrl-O). In P2, local roots (builtin and saved) are
    // mounted eagerly; non-local profiles are enumerated as NeedsOpen or NeedsVault and opened
    // lazily when the user selects them. The deferred vec is always empty in P2.
    let (_deferred, descriptors) = {
        let (choices, deferred, descriptors) =
            register_connections(&registry, &config, &opener).await;
        state.connections = choices;
        // Populate the saved-profile mirror so the connection form (P4) can pre-fill fields.
        for prof in &config.connections {
            state.saved_profiles.insert(
                prof.id,
                cairn_core::forms::ProfileData {
                    id: prof.id,
                    scheme: prof.scheme.clone(),
                    display_name: prof.display_name.clone(),
                    endpoint: prof.endpoint.clone(),
                    secret_ref: prof.secret_ref,
                },
            );
        }
        (deferred, descriptors)
    };
    debug_assert!(
        _deferred.is_empty(),
        "P2 coordinator must not defer any connections"
    );

    state.vault_unlocked = broker.is_unlocked(); // false at startup; flips on unlock
                                                 // Snapshot whether the vault file exists so the reducer can branch Ctrl-U and NeedsVault
                                                 // selections between the create and unlock flows without ever doing I/O itself.
                                                 // One blocking `Path::exists()` call before the event loop starts is acceptable; the
                                                 // async loop never touches the filesystem for vault routing.
    state.vault_file_exists = config.vault_path().is_some_and(|p| p.exists());
    // Drive has_locked_connections from switcher entries rather than the (always-empty) deferred list.
    let n_needs_vault = state
        .connections
        .iter()
        .filter(|c| c.status == ChoiceStatus::NeedsVault)
        .count();
    state.has_locked_connections = n_needs_vault > 0;
    if state.has_locked_connections {
        let hint = if state.vault_file_exists {
            format!("{n_needs_vault} connection(s) need the vault — press Ctrl-U to unlock")
        } else {
            format!("{n_needs_vault} connection(s) need the vault — press Ctrl-U to create it")
        };
        state.status = Some(hint);
    }

    // Runtime-side context: the shared broker, the resolved vault file path, and the opener.
    // The vault-unlock effect reads broker + vault_path; the OpenConnection effect uses the opener.
    let vault_ctx = VaultContext {
        broker,
        vault_path: config.vault_path(),
        opener: opener.clone(),
    };

    // Seed the live theme name from config, normalizing to a canonical preset so cycling (Shift-T)
    // always advances visibly — an alias/typo (`"default"`, `""`, unknown) that `Theme::resolve`
    // renders as `dark` is stored as `"dark"`, not the raw string (which `next_theme` wouldn't
    // recognize, wasting the first press). Then resolve once at startup only to surface any warnings
    // (bad preset/role/color). The palette itself is re-resolved each frame from `state.theme_name`
    // (see `Ui::theme`) so it can be switched live.
    state.theme_name = if cairn_core::THEME_PRESETS.contains(&config.ui.theme.as_str()) {
        config.ui.theme.clone()
    } else {
        cairn_core::THEME_PRESETS[0].to_owned()
    };
    let (_theme, theme_warnings) = Theme::resolve(&config.ui.theme, &config.ui.colors);
    for w in theme_warnings {
        tracing::warn!("{w}");
    }
    let ui = Ui {
        keymap,
        colors: config.ui.colors.clone(),
    };

    let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(256);
    let (input_tx, mut input_rx) = mpsc::channel::<Event>(256);
    // Shared with the blocking input-reader thread so the editor-suspend path (RFC-0012 P2) can
    // pause it (and wait for its ack) before an external editor takes over the real TTY, and
    // resume it afterward. See `InputGate` and `run_editor_suspend`.
    let input_gate = InputGate::new();
    spawn_input_reader(input_tx, input_gate.clone());

    let mut terminal = ratatui::init();
    install_terminal_panic_hook();

    // Initial effects are only directory listings — no transfer, so no token slot needed.
    let initial = initial_effects(&state);
    debug_assert!(
        initial
            .iter()
            .all(|e| matches!(e, AppEffect::List { .. } | AppEffect::DetectOsSources)),
        "initial_effects may only emit List and DetectOsSources effects at startup"
    );
    let mut startup_controls = HashMap::new();
    let mut startup_log_controls: HashMap<LogViewerId, CancellationToken> = HashMap::new();
    let mut startup_pager_controls: HashMap<PagerId, CancellationToken> = HashMap::new();
    let mut startup_session_controls: HashMap<SessionId, SessionControls> = HashMap::new();
    // Initial effects are List effects only (asserted above); empty descriptor map and an
    // empty in-flight set are safe here because OpenConnection is never emitted before the
    // event loop starts.
    let empty_descriptors: HashMap<ConnectionId, ConnectionDescriptor> = HashMap::new();
    let mut startup_in_flight: HashSet<ConnectionId> = HashSet::new();
    let mut startup_test_in_flight: HashSet<ConnectionId> = HashSet::new();
    let mut startup_next_archive_conn_id: u64 = ARCHIVE_CONN_ID_BASE;
    let mut startup_remote_edit_temps: HashMap<RemoteEditId, tempfile::TempDir> = HashMap::new();
    for effect in initial {
        dispatch(
            effect,
            &registry,
            &event_tx,
            &mut startup_controls,
            &mut None,
            &mut startup_log_controls,
            &mut startup_pager_controls,
            &mut startup_session_controls,
            &shell_action_defs,
            &vault_ctx,
            &empty_descriptors,
            &mut startup_in_flight,
            &mut startup_test_in_flight,
            &mut startup_next_archive_conn_id,
            &mut startup_remote_edit_temps,
        );
    }
    terminal.draw(|f| cairn_tui::render(f, &state, &ui.theme(&state)))?;

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
        &input_gate,
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

/// Register the connections offered by the switcher and return their UI choices, the (always-empty
/// in P2) deferred list, and the runtime descriptor side-map.
///
/// Delegates to [`ConnectionCoordinator`] (RFC-0011 P2). Built-in local roots (`/` and `$HOME`
/// when set) and `scheme = "local"` config profiles are eagerly mounted as `Ready`; all `Profile`
/// targets (remote or credential-bearing) are placed in the switcher as `NeedsOpen` or
/// `NeedsVault` without opening. Pass `&HashMap::new()` as `prior_descriptors` on startup; pass
/// the previous descriptor map on re-enumeration so ids are reused for keys already live in a pane.
async fn register_connections(
    registry: &VfsRegistry,
    config: &Config,
    opener: &crate::connect::ConnectionOpener,
) -> (
    Vec<ConnectionChoice>,
    Vec<crate::connect::coordinator::DeferredConnection>,
    HashMap<ConnectionId, ConnectionDescriptor>,
) {
    let coordinator = ConnectionCoordinator::new(opener.clone(), RIGHT.0 + 1);
    // Derive vault_locked from the live broker state so this call site and future
    // re-enumeration calls automatically reflect the current lock status.
    let ctx = DiscoveryCtx {
        config,
        vault_locked: opener.vault_locked(),
    };
    coordinator.run(registry, &ctx, &HashMap::new()).await
}

/// Process-wide lock serializing every `cairn.toml` read-modify-write cycle.
///
/// `run_save_connection_effect`, `run_delete_connection_effect`, and
/// `run_set_discovery_flag_effect` (RFC-0011 P6 pin/hide) each do `Config::load` → mutate →
/// `Config::save` with no coordination between them. Pin/hide are one-keystroke, rapid-fire
/// actions in the switcher, so two of these effects can easily run concurrently on the
/// multi-thread runtime; without a lock they interleave and the second `save` silently clobbers
/// the first's change (in-memory state shows it applied, the file on disk does not) even though
/// each individual write is atomic (`Config::save`'s temp-file + rename only makes *one* write
/// safe against a torn file — it says nothing about *two* writers racing each other). All of
/// these effects target the same file, so a single process-wide lock is the right granularity:
/// there is only ever one writer for the whole file at a time.
static CONFIG_WRITE_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

/// Acquire the process-wide config-write lock. Hold the returned guard for the entire
/// load→mutate→save cycle; drop it only once the save has completed (or failed).
async fn config_write_lock() -> tokio::sync::MutexGuard<'static, ()> {
    CONFIG_WRITE_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Convert a [`cairn_core::forms::ProfileData`] into a [`cairn_config::ConnectionProfile`].
///
/// The mapping is one-to-one: both structs share the same field names and types (the core
/// `ProfileData` was designed as a dep-free mirror of `ConnectionProfile`).
fn profile_data_to_config(
    profile: &cairn_core::forms::ProfileData,
) -> cairn_config::ConnectionProfile {
    cairn_config::ConnectionProfile {
        id: profile.id,
        scheme: profile.scheme.clone(),
        display_name: profile.display_name.clone(),
        endpoint: profile.endpoint.clone(),
        secret_ref: profile.secret_ref,
    }
}

/// Save (create or update) a connection profile to the cairn config file.
///
/// Loads the config, upserts the profile, and saves. Does NOT call `register_connections`
/// (which would alias ConnectionIds and corrupt the descriptor_map). Always returns an
/// `AppEvent` (success or `ConnectionOpFailed`).
async fn run_save_connection_effect(
    profile: cairn_core::forms::ProfileData,
    is_edit: bool,
) -> AppEvent {
    // Held for the whole load→mutate→save cycle below — see `CONFIG_WRITE_LOCK`'s doc.
    let _guard = config_write_lock().await;

    let Some(config_path) = cairn_config::default_config_path() else {
        return AppEvent::ConnectionOpFailed {
            message: "Cannot determine config file path".to_owned(),
        };
    };

    let mut config = match cairn_config::Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load config for save");
            return AppEvent::ConnectionOpFailed {
                message: format!("Failed to load config: {e}"),
            };
        }
    };

    let cfg_profile = profile_data_to_config(&profile);
    let id = cfg_profile.id;
    let display_name = cfg_profile.display_name.clone();
    // Compute the switcher label using the same convention the provider uses:
    // "local: {path}" for local profiles, display_name for all others.
    let label = if profile.scheme == "local" {
        let path = profile
            .endpoint
            .get("path")
            .map(String::as_str)
            .unwrap_or("");
        format!("local: {path}")
    } else {
        profile.display_name.clone()
    };
    if is_edit {
        if let Some(existing) = config.connections.iter_mut().find(|p| p.id == id) {
            *existing = cfg_profile;
        } else {
            config.connections.push(cfg_profile);
        }
    } else {
        config.connections.push(cfg_profile);
    }

    if let Err(e) = config.save(&config_path) {
        tracing::warn!(error = %e, "failed to save config");
        return AppEvent::ConnectionOpFailed {
            message: format!("Failed to save config: {e}"),
        };
    }

    AppEvent::ConnectionSaved {
        id,
        display_name,
        label,
        is_edit,
        profile,
    }
}

/// Delete a connection profile from the cairn config file, and remove its vault credential.
///
/// Does NOT call `register_connections` (which would alias ConnectionIds). The reducer handles
/// the in-memory switcher update via `ConnectionDeleted`. Always returns an `AppEvent` (success
/// or `ConnectionOpFailed`).
///
/// If `secret_ref` is `Some`, the vault credential is removed first. A vault removal failure is
/// logged but does not abort the config delete — the profile would be orphaned from the vault
/// entry in that case (a recoverable inconsistency), and leaving the config entry in place would
/// be worse.
async fn run_delete_connection_effect(
    broker: Arc<Broker>,
    id: uuid::Uuid,
    secret_ref: Option<uuid::Uuid>,
) -> AppEvent {
    use cairn_broker::Actor;

    // Remove the vault credential before the config entry so that a crash between the two
    // leaves an unreferenced vault entry (safe, just wasteful) rather than a dangling reference.
    if let Some(cred_id) = secret_ref {
        if let Err(e) = broker.remove(Actor::User, cred_id) {
            tracing::warn!(error = %e, %cred_id, "failed to remove vault credential on delete; proceeding");
        }
    }

    // Held for the whole load→mutate→save cycle below — see `CONFIG_WRITE_LOCK`'s doc. The vault
    // removal above is a separate resource (the broker, not `cairn.toml`) and does not need it.
    let _guard = config_write_lock().await;

    let Some(config_path) = cairn_config::default_config_path() else {
        return AppEvent::ConnectionOpFailed {
            message: "Cannot determine config file path".to_owned(),
        };
    };

    let mut config = match cairn_config::Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load config for delete");
            return AppEvent::ConnectionOpFailed {
                message: format!("Failed to load config: {e}"),
            };
        }
    };

    config.connections.retain(|p| p.id != id);

    if let Err(e) = config.save(&config_path) {
        tracing::warn!(error = %e, "failed to save config after delete");
        return AppEvent::ConnectionOpFailed {
            message: format!("Failed to save config: {e}"),
        };
    }

    AppEvent::ConnectionDeleted { id }
}

/// P5: provision a credential into the vault, then save the connection profile.
///
/// This is the binary-edge function that assembles a [`cairn_vault::CredentialSecret`] from a
/// [`cairn_core::CredentialDraft`]. It is the **only** place in the codebase that converts between
/// the two representations; `cairn-core` never imports `cairn-vault`.
///
/// ## Security invariants
/// - `CredentialDraft::SshAgent`, `AwsDefaultChain`, `AwsProfile`, `GcpApplicationDefault`,
///   `AzureAd`, and `KeepExisting` are delegation methods: no vault op is performed (they either
///   need no vault entry or preserve the existing `secret_ref`). The profile is saved directly.
/// - Deferred drafts (non-functional variants): the profile is saved without a `secret_ref`. If
///   in edit mode with an existing credential, the old vault entry is removed to avoid orphaning.
/// - On edit with a new vault-backed credential, the old `secret_ref` vault entry is removed
///   after the new entry is successfully stored.
/// - Error messages are redacted (no host, no secret, no path).
async fn run_provision_and_save_connection_effect(
    broker: Arc<Broker>,
    mut profile: cairn_core::forms::ProfileData,
    draft: cairn_core::forms::CredentialDraft,
    is_edit: bool,
) -> AppEvent {
    use cairn_broker::Actor;
    use cairn_core::forms::CredentialDraft;
    use cairn_vault::{
        AwsCredential, AzureCredential, CredentialSecret, GcpCredential, SshCredential,
    };

    // Capture the old credential reference before we potentially overwrite it.
    // Used after a successful edit to remove the now-orphaned vault entry.
    let old_secret_ref = profile.secret_ref;

    // Assemble a CredentialSecret from the draft. For delegation and KeepExisting, we skip the
    // vault operation and fall through to the profile save.
    let secret: Option<CredentialSecret> = match draft {
        // ── Delegation / no-vault methods ──
        CredentialDraft::SshAgent => Some(CredentialSecret::Ssh(SshCredential::Agent)),
        CredentialDraft::AwsDefaultChain => {
            Some(CredentialSecret::Aws(AwsCredential::DefaultChain))
        }
        CredentialDraft::AwsProfile { profile_name } => {
            Some(CredentialSecret::Aws(AwsCredential::Profile(profile_name)))
        }
        CredentialDraft::GcpApplicationDefault => {
            Some(CredentialSecret::Gcp(GcpCredential::ApplicationDefault))
        }
        CredentialDraft::AzureAd => Some(CredentialSecret::Azure(AzureCredential::AzureAd)),

        // ── Secret-bearing methods ──
        CredentialDraft::SshPrivateKeyFile { path, passphrase } => {
            Some(CredentialSecret::Ssh(SshCredential::PrivateKeyFile {
                path: std::path::PathBuf::from(path),
                passphrase,
            }))
        }
        CredentialDraft::SshInlinePem {
            key_pem,
            passphrase,
        } => Some(CredentialSecret::Ssh(SshCredential::PrivateKey {
            key_pem,
            passphrase,
        })),
        CredentialDraft::SshPassword { password } => {
            Some(CredentialSecret::Ssh(SshCredential::Password(password)))
        }
        CredentialDraft::AwsStatic {
            access_key_id,
            secret_access_key,
            session_token,
        } => Some(CredentialSecret::Aws(AwsCredential::Static {
            access_key_id,
            secret_access_key,
            session_token,
        })),

        // ── Deferred / placeholder drafts — save profile without vault credential ──
        //
        // If this is an edit and the profile previously had a vault entry, remove it now so
        // the user switching to a deferred method does not leave an orphaned vault entry.
        CredentialDraft::GcpServiceAccountJson { .. }
        | CredentialDraft::AzureSharedKey { .. }
        | CredentialDraft::AzureSasToken { .. }
        | CredentialDraft::AzureConnectionString { .. } => {
            if is_edit {
                if let Some(old_ref) = old_secret_ref {
                    if let Err(e) = broker.remove(Actor::User, old_ref) {
                        tracing::warn!(error = %e, "failed to remove old vault entry for deferred edit");
                    }
                }
            }
            profile.secret_ref = None;
            None
        }

        // ── Edit mode: preserve existing secret_ref, no vault operation ──
        CredentialDraft::KeepExisting => {
            // profile.secret_ref is already set from the overlay (existing_secret_ref).
            // Just save the profile as-is.
            return run_save_connection_effect(profile, is_edit).await;
        }
        // Catch-all for future non-exhaustive variants added without updating this match.
        // `CredentialDraft` is `#[non_exhaustive]` and this crate controls both sides, so this
        // arm should never be reached at runtime.
        _ => None,
    };

    if let Some(secret) = secret {
        // All methods — including delegation — store a vault entry so the connect layer can look
        // up which OS credential source to use via `secret_ref`.
        let label = profile.display_name.clone();
        match broker.store(Actor::User, &label, secret) {
            Ok(new_id) => {
                // Wire the new vault id into the profile.
                profile.secret_ref = Some(new_id);
                // If this is an edit, the old vault entry is now orphaned. Remove it after the
                // new entry is safely stored (so a remove failure doesn't block the save).
                if is_edit {
                    if let Some(old_ref) = old_secret_ref {
                        if old_ref != new_id {
                            if let Err(e) = broker.remove(Actor::User, old_ref) {
                                tracing::warn!(error = %e, "failed to remove old vault entry on edit");
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to store credential in vault");
                return AppEvent::ConnectionOpFailed {
                    message: "Failed to store credential (vault error — see logs)".to_owned(),
                };
            }
        }
    }

    // Save the profile (with or without an updated secret_ref).
    run_save_connection_effect(profile, is_edit).await
}

/// P5: detect OS credential source availability by checking env vars and file existence.
///
/// **Security invariant:** reads only names and existence — never reads file contents, key
/// material, or environment variable values (except to test for presence).
async fn run_detect_os_sources_effect() -> cairn_core::OsSources {
    use cairn_core::OsSources;

    // All detection is blocking I/O (stat + env lookup). Run off the async runtime.
    tokio::task::spawn_blocking(|| {
        let ssh_agent = std::env::var("SSH_AUTH_SOCK").is_ok();

        // AWS: read profile section names from ~/.aws/credentials — names only, never values.
        let aws_profiles = detect_aws_profiles();

        // GCP ADC: either GOOGLE_APPLICATION_CREDENTIALS is set, or the well-known JSON file exists.
        // XDG_CONFIG_HOME is preferred; fall back to ~/.config (Unix/macOS) or %APPDATA% (Windows).
        let gcp_adc = std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_ok() || {
            let config_dir = std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                // Unix/macOS: $HOME/.config
                .or_else(|_| {
                    std::env::var("HOME")
                        .map(|h| PathBuf::from(h).join(".config"))
                })
                // Windows: %APPDATA%
                .or_else(|_| std::env::var("APPDATA").map(PathBuf::from))
                .ok();
            config_dir
                .map(|d| {
                    d.join("gcloud")
                        .join("application_default_credentials.json")
                        .exists()
                })
                .unwrap_or(false)
        };

        // Azure AD: heuristic — look for the standard Azure SDK env vars.
        let azure_ad_likely = std::env::var("AZURE_CLIENT_ID").is_ok()
            || std::env::var("AZURE_TENANT_ID").is_ok()
            || std::env::var("AZURE_CLIENT_SECRET").is_ok();

        OsSources {
            ssh_agent,
            aws_profiles,
            gcp_adc,
            azure_ad_likely,
        }
    })
    .await
    .unwrap_or_else(|e| {
        // A panic inside spawn_blocking would be silently swallowed by unwrap_or_default.
        // Log it so it's visible in development; OS source detection failures are non-fatal.
        tracing::warn!(error = ?e, "OS source detection task panicked; using defaults");
        OsSources::default()
    })
}

/// Parse AWS profile names from `~/.aws/credentials` (or `$AWS_SHARED_CREDENTIALS_FILE`).
///
/// Returns only the section header names (`[profile]`), never any key values.
/// Returns an empty `Vec` if the file is absent or unreadable.
///
/// **Security invariant:** reads line-by-line via `BufReader` so the secret key values on
/// non-header lines are never held in a heap-allocated `String`. Non-header lines are processed
/// in the iterator and dropped immediately; only the `[header]` names are retained.
fn detect_aws_profiles() -> Vec<String> {
    use std::io::{BufRead, BufReader};
    use zeroize::Zeroizing;

    // Honour the AWS SDK override env var; fall back to the conventional location.
    // `HOME` on Unix/macOS, `USERPROFILE` on Windows (both are set in standard environments).
    let path = if let Ok(custom) = std::env::var("AWS_SHARED_CREDENTIALS_FILE") {
        PathBuf::from(custom)
    } else {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()
            .filter(|s| !s.is_empty());
        match home {
            Some(h) => PathBuf::from(h).join(".aws").join("credentials"),
            None => return Vec::new(),
        }
    };

    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    // Parse ini-style section headers: `[profile_name]` on their own line.
    // Non-header lines (the key/secret pairs) are wrapped in `Zeroizing` and dropped
    // immediately so they never linger in memory beyond the iterator step.
    BufReader::new(file)
        .lines()
        .filter_map(|line| {
            // Wrap the raw line in Zeroizing so it is wiped when this closure returns.
            let line = Zeroizing::new(line.ok()?);
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let name = trimmed[1..trimmed.len() - 1].trim().to_owned();
                // Skip empty section names (malformed INI files).
                if name.is_empty() {
                    None
                } else {
                    Some(name)
                }
            } else {
                // Non-header line: `line` is dropped and zeroized here.
                None
            }
        })
        .collect()
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
    input_gate: &Arc<InputGate>,
) -> anyhow::Result<()> {
    // `descriptor_map` is looked up by the OpenConnection effect runner to find what to open.
    // P3: needs Arc<RwLock<_>> or a re-enumeration message to swap this map without restarting
    // the loop (RFC-0011 §3: live config reload while panes are browsing connections).
    // Control channels of the in-flight transfer / AI plan (if any), held runtime-side so the
    // matching effect can signal them. Each is cleared when its Done event arrives.
    // Per-transfer control, keyed by `TransferId`: the cancel token + pause sender form a *control
    // pair* created together (in `AppEffect::Transfer`) and removed together (on that transfer's
    // `TransferDone`/`TransferConflict`). Multiple transfers run concurrently, so this is a map.
    let mut transfer_controls: HashMap<TransferId, TransferControls> = HashMap::new();
    let mut ai_cancel: Option<CancellationToken> = None;
    let mut log_viewer_controls: HashMap<LogViewerId, CancellationToken> = HashMap::new();
    let mut pager_controls: HashMap<PagerId, CancellationToken> = HashMap::new();
    let mut session_controls: HashMap<SessionId, SessionControls> = HashMap::new();
    // Tracks which ConnectionIds currently have an open task in flight. A duplicate
    // OpenConnection effect for the same id (e.g. the user selects a NeedsOpen entry twice
    // before the first open completes) is dropped here so only one backend connection is
    // established. The id is removed when the matching ConnectionOpened event arrives.
    let mut open_connection_in_flight: HashSet<ConnectionId> = HashSet::new();
    // Same idea as `open_connection_in_flight`, but for `AppEffect::TestConnection` (RFC-0011 P6):
    // a duplicate probe for the same id (repeated `t` presses before the first completes) is
    // dropped so only one probe runs at a time. Removed when `ConnectionTested` arrives.
    let mut test_connection_in_flight: HashSet<ConnectionId> = HashSet::new();
    // Monotonic id source for ephemeral archive-mount connections (RFC-0013). Lives for the whole
    // event-loop lifetime (unlike the per-transfer/session maps above) because, unlike those, there
    // is no "done" event that could reclaim an id for reuse — each mount is a genuinely new,
    // permanently-registered (for the session) connection. See `ARCHIVE_CONN_ID_BASE`.
    let mut next_archive_conn_id: u64 = ARCHIVE_CONN_ID_BASE;
    // Owns the RAII temp directory for each in-flight remote-edit session (RFC-0012 P3), keyed by
    // `RemoteEditId`. Created in `dispatch`'s `DownloadForEdit` arm; removed (deleting the temp
    // file) on every terminal outcome below, on an explicit `CancelRemoteEdit` (`dispatch`), and —
    // as a last-resort safety net — when the whole event loop exits (dropping this map along with
    // every other local here), so a session left open when the app quits is still cleaned up.
    let mut remote_edit_temps: HashMap<RemoteEditId, tempfile::TempDir> = HashMap::new();
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
        // The reducer's own cap-hit path (`AppEvent::PagerChunk` reaching `PAGER_MAX_BYTES`)
        // fires `AppEffect::ClosePager` itself (handled in `dispatch`, below) rather than waiting
        // for `PagerDone` — so this only needs to clean up the natural EOF/error/cancel paths.
        if let Msg::Event(AppEvent::PagerDone { id, .. }) = &msg {
            pager_controls.remove(id);
        }
        // Session cleanup: remove the controls entry when the session ends so the oneshot/mpsc
        // senders are dropped (closing stdin and signalling the relay task) if they haven't been
        // consumed already. The session record in `AppState::sessions` is cleaned up by the reducer.
        if let Msg::Event(AppEvent::SessionEnded { id, .. }) = &msg {
            session_controls.remove(id);
        }
        // Remove the in-flight marker when the open result arrives so duplicate effects
        // for the same id are unblocked (the first open is done; a retry is now allowed).
        if let Msg::Event(AppEvent::ConnectionOpened { conn, .. }) = &msg {
            open_connection_in_flight.remove(conn);
        }
        // Same for `ConnectionTested` (RFC-0011 P6): unblock a future probe of the same id.
        if let Msg::Event(AppEvent::ConnectionTested { conn, .. }) = &msg {
            test_connection_in_flight.remove(conn);
        }
        // RFC-0012 P3: every terminal outcome of a remote-edit session drops (and thus deletes)
        // its held temp directory. `WriteBackConflict` is deliberately excluded — the flow
        // continues (the overlay is open, or `KeepEditing`/`SaveAs` will re-use the same temp
        // file), so cleanup must not run yet.
        if let Msg::Event(
            AppEvent::RemoteEditNoChange { id, .. }
            | AppEvent::RemoteEditFailed { id, .. }
            | AppEvent::WriteBackDone { id, .. },
        ) = &msg
        {
            remote_edit_temps.remove(id);
        }
        let effects = update(state, msg);
        if state.should_quit {
            break;
        }
        terminal.draw(|f| cairn_tui::render(f, state, &ui.theme(state)))?;
        for effect in effects {
            // `SuspendAndEdit` needs exclusive terminal + stdin ownership to hand off to an
            // external editor — `dispatch` has neither (no `&mut Terminal`, and effects normally
            // run concurrently via `tokio::spawn`), so it is special-cased here instead of routed
            // through the normal effect runner. See `run_editor_suspend` and
            // `docs/adr/0011-terminal-suspend-and-editor-launch.md`.
            if let AppEffect::SuspendAndEdit { conn, path } = effect {
                run_editor_suspend(terminal, input_gate, registry, event_tx, conn, path).await;
                continue;
            }
            // `EditRemoteTemp` (RFC-0012 P3) needs the same exclusive terminal/stdin ownership as
            // `SuspendAndEdit` — see `run_remote_edit_temp`.
            if let AppEffect::EditRemoteTemp {
                id,
                conn,
                path,
                temp_path,
                v0,
                orig_size,
                orig_perms,
                download_hash,
                hash,
            } = effect
            {
                run_remote_edit_temp(
                    terminal,
                    input_gate,
                    event_tx,
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
                .await;
                continue;
            }
            dispatch(
                effect,
                registry,
                event_tx,
                &mut transfer_controls,
                &mut ai_cancel,
                &mut log_viewer_controls,
                &mut pager_controls,
                &mut session_controls,
                shell_action_defs,
                vault_ctx,
                &descriptor_map,
                &mut open_connection_in_flight,
                &mut test_connection_in_flight,
                &mut next_archive_conn_id,
                &mut remote_edit_temps,
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

/// Drop-guard for a spawned `OpenConnection` task — mirrors [`TransferDoneGuard`].
///
/// If the task panics or is dropped before completing, the guard fires
/// `ConnectionOpened { Err }` via `try_send` so the reducer can clear its `NeedsOpen` status
/// and the in-flight tracker removes the entry. Disarm before the explicit final send.
struct ConnectionOpenGuard {
    conn: ConnectionId,
    event_tx: mpsc::Sender<AppEvent>,
    armed: bool,
}

impl ConnectionOpenGuard {
    fn new(conn: ConnectionId, event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self {
            conn,
            event_tx,
            armed: true,
        }
    }

    /// Disarm before emitting the real outcome so the guard does not fire on drop.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ConnectionOpenGuard {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort (`try_send`, no `.await` allowed in Drop). If the bounded event channel
            // is momentarily full the synthetic error is silently dropped. The consequence is
            // worse than `TransferDoneGuard`'s slot leak: the `ConnectionId` stays in
            // `open_connection_in_flight` indefinitely (it is only removed on `ConnectionOpened`),
            // making the connection permanently un-openable for the lifetime of the process. In
            // practice the channel (256 deep) is ~never near full when a single open task panics,
            // so the race is negligible — but the asymmetry with transfer is worth documenting.
            let _ = self.event_tx.try_send(AppEvent::ConnectionOpened {
                conn: self.conn,
                result: Err("connection open task interrupted".to_owned()),
            });
        }
    }
}

/// Same role as [`ConnectionOpenGuard`], for [`AppEffect::TestConnection`] (RFC-0011 P6): if the
/// probe task panics or is dropped before completing, a synthetic failure `ConnectionTested`
/// unblocks `test_connection_in_flight` rather than leaving the id permanently un-testable.
struct ConnectionTestGuard {
    conn: ConnectionId,
    event_tx: mpsc::Sender<AppEvent>,
    armed: bool,
}

impl ConnectionTestGuard {
    fn new(conn: ConnectionId, event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self {
            conn,
            event_tx,
            armed: true,
        }
    }

    /// Disarm before emitting the real outcome so the guard does not fire on drop.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ConnectionTestGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.event_tx.try_send(AppEvent::ConnectionTested {
                conn: self.conn,
                result: Err("connection test task interrupted".to_owned()),
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
            // Inside the VaultCreate overlay, Ctrl-R toggles the keychain "remember" opt-in
            // without inserting 'r' into the passphrase field. This is the only action bypass
            // for a capturing overlay other than Ctrl-C (quit) and the ExecPane controls above.
            if matches!(&state.overlay, Some(Overlay::VaultCreate { .. }))
                && ctrl
                && key.code == KeyCode::Char('r')
            {
                return Some(Msg::Action(Action::ToggleRemember));
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
/// [`AppEffect::SetTransferPaused`] can target the right transfer task. `descriptor_map` is looked
/// up by [`AppEffect::OpenConnection`] to find the [`ConnectionDescriptor`] for a selected id.
/// `open_connection_in_flight` prevents duplicate concurrent backend connections for the same id.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    effect: AppEffect,
    registry: &VfsRegistry,
    event_tx: &mpsc::Sender<AppEvent>,
    transfer_controls: &mut HashMap<TransferId, TransferControls>,
    ai_cancel: &mut Option<CancellationToken>,
    log_viewer_controls: &mut HashMap<LogViewerId, CancellationToken>,
    pager_controls: &mut HashMap<PagerId, CancellationToken>,
    session_controls: &mut HashMap<SessionId, SessionControls>,
    shell_action_defs: &Arc<[cairn_config::ShellActionDef]>,
    vault_ctx: &VaultContext,
    descriptor_map: &HashMap<ConnectionId, ConnectionDescriptor>,
    open_connection_in_flight: &mut HashSet<ConnectionId>,
    test_connection_in_flight: &mut HashSet<ConnectionId>,
    next_archive_conn_id: &mut u64,
    remote_edit_temps: &mut HashMap<RemoteEditId, tempfile::TempDir>,
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
        AppEffect::Delete { id, conn, paths } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            // A delete is a tracked op (shown in the dialog with progress) and cancellable, so it
            // registers a control entry like a transfer. Pause is not supported for delete (the
            // sender exists only to keep the `TransferControls` shape uniform), so the reducer never
            // sends `SetTransferPaused` for a delete id.
            let cancel = CancellationToken::new();
            let (pause_tx, _paused) = watch::channel(false);
            transfer_controls.insert(
                id,
                TransferControls {
                    cancel: cancel.clone(),
                    pause: pause_tx,
                },
            );
            tokio::spawn(async move {
                let mut guard = TransferDoneGuard {
                    id,
                    event_tx: event_tx.clone(),
                    armed: true,
                };
                let ev = run_delete_effect(&registry, id, conn, paths, &event_tx, cancel).await;
                guard.armed = false;
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
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let result = run_vault_unlock_effect(&broker, vault_path, passphrase).await;
                let _ = event_tx.send(AppEvent::VaultUnlocked { result }).await;
            });
        }
        AppEffect::CreateVault {
            passphrase,
            remember,
        } => {
            let broker = vault_ctx.broker.clone();
            let vault_path = vault_ctx.vault_path.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let (result, already_exists) =
                    run_create_vault_effect(&broker, vault_path, passphrase, remember).await;
                let _ = event_tx
                    .send(AppEvent::VaultCreated {
                        result,
                        already_exists,
                    })
                    .await;
            });
        }
        AppEffect::OpenConnection { conn } => {
            // In-flight guard: if another open task is already running for this id, drop the
            // duplicate effect. The first task will emit ConnectionOpened and unblock future
            // selections. This prevents two concurrent backend handshakes for the same connection
            // (e.g. when the user selects a NeedsOpen entry twice before the first open finishes).
            if open_connection_in_flight.contains(&conn) {
                return;
            }
            open_connection_in_flight.insert(conn);

            // Lazy open: look up the descriptor, guard against already-mounted, open in background.
            let Some(desc) = descriptor_map.get(&conn).cloned() else {
                // Descriptor missing — coordinator bug or race on re-enumeration; report error.
                tracing::error!(conn = %conn.0, "OpenConnection: no descriptor found for id");
                let event_tx = event_tx.clone();
                // No ConnectionOpenGuard needed: the spawn only calls `.send()` and cannot panic.
                tokio::spawn(async move {
                    let _ = event_tx
                        .send(AppEvent::ConnectionOpened {
                            conn,
                            result: Err("connection descriptor not found".to_owned()),
                        })
                        .await;
                });
                return;
            };
            let registry = registry.clone();
            let opener = vault_ctx.opener.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let mut guard = ConnectionOpenGuard::new(conn, event_tx.clone());
                // Already-mounted guard: if the registry already has a live VFS for this id
                // (e.g. eager-mount at startup or a prior open that beat us here), report
                // success immediately without re-opening (idempotent).
                if registry.get(conn).await.is_some() {
                    guard.disarm();
                    let _ = event_tx
                        .send(AppEvent::ConnectionOpened {
                            conn,
                            result: Ok(()),
                        })
                        .await;
                    return;
                }
                let result = run_open_connection_effect(&registry, &opener, conn, &desc).await;
                guard.disarm();
                let _ = event_tx
                    .send(AppEvent::ConnectionOpened { conn, result })
                    .await;
            });
        }
        AppEffect::TestConnection { conn } => {
            // In-flight guard mirrors `OpenConnection`'s: a duplicate probe for the same id
            // (repeated `t` presses before the first completes) is dropped.
            if test_connection_in_flight.contains(&conn) {
                return;
            }
            test_connection_in_flight.insert(conn);

            let Some(desc) = descriptor_map.get(&conn).cloned() else {
                tracing::error!(conn = %conn.0, "TestConnection: no descriptor found for id");
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let _ = event_tx
                        .send(AppEvent::ConnectionTested {
                            conn,
                            result: Err("connection descriptor not found".to_owned()),
                        })
                        .await;
                });
                return;
            };
            let opener = vault_ctx.opener.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let mut guard = ConnectionTestGuard::new(conn, event_tx.clone());
                let result = run_test_connection_effect(&opener, conn, &desc).await;
                guard.disarm();
                let _ = event_tx
                    .send(AppEvent::ConnectionTested { conn, result })
                    .await;
            });
        }
        AppEffect::SetConnectionPinned { conn, pinned } => {
            let Some(desc) = descriptor_map.get(&conn) else {
                tracing::error!(conn = %conn.0, "SetConnectionPinned: no descriptor found for id");
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let _ = event_tx
                        .send(AppEvent::ConnectionPinSet {
                            conn,
                            pinned,
                            result: Err("connection descriptor not found".to_owned()),
                        })
                        .await;
                });
                return;
            };
            let key_str = desc.key.as_key_str();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let result =
                    run_set_discovery_flag_effect(DiscoveryFlag::Pinned, &key_str, pinned).await;
                let _ = event_tx
                    .send(AppEvent::ConnectionPinSet {
                        conn,
                        pinned,
                        result,
                    })
                    .await;
            });
        }
        AppEffect::SetConnectionHidden { conn, hidden } => {
            let Some(desc) = descriptor_map.get(&conn) else {
                tracing::error!(conn = %conn.0, "SetConnectionHidden: no descriptor found for id");
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let _ = event_tx
                        .send(AppEvent::ConnectionHideSet {
                            conn,
                            hidden,
                            result: Err("connection descriptor not found".to_owned()),
                        })
                        .await;
                });
                return;
            };
            let key_str = desc.key.as_key_str();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let result =
                    run_set_discovery_flag_effect(DiscoveryFlag::Hidden, &key_str, hidden).await;
                let _ = event_tx
                    .send(AppEvent::ConnectionHideSet {
                        conn,
                        hidden,
                        result,
                    })
                    .await;
            });
        }
        AppEffect::SaveTheme { name } => {
            // Fire-and-forget persist of the live theme choice (Shift-T) to `[ui] theme`. No event: a
            // failure is logged inside the effect, and the in-memory theme already applied regardless.
            tokio::spawn(async move {
                let _ = run_save_theme_effect(&name).await;
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
        AppEffect::SniffFile { pane, conn, path } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ev = run_sniff_file_effect(&registry, pane, conn, path).await;
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::MountArchive { pane, conn, path } => {
            // Minted here, not by the reducer: `ConnectionId` allocation is the coordinator's job
            // (RFC-0011) everywhere else, and this is the runtime-side equivalent for the one kind
            // of connection that isn't enumerated at startup. See `ARCHIVE_CONN_ID_BASE`.
            let new_conn = ConnectionId(*next_archive_conn_id);
            *next_archive_conn_id += 1;
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ev = run_mount_archive_effect(&registry, pane, conn, path, new_conn).await;
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::OpenPager {
            id,
            conn,
            path,
            skip,
        } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            let cancel = CancellationToken::new();
            pager_controls.insert(id, cancel.clone());
            tokio::spawn(async move {
                run_pager_effect(registry, id, conn, path, skip, event_tx, cancel).await;
            });
        }
        AppEffect::ClosePager { id } => {
            if let Some(token) = pager_controls.remove(&id) {
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
        AppEffect::SaveConnection { profile, is_edit } => {
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ev = run_save_connection_effect(profile, is_edit).await;
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::DeleteConnection { id, secret_ref } => {
            let event_tx = event_tx.clone();
            let broker = vault_ctx.broker.clone();
            tokio::spawn(async move {
                let ev = run_delete_connection_effect(broker, id, secret_ref).await;
                let _ = event_tx.send(ev).await;
            });
        }
        // P5: provision vault credential then save the connection profile.
        AppEffect::ProvisionAndSaveConnection {
            profile,
            draft,
            is_edit,
        } => {
            let event_tx = event_tx.clone();
            let broker = vault_ctx.broker.clone();
            tokio::spawn(async move {
                let ev =
                    run_provision_and_save_connection_effect(broker, profile, draft, is_edit).await;
                let _ = event_tx.send(ev).await;
            });
        }
        // P5: detect OS credential sources at startup (env vars + file existence only).
        AppEffect::DetectOsSources => {
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let os_sources = run_detect_os_sources_effect().await;
                let _ = event_tx
                    .send(AppEvent::OsSourcesDetected { os_sources })
                    .await;
            });
        }
        AppEffect::DownloadForEdit {
            id,
            conn,
            path,
            v0,
            size,
            orig_perms,
        } => {
            // The temp dir + file are created here, synchronously (a handful of fast std::fs
            // calls — no different in cost from the `cancel`/`pause_tx` setup `AppEffect::Transfer`
            // does inline above), so this map — the sole owner of the RAII `TempDir` value for the
            // whole remote-edit session — always has an entry before any event referencing `id`
            // can arrive, no matter how the spawned download task below finishes.
            let leaf = path.file_name().unwrap_or("edited-file").to_owned();
            let outcome = new_remote_edit_dir().and_then(|dir| {
                let file = create_temp_edit_file(dir.path(), &leaf)?;
                Ok((dir, file))
            });
            let (dir, temp_path) = match outcome {
                Ok(pair) => pair,
                Err(e) => {
                    let event_tx = event_tx.clone();
                    tokio::spawn(async move {
                        let _ = event_tx
                            .send(AppEvent::RemoteEditFailed {
                                id,
                                status: format!("could not create a temp file: {e}"),
                            })
                            .await;
                    });
                    return;
                }
            };
            remote_edit_temps.insert(id, dir);
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ev = run_download_for_edit(
                    &registry, id, conn, path, v0, size, orig_perms, temp_path,
                )
                .await;
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::WriteBack {
            id,
            conn,
            path,
            temp_path,
            v0,
            orig_size,
            orig_perms,
            download_hash,
            mode,
        } => {
            let registry = registry.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ev = run_writeback(
                    &registry,
                    id,
                    conn,
                    path,
                    temp_path,
                    v0,
                    orig_size,
                    orig_perms,
                    download_hash,
                    mode,
                )
                .await;
                let _ = event_tx.send(ev).await;
            });
        }
        AppEffect::CancelRemoteEdit { id } => {
            // Synchronous, like `CancelTransfer`: dropping the map entry deletes the temp
            // directory (and the file within it) via `tempfile::TempDir`'s `Drop` impl.
            remote_edit_temps.remove(&id);
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

/// Bounded prefix read for file classification (`Action::Enter` on a non-directory entry). ~8 KiB
/// is enough for the NUL-byte heuristic ([`cairn_core::detect_file_kind`]) while keeping the
/// synchronous-feeling "Enter → pager" path fast even over a slow remote connection.
const SNIFF_PREFIX_BYTES: usize = 8 * 1024;

/// Read up to [`SNIFF_PREFIX_BYTES`] from the start of `path`, classify it, and report the result.
/// Runs off the render path (spawned by `dispatch`). Requests a ranged read as a hint (cheaper
/// over the wire for backends that support `Caps::RANDOM_READ`); backends that ignore the hint
/// and stream the whole file are still bounded by the `take` below. Never panics on
/// backend/user-reachable input — every I/O error becomes a redacted `AppEvent::SniffFailed`.
async fn run_sniff_file_effect(
    registry: &VfsRegistry,
    pane: Side,
    conn: ConnectionId,
    path: VfsPath,
) -> AppEvent {
    use tokio::io::AsyncReadExt;

    let Some(vfs) = registry.get(conn).await else {
        return AppEvent::SniffFailed {
            message: "connection unavailable".to_owned(),
        };
    };
    let range = Some(ByteRange {
        offset: 0,
        len: Some(SNIFF_PREFIX_BYTES as u64),
    });
    let reader = match vfs.open_read(&path, range).await {
        Ok(r) => r,
        Err(e) => {
            return AppEvent::SniffFailed {
                message: e.redacted().to_string(),
            };
        }
    };
    let mut buf = Vec::with_capacity(SNIFF_PREFIX_BYTES);
    if let Err(e) = reader
        .take(SNIFF_PREFIX_BYTES as u64)
        .read_to_end(&mut buf)
        .await
    {
        return AppEvent::SniffFailed {
            message: VfsError::Io(e).redacted().to_string(),
        };
    }
    let kind = cairn_core::detect_file_kind(&buf);
    AppEvent::FileSniffed {
        pane,
        conn,
        path,
        kind,
        prefetch: bytes::Bytes::from(buf),
    }
}

/// Mount `path` (already classified [`cairn_core::FileKind::Archive`] by the sniff) as a read-only
/// [`cairn_backend_archive::ArchiveVfs`] and register it as `new_conn`, or report a clean failure
/// (RFC-0013, `docs/adr/0012-archive-mount-model.md`).
///
/// `Vfs::local_path` is resolved first, off the render path: a `None` result (a remote backend, or
/// a path that doesn't resolve to a real local file) refuses cleanly — v1 requires the archive
/// itself to already be on a local pane; auto-staging a remote archive to a temp file is deferred
/// (see the RFC's "Deferred" section). Indexing (`ArchiveVfs::open`) is CPU/IO-bound sync work that
/// runs in its own `spawn_blocking` inside that call, so nothing here blocks the runtime either.
async fn run_mount_archive_effect(
    registry: &VfsRegistry,
    pane: Side,
    conn: ConnectionId,
    path: VfsPath,
    new_conn: ConnectionId,
) -> AppEvent {
    let Some(vfs) = registry.get(conn).await else {
        return AppEvent::ArchiveMountFailed {
            pane,
            message: "connection unavailable".to_owned(),
        };
    };
    let real_path = {
        let (vfs, path) = (vfs.clone(), path.clone());
        match tokio::task::spawn_blocking(move || vfs.local_path(&path)).await {
            Ok(Some(p)) => p,
            _ => {
                return AppEvent::ArchiveMountFailed {
                    pane,
                    message: "Copy the archive to a local pane to browse it".to_owned(),
                };
            }
        }
    };
    match open_archive(new_conn, real_path).await {
        Ok(archive_vfs) => {
            registry.insert(new_conn, archive_vfs).await;
            AppEvent::ArchiveMounted {
                pane,
                conn: new_conn,
                root: VfsPath::root(),
            }
        }
        Err(e) => AppEvent::ArchiveMountFailed {
            pane,
            message: e.redacted().to_string(),
        },
    }
}

/// Build the real [`cairn_backend_archive::ArchiveVfs`] (behind the `archive` feature).
#[cfg(feature = "archive")]
async fn open_archive(conn: ConnectionId, path: PathBuf) -> Result<Arc<dyn Vfs>, VfsError> {
    let vfs = cairn_backend_archive::ArchiveVfs::open(conn, path).await?;
    Ok(Arc::new(vfs))
}

/// Lean-build stand-in: reports "not built in" instead of failing to compile without the
/// `archive` feature. Mirrors `ConnectionOpener::open_docker`/`open_k8s`'s feature-gated pattern
/// in `crates/cairn/src/connect/mod.rs`.
#[cfg(not(feature = "archive"))]
async fn open_archive(_conn: ConnectionId, _path: PathBuf) -> Result<Arc<dyn Vfs>, VfsError> {
    Err(VfsError::Backend {
        code: "archive_not_built".to_owned(),
        msg: "archive support not built into this binary".to_owned(),
        retryable: false,
    })
}

/// Read buffer size for the pager stream (matches the transfer engine's chunk size).
const PAGER_CHUNK_BYTES: usize = 64 * 1024;

/// Stream `path`'s contents into the pager overlay `id`, forwarding each chunk as
/// [`AppEvent::PagerChunk`] until EOF (`PagerDone{error: None, truncated: false}`), the
/// [`CancellationToken`] fires (the pager closed, or the reducer itself hit
/// [`cairn_core::PAGER_MAX_BYTES`] and fired `AppEffect::ClosePager` — no terminal event needed in
/// that case, the reducer already marked the view `Truncated`), or an I/O error occurs (a
/// redacted `PagerDone`).
///
/// `skip` bytes are always discarded client-side after a plain `open_read(path, None)` rather
/// than re-deriving backend range-read support (already probed once by the sniff): this keeps the
/// resume logic correct even for backends that don't support `Caps::RANDOM_READ`, at the cost of a
/// tiny (≤ ~8 KiB) redundant read over the wire for remote backends.
async fn run_pager_effect(
    registry: VfsRegistry,
    id: PagerId,
    conn: ConnectionId,
    path: VfsPath,
    skip: u64,
    event_tx: mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
) {
    use tokio::io::AsyncReadExt;

    let Some(vfs) = registry.get(conn).await else {
        let _ = event_tx
            .send(AppEvent::PagerDone {
                id,
                error: Some("connection unavailable".to_owned()),
                truncated: false,
            })
            .await;
        return;
    };
    // Race the open against cancellation: opening a special file (a symlink pointing at a FIFO, say)
    // or a slow remote path can block inside `open_read` before the read loop's cancel check is ever
    // reached, so a closed pager could otherwise leave this task hung indefinitely.
    let opened = tokio::select! {
        () = cancel.cancelled() => return,
        r = vfs.open_read(&path, None) => r,
    };
    let mut reader = match opened {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx
                .send(AppEvent::PagerDone {
                    id,
                    error: Some(e.redacted().to_string()),
                    truncated: false,
                })
                .await;
            return;
        }
    };

    let mut to_skip = skip;
    // Independent safety cap: the reducer normally fires `ClosePager` once its own decoded
    // `PAGER_MAX_BYTES` budget is hit, but a superseded/orphaned stream (whose `ClosePager` raced or
    // whose chunks are dropped by the id guard) must still terminate on its own rather than read a
    // multi-GB file to EOF. Count forwarded (post-skip) bytes and stop at the same ceiling.
    let mut forwarded: u64 = 0;
    let mut buf = vec![0u8; PAGER_CHUNK_BYTES];
    loop {
        let n = tokio::select! {
            () = cancel.cancelled() => return,
            r = reader.read(&mut buf) => match r {
                Ok(n) => n,
                Err(e) => {
                    let _ = event_tx
                        .send(AppEvent::PagerDone {
                            id,
                            error: Some(VfsError::Io(e).redacted().to_string()),
                            truncated: false,
                        })
                        .await;
                    return;
                }
            },
        };
        if n == 0 {
            let _ = event_tx
                .send(AppEvent::PagerDone {
                    id,
                    error: None,
                    truncated: false,
                })
                .await;
            return;
        }
        let mut chunk = &buf[..n];
        if to_skip > 0 {
            let skip_now = to_skip.min(n as u64) as usize;
            chunk = &chunk[skip_now..];
            to_skip -= skip_now as u64;
        }
        if !chunk.is_empty() {
            let _ = event_tx
                .send(AppEvent::PagerChunk {
                    id,
                    bytes: bytes::Bytes::copy_from_slice(chunk),
                })
                .await;
            forwarded = forwarded.saturating_add(chunk.len() as u64);
            if forwarded >= cairn_core::PAGER_MAX_BYTES as u64 {
                let _ = event_tx
                    .send(AppEvent::PagerDone {
                        id,
                        error: None,
                        truncated: true,
                    })
                    .await;
                return;
            }
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

/// Unlock the secrets vault with `passphrase` and install it into the shared broker (P2).
///
/// Returns `Ok(())` on success or `Err(message)` with a secret-free, retryable reason (missing
/// vault / wrong passphrase). The passphrase is consumed here and zeroized on drop; it is never
/// logged. Connecting deferred profiles is no longer done here — in P2, NeedsVault connections are
/// opened lazily via [`AppEffect::OpenConnection`] after the vault is unlocked.
///
/// `Vault::open` runs Argon2id key derivation (CPU-bound) plus a file read, so it is offloaded to a
/// blocking thread to keep the async runtime — and the render path — responsive.
async fn run_vault_unlock_effect(
    broker: &Arc<Broker>,
    vault_path: Option<PathBuf>,
    passphrase: cairn_secrets::SecretString,
) -> Result<(), String> {
    let Some(path) = vault_path else {
        return Err("no vault path configured".to_owned());
    };
    // Open + decrypt off the async runtime: `Vault::open` runs Argon2id (CPU-bound) plus a file read,
    // and the existence check is itself a blocking `stat`, so *all* filesystem I/O is isolated here.
    // The owned `SecretString` lives in the closure and is zeroized when it returns.
    let vault = match tokio::task::spawn_blocking(move || {
        if !path.exists() {
            // The reducer only emits UnlockVault when vault_file_exists == true, so reaching
            // this branch means the file was removed after startup — report a clear error.
            return Err(
                "vault file not found — it may have been deleted since Cairn started".to_owned(),
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
    Ok(())
}

/// Create a new vault at `vault_path`, then immediately unlock the broker with it.
///
/// Called from the [`AppEffect::CreateVault`] handler in [`dispatch`]. Runs Argon2id key
/// derivation inside `spawn_blocking` so the async runtime and render path stay responsive.
///
/// Returns `(result, already_exists)` where `already_exists` is `true` only when the failure
/// was specifically `VaultError::AlreadyExists` (the vault appeared out-of-band after the app
/// started). The reducer uses this to flip `vault_file_exists = true` so the user can unlock
/// without restarting.
///
/// **Security invariants:**
/// - The passphrase is never logged or included in any error message.
/// - Errors returned to the reducer are secret-free and value-free (no path, no passphrase).
/// - The vault file is created with `0600` permissions (enforced by `atomic_create` in cairn-vault).
async fn run_create_vault_effect(
    broker: &Arc<Broker>,
    vault_path: Option<PathBuf>,
    passphrase: cairn_secrets::SecretString,
    remember: bool,
) -> (Result<(), String>, bool) {
    let Some(path) = vault_path else {
        return (Err("no vault path configured".to_owned()), false);
    };

    // Clone the broker Arc for the move closure.
    let broker = broker.clone();

    // Argon2id is CPU-bound (~100 ms). Blocking in the async runtime would stall renders.
    let result = tokio::task::spawn_blocking(move || -> (Result<(), String>, bool) {
        // Vault::create uses `atomic_create` (persist_noclobber) so a cross-process race cannot
        // silently overwrite an existing vault. VaultError::AlreadyExists is returned for both
        // the pre-flight check and any race-window collision.
        // The passphrase SecretString is consumed here and zeroized on drop.
        let vault_result = cairn_vault::Vault::create(&path, &passphrase);
        let already_exists = matches!(&vault_result, Err(cairn_vault::VaultError::AlreadyExists));
        match vault_result {
            Ok(vault) => {
                // Immediately unlock the broker so NeedsVault connections can open.
                broker.unlock(vault);

                // Keychain store: best-effort. A keychain failure does NOT roll back the vault —
                // the vault file is already persisted and unlocked. We log and continue.
                if remember {
                    #[cfg(feature = "keychain")]
                    {
                        // `UnlockProvider` and `KeychainUnlockProvider` are re-exported from the
                        // crate root (not the private `unlock` module) when `keychain` is enabled.
                        use cairn_vault::UnlockProvider as _;
                        let provider = cairn_vault::KeychainUnlockProvider::default();
                        if let Err(e) = provider.store(&passphrase) {
                            tracing::warn!("keychain store after vault creation failed: {e}");
                        }
                    }
                    #[cfg(not(feature = "keychain"))]
                    {
                        // Keychain feature not built; the `remember` flag is silently ignored.
                        tracing::debug!("keychain feature not built; skipping passphrase store");
                    }
                }
                (Ok(()), false)
            }
            Err(e) => (Err(e.to_string()), already_exists),
        }
    })
    .await;

    match result {
        Ok(pair) => pair,
        // spawn_blocking only panics if the closure panics; treat as an internal error.
        Err(_) => (Err("vault creation task failed".to_owned()), false),
    }
}

/// Open a connection on demand (P2/P3 lazy open) and insert it into the registry.
///
/// Called from the [`AppEffect::OpenConnection`] handler in [`dispatch`]. For `LocalRoot` targets
/// the VFS is constructed directly; for `Profile` targets the broker-backed opener is used; for
/// the P3 discovered targets (`DockerSocket`, `KubeconfigDefault`, `InCluster`) the relevant
/// backend constructors are used — feature-gated so the lean build falls back to a clear error.
/// Errors are returned as a secret-free string for the reducer to display in the status line.
async fn run_open_connection_effect(
    registry: &VfsRegistry,
    opener: &crate::connect::ConnectionOpener,
    conn: ConnectionId,
    desc: &ConnectionDescriptor,
) -> Result<(), String> {
    match &desc.target {
        OpenTarget::LocalRoot(path) => {
            // Defensive: the coordinator eagerly mounts all LocalRoot targets at startup, so
            // this arm is only reached if the registry entry was evicted or the initial mount
            // failed. Re-mount without error; the already-mounted guard in the caller handles
            // the common case where the entry is still live.
            let vfs = cairn_backend_local::LocalVfs::new(conn, path.clone());
            registry.insert(conn, Arc::new(vfs)).await;
            Ok(())
        }
        OpenTarget::Profile(profile) => {
            match opener.open(Actor::User, conn, profile).await {
                Ok(vfs) => {
                    registry.insert(conn, vfs).await;
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(
                        conn = %conn.0,
                        scheme = %profile.scheme,
                        name = %profile.display_name,
                        error = %e,
                        "lazy open failed"
                    );
                    // `OpenError`'s Display is already redacted: the Vfs variant delegates to
                    // `VfsError::redacted()`, and the Broker/BackendNotBuilt variants carry only
                    // safe category strings. Surface as-is; never include raw hostname or creds.
                    Err(format!("{}: {e}", profile.scheme))
                }
            }
        }
        // ── P3 discovered targets ──────────────────────────────────────────────────────────
        OpenTarget::DockerSocket { path } => open_docker_socket(registry, conn, path).await,
        OpenTarget::KubeconfigDefault => open_kubeconfig(registry, conn).await,
        OpenTarget::InCluster => open_incluster(registry, conn).await,
    }
}

/// Open a Docker VFS backed by the socket at `path` (or the platform default when `None`).
///
/// Feature-gated: the `docker` feature must be enabled. In lean builds this always returns an
/// error — the coordinator never routes a `DockerSocket` target to this function in lean mode
/// (the `DockerProvider` is absent), but the compiler still requires the arm to be present for
/// match exhaustiveness.
#[cfg(feature = "docker")]
async fn open_docker_socket(
    registry: &VfsRegistry,
    conn: ConnectionId,
    path: &Option<std::path::PathBuf>,
) -> Result<(), String> {
    let ops = match path.as_deref() {
        Some(p) => cairn_backend_docker::BollardDocker::connect_with_socket(p),
        None => cairn_backend_docker::BollardDocker::connect_local(),
    }
    .map_err(|e| format!("docker: {e}"))?;
    // Start the image-browse ephemeral-container reapers (ADR-0010) now, at real connection-open
    // time, rather than deferring to the first image browse — so the crash-safety sweep begins
    // reaping any orphaned containers from a prior crashed run as soon as this daemon is talked
    // to, not only if/when the user happens to enter `/images/<tag>`. Idempotent/cheap: harmless
    // to call even if the user never browses an image on this connection.
    ops.ensure_background_tasks().await;
    let vfs = cairn_backend_docker::DockerVfs::new(conn, ops);
    registry.insert(conn, Arc::new(vfs)).await;
    Ok(())
}

#[cfg(not(feature = "docker"))]
async fn open_docker_socket(
    _registry: &VfsRegistry,
    _conn: ConnectionId,
    _path: &Option<std::path::PathBuf>,
) -> Result<(), String> {
    Err("docker backend not built into this binary".to_owned())
}

/// Open a Kubernetes VFS backed by the user's kubeconfig.
///
/// Feature-gated: the `k8s` feature must be enabled.
#[cfg(feature = "k8s")]
async fn open_kubeconfig(registry: &VfsRegistry, conn: ConnectionId) -> Result<(), String> {
    let ops = cairn_backend_k8s::KubeRsOps::new();
    let vfs = cairn_backend_k8s::KubeVfs::new(conn, ops);
    registry.insert(conn, Arc::new(vfs)).await;
    Ok(())
}

#[cfg(not(feature = "k8s"))]
async fn open_kubeconfig(_registry: &VfsRegistry, _conn: ConnectionId) -> Result<(), String> {
    Err("k8s backend not built into this binary".to_owned())
}

/// Open a Kubernetes VFS using the pod's in-cluster service-account credentials.
///
/// Feature-gated: the `k8s` feature must be enabled.
#[cfg(feature = "k8s")]
async fn open_incluster(registry: &VfsRegistry, conn: ConnectionId) -> Result<(), String> {
    let ops = cairn_backend_k8s::KubeRsOps::new_incluster();
    let vfs = cairn_backend_k8s::KubeVfs::new(conn, ops);
    registry.insert(conn, Arc::new(vfs)).await;
    Ok(())
}

#[cfg(not(feature = "k8s"))]
async fn open_incluster(_registry: &VfsRegistry, _conn: ConnectionId) -> Result<(), String> {
    Err("k8s backend not built into this binary".to_owned())
}

/// Probe a connection's reachability without mounting it into any pane (RFC-0011 P6, "test
/// connection"). Called from the [`AppEffect::TestConnection`] handler in [`dispatch`].
///
/// Reuses the exact same per-scheme construction [`run_open_connection_effect`] uses for a real
/// open — the vetted path, no new credential handling — but never inserts the resulting `Vfs`
/// into the registry: the handle (if any) is dropped immediately on success, so a probe never
/// leaves a live backend connection running. Errors are the same redacted strings
/// `run_open_connection_effect` produces.
async fn run_test_connection_effect(
    opener: &crate::connect::ConnectionOpener,
    conn: ConnectionId,
    desc: &ConnectionDescriptor,
) -> Result<(), String> {
    match &desc.target {
        // Never actually dispatched in practice: the coordinator eager-mounts LocalRoot targets
        // as `ChoiceStatus::Ready`, and the reducer's `TestConnection` arm short-circuits `Ready`
        // entries without emitting this effect at all. Kept for match exhaustiveness and
        // defensive symmetry with `run_open_connection_effect`.
        OpenTarget::LocalRoot(_) => Ok(()),
        // The credential-bearing schemes: `opener.open` performs the real broker resolution +
        // network handshake, so a successful probe here is a genuine reachability signal. A
        // vault-locked profile is never reached — the reducer already reports "needs unlock"
        // for `ChoiceStatus::NeedsVault` without dispatching this effect.
        OpenTarget::Profile(profile) => match opener.open(Actor::User, conn, profile).await {
            Ok(_vfs) => Ok(()), // constructed (and authenticated) successfully; drop, don't mount
            Err(e) => Err(format!("{}: {e}", profile.scheme)),
        },
        OpenTarget::DockerSocket { path } => test_docker_socket(path).await,
        OpenTarget::KubeconfigDefault => test_kubeconfig().await,
        OpenTarget::InCluster => test_incluster().await,
    }
}

/// Probe Docker daemon reachability: construct the client, then `ping()` it — a real signal,
/// unlike `open_docker_socket`'s construct-only path. Deliberately skips
/// `ensure_background_tasks()`: starting the image-browse ephemeral-container reapers for a
/// connection that is about to be dropped would be pure waste.
#[cfg(feature = "docker")]
async fn test_docker_socket(path: &Option<std::path::PathBuf>) -> Result<(), String> {
    let ops = match path.as_deref() {
        Some(p) => cairn_backend_docker::BollardDocker::connect_with_socket(p),
        None => cairn_backend_docker::BollardDocker::connect_local(),
    }
    .map_err(|e| format!("docker: {e}"))?;
    ops.ping().await.map_err(|e| format!("docker: {e}"))
}

#[cfg(not(feature = "docker"))]
async fn test_docker_socket(_path: &Option<std::path::PathBuf>) -> Result<(), String> {
    Err("docker backend not built into this binary".to_owned())
}

/// Probe Kubernetes reachability via the default kubeconfig.
///
/// **Known limitation (RFC-0011 P6 follow-up):** `KubeRsOps::new()` is an infallible, I/O-free
/// constructor — building a `kube::Client` (and any exec-credential plugin it might run) happens
/// lazily on the first real API call, matching `open_kubeconfig`'s own construct-only behavior.
/// This probe therefore only confirms the feature is compiled in, not that the cluster is
/// actually reachable. A genuine network probe needs a specific context to call a cheap
/// version/list endpoint against — deferred pending `kube-staff-engineer` design input (the
/// switcher's "k8s" entry represents the *whole* kubeconfig, not one context).
#[cfg(feature = "k8s")]
async fn test_kubeconfig() -> Result<(), String> {
    let _ops = cairn_backend_k8s::KubeRsOps::new();
    Ok(())
}

#[cfg(not(feature = "k8s"))]
async fn test_kubeconfig() -> Result<(), String> {
    Err("k8s backend not built into this binary".to_owned())
}

/// Probe in-cluster Kubernetes reachability. Same known limitation as [`test_kubeconfig`].
#[cfg(feature = "k8s")]
async fn test_incluster() -> Result<(), String> {
    let _ops = cairn_backend_k8s::KubeRsOps::new_incluster();
    Ok(())
}

#[cfg(not(feature = "k8s"))]
async fn test_incluster() -> Result<(), String> {
    Err("k8s backend not built into this binary".to_owned())
}

/// Which `[discovery]` list a pin/hide toggle mutates (RFC-0011 P6). The two effects are
/// identical except which `Vec<String>` in `cairn.toml` they touch, so
/// [`run_set_discovery_flag_effect`] shares one implementation between them.
enum DiscoveryFlag {
    /// `[discovery].pinned`.
    Pinned,
    /// `[discovery].hidden`.
    Hidden,
}

/// Toggle `key_str`'s membership in `[discovery].pinned` or `[discovery].hidden` and persist the
/// config atomically (`Config::save` writes via temp-file + rename). Called from the
/// [`AppEffect::SetConnectionPinned`]/[`AppEffect::SetConnectionHidden`] handlers in [`dispatch`].
///
/// `key_str` is a stable `ConnectionKey` string
/// (e.g. `"builtin:/"`, `"saved:<uuid>"`, `"docker:socket:default"`, `"kube:kubeconfig"`) — never
/// a display label, so a later rename never orphans a pin/hide entry.
async fn run_set_discovery_flag_effect(
    flag: DiscoveryFlag,
    key_str: &str,
    set: bool,
) -> Result<(), String> {
    let Some(config_path) = cairn_config::default_config_path() else {
        return Err("cannot determine config file path".to_owned());
    };
    set_discovery_flag_at_path_locked(&config_path, flag, key_str, set).await
}

/// The locked core of [`run_set_discovery_flag_effect`]: acquires [`CONFIG_WRITE_LOCK`] for the
/// entire load→mutate→save cycle, then delegates to [`set_discovery_flag_at_path`]. Split out
/// (taking an explicit `&Path`) so a test can exercise the actual concurrency-safety guarantee —
/// two of these racing must never lose one's update — against a temp file instead of the real
/// platform config path.
async fn set_discovery_flag_at_path_locked(
    config_path: &Path,
    flag: DiscoveryFlag,
    key_str: &str,
    set: bool,
) -> Result<(), String> {
    let _guard = config_write_lock().await;
    set_discovery_flag_at_path(config_path, flag, key_str, set)
}

/// The testable core of [`run_set_discovery_flag_effect`], split out so unit tests can point it
/// at a temp file instead of the real platform config path (which `default_config_path()` always
/// resolves to and which a hermetic test must never touch).
fn set_discovery_flag_at_path(
    config_path: &Path,
    flag: DiscoveryFlag,
    key_str: &str,
    set: bool,
) -> Result<(), String> {
    let mut config = match cairn_config::Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load config for discovery-flag update");
            return Err(format!("failed to load config: {e}"));
        }
    };
    let list = match flag {
        DiscoveryFlag::Pinned => &mut config.discovery.pinned,
        DiscoveryFlag::Hidden => &mut config.discovery.hidden,
    };
    if set {
        if !list.iter().any(|k| k == key_str) {
            list.push(key_str.to_owned());
        }
    } else {
        list.retain(|k| k != key_str);
    }
    if let Err(e) = config.save(config_path) {
        tracing::warn!(error = %e, "failed to save config after discovery-flag update");
        return Err(format!("failed to save config: {e}"));
    }
    Ok(())
}

/// Persist the live color-theme choice to `[ui] theme` in the config file (so a Shift-T switch
/// survives a restart). Modeled on [`run_set_discovery_flag_effect`]: load → mutate → save under the
/// config write lock. Fire-and-forget — the caller ignores the result; a failure is logged here.
async fn run_save_theme_effect(name: &str) -> Result<(), String> {
    let Some(config_path) = cairn_config::default_config_path() else {
        return Err("cannot determine config file path".to_owned());
    };
    save_theme_at_path_locked(&config_path, name).await
}

/// Locked core of [`run_save_theme_effect`]: holds [`CONFIG_WRITE_LOCK`] for the whole
/// load→mutate→save cycle so rapid Shift-T presses (or a concurrent pin/hide save) can't lose an
/// update. Split out with an explicit `&Path` so a test can exercise it against a temp file.
async fn save_theme_at_path_locked(config_path: &Path, name: &str) -> Result<(), String> {
    let _guard = config_write_lock().await;
    save_theme_at_path(config_path, name)
}

/// Testable core of [`run_save_theme_effect`]: load the config, set `ui.theme`, save. Preserves every
/// other setting (keybindings, colors, connections, …) since it round-trips the whole config.
fn save_theme_at_path(config_path: &Path, name: &str) -> Result<(), String> {
    let mut config = match cairn_config::Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load config for theme save");
            return Err(format!("failed to load config: {e}"));
        }
    };
    config.ui.theme = name.to_owned();
    if let Err(e) = config.save(config_path) {
        tracing::warn!(error = %e, "failed to save config after theme change");
        return Err(format!("failed to save config: {e}"));
    }
    Ok(())
}

/// A compact "N file(s), M dir(s)" summary shared by the success and cancelled status messages.
fn outcome_summary(o: &cairn_transfer::TransferOutcome) -> String {
    format!("{} file(s), {} dir(s)", o.files, o.dirs)
}

/// Throughput in bytes/sec: `bytes` transferred over `secs` of (windowed) elapsed time. The elapsed
/// time is floored at a small epsilon so a near-instant window reports a sane number rather than an
/// absurd one-frame spike; the `f64`→`u64` cast saturates (never wraps/panics).
fn avg_rate(bytes: u64, secs: f64) -> u64 {
    let secs = secs.max(0.05);
    (bytes as f64 / secs) as u64
}

/// The trailing window, in milliseconds, over which transfer throughput is measured.
const RATE_WINDOW_MS: u64 = 3_000;

/// Near-instantaneous throughput from a sliding window of `(elapsed_ms, cumulative_bytes)` samples
/// (oldest first, monotonic). Appends the current sample, drops samples that fall entirely before
/// the window — keeping the newest one at/before the `now_ms - RATE_WINDOW_MS` edge as the anchor so
/// the window still spans ~`RATE_WINDOW_MS` between sparse updates — then returns recent-bytes over
/// recent-seconds. Pure (clock passed in as `now_ms`) so it is unit-testable without sleeps; a
/// lifetime average instead would stay depressed long after a slow stretch (e.g. contention from a
/// second concurrent transfer) ended.
fn windowed_rate(samples: &mut VecDeque<(u64, u64)>, now_ms: u64, bytes: u64) -> u64 {
    samples.push_back((now_ms, bytes));
    let cutoff = now_ms.saturating_sub(RATE_WINDOW_MS);
    while samples.len() > 2 && samples[1].0 <= cutoff {
        samples.pop_front();
    }
    let (t0, b0) = samples.front().copied().unwrap_or((0, 0));
    let secs = now_ms.saturating_sub(t0) as f64 / 1000.0;
    avg_rate(bytes.saturating_sub(b0), secs)
}

/// Recursively sum the byte size of `items` for the percentage/ETA total. `on_scan(bytes_so_far,
/// current_path)` is called for each node visited so the caller can surface a live "Counting" phase
/// (it throttles its own emission). Best-effort: any stat/list error returns `None` (degrades to a
/// byte+rate display). The walk is cancellable by the caller via `tokio::select!` on the outer future.
async fn scan_total_bytes(
    src: &Arc<dyn Vfs>,
    items: &[(VfsPath, VfsPath)],
    on_scan: &mut (dyn FnMut(u64, &str) + Send),
) -> Option<u64> {
    let mut total: u64 = 0;
    for (from, _) in items {
        let mut stack = vec![from.clone()];
        while let Some(path) = stack.pop() {
            on_scan(total, &path.as_str());
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
    // Pre-flight "Counting" phase emitter: the conflict pre-check and the size scan below both walk
    // paths before any byte moves, so they share one monotonic entry counter and a wall-clock
    // throttle. Each call reports the running entry count, bytes discovered so far, and the path being
    // visited, so the dialog shows the walk descending the tree in real time instead of a frozen 0%.
    let mut scan_entries = 0u64;
    let mut last_scan_emit: Option<std::time::Instant> = None;
    let mut on_scan = |bytes_found: u64, current: &str| {
        scan_entries += 1;
        let now = std::time::Instant::now();
        let due = last_scan_emit.is_none_or(|t| now.duration_since(t) >= TRANSFER_UI_MIN_INTERVAL);
        if due {
            last_scan_emit = Some(now);
            let _ = event_tx.try_send(AppEvent::TransferScanning {
                id,
                entries: scan_entries,
                bytes: bytes_found,
                current: current.to_owned(),
            });
        }
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
            on_scan(0, &to.as_str());
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
            t = scan_total_bytes(&src, &items, &mut on_scan) => t,
            () = cancel.cancelled() => None,
        }
    };
    // Emit coalesced, non-blocking progress: accumulate bytes and notify the UI at most every
    // `TRANSFER_PROGRESS_STEP` bytes via `try_send`, which drops the update if the bounded channel is
    // full rather than stalling the transfer task (the render path must never be blocked here). The
    // reported rate is a short trailing window (see below), so a pause needs no special accounting:
    // no progress arrives while paused and the render hides a paused transfer's rate. The one caveat
    // is a bounded artifact — for up to `RATE_WINDOW_MS` after *resuming* a long pause, the stale
    // pre-pause anchor makes the rate read low until it ages out (never high, since elapsed only
    // grows). A strict improvement over the old lifetime average, which could stay depressed forever.
    let started = std::time::Instant::now();
    // Sliding-window (near-instantaneous) throughput: recent bytes over recent wall-time. Reporting
    // a *lifetime* average (cumulative bytes ÷ total elapsed) means that after any slow stretch —
    // e.g. while a second concurrent transfer contended for the same disk/link — the number stays
    // dragged down long after real speed recovered, so the user sees it "stuck" at KB/s. A short
    // trailing window instead tracks current speed and recovers within `RATE_WINDOW_MS`. The reducer
    // can't do this (it's clock-free and pure), so the rate is measured here at the I/O edge.
    // `(elapsed_ms_since_started, cumulative_bytes)` samples for the trailing throughput window.
    let mut rate_samples: VecDeque<(u64, u64)> = VecDeque::from([(0u64, 0u64)]);
    // The most recent windowed rate, so the one-shot final flush below can read it without
    // re-borrowing `rate_samples` (which `on_progress` owns for the copy's duration).
    let last_rate = Arc::new(AtomicU64::new(0));

    let mut bytes = 0u64;
    let mut last_sent = 0u64;
    // Wall-clock of the last UI byte-update, so progress ticks on time too — not only every
    // `TRANSFER_PROGRESS_STEP` bytes. Without this a copy whose *total* is under one step (many tiny
    // files, or a slow trickle) sits at "0 B / … (0%)" until it completes.
    let mut last_sent_ms = 0u64;
    let mut on_progress = {
        let last_rate = last_rate.clone();
        move |ev: cairn_transfer::ProgressEvent| {
            // `Finalizing` marks the flush/verify tail of a file — no bytes, but it must reach the UI
            // immediately (unthrottled, once per file) so the bar reads an honest 100% + "Finalizing…"
            // instead of appearing stuck; it never shares the byte throttle counter.
            if matches!(ev, cairn_transfer::ProgressEvent::Finalizing) {
                let _ = event_tx.try_send(AppEvent::TransferProgress {
                    id,
                    bytes,
                    rate_bps: last_rate.load(Ordering::Relaxed),
                    total,
                    finalizing: true,
                });
                return;
            }
            let cairn_transfer::ProgressEvent::Bytes(b) = ev else {
                return;
            };
            bytes += b;
            debug_assert!(bytes >= last_sent, "progress bytes must be cumulative");
            // Update the window on *every* progress call (cheap, amortized O(1)) so `last_rate` is
            // always current — including for a transfer that completes in a single sub-step callback
            // (e.g. the same-connection server-copy fast path), which the final flush relies on.
            // Only the UI *event* is throttled to every `TRANSFER_PROGRESS_STEP` bytes.
            let now_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let rate = windowed_rate(&mut rate_samples, now_ms, bytes);
            last_rate.store(rate, Ordering::Relaxed);
            // Emit on either enough bytes *or* enough elapsed time, so both a fast big-file copy
            // (byte-driven) and a slow/many-small-file copy (time-driven) tick smoothly.
            if progress_emit_due(bytes, last_sent, now_ms, last_sent_ms) {
                last_sent = bytes;
                last_sent_ms = now_ms;
                let _ = event_tx.try_send(AppEvent::TransferProgress {
                    id,
                    bytes,
                    rate_bps: rate,
                    total,
                    finalizing: false,
                });
            }
        }
    };
    match cairn_transfer::run_transfer(&src, &dst, &items, spec, &cancel, &paused, &mut on_progress)
        .await
    {
        Ok(out) => {
            // Flush the exact final total for one frame before `TransferDone` clears the indicator
            // (so a transfer smaller than the coalescing step doesn't only ever show "0 B"). Reuse
            // the last windowed rate — kept current on every progress call above, so even a tiny
            // transfer (one sub-step callback) reports a real rate rather than flashing "0 B/s".
            let _ = event_tx.try_send(AppEvent::TransferProgress {
                id,
                bytes: out.bytes,
                rate_bps: last_rate.load(Ordering::Relaxed),
                total,
                finalizing: false,
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

/// Delete `paths` as a tracked operation: walk each tree at the app edge (list children → remove
/// files → remove dirs deepest-first), emitting coalesced per-item progress and honoring
/// cancellation between nodes. Returns a [`AppEvent::TransferDone`] (not `OpDone`) so the reducer's
/// transfer-completion path removes the op, closes the dialog, and re-lists both panes via
/// `finish_op`. Continue-on-error: a single un-deletable node counts as a failure and the walk goes
/// on, so one bad file never aborts the rest.
async fn run_delete_effect(
    registry: &VfsRegistry,
    id: TransferId,
    conn: ConnectionId,
    paths: Vec<VfsPath>,
    event_tx: &mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
) -> AppEvent {
    let Some(vfs) = registry.get(conn).await else {
        return AppEvent::TransferDone {
            id,
            status: "connection unavailable".to_owned(),
            error: true,
        };
    };
    let mut done = 0u64;
    let mut failures = 0u64;
    // Coalesced progress: emit the running item count + current path at most every
    // `TRANSFER_UI_MIN_INTERVAL`, best-effort (`try_send`) so a fast local delete can't flood the
    // render channel and a slow remote one still ticks.
    let mut last_emit: Option<std::time::Instant> = None;
    let mut emit = |done: u64, current: &str| {
        let now = std::time::Instant::now();
        if last_emit.is_none_or(|t| now.duration_since(t) >= TRANSFER_UI_MIN_INTERVAL) {
            last_emit = Some(now);
            let _ = event_tx.try_send(AppEvent::TransferScanning {
                id,
                entries: done,
                bytes: 0,
                current: current.to_owned(),
            });
        }
    };
    let cancelled = |done: u64| AppEvent::TransferDone {
        id,
        status: format!("Delete cancelled ({done} removed)"),
        error: false,
    };

    // DFS stack of (path, is_dir). Files are removed as they're popped; directories are recorded and
    // removed deepest-first afterward (they're empty by then). Root kinds come from a stat; child
    // kinds come from their parent's listing, so there's no extra stat per child.
    let mut stack: Vec<(VfsPath, bool)> = Vec::new();
    for root in &paths {
        match vfs.stat(root).await {
            Ok(meta) => stack.push((root.clone(), meta.is_dir())),
            Err(VfsError::NotFound(_)) => {}
            Err(_) => failures += 1,
        }
    }
    let mut dirs_post: Vec<VfsPath> = Vec::new();
    while let Some((p, is_dir)) = stack.pop() {
        if cancel.is_cancelled() {
            return cancelled(done);
        }
        if is_dir {
            dirs_post.push(p.clone());
            let mut stream = vfs.list(&p, ListOpts { all: true });
            loop {
                let page = tokio::select! {
                    pg = stream.next() => pg,
                    () = cancel.cancelled() => return cancelled(done),
                };
                let Some(page) = page else { break };
                match page {
                    Ok(pg) => {
                        for e in pg.entries {
                            if let Ok(child) = p.join(&e.name) {
                                stack.push((child, e.is_dir()));
                            }
                        }
                        // Tick during enumeration too, so listing a huge/remote directory (which can
                        // take seconds) still shows motion instead of the count sitting still.
                        emit(done, &p.as_str());
                    }
                    Err(_) => {
                        failures += 1;
                        break;
                    }
                }
            }
        } else {
            match vfs.remove(&p, Recurse::No).await {
                Ok(()) => done += 1,
                Err(VfsError::NotFound(_)) => {}
                Err(_) => failures += 1,
            }
            emit(done, &p.as_str());
        }
    }
    // Remove directories deepest-first (reverse discovery order → children before parents).
    for d in dirs_post.iter().rev() {
        if cancel.is_cancelled() {
            return cancelled(done);
        }
        match vfs.remove(d, Recurse::No).await {
            Ok(()) => done += 1,
            Err(VfsError::NotFound(_)) => {}
            Err(_) => failures += 1,
        }
        emit(done, &d.as_str());
    }

    let status = if failures > 0 {
        format!("Deleted {done} item(s), {failures} failed")
    } else {
        format!("Deleted {done} item(s)")
    };
    AppEvent::TransferDone {
        id,
        status,
        error: failures > 0,
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
///
/// `gate` lets the RFC-0012 P2 editor-suspend path pause this thread (so it never steals
/// keystrokes meant for an external editor holding the real TTY) and resume it afterward. The gate
/// is checked once per loop iteration, strictly *between* a `poll`/`read` pair and the next
/// `poll` — never mid-`read()` — so a pause request can't interrupt an in-flight read.
fn spawn_input_reader(tx: mpsc::Sender<Event>, gate: Arc<InputGate>) {
    std::thread::spawn(move || {
        loop {
            gate.reader_tick();
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
        }
        // The reader is exiting: mark the gate `Dead` so a concurrent/subsequent `request_pause`
        // (from an edit) bails instead of blocking forever on an ack that will never arrive.
        gate.mark_dead();
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

// ─────────────────────────────────────────────────────────────────────────────────────────────
// RFC-0012 P2: terminal suspend + external-editor launch
//
// See `docs/adr/0011-terminal-suspend-and-editor-launch.md` for the full design rationale.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// The blocking input-reader thread's pause/resume state, coordinated with the async
/// editor-suspend path via a [`Condvar`]. Three states rather than a plain bool so the pausing
/// side can block until the reader has actually *acknowledged* the request (rather than racing a
/// `poll`/`read` that was already in flight): `PauseRequested` is the ask, `Paused` is the ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderState {
    Running,
    PauseRequested,
    Paused,
    /// The reader thread has exited (a `poll`/`read` error, or the event channel closed). It will
    /// never acknowledge a pause again, so `request_pause` must observe this and bail rather than
    /// wait for an ack that can never come.
    Dead,
}

/// Shared handle used to pause the blocking input-reader thread (`spawn_input_reader`) while an
/// external editor owns the real TTY, and to resume it afterward.
///
/// `std::sync::Mutex`/`Condvar` (not tokio's) are correct here: the reader is a real OS thread
/// doing blocking I/O (`crossterm::event::poll`/`read`), not a tokio task, so it must synchronize
/// with a real OS-level primitive. The pausing side (`request_pause`) performs a genuine blocking
/// wait and must be run inside `tokio::task::spawn_blocking` by its caller — never awaited
/// directly on a tokio worker thread.
struct InputGate {
    state: Mutex<ReaderState>,
    cv: Condvar,
}

impl InputGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ReaderState::Running),
            cv: Condvar::new(),
        })
    }

    /// Recover a poisoned lock rather than propagating the panic: `ReaderState` is plain data with
    /// no invariants that a panicking holder could have left inconsistent, so continuing with
    /// whatever value is there is safe and preferable to taking down the whole app over a
    /// (currently unreachable) panic elsewhere while the lock was held.
    fn lock(&self) -> std::sync::MutexGuard<'_, ReaderState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Ask the reader thread to pause, and **block the calling OS thread** until it acknowledges
    /// (flips to `Paused`). Must be called from inside `tokio::task::spawn_blocking` — this is a
    /// real `Condvar::wait`, not an async operation. Returns `Err(())` if the reader thread has
    /// already died (state `Dead`): it will never ack, so the caller must not wait forever.
    fn request_pause(&self) -> Result<(), ()> {
        let mut guard = self.lock();
        if *guard == ReaderState::Dead {
            return Err(());
        }
        *guard = ReaderState::PauseRequested;
        self.cv.notify_all();
        loop {
            match *guard {
                ReaderState::Paused => return Ok(()),
                ReaderState::Dead => return Err(()),
                _ => {
                    guard = self
                        .cv
                        .wait(guard)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
        }
    }

    /// Flip back to `Running` and wake the parked reader thread. Cheap and non-blocking; safe to
    /// call from async code directly. Never resurrects a `Dead` reader.
    fn resume(&self) {
        let mut guard = self.lock();
        if *guard != ReaderState::Dead {
            *guard = ReaderState::Running;
        }
        self.cv.notify_all();
    }

    /// Called by the reader thread when it exits (poll/read error, or the event channel closed), so
    /// a subsequent `request_pause` bails instead of waiting for an ack that will never come.
    fn mark_dead(&self) {
        let mut guard = self.lock();
        *guard = ReaderState::Dead;
        self.cv.notify_all();
    }

    /// Called by the reader thread at the top of each loop iteration (after the previous
    /// `poll`/`read`, before the next `poll`). A no-op unless a pause is pending; otherwise acks
    /// the request and parks (zero stdin access, zero CPU) until `resume()` flips it back.
    fn reader_tick(&self) {
        let mut guard = self.lock();
        if *guard == ReaderState::PauseRequested {
            *guard = ReaderState::Paused;
            self.cv.notify_all();
            while *guard == ReaderState::Paused {
                guard = self
                    .cv
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }
}

/// RAII guard that restores the TUI's terminal mode (raw + alternate screen) and unpauses the
/// input reader if dropped while still armed — i.e. if `run_editor_suspend` panics or returns
/// early anywhere between suspending the terminal and performing its own explicit resume. Without
/// this, such a path would leave the reader thread parked forever (the whole app going deaf to
/// input — a hang, not just a cosmetic issue) and/or the terminal in the wrong mode for Cairn's
/// own subsequent `terminal.draw()` calls.
///
/// Constructed *after* the terminal has been suspended (so its Drop path mirrors "resume"), and
/// disarmed only after `run_editor_suspend` has performed the resume itself. The re-entry this
/// performs (`enable_raw_mode`/`EnterAlternateScreen`) is best-effort and does not call
/// `Terminal::clear` (Drop has no `&mut Terminal` to work with) — acceptable because this path is
/// a safety net for an exceptional early exit, not the normal-completion path (which does call
/// `clear()`). On an actual panic, [`install_terminal_panic_hook`] restores the terminal to normal
/// mode *before* unwinding begins (panic hooks run pre-unwind) and prints the panic message there;
/// this guard's Drop running afterward during unwind is a deliberately accepted, pre-existing
/// class of risk for any raw-mode TUI that suspends around a child process — always resuming the
/// input reader (preventing a permanent hang) is the invariant that must never be skipped, and is
/// unconditional here regardless of the terminal-mode outcome.
struct EditorRestoreGuard<'a> {
    input_gate: &'a Arc<InputGate>,
    armed: bool,
}

impl Drop for EditorRestoreGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = enable_raw_mode();
            let _ = execute!(std::io::stdout(), EnterAlternateScreen);
            self.input_gate.resume();
        }
    }
}

/// Environment variables always forwarded to the editor child (never secrets/vault material). Both
/// broader than `spawn_shell_action`'s allow-list (an interactive editor needs `TERM`/`SHELL`/XDG
/// dirs to render and behave correctly) and just as strict about excluding everything else —
/// `AWS_*`/`GOOGLE_*`/`AZURE_*`/`GITHUB_TOKEN`/`VAULT_*`/`CAIRN_*`/`SSH_AUTH_SOCK`/`LD_PRELOAD`/
/// `LD_LIBRARY_PATH`/`DYLD_*` are all dropped by `env_clear()` and never re-added. `LC_*` is
/// handled separately below (prefix match, like `spawn_shell_action`).
const EDITOR_ENV_ALLOWLIST: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TZ",
    "TMPDIR",
    "TERM",
    "COLORTERM",
    "TERMINFO",
    "SHELL",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
];

/// Resolve the editor command string: `$VISUAL` → `$EDITOR` → `vi` (Unix). On Windows, if neither
/// is set, this is a hard refusal (never guess `notepad`) with a clear, actionable message.
fn resolve_editor_command() -> Result<String, String> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
    }
    if cfg!(windows) {
        Err("No editor configured — set $EDITOR (or $VISUAL) and try again".to_owned())
    } else {
        Ok("vi".to_owned())
    }
}

/// Split the resolved editor command into `(program, fixed_args)` via **POSIX shell-word quoting
/// only** (`shlex`) — deterministic, and never glob/variable/command-substitution expansion. E.g.
/// `"code --wait"` → `("code", ["--wait"])`; `emacs -nw` → `("emacs", ["-nw"])`; a value containing
/// `$(...)` or `*` is passed through as inert literal text, never interpreted.
fn resolve_editor_argv() -> Result<(String, Vec<String>), String> {
    let cmd = resolve_editor_command()?;
    let mut argv =
        shlex::split(&cmd).ok_or_else(|| "$EDITOR/$VISUAL has unmatched quoting".to_owned())?;
    if argv.is_empty() {
        return Err("$EDITOR/$VISUAL is empty".to_owned());
    }
    let program = argv.remove(0);
    Ok((program, argv))
}

/// Spawn the editor as a hardened child: **argv only, never a shell** (`Command::new(program)`,
/// never `sh -c`), a `--` terminator before the (always-absolute) file path so a filename like
/// `-c :!sh` is treated as a plain argument rather than a flag (Unix only — see below), a scrubbed
/// environment (`env_clear()` + [`EDITOR_ENV_ALLOWLIST`] + a sanitized `PATH`), and the file's
/// parent directory as `cwd`. Unlike `spawn_shell_action`, stdio is **inherited** (the editor needs
/// the real TTY) and there is **no timeout** (editing is open-ended, interactive, foreground work).
///
/// Crucially — and unlike `spawn_shell_action` — the editor is **not** put in its own process group
/// (`process_group(0)`). That helper can safely reparent because its child never touches the TTY
/// (stdin is `/dev/null`, stdout/stderr piped). A full-screen editor *reads* the inherited
/// controlling terminal, and a process in a **background** process group that reads the TTY is sent
/// `SIGTTIN` (default: stop) — so reparenting would freeze the editor (and Cairn, parked in
/// `wait().await`) on its first keystroke. The editor therefore inherits Cairn's process group,
/// which is the terminal's foreground group, and reads without `SIGTTIN`. Interactive editors put
/// the terminal into their own raw mode (ISIG off) so a Ctrl-C during editing is delivered to the
/// editor as a keystroke, not as a `SIGINT` to the shared group; the only residual exposure is the
/// sub-millisecond cooked-mode windows around spawn/exit, an accepted trade-off shared by every
/// program that shells out to `$EDITOR` (e.g. `git`).
async fn spawn_editor_hardened(
    program: &str,
    fixed_args: &[String],
    abs_path: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<std::process::ExitStatus, String> {
    let mut std_cmd = std::process::Command::new(program);
    std_cmd.args(fixed_args);
    // `--` end-of-options terminator: Unix editors (vi/vim/nano/emacs, `code --wait`) honor it, so a
    // file named `-c :!sh` can't be read as a flag. Skipped on Windows — plain GUI editors like
    // `notepad.exe` don't recognize `--` and would try to open a file literally named `--`; the file
    // path is always absolute (`C:\…`) there anyway, so it can never be mistaken for a flag.
    #[cfg(unix)]
    std_cmd.arg("--");
    std_cmd
        .arg(abs_path)
        .current_dir(cwd)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    std_cmd.env_clear();
    if let Some(path) = sanitized_path() {
        std_cmd.env("PATH", path);
    }
    for key in EDITOR_ENV_ALLOWLIST {
        if let Some(v) = std::env::var_os(key) {
            std_cmd.env(key, v);
        }
    }
    for (k, v) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("LC_") {
            std_cmd.env(k, v);
        }
    }

    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start editor '{program}': {e}"))?;
    child
        .wait()
        .await
        .map_err(|e| format!("editor wait failed: {e}"))
}

/// Format a child [`std::process::ExitStatus`] as a short, secret-free description (exit code, or
/// the terminating signal on Unix).
fn describe_exit_status(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("killed by signal {sig}");
        }
    }
    "terminated abnormally".to_owned()
}

/// Send an `AppEvent` without blocking the caller on a full channel. [`run_editor_suspend`] runs
/// *inline on the event-loop task*, which is the sole consumer of `event_rx`; while it runs (for the
/// whole open-ended duration of an edit) nothing drains the channel. An awaited `send` on a
/// momentarily-full bounded channel would therefore deadlock permanently — the only task that could
/// free capacity is the one parked on the send. Spawning the send lets `run_editor_suspend` return;
/// the event loop then resumes, drains, and this detached send completes.
fn send_event_detached(event_tx: &mpsc::Sender<AppEvent>, ev: AppEvent) {
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(ev).await;
    });
}

/// Pause the input reader, suspend the terminal, run the already-resolved editor on `target` (with
/// `cwd` as its working directory), then resume the terminal and un-pause input.
///
/// Shared by [`run_editor_suspend`] (RFC-0012 P2: `target` is a resolved local path) and
/// [`run_remote_edit_temp`] (RFC-0012 P3: `target` is an already-local temp file) — the two differ
/// only in how `target`/`cwd` were obtained and in what the caller does with the outcome
/// afterward. Steps and rationale are unchanged from the original P2 implementation; see
/// `docs/adr/0011-terminal-suspend-and-editor-launch.md`.
async fn suspend_and_run_editor(
    terminal: &mut ratatui::DefaultTerminal,
    input_gate: &Arc<InputGate>,
    program: &str,
    fixed_args: &[String],
    target: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<std::process::ExitStatus, String> {
    // 1. Pause the blocking input-reader thread and wait for its ack. `request_pause` performs a
    // real OS-level `Condvar::wait`, so it must run inside `spawn_blocking`, not directly `.await`ed.
    {
        let gate = input_gate.clone();
        let paused = tokio::task::spawn_blocking(move || gate.request_pause()).await;
        // `Ok(Ok(()))` = the reader acknowledged the pause. `Ok(Err(()))` = the reader thread has
        // died (e.g. the controlling terminal dropped), so it will never ack — bail rather than
        // wait forever. `Err(_)` = the blocking task panicked (has no panicking path, but handle it).
        // In every non-ack case we have NOT touched the terminal yet, so fail cleanly.
        if !matches!(paused, Ok(Ok(()))) {
            return Err("cannot edit: input is unavailable".to_owned());
        }
    }

    // 2. Suspend: leave raw mode + the alternate screen. Deliberately NOT `ratatui::init()` again
    // on resume (see the ADR) — `init()` re-installs a panic hook and stacks a new closure on
    // every call; `restore()` here is the suspend half, the manual re-init below is the resume half.
    ratatui::restore();

    // RAII guard: if anything below panics or returns early before the explicit resume, this
    // still re-enters raw mode/alt-screen and unpauses the reader rather than leaving the app
    // permanently deaf to input.
    let mut guard = EditorRestoreGuard {
        input_gate,
        armed: true,
    };

    // 3. Spawn the editor and await it in the foreground — the documented exception to "the
    // render path never blocks": this specific await is the entire point of suspending.
    let outcome = spawn_editor_hardened(program, fixed_args, target, cwd).await;

    // 4. Resume: manual re-init (not `ratatui::init()`) + a full non-diffed repaint, since the
    // terminal size may have changed and the editor's own screen is sitting in ratatui's diff
    // buffer's blind spot.
    if let Err(e) = enable_raw_mode() {
        tracing::error!(error = %e, "failed to re-enable raw mode after editor exit");
    }
    if let Err(e) = execute!(std::io::stdout(), EnterAlternateScreen) {
        tracing::error!(error = %e, "failed to re-enter the alternate screen after editor exit");
    }
    if let Err(e) = terminal.clear() {
        tracing::error!(error = %e, "failed to repaint the terminal after editor exit");
    }
    input_gate.resume();
    guard.armed = false; // resume already performed above; disarm so Drop is a no-op

    outcome
}

/// Handle [`AppEffect::SuspendAndEdit`] — the one effect not routed through [`dispatch`] because it
/// needs exclusive terminal + stdin ownership. Resolves the local path *before* touching the
/// terminal (so a refusal or a hand-off to the RFC-0012 P3 remote-edit flow never disturbs the
/// TUI), then suspends the terminal, spawns the hardened editor and awaits it (foreground — the
/// deliberate, documented exception to "the render path never blocks": the whole point is that the
/// editor exclusively owns the TTY while it runs), manually resumes the terminal with a full
/// non-diffed repaint, and reports the outcome.
async fn run_editor_suspend(
    terminal: &mut ratatui::DefaultTerminal,
    input_gate: &Arc<InputGate>,
    registry: &VfsRegistry,
    event_tx: &mpsc::Sender<AppEvent>,
    conn: ConnectionId,
    path: VfsPath,
) {
    // 1. Resolve the real, local OS path *before* touching the terminal at all. `local_path` does
    // a blocking `canonicalize`, so it runs off the async runtime.
    let Some(vfs) = registry.get(conn).await else {
        send_event_detached(
            event_tx,
            AppEvent::EditFinished {
                status: "connection unavailable".to_owned(),
                error: true,
            },
        );
        return;
    };
    let real = {
        let (vfs2, path2) = (vfs.clone(), path.clone());
        match tokio::task::spawn_blocking(move || vfs2.local_path(&path2)).await {
            Ok(Some(p)) => p,
            // `None` covers "remote backend" *and* a local path that won't resolve (a dangling
            // symlink, or one that escapes the confined root) — see `Vfs::local_path`'s doc. Both
            // cases are handed to the RFC-0012 P3 remote-edit flow (download to a temp file, edit
            // that, write back) rather than refused outright: it works correctly for a genuinely
            // remote backend, and also gracefully covers the local-but-unresolvable-symlink case P2
            // used to refuse, since the P3 path never needs `local_path` to succeed — only
            // `stat`/`open_read`/`open_write`. Neither case has touched the terminal.
            _ => {
                begin_remote_edit(&vfs, conn, path, event_tx).await;
                return;
            }
        }
    };
    let name = real
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cwd = real
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| real.clone());

    // Resolve $VISUAL/$EDITOR/vi and split it *before* pausing input / suspending the terminal, so
    // a misconfigured editor var is reported without ever touching the TTY either.
    let (program, fixed_args) = match resolve_editor_argv() {
        Ok(pair) => pair,
        Err(message) => {
            send_event_detached(
                event_tx,
                AppEvent::EditFinished {
                    status: message,
                    error: true,
                },
            );
            return;
        }
    };

    let outcome =
        suspend_and_run_editor(terminal, input_gate, &program, &fixed_args, &real, &cwd).await;

    let (status, error) = match outcome {
        Ok(status) if status.success() => (format!("edited {name}"), false),
        Ok(status) => (
            format!("editor exited: {}", describe_exit_status(&status)),
            true,
        ),
        Err(message) => (message, true),
    };
    send_event_detached(event_tx, AppEvent::EditFinished { status, error });
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// RFC-0012 P3: remote file edit — download / conflict-check / write-back
//
// Extends P2's local-only `$EDITOR` shell-out to any backend whose `Vfs::local_path` is `None`:
// download the remote file to a private local temp copy, edit that copy through the exact same
// terminal-suspend machinery P2 uses, then (if it changed) re-check the remote version and write
// the edited copy back. See `docs/rfcs/0012-file-open-view-edit.md`'s P3 section for the full
// design and `docs/adr/0011-terminal-suspend-and-editor-launch.md` for the terminal model this
// reuses. Security review: temp files hold plaintext remote content (see `new_remote_edit_dir`).
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Called from [`run_editor_suspend`] before the terminal is touched, once `Vfs::local_path` has
/// resolved to `None`. Performs every cheap, pre-flight check that can fail *before* committing to
/// a download: the backend must be writable (no point downloading from a read-only mount like a
/// mounted archive), `stat` must succeed and report a size within [`REMOTE_EDIT_MAX_BYTES`], and
/// `$VISUAL`/`$EDITOR`/`vi` must resolve. Any failure here reports the existing
/// [`AppEvent::EditFinished`] (mirrors P2's pre-terminal refusal path) — no [`RemoteEditId`] has
/// been minted yet, so there is nothing for the runtime to track or clean up.
///
/// On success, emits [`AppEvent::RemoteEditNeedsDownload`] so the reducer mints a
/// [`RemoteEditId`] and kicks off the download as a normal (spawned, non-blocking) effect.
async fn begin_remote_edit(
    vfs: &Arc<dyn Vfs>,
    conn: ConnectionId,
    path: VfsPath,
    event_tx: &mpsc::Sender<AppEvent>,
) {
    // `caps_at`, not the backend-wide `caps()`: some backends (Docker/K8s) refine capabilities per
    // path/depth, so the write-check must be scoped to the file actually being edited.
    if !vfs.caps_at(&path).contains(Caps::WRITE) {
        send_event_detached(
            event_tx,
            AppEvent::EditFinished {
                status: "This location is read-only — cannot edit".to_owned(),
                error: true,
            },
        );
        return;
    }
    let entry = match vfs.stat(&path).await {
        Ok(e) => e,
        Err(e) => {
            send_event_detached(
                event_tx,
                AppEvent::EditFinished {
                    status: format!("Cannot edit: {}", e.redacted()),
                    error: true,
                },
            );
            return;
        }
    };
    let Some(size) = entry.size else {
        send_event_detached(
            event_tx,
            AppEvent::EditFinished {
                status: "Cannot edit: this backend does not report a size for this file".to_owned(),
                error: true,
            },
        );
        return;
    };
    if size > REMOTE_EDIT_MAX_BYTES {
        send_event_detached(
            event_tx,
            AppEvent::EditFinished {
                status: format!(
                    "Cannot edit: {size} bytes exceeds the {REMOTE_EDIT_MAX_BYTES}-byte edit cap \
                     — copy it to a local pane to edit a file this large"
                ),
                error: true,
            },
        );
        return;
    }
    // Fail fast on a misconfigured editor before spending a (potentially large) download on it —
    // re-checked again in `run_remote_edit_temp` immediately before the editor actually launches
    // (cheap, deterministic, and consistent with `run_shell_action_effect`'s "defensive
    // re-validation" convention elsewhere in this file).
    if let Err(message) = resolve_editor_argv() {
        send_event_detached(
            event_tx,
            AppEvent::EditFinished {
                status: message,
                error: true,
            },
        );
        return;
    }
    let v0 = RemoteVersion::from_entry(&entry);
    let orig_perms = entry.perms;
    send_event_detached(
        event_tx,
        AppEvent::RemoteEditNeedsDownload {
            conn,
            path,
            v0,
            size,
            orig_perms,
        },
    );
}

/// Pick the private, per-user base directory for a remote-edit temp file: `$XDG_RUNTIME_DIR`
/// (per-user, usually tmpfs, torn down at logout) when it's set and really is a directory, else
/// `None` (the caller falls back to the OS temp dir). Mirrors
/// `cairn_backend_archive::compressed_tar::preferred_temp_dir` — kept as an independent copy here
/// (rather than a shared crate) since `cairn` does not otherwise depend on `cairn-backend-archive`
/// and this is a two-line, dependency-free check.
fn preferred_temp_base_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let path = PathBuf::from(dir);
    path.is_dir().then_some(path)
}

/// Create a fresh, private, per-edit temp directory: a non-predictable subdirectory (`tempfile`'s
/// own randomized naming) under [`preferred_temp_base_dir`] or the OS temp dir, forced to `0700` on
/// Unix regardless of the process umask. Scoping the whole remote-edit download to its own
/// directory (rather than dropping a single temp file directly into a shared, multi-tenant
/// `$XDG_RUNTIME_DIR`/`/tmp`) means the [`LocalVfs`] built over it in [`run_download_for_edit`] can
/// only ever address the one file this session creates — no sibling in that shared directory is
/// reachable through it.
fn new_remote_edit_dir() -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".cairn-edit-");
    let dir = match preferred_temp_base_dir() {
        Some(base) => builder.tempdir_in(base)?,
        None => builder.tempdir()?,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Create the temp file itself inside `dir`, preserving the original leaf name (so `$EDITOR` still
/// sees the right extension for syntax highlighting) — safe here specifically because `dir` is
/// already a freshly-minted, non-predictable, private (`0700`) directory containing nothing else
/// (see [`new_remote_edit_dir`]), so a predictable *filename* within it is not a predictable *path*.
/// `create_new` (⇒ `O_EXCL` on Unix) still guards against a same-name collision; mode `0600` is set
/// explicitly rather than relying on umask.
fn create_temp_edit_file(dir: &std::path::Path, leaf: &str) -> std::io::Result<PathBuf> {
    // Defensive: `leaf` comes from `VfsPath::file_name()`, which is already a single path segment
    // with no separators, but this keeps the guarantee local rather than trusting a caller forever.
    let safe_leaf = leaf.rsplit(['/', '\\']).next().filter(|s| !s.is_empty());
    let path = dir.join(safe_leaf.unwrap_or("edited-file"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).read(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&path)?;
    Ok(path)
}

/// SHA-256 of `path`'s current content, computed off the async runtime (`spawn_blocking`) since
/// hashing is CPU/IO-bound sync work. Used purely as a local before/after diff to detect a no-op
/// edit — never a cross-system integrity check.
async fn hash_file(path: &std::path::Path) -> std::io::Result<ContentHash> {
    use sha2::{Digest, Sha256};
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<ContentHash> {
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(hasher.finalize().into())
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// The runtime task body for [`AppEffect::DownloadForEdit`]. `temp_path` was already created (empty,
/// `0600`) synchronously in `dispatch` before this was spawned. Downloads `path` into it via the
/// transfer engine (remote `Vfs` → a [`LocalVfs`] rooted at the temp file's parent directory),
/// preserving the pre-created file's permissions (the write opens with `overwrite: true`, which
/// truncates the existing inode rather than recreating it). On success, hashes the result and
/// reports [`AppEvent::RemoteEditDownloaded`]; on any failure, reports
/// [`AppEvent::RemoteEditFailed`] (the caller in `dispatch` cleans up the temp directory for `id`
/// when that event arrives).
#[allow(clippy::too_many_arguments)]
async fn run_download_for_edit(
    registry: &VfsRegistry,
    id: RemoteEditId,
    conn: ConnectionId,
    path: VfsPath,
    v0: RemoteVersion,
    size: u64,
    orig_perms: Option<UnixPerms>,
    temp_path: PathBuf,
) -> AppEvent {
    let Some(src) = registry.get(conn).await else {
        return AppEvent::RemoteEditFailed {
            id,
            status: "connection unavailable".to_owned(),
        };
    };
    let Some(temp_dir) = temp_path.parent() else {
        return AppEvent::RemoteEditFailed {
            id,
            status: "internal error: temp file has no parent directory".to_owned(),
        };
    };
    let dst: Arc<dyn Vfs> = Arc::new(LocalVfs::new(REMOTE_EDIT_TEMP_CONN_ID, temp_dir));
    let leaf = match temp_path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => {
            return AppEvent::RemoteEditFailed {
                id,
                status: "internal error: temp file name is not valid UTF-8".to_owned(),
            };
        }
    };
    let dst_path = match VfsPath::root().join(leaf) {
        Ok(p) => p,
        Err(_) => {
            return AppEvent::RemoteEditFailed {
                id,
                status: "internal error: temp file name is not a valid path segment".to_owned(),
            };
        }
    };
    let spec = TransferSpec {
        op: TransferOp::Copy,
        conflict: ConflictPolicy::Overwrite,
        verify: VerifyPolicy::Size,
    };
    let cancel = CancellationToken::new();
    let paused = watch::channel(false).1;
    let result = cairn_transfer::run_transfer(
        &src,
        &dst,
        &[(path.clone(), dst_path)],
        spec,
        &cancel,
        &paused,
        &mut |_bytes| {},
    )
    .await;
    if let Err(e) = result {
        return AppEvent::RemoteEditFailed {
            id,
            status: format!("download failed: {}", e.redacted()),
        };
    }
    match hash_file(&temp_path).await {
        Ok(download_hash) => AppEvent::RemoteEditDownloaded {
            id,
            conn,
            path,
            temp_path,
            v0,
            orig_size: size,
            orig_perms,
            download_hash,
        },
        Err(e) => AppEvent::RemoteEditFailed {
            id,
            status: format!("could not hash downloaded file: {e}"),
        },
    }
}

/// Handle [`AppEffect::EditRemoteTemp`] — the RFC-0012 P3 analogue of
/// [`run_editor_suspend`]/`AppEffect::SuspendAndEdit` once a remote file is already a local temp
/// copy (or the user chose [`cairn_core::WritebackChoice::KeepEditing`] and we're re-entering the
/// editor on the same copy). Special-cased inline in `event_loop` for the same reason
/// `SuspendAndEdit` is: exclusive terminal/stdin ownership.
///
/// After the editor exits, re-hashes `temp_path` and compares it against **`download_hash`** — the
/// hash captured right after the *original* download, stable for the whole session — never against
/// `hash` (the pre-round baseline for *this* invocation only). Comparing against `hash` instead was
/// a real bug: after a `KeepEditing` re-open, `hash` is the conflict-time content, so an editor exit
/// with no further changes would compare equal to it and report a false no-op — silently discarding
/// an edit that still differs from the remote and was never written back. Comparing against the
/// stable `download_hash` instead means *any* content that differs from the original download
/// always reports [`AppEvent::RemoteEditModified`] (routing to [`AppEffect::WriteBack`]), no matter
/// how many `KeepEditing` rounds it took to get there. Only a match against `download_hash` reports
/// [`AppEvent::RemoteEditNoChange`] (truly nothing to write back). A launch failure or non-zero
/// exit reports [`AppEvent::RemoteEditFailed`].
#[allow(clippy::too_many_arguments)]
async fn run_remote_edit_temp(
    terminal: &mut ratatui::DefaultTerminal,
    input_gate: &Arc<InputGate>,
    event_tx: &mpsc::Sender<AppEvent>,
    id: RemoteEditId,
    conn: ConnectionId,
    path: VfsPath,
    temp_path: PathBuf,
    v0: RemoteVersion,
    orig_size: u64,
    orig_perms: Option<UnixPerms>,
    download_hash: ContentHash,
    hash: ContentHash,
) {
    let (program, fixed_args) = match resolve_editor_argv() {
        Ok(pair) => pair,
        Err(message) => {
            send_event_detached(
                event_tx,
                AppEvent::RemoteEditFailed {
                    id,
                    status: message,
                },
            );
            return;
        }
    };
    let cwd = temp_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| temp_path.clone());
    let name = temp_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let outcome = suspend_and_run_editor(
        terminal,
        input_gate,
        &program,
        &fixed_args,
        &temp_path,
        &cwd,
    )
    .await;

    let status = match outcome {
        Ok(s) if s.success() => None,
        Ok(s) => Some(format!("editor exited: {}", describe_exit_status(&s))),
        Err(message) => Some(message),
    };
    if let Some(status) = status {
        send_event_detached(event_tx, AppEvent::RemoteEditFailed { id, status });
        return;
    }

    let new_hash = match hash_file(&temp_path).await {
        Ok(h) => h,
        Err(e) => {
            send_event_detached(
                event_tx,
                AppEvent::RemoteEditFailed {
                    id,
                    status: format!("could not hash edited file: {e}"),
                },
            );
            return;
        }
    };
    match decide_remote_edit_outcome(new_hash, download_hash, hash) {
        RemoteEditOutcome::NoChange => {
            send_event_detached(event_tx, AppEvent::RemoteEditNoChange { id, name });
        }
        RemoteEditOutcome::Modified {
            unchanged_since_reopen,
        } => {
            if unchanged_since_reopen {
                // Nothing changed in *this* invocation, but the content still differs from the
                // original download (a prior round already diverged) — still must be written back.
                tracing::debug!(
                    remote_edit_id = id,
                    "content unchanged since reopen but still differs from the original download; \
                     writing back"
                );
            }
            send_event_detached(
                event_tx,
                AppEvent::RemoteEditModified {
                    id,
                    conn,
                    path,
                    temp_path,
                    v0,
                    orig_size,
                    orig_perms,
                    download_hash,
                    hash: new_hash,
                },
            );
        }
    }
}

/// The outcome of [`decide_remote_edit_outcome`] — whether a completed edit round leaves anything
/// to write back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteEditOutcome {
    /// `new_hash` matches `download_hash` — truly nothing to write back for the whole session.
    NoChange,
    /// `new_hash` differs from `download_hash` — something must be written back.
    Modified {
        /// Whether `new_hash` also matches `hash` (the pre-round baseline for *this* invocation
        /// only) — i.e. this particular editor invocation changed nothing further, but an earlier
        /// round already diverged from the original download and that edit still needs writing
        /// back. Purely informational (used for a debug-level log), never gates the outcome.
        unchanged_since_reopen: bool,
    },
}

/// Pure decision made after an edit round completes (the editor has already run and `new_hash` is
/// the temp file's freshly-observed content hash): compares `new_hash` against **`download_hash`**
/// — the hash captured once, right after the original download, stable for the whole remote-edit
/// session — never against `hash` (the pre-round baseline for only this invocation).
///
/// This split exists to fix a real bug: an earlier implementation compared `new_hash` against
/// `hash` (which is *updated* to the latest content after every edit round, including a
/// `KeepEditing` re-open). After a conflict, `KeepEditing` re-opens the temp file with `hash` set to
/// the conflict-time content; if the user then quit the editor **without making any further
/// changes**, `new_hash == hash` was true, and the old code reported [`RemoteEditOutcome::NoChange`]
/// — silently discarding an edit that had never been written back (it still differed from the
/// remote, which is exactly why the conflict/`KeepEditing` cycle happened in the first place).
/// Comparing against the *stable* `download_hash` instead means the only way to reach `NoChange` is
/// for the content to be genuinely, literally identical to what was downloaded — regardless of how
/// many edit/conflict/`KeepEditing` rounds it took to get there.
#[must_use]
fn decide_remote_edit_outcome(
    new_hash: ContentHash,
    download_hash: ContentHash,
    hash: ContentHash,
) -> RemoteEditOutcome {
    if new_hash == download_hash {
        RemoteEditOutcome::NoChange
    } else {
        RemoteEditOutcome::Modified {
            unchanged_since_reopen: new_hash == hash,
        }
    }
}

/// Find a sibling path that does not currently exist on `vfs`, for the `SaveAs` write-back choice:
/// `"<stem> (edited)<ext>"`, then `"<stem> (edited) (2)<ext>"`, etc. Small, local re-implementation
/// of the same idea as `cairn_transfer`'s private `unique_name` (not reusable directly — it's a
/// module-private helper of that crate) — bounded the same way (gives up after a fixed number of
/// attempts rather than looping forever against a pathological or adversarial listing).
async fn unique_sibling_path(vfs: &Arc<dyn Vfs>, original: &VfsPath) -> Result<VfsPath, VfsError> {
    let parent = original.parent().unwrap_or_else(VfsPath::root);
    let base = original.file_name().unwrap_or("file");
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (base, String::new()),
    };
    for n in 0..1000 {
        let candidate_name = if n == 0 {
            format!("{stem} (edited){ext}")
        } else {
            format!("{stem} (edited) ({n}){ext}")
        };
        // `candidate_name` is built from `stem`/`ext` (already a valid path segment, since it came
        // from `VfsPath::file_name()`) plus a literal `" (edited)"`/`" (N)"` suffix — never a `/`
        // or control character — so this can't actually fail; mapped rather than `?` only because
        // `PathError` has no `From` conversion into `VfsError`.
        let candidate = parent
            .join(&candidate_name)
            .map_err(|_| VfsError::Backend {
                code: "remote_edit_invalid_sibling_name".to_owned(),
                msg: "could not build a sibling path".to_owned(),
                retryable: false,
            })?;
        match vfs.stat(&candidate).await {
            Err(VfsError::NotFound(_)) => return Ok(candidate),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Err(VfsError::Backend {
        code: "remote_edit_no_unique_sibling".to_owned(),
        msg: "could not find a free sibling name".to_owned(),
        retryable: false,
    })
}

/// Write `temp_path`'s content to `target` on `vfs`. Where the backend advertises `Caps::RENAME` at
/// `target`, this stages the content at a sibling name first and renames it over `target` (an
/// atomic-ish replace on backends with atomic rename; best-effort ordering otherwise) — cleaning up
/// the staged sibling on a failed staging write *or* a failed rename. Where it does not (Docker/K8s:
/// no atomic rename), this writes directly to `target`, which has a real (documented) non-atomic
/// window: a reader could see a partially-written file mid-copy. Either way conflicts are resolved
/// by direct overwrite here — the caller (`run_writeback`) has already decided writing is safe.
///
/// **Mode preservation on the RENAME path:** staging writes to a brand-new inode, so after a
/// successful rename the target's mode is whatever the staged file happened to get (umask-default),
/// not the original's — silently loosening (or tightening) permissions on every remote edit. When
/// `orig_perms` is `Some` and the backend advertises `Caps::CHMOD`, this restores the original mode
/// on `target` afterward, best-effort (a failure here does **not** fail an otherwise-successful
/// write-back — the content landed correctly; only the mode restoration is advisory). The
/// direct-overwrite path needs no such fix-up: it truncates the existing inode in place, so the
/// mode was never touched.
async fn write_temp_to_remote(
    vfs: &Arc<dyn Vfs>,
    temp_dir_vfs: &Arc<dyn Vfs>,
    temp_dst_path: &VfsPath,
    target: &VfsPath,
    orig_perms: Option<UnixPerms>,
) -> Result<(), cairn_transfer::TransferError> {
    let spec = TransferSpec {
        op: TransferOp::Copy,
        conflict: ConflictPolicy::Overwrite,
        verify: VerifyPolicy::Size,
    };
    let cancel = CancellationToken::new();
    let paused = watch::channel(false).1;

    if vfs.caps_at(target).contains(Caps::RENAME) {
        let staging_name = format!(".cairn-edit-tmp-{}", target.file_name().unwrap_or("file"));
        let staging = target
            .parent()
            .unwrap_or_else(VfsPath::root)
            .join(&staging_name)?;
        if let Err(e) = cairn_transfer::run_transfer(
            temp_dir_vfs,
            vfs,
            &[(temp_dst_path.clone(), staging.clone())],
            spec,
            &cancel,
            &paused,
            &mut |_| {},
        )
        .await
        {
            // Best-effort cleanup: a failed staging write can still have left a partial object at
            // `staging` (e.g. a one-off network failure mid-copy) — don't leave debris on the remote.
            let _ = vfs.remove(&staging, Recurse::No).await;
            return Err(e);
        }
        if let Err(e) = vfs.rename(&staging, target).await {
            // Best-effort cleanup of the staged copy so a failed rename doesn't leave debris.
            let _ = vfs.remove(&staging, Recurse::No).await;
            return Err(e.into());
        }
        if let Some(perms) = orig_perms {
            if vfs.caps_at(target).contains(Caps::CHMOD) {
                if let Err(e) = vfs
                    .set_perms(target, UnixPerms::from_mode(perms.mode))
                    .await
                {
                    // Advisory only: the content write-back already succeeded; losing the original
                    // mode is a (logged, redacted) degradation, not a failed write-back.
                    tracing::warn!(
                        error = %e.redacted(),
                        "could not restore original file permissions after remote edit write-back"
                    );
                }
            }
        }
        Ok(())
    } else {
        cairn_transfer::run_transfer(
            temp_dir_vfs,
            vfs,
            &[(temp_dst_path.clone(), target.clone())],
            spec,
            &cancel,
            &paused,
            &mut |_| {},
        )
        .await
        .map(|_| ())
    }
}

/// The runtime task body for [`AppEffect::WriteBack`]. See [`WriteBackMode`] for what each mode
/// does; `CheckThenWrite` performs both safety guards (remote-version re-check, zero-length guard)
/// before writing, `ForceOverwrite`/`SaveAsSibling` skip them (the user already confirmed via
/// [`Overlay::ConfirmWriteback`]).
#[allow(clippy::too_many_arguments)]
async fn run_writeback(
    registry: &VfsRegistry,
    id: RemoteEditId,
    conn: ConnectionId,
    path: VfsPath,
    temp_path: PathBuf,
    v0: RemoteVersion,
    orig_size: u64,
    orig_perms: Option<UnixPerms>,
    download_hash: ContentHash,
    mode: WriteBackMode,
) -> AppEvent {
    let Some(vfs) = registry.get(conn).await else {
        return AppEvent::RemoteEditFailed {
            id,
            status: "connection unavailable".to_owned(),
        };
    };
    let Some(temp_dir) = temp_path.parent() else {
        return AppEvent::RemoteEditFailed {
            id,
            status: "internal error: temp file has no parent directory".to_owned(),
        };
    };
    let temp_dir_vfs: Arc<dyn Vfs> = Arc::new(LocalVfs::new(REMOTE_EDIT_TEMP_CONN_ID, temp_dir));
    let Some(leaf) = temp_path.file_name().and_then(|n| n.to_str()) else {
        return AppEvent::RemoteEditFailed {
            id,
            status: "internal error: temp file name is not valid UTF-8".to_owned(),
        };
    };
    let Ok(temp_dst_path) = VfsPath::root().join(leaf) else {
        return AppEvent::RemoteEditFailed {
            id,
            status: "internal error: temp file name is not a valid path segment".to_owned(),
        };
    };

    if mode == WriteBackMode::CheckThenWrite {
        // Zero-length guard: a crashed/misbehaving editor truncating a previously non-empty file.
        // Local-only, cheap — checked before bothering with a remote round trip.
        let current_size = match tokio::fs::metadata(&temp_path).await {
            Ok(m) => m.len(),
            Err(e) => {
                return AppEvent::RemoteEditFailed {
                    id,
                    status: format!("could not stat the edited temp file: {e}"),
                };
            }
        };
        if orig_size > 0 && current_size == 0 {
            // Real hash of the (empty) temp file, not a placeholder: if the user picks
            // `KeepEditing` from the conflict overlay, this becomes the next edit session's
            // no-op baseline — an all-zero placeholder would never match a real SHA-256 (not even
            // the well-known empty-input digest), silently defeating the no-op-skip optimization
            // on the very next round.
            let hash = match hash_file(&temp_path).await {
                Ok(h) => h,
                Err(e) => {
                    return AppEvent::RemoteEditFailed {
                        id,
                        status: format!("could not hash the edited temp file: {e}"),
                    };
                }
            };
            return AppEvent::WriteBackConflict {
                id,
                conn,
                path,
                temp_path,
                v0,
                orig_size,
                orig_perms,
                download_hash,
                hash,
                reason: cairn_core::WritebackConflictReason::ZeroLengthGuard,
            };
        }

        // Conflict re-check: this is the TOCTOU-sensitive comparison — `v0` is the snapshot taken
        // at download time (held since; never recomputed from a re-opened path), `v1` is a fresh
        // `stat` right now, immediately before the write decision.
        let v1 = match vfs.stat(&path).await {
            Ok(entry) => RemoteVersion::from_entry(&entry),
            Err(VfsError::NotFound(_)) => RemoteVersion::Unknown,
            Err(e) => {
                return AppEvent::RemoteEditFailed {
                    id,
                    status: format!("write-back aborted: {}", e.redacted()),
                };
            }
        };
        if !v0.confirmed_equal(&v1) {
            // See the comment above: this must be the real content hash, not a placeholder, so a
            // subsequent `KeepEditing` pass has a correct no-op baseline.
            let hash = match hash_file(&temp_path).await {
                Ok(h) => h,
                Err(e) => {
                    return AppEvent::RemoteEditFailed {
                        id,
                        status: format!("could not hash the edited temp file: {e}"),
                    };
                }
            };
            return AppEvent::WriteBackConflict {
                id,
                conn,
                path,
                temp_path,
                v0,
                orig_size,
                orig_perms,
                download_hash,
                hash,
                reason: cairn_core::WritebackConflictReason::RemoteChanged,
            };
        }
    }

    let (target, name) = match mode {
        WriteBackMode::SaveAsSibling => match unique_sibling_path(&vfs, &path).await {
            Ok(sibling) => {
                let n = sibling.file_name().unwrap_or("file").to_owned();
                (sibling, n)
            }
            Err(e) => {
                return AppEvent::RemoteEditFailed {
                    id,
                    status: format!("could not choose a sibling name: {}", e.redacted()),
                };
            }
        },
        WriteBackMode::CheckThenWrite | WriteBackMode::ForceOverwrite => {
            let n = path.file_name().unwrap_or("file").to_owned();
            (path, n)
        }
    };

    match write_temp_to_remote(&vfs, &temp_dir_vfs, &temp_dst_path, &target, orig_perms).await {
        Ok(()) => AppEvent::WriteBackDone { id, name },
        Err(e) => AppEvent::RemoteEditFailed {
            id,
            status: format!("write-back failed: {}", e.redacted()),
        },
    }
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
    // Used only by the feature-gated deferral test below; gated to avoid an unused-import
    // warning in the lean build.
    #[cfg(feature = "s3")]
    use cairn_config::ConnectionProfile;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn ui_theme_resolves_the_live_preset_with_config_overrides() {
        // `Ui::theme` must resolve the palette from the *live* `state.theme_name` (so a Shift-T switch
        // takes effect) with the config `[ui.colors]` overrides applied on top.
        let mut colors = std::collections::BTreeMap::new();
        colors.insert("dir".to_owned(), "#010203".to_owned());
        let ui = Ui {
            keymap: Keymap::from_overrides(std::iter::empty::<(&str, &str)>()).0,
            colors: colors.clone(),
        };
        let mut state = AppState::new(LEFT, RIGHT, VfsPath::root());
        // A live switch to `mc` is reflected, and the override is layered on the mc preset.
        state.theme_name = "mc".to_owned();
        let expected = Theme::resolve("mc", &colors).0;
        assert_eq!(ui.theme(&state), expected);
        assert_eq!(ui.theme(&state).dir, ratatui::style::Color::Rgb(1, 2, 3));
        // Switching the name switches the palette.
        state.theme_name = "nord".to_owned();
        assert_eq!(ui.theme(&state), Theme::resolve("nord", &colors).0);
    }

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
    async fn delete_walks_the_tree_emits_progress_and_reports_done() {
        // End-to-end: a recursive delete removes the whole tree, streams `TransferScanning` progress
        // (item count + current path) so the dialog isn't frozen, and finishes with `TransferDone`.
        let dir = tempfile_dir();
        // A multi-level tree with sibling subdirs, to lock in the deepest-first post-order removal
        // (a parent dir is only removed after all its children — files and subdirs — are gone).
        std::fs::create_dir_all(dir.path().join("d/sub1/deep")).unwrap();
        std::fs::create_dir(dir.path().join("d/sub2")).unwrap();
        std::fs::write(dir.path().join("d/a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("d/b.txt"), b"b").unwrap();
        std::fs::write(dir.path().join("d/sub1/x.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("d/sub1/deep/y.txt"), b"y").unwrap();
        std::fs::write(dir.path().join("d/sub2/z.txt"), b"z").unwrap();
        std::fs::write(dir.path().join("top.txt"), b"t").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        let (tx, mut rx) = mpsc::channel(64);
        let ev = run_delete_effect(
            &registry,
            7,
            LEFT,
            vec![
                VfsPath::parse("/d").unwrap(),
                VfsPath::parse("/top.txt").unwrap(),
            ],
            &tx,
            CancellationToken::new(),
        )
        .await;
        match ev {
            AppEvent::TransferDone { id, error, status } => {
                assert_eq!(id, 7);
                assert!(!error, "clean delete: {status}");
                assert!(status.contains("Deleted"), "got: {status}");
            }
            _ => panic!("expected TransferDone"),
        }
        // Everything is gone (d/, its two files, top.txt).
        assert!(!dir.path().join("d").exists());
        assert!(!dir.path().join("top.txt").exists());
        // At least one progress event carried a non-empty path.
        let mut saw_progress = false;
        while let Ok(msg) = rx.try_recv() {
            if let AppEvent::TransferScanning { id, current, .. } = msg {
                assert_eq!(id, 7);
                assert!(!current.is_empty());
                saw_progress = true;
            }
        }
        assert!(saw_progress, "expected at least one delete progress event");
    }

    #[tokio::test]
    async fn delete_removes_hidden_files_and_subdirectories() {
        // Regression: a folder that contains a *hidden* (dot-prefixed) file or subdirectory must be
        // fully removed — the recursive walk lists with `all: true`, so dotfiles aren't skipped and
        // the parent isn't left behind non-empty.
        let dir = tempfile_dir();
        std::fs::create_dir_all(dir.path().join("d/.hidden/deep")).unwrap();
        std::fs::write(dir.path().join("d/.hidden/deep/.secret"), b"s").unwrap();
        std::fs::write(dir.path().join("d/.dotfile"), b"d").unwrap();
        std::fs::write(dir.path().join("d/visible.txt"), b"v").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        let (tx, _rx) = mpsc::channel(64);
        let ev = run_delete_effect(
            &registry,
            9,
            LEFT,
            vec![VfsPath::parse("/d").unwrap()],
            &tx,
            CancellationToken::new(),
        )
        .await;
        match ev {
            AppEvent::TransferDone { error, status, .. } => {
                assert!(
                    !error,
                    "clean delete of a tree with hidden entries: {status}"
                );
            }
            _ => panic!("expected TransferDone"),
        }
        // The whole tree — including the hidden subdir and dotfiles — is gone.
        assert!(
            !dir.path().join("d").exists(),
            "hidden entries left the dir behind"
        );
    }

    #[tokio::test]
    async fn cancelled_delete_reports_a_non_error_completion() {
        // A pre-cancelled token stops the walk immediately; the op still reports completion (so the
        // dialog closes + panes refresh) as a non-error partial.
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("keep.txt"), b"k").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (tx, _rx) = mpsc::channel(8);
        let ev = run_delete_effect(
            &registry,
            1,
            LEFT,
            vec![VfsPath::parse("/keep.txt").unwrap()],
            &tx,
            cancel,
        )
        .await;
        match ev {
            AppEvent::TransferDone { error, status, .. } => {
                assert!(!error, "cancellation is not a failure: {status}");
                assert!(status.contains("cancelled"), "got: {status}");
            }
            _ => panic!("expected TransferDone"),
        }
        // The file survives (the walk was cancelled before removing it).
        assert!(dir.path().join("keep.txt").exists());
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
    async fn transfer_emits_scanning_then_finalizing_events() {
        // End-to-end wiring of the pre-flight scan + finalize signals through `run_transfer_effect`:
        // the pre-scan must put at least one `TransferScanning` on the channel (so the dialog can show
        // the "Counting" phase), and the copy must emit a `TransferProgress { finalizing: true }`
        // before completion (so the bar can reach an honest 100%).
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("src.txt"), b"hello world").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, dir.path())))
            .await;
        registry
            .insert(RIGHT, Arc::new(LocalVfs::new(RIGHT, dir.path())))
            .await;
        let (tx, mut rx) = mpsc::channel(64);
        let ev = run_transfer_effect(
            &registry,
            1,
            LEFT,
            RIGHT,
            vec![(
                VfsPath::parse("/src.txt").unwrap(),
                VfsPath::parse("/dst.txt").unwrap(),
            )],
            false,
            true, // overwrite: the scanning here comes from the size scan, not the conflict loop
            &tx,
            CancellationToken::new(),
            watch::channel(false).1,
        )
        .await;
        assert!(matches!(ev, AppEvent::TransferDone { error: false, .. }));

        let mut saw_scanning = false;
        let mut saw_finalizing = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                AppEvent::TransferScanning { id, current, .. } => {
                    assert_eq!(id, 1);
                    assert!(
                        !current.is_empty(),
                        "scanning reports the path being walked"
                    );
                    saw_scanning = true;
                }
                AppEvent::TransferProgress {
                    finalizing: true, ..
                } => saw_finalizing = true,
                _ => {}
            }
        }
        assert!(saw_scanning, "expected at least one TransferScanning event");
        assert!(
            saw_finalizing,
            "expected a finalizing TransferProgress before completion"
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
        // The scan callback fires for every visited node (a.txt; d and its child b.txt) so the UI
        // can show the walk progressing — 3 visits for these two items.
        let mut visits = 0u32;
        assert_eq!(
            scan_total_bytes(&src, &items, &mut |_b, _p| visits += 1).await,
            Some(11)
        );
        assert_eq!(visits, 3, "on_scan fires once per visited node");
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
            scan_total_bytes(
                &src,
                &[(VfsPath::root(), VfsPath::root())],
                &mut |_b, _p| {}
            )
            .await,
            Some(5)
        );
        // A missing source degrades to None (best-effort) rather than panicking.
        let missing = vec![(VfsPath::parse("/nope").unwrap(), VfsPath::root())];
        assert_eq!(
            scan_total_bytes(&src, &missing, &mut |_b, _p| {}).await,
            None
        );
    }

    #[test]
    fn progress_emit_due_fires_on_bytes_or_time_but_not_on_a_stall() {
        // Byte-driven: a full step emits even with no elapsed time.
        assert!(progress_emit_due(TRANSFER_PROGRESS_STEP, 0, 0, 0));
        // Under a step and under the interval: no emit yet.
        assert!(!progress_emit_due(100, 0, 50, 0));
        // Time-driven: past the interval with real progress → emit (this is the small/slow-copy fix).
        assert!(progress_emit_due(100, 0, TRANSFER_UI_MIN_INTERVAL_MS, 0));
        // Enough time but NO new bytes (a stall): must not emit a redundant frame.
        assert!(!progress_emit_due(
            42,
            42,
            10 * TRANSFER_UI_MIN_INTERVAL_MS,
            0
        ));
        // Clock going backwards (defensive) saturates rather than underflowing → no spurious emit.
        assert!(!progress_emit_due(50, 50, 0, 1000));
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

    #[test]
    fn windowed_rate_reflects_recent_speed_and_recovers_after_a_slow_stretch() {
        let mut s: VecDeque<(u64, u64)> = VecDeque::from([(0u64, 0u64)]);
        // Steady 1 MiB/s for the first second → 1 MiB/s.
        assert_eq!(windowed_rate(&mut s, 1_000, 1 << 20), 1 << 20);
        // A long slow stretch: over the next ~4 s only a trickle arrives. A *lifetime* average would
        // now read ~(1 MiB + trickle)/5 s ≈ 0.2 MiB/s and stay there. The window instead only sees
        // the recent (slow) samples.
        windowed_rate(&mut s, 2_000, (1 << 20) + 10_000);
        windowed_rate(&mut s, 3_000, (1 << 20) + 20_000);
        windowed_rate(&mut s, 4_000, (1 << 20) + 30_000);
        let slow = windowed_rate(&mut s, 5_000, (1 << 20) + 40_000);
        assert!(
            slow < 100_000,
            "window shows the slow recent rate, got {slow}"
        );
        // Speed recovers to 10 MiB/s: within the window the rate climbs back up promptly, unlike a
        // lifetime average which would stay depressed by the earlier slow stretch.
        windowed_rate(&mut s, 6_000, (1 << 20) + 40_000 + (10 << 20));
        windowed_rate(&mut s, 7_000, (1 << 20) + 40_000 + (20 << 20));
        let recovered = windowed_rate(&mut s, 8_000, (1 << 20) + 40_000 + (30 << 20));
        assert!(
            recovered > 8 * (1 << 20),
            "rate recovered to ~10 MiB/s, got {recovered}"
        );
    }

    #[test]
    fn windowed_rate_trims_to_the_window_and_never_panics() {
        let mut s: VecDeque<(u64, u64)> = VecDeque::from([(0u64, 0u64)]);
        // Feed many samples well past the window; the deque must stay bounded (anchor + recent), not
        // grow without limit, and never panic on the front()/index accesses.
        for i in 1..=1000u64 {
            windowed_rate(&mut s, i * 100, i * 1_000);
        }
        assert!(
            s.len() <= 40,
            "samples trimmed to ~window, not unbounded: {}",
            s.len()
        );
        // A zero-elapsed first call is floored by avg_rate, no divide-by-zero.
        let mut z: VecDeque<(u64, u64)> = VecDeque::from([(0u64, 0u64)]);
        let _ = windowed_rate(&mut z, 0, 5_000);
    }

    #[test]
    fn windowed_rate_after_a_long_pause_reads_low_never_high_then_recovers() {
        // No samples arrive while a transfer is paused, so on resume the pre-pause sample is still
        // the anchor across a huge time gap. The rate must read LOW (bytes barely moved over a long
        // span) — never an absurd spike — and recover once the stale anchor ages past the window.
        let mut s: VecDeque<(u64, u64)> = VecDeque::from([(0u64, 0u64)]);
        windowed_rate(&mut s, 1_000, 5 << 20); // steady before the pause
        windowed_rate(&mut s, 2_000, 10 << 20);
        // 60 s pause, then resume at t=62 s with only a little new data.
        let just_after = windowed_rate(&mut s, 62_000, (10 << 20) + 20_000);
        assert!(
            just_after < (1 << 20),
            "post-resume rate reads low, not a spike: {just_after}"
        );
        // Real speed after resume is 10 MiB/s; once the pre-pause anchor ages out of the window the
        // reported rate climbs back to it.
        windowed_rate(&mut s, 63_000, (10 << 20) + 20_000 + (10 << 20));
        windowed_rate(&mut s, 64_000, (10 << 20) + 20_000 + (20 << 20));
        let recovered = windowed_rate(&mut s, 65_500, (10 << 20) + 20_000 + (35 << 20));
        assert!(
            recovered > 8 * (1 << 20),
            "rate recovered after the stale anchor aged out: {recovered}"
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
        // P2: run_vault_unlock_effect only unlocks the broker; connections open lazily.
        let res = run_vault_unlock_effect(
            &broker,
            Some(path),
            SecretString::from("open-sesame".to_owned()),
        )
        .await;
        assert!(res.is_ok(), "unlock should succeed");
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
        let err =
            run_vault_unlock_effect(&broker, Some(path), SecretString::from("wrong".to_owned()))
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
        let err = run_vault_unlock_effect(&broker, None, SecretString::from("x".to_owned()))
            .await
            .unwrap_err();
        assert!(err.contains("no vault path"), "got: {err}");
        assert!(!broker.is_unlocked());
    }

    #[tokio::test]
    async fn vault_unlock_effect_missing_file_is_a_clear_message() {
        use cairn_secrets::SecretString;
        let broker = Arc::new(Broker::locked());
        let err = run_vault_unlock_effect(
            &broker,
            Some(PathBuf::from("/no/such/dir/vault.cvlt")),
            SecretString::from("x".to_owned()),
        )
        .await
        .unwrap_err();
        // The message must be user-friendly and mention the vault (no passphrase, no path).
        assert!(
            err.contains("vault"),
            "expected a user-friendly vault message, got: {err}"
        );
        assert!(!broker.is_unlocked());
    }

    // ---- P4.5: vault-create effect ---------------------------------------------------------------
    //
    // These tests call `run_create_vault_effect` directly. The function calls `Vault::create`
    // which uses `KdfParams::recommended()` (Argon2id, ~100 ms). Marked `#[tokio::test]` so they
    // run in the standard async test harness — acceptable latency for CI.
    // No real keychain is touched: `remember = false` skips the store path entirely.

    #[tokio::test]
    async fn create_vault_effect_creates_and_unlocks_broker() {
        use cairn_secrets::SecretString;
        let dir = tempfile_dir();
        let path = dir.path().join("new.cvlt");
        assert!(!path.exists(), "precondition: vault file must not exist");
        let broker = Arc::new(Broker::locked());
        let (result, already_exists) = run_create_vault_effect(
            &broker,
            Some(path.clone()),
            SecretString::from("correct horse battery staple".to_owned()),
            false, // no keychain
        )
        .await;
        assert!(result.is_ok(), "create should succeed, got: {result:?}");
        assert!(!already_exists, "already_exists must be false on success");
        assert!(broker.is_unlocked(), "broker must be unlocked after create");
        assert!(path.exists(), "vault file must be on disk after create");
        // Verify 0600 permissions on Unix (the OS-level security requirement).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "vault file must be 0600, got {mode:o}");
        }
    }

    #[tokio::test]
    async fn create_vault_effect_no_path_configured_returns_clear_error() {
        use cairn_secrets::SecretString;
        let broker = Arc::new(Broker::locked());
        let (result, already_exists) = run_create_vault_effect(
            &broker,
            None,
            SecretString::from("passphrase".to_owned()),
            false,
        )
        .await;
        let err = result.unwrap_err();
        assert!(err.contains("no vault path"), "got: {err}");
        assert!(!already_exists);
        assert!(!broker.is_unlocked());
    }

    #[tokio::test]
    async fn create_vault_effect_already_existing_file_returns_error_and_leaves_broker_locked() {
        use cairn_secrets::SecretString;
        use cairn_vault::{KdfParams, Vault};
        let dir = tempfile_dir();
        let path = dir.path().join("existing.cvlt");
        // Create a vault file first (fast params to keep the test quick).
        Vault::create_with_params(
            &path,
            &SecretString::from("first".to_owned()),
            KdfParams::fast_for_tests(),
        )
        .unwrap();
        let broker = Arc::new(Broker::locked());
        // Attempting to create again must fail cleanly.
        let (result, already_exists) = run_create_vault_effect(
            &broker,
            Some(path.clone()),
            SecretString::from("second".to_owned()),
            false,
        )
        .await;
        let err = result.unwrap_err();
        // The error must not contain the passphrase or the vault file path (Fix 4).
        assert!(
            !err.contains("second"),
            "error must not reveal the passphrase, got: {err}"
        );
        assert!(
            !err.contains(path.to_str().unwrap_or("")),
            "error must not reveal the vault path, got: {err}"
        );
        // The error should mention "already" or "exists".
        assert!(
            err.to_lowercase().contains("already") || err.to_lowercase().contains("exist"),
            "expected a 'file already exists' error, got: {err}"
        );
        assert!(
            already_exists,
            "already_exists must be true for AlreadyExists failure"
        );
        assert!(!broker.is_unlocked(), "broker must stay locked on failure");
    }

    /// Fix 2 (runtime side): `run_create_vault_effect` must not silently overwrite an existing
    /// vault — the existing file must be byte-for-byte identical after the failed attempt.
    ///
    /// This exercises the `atomic_create` (persist_noclobber) path in `cairn-vault`.
    #[tokio::test]
    async fn create_vault_effect_does_not_clobber_existing_vault_file() {
        use cairn_secrets::SecretString;
        use cairn_vault::{KdfParams, Vault};
        let dir = tempfile_dir();
        let path = dir.path().join("existing.cvlt");
        // Create an original vault (fast params) and read its bytes.
        Vault::create_with_params(
            &path,
            &SecretString::from("original-passphrase".to_owned()),
            KdfParams::fast_for_tests(),
        )
        .unwrap();
        let original_bytes = std::fs::read(&path).unwrap();
        // A second `create` attempt (simulating the race window) must fail.
        let broker = Arc::new(Broker::locked());
        let (result, already_exists) = run_create_vault_effect(
            &broker,
            Some(path.clone()),
            SecretString::from("attacker-passphrase".to_owned()),
            false,
        )
        .await;
        assert!(result.is_err(), "second create must fail");
        assert!(already_exists, "must be flagged as AlreadyExists");
        // The vault file must be unchanged.
        let bytes_after = std::fs::read(&path).unwrap();
        assert_eq!(
            original_bytes, bytes_after,
            "vault file must not be modified by a failed create"
        );
        assert!(!broker.is_unlocked(), "broker must stay locked");
    }

    // P2: A locked vault means credential-bearing profiles appear as NeedsVault in the switcher
    // rather than being deferred. Unlocking the broker (via run_vault_unlock_effect) does NOT
    // open them; opening happens lazily via run_open_connection_effect on selection. This test
    // verifies the P2 coordinator behaviour and that vault unlock leaves the broker unlocked.
    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn locked_vault_makes_credentialed_profiles_needs_vault_and_unlock_unblocks_broker() {
        use cairn_core::ChoiceStatus;
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
        let (choices, deferred, _descriptors) =
            register_connections(&registry, &cfg, &opener).await;

        // P2: coordinator never defers; credential-bearing profile is NeedsVault in switcher.
        assert!(
            deferred.is_empty(),
            "P2 coordinator must not produce any deferred connections"
        );
        let s3 = choices.iter().find(|c| c.label.starts_with("s3:"));
        assert!(s3.is_some(), "s3 profile must appear in choices");
        assert_eq!(
            s3.unwrap().status,
            ChoiceStatus::NeedsVault,
            "vault-locked s3 profile must be NeedsVault, not absent"
        );

        // Vault unlock just unlocks the broker; no connections are opened here.
        run_vault_unlock_effect(&broker, Some(path), SecretString::from("pw".to_owned()))
            .await
            .unwrap();
        assert!(
            broker.is_unlocked(),
            "the broker must be unlocked after a successful vault open"
        );
        // The s3 choice remains NeedsVault in the switcher until the reducer flips it to
        // NeedsOpen on receiving AppEvent::VaultUnlocked; actual open happens on selection.
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

    // ── RFC-0012 P2: editor-launch hardening ──────────────────────────────────────────────────
    //
    // These tests mutate process-global environment variables (`$EDITOR`/`$VISUAL` and a handful
    // of canary secrets), which is inherently racy against other tests in this binary running
    // concurrently on other threads. `env_test_lock()` serializes every test below against each
    // other; `EnvVarGuard` snapshots and restores the prior value of each var it touches.

    /// Serializes every test that mutates process env vars below, so parallel test threads don't
    /// race on `$EDITOR`/`$VISUAL`/the canary vars. A `tokio::sync::Mutex` (not `std::sync::Mutex`)
    /// so both plain `#[test]` fns (`blocking_lock`) and `#[tokio::test]` fns (`lock().await`) can
    /// use the same lock.
    fn env_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// RAII guard: sets each `(key, value)` pair, remembering the prior value (if any), and
    /// restores it (or removes the key if it was previously unset) on drop.
    struct EnvVarGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvVarGuard {
        fn set(vars: &[(&str, &str)]) -> Self {
            let mut saved = Vec::with_capacity(vars.len());
            for (key, value) in vars {
                saved.push(((*key).to_owned(), std::env::var(key).ok()));
                std::env::set_var(key, value);
            }
            Self { saved }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (key, prev) in self.saved.drain(..) {
                match prev {
                    Some(v) => std::env::set_var(&key, v),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }

    /// Writes a tiny POSIX-sh "fake editor" into `dir` that dumps its argv (one element per line)
    /// to `argv.out` and its full environment to `env.out`, both alongside the script itself
    /// (resolved via the script's own `$0`, not `cwd`, since `spawn_editor_hardened` sets `cwd` to
    /// the *target file's* parent directory, which may differ from `dir`). Exits `0`.
    #[cfg(unix)]
    fn write_fake_editor(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake_editor.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             SCRIPT_DIR=$(cd \"$(dirname \"$0\")\" && pwd)\n\
             for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$SCRIPT_DIR/argv.out\"\n\
             env > \"$SCRIPT_DIR/env.out\"\n\
             exit 0\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    /// A value that would chain a shell command (`; touch pwned`) if `$EDITOR` were ever handed to
    /// a shell. Since we spawn argv-only, the whole string `"vi; touch pwned"` splits (via
    /// `shlex`, whitespace-only, no metacharacter awareness) into a program literally named `"vi;"`
    /// plus args `["touch", "pwned"]` — `"vi;"` cannot exist as an executable, so the spawn fails
    /// cleanly and, crucially, `touch pwned` is never *run* as a separate command.
    #[cfg(unix)]
    #[tokio::test]
    async fn editor_command_is_never_shell_interpreted() {
        let _serialize = env_test_lock().lock().await;
        let _env = EnvVarGuard::set(&[("VISUAL", ""), ("EDITOR", "vi; touch pwned")]);
        let dir = tempfile_dir();
        let target = dir.path().join("f.txt");
        std::fs::write(&target, b"x").unwrap();

        let (program, args) =
            resolve_editor_argv().expect("well-formed quoting must split successfully");
        let result = spawn_editor_hardened(&program, &args, &target, dir.path()).await;

        assert!(
            result.is_err(),
            "a program literally named 'vi;' cannot exist; spawn must fail cleanly, got {result:?}"
        );
        assert!(
            !dir.path().join("pwned").exists(),
            "a shell would have created 'pwned' via `; touch pwned`; argv exec must not"
        );
    }

    /// `$EDITOR` splitting uses POSIX shell-word *quoting* only — never variable/command
    /// substitution. `vi "$(id)"` must split to the literal argv `["vi", "$(id)"]`; the `$(id)`
    /// text is inert (never handed to a shell to expand).
    #[test]
    fn editor_argv_split_never_expands_command_substitution() {
        let _serialize = env_test_lock().blocking_lock();
        let _env = EnvVarGuard::set(&[("VISUAL", ""), ("EDITOR", r#"vi "$(id)""#)]);

        let (program, args) = resolve_editor_argv().expect("well-formed quoting must split");
        assert_eq!(program, "vi");
        assert_eq!(
            args,
            vec!["$(id)".to_owned()],
            "the quoted section must be preserved literally, not substituted"
        );
    }

    /// Fixed flags survive splitting too — `code --wait` must resolve to `("code", ["--wait"])`,
    /// not a single mangled token.
    #[test]
    fn editor_argv_split_preserves_fixed_flags() {
        let _serialize = env_test_lock().blocking_lock();
        let _env = EnvVarGuard::set(&[("VISUAL", ""), ("EDITOR", "code --wait")]);

        let (program, args) = resolve_editor_argv().expect("well-formed quoting must split");
        assert_eq!(program, "code");
        assert_eq!(args, vec!["--wait".to_owned()]);
    }

    /// A `--` terminator always precedes the (always-absolute) target path, and the path is
    /// forwarded as one unmodified argv element — so a file literally named `-c :!sh` is passed as
    /// a plain filename, never parsed as a flag by the editor's own argument parser.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_editor_terminates_flags_with_double_dash_before_the_path() {
        // `spawn_editor_hardened` reads process env (PATH + allowlist) while building the child, so
        // it must not run concurrently with the env-mutating editor tests — a transiently-cleared
        // PATH would break the fake `#!/bin/sh` editor's `dirname`/`env`/`printf` and fail the spawn.
        let _serialize = env_test_lock().lock().await;
        let dir = tempfile_dir();
        let editor = write_fake_editor(dir.path());
        let target = dir.path().join("-c :!sh");
        std::fs::write(&target, b"contents").unwrap();

        let status = spawn_editor_hardened(&editor.to_string_lossy(), &[], &target, dir.path())
            .await
            .expect("the fake editor must spawn and exit cleanly");
        assert!(status.success());

        let argv_dump = std::fs::read_to_string(dir.path().join("argv.out")).unwrap();
        let lines: Vec<&str> = argv_dump.lines().collect();
        let target_str = target.to_string_lossy().into_owned();
        assert_eq!(
            lines.last().copied(),
            Some(target_str.as_str()),
            "the absolute path must be the final, unmodified argv element, got {lines:?}"
        );
        assert_eq!(
            lines.get(lines.len().wrapping_sub(2)).copied(),
            Some("--"),
            "a `--` terminator must immediately precede the path, got {lines:?}"
        );
    }

    /// The editor child's environment is `env_clear()` + an explicit allow-list + a sanitized
    /// `PATH` — secret-shaped variables (`AWS_*`, `GITHUB_TOKEN`, `LD_PRELOAD`, `CAIRN_*`) never
    /// reach it, while a plain, non-secret var the editor needs to render correctly (`TERM`) does.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_editor_scrubs_environment_to_an_allowlist() {
        let _serialize = env_test_lock().lock().await;
        let _env = EnvVarGuard::set(&[
            ("TERM", "xterm-cairn-test"),
            ("AWS_SECRET_ACCESS_KEY", "leak-aws-secret"),
            ("GITHUB_TOKEN", "leak-gh-token"),
            ("LD_PRELOAD", "/tmp/evil.so"),
            ("CAIRN_SOMETHING", "leak-cairn-internal"),
        ]);
        let dir = tempfile_dir();
        let editor = write_fake_editor(dir.path());
        let target = dir.path().join("f.txt");
        std::fs::write(&target, b"x").unwrap();

        let status = spawn_editor_hardened(&editor.to_string_lossy(), &[], &target, dir.path())
            .await
            .expect("the fake editor must spawn and exit cleanly");
        assert!(status.success());

        let env_dump = std::fs::read_to_string(dir.path().join("env.out")).unwrap();
        assert!(
            env_dump.contains("TERM=xterm-cairn-test"),
            "TERM must reach the editor so it can render correctly, got env: {env_dump}"
        );
        for blocked in [
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "LD_PRELOAD",
            "CAIRN_SOMETHING",
        ] {
            assert!(
                !env_dump.contains(blocked),
                "{blocked} must never reach the editor child, got env: {env_dump}"
            );
        }
    }

    /// A vault-adjacent secret value must appear in **neither** the child's environment nor its
    /// argv — the structural guarantee is that `AppEffect::SuspendAndEdit` never carries secret
    /// material in the first place (only a `ConnectionId` + `VfsPath`), and this test additionally
    /// proves the env-scrub doesn't accidentally forward an ambient secret sitting in Cairn's own
    /// process environment (e.g. inherited from a parent shell) either.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_editor_never_leaks_a_vault_secret_canary() {
        let _serialize = env_test_lock().lock().await;
        const CANARY_KEY: &str = "CAIRN_VAULT_CANARY_TEST_ONLY";
        const CANARY_VALUE: &str = "do-not-leak-canary-4f7a9c21";
        let _env = EnvVarGuard::set(&[(CANARY_KEY, CANARY_VALUE)]);
        let dir = tempfile_dir();
        let editor = write_fake_editor(dir.path());
        let target = dir.path().join("f.txt");
        std::fs::write(&target, b"x").unwrap();

        let status = spawn_editor_hardened(&editor.to_string_lossy(), &[], &target, dir.path())
            .await
            .expect("the fake editor must spawn and exit cleanly");
        assert!(status.success());

        let env_dump = std::fs::read_to_string(dir.path().join("env.out")).unwrap();
        let argv_dump = std::fs::read_to_string(dir.path().join("argv.out")).unwrap();
        assert!(
            !env_dump.contains(CANARY_VALUE),
            "the canary secret must never reach the child's environment"
        );
        assert!(
            !argv_dump.contains(CANARY_VALUE),
            "the canary secret must never reach the child's argv"
        );
    }

    /// On Windows, with neither `$EDITOR` nor `$VISUAL` set, we must refuse with a clear message
    /// rather than guess `notepad`.
    #[test]
    fn resolve_editor_command_unix_falls_back_to_vi() {
        let _serialize = env_test_lock().blocking_lock();
        let _env = EnvVarGuard::set(&[("VISUAL", ""), ("EDITOR", "")]);
        if cfg!(windows) {
            assert!(resolve_editor_command().is_err());
        } else {
            assert_eq!(resolve_editor_command(), Ok("vi".to_owned()));
        }
    }

    /// `$VISUAL` takes priority over `$EDITOR` when both are set.
    #[test]
    fn resolve_editor_command_prefers_visual_over_editor() {
        let _serialize = env_test_lock().blocking_lock();
        let _env = EnvVarGuard::set(&[("VISUAL", "myvisual"), ("EDITOR", "myeditor")]);
        assert_eq!(resolve_editor_command(), Ok("myvisual".to_owned()));
    }

    /// Malformed quoting (an unterminated quote) is a clean, typed error — never a panic.
    #[test]
    fn editor_argv_split_rejects_malformed_quoting() {
        let _serialize = env_test_lock().blocking_lock();
        let _env = EnvVarGuard::set(&[("VISUAL", ""), ("EDITOR", "vi 'unterminated")]);
        assert!(resolve_editor_argv().is_err());
    }

    // --- InputGate pause/resume handshake ---

    /// `request_pause` must block until a live reader thread acknowledges (flips to `Paused`), then
    /// `resume` must unpark it. Failure mode of the primitive is a permanent hang, so this drives it
    /// with a real second thread and bounds the whole thing with a timeout.
    #[test]
    fn input_gate_pause_waits_for_reader_ack_then_resume_unparks() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let gate = InputGate::new();
        let ticks = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        // A stand-in reader thread: loop calling reader_tick (which parks it while Paused) and
        // counting iterations, until told to stop.
        let reader = {
            let (gate, ticks, stop) = (gate.clone(), ticks.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    gate.reader_tick();
                    ticks.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };

        // request_pause blocks until the reader acks; it must return Ok and leave state Paused.
        assert_eq!(gate.request_pause(), Ok(()));
        assert_eq!(*gate.lock(), ReaderState::Paused);

        // While paused, the reader is parked inside reader_tick → the tick count must not advance.
        let parked_at = ticks.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            parked_at,
            "reader must be parked (no ticks) while Paused"
        );

        // Resume unparks it → ticks advance again.
        gate.resume();
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            ticks.load(Ordering::SeqCst) > parked_at,
            "reader must resume ticking after resume()"
        );

        stop.store(true, Ordering::SeqCst);
        reader.join().unwrap();
    }

    /// If the reader thread has died (`mark_dead`), `request_pause` must return `Err(())` promptly
    /// rather than block forever waiting for an ack that can never come.
    #[test]
    fn input_gate_pause_returns_err_when_reader_is_dead() {
        let gate = InputGate::new();
        gate.mark_dead();
        assert_eq!(
            gate.request_pause(),
            Err(()),
            "a dead reader must make request_pause bail, not hang"
        );
        // resume() must not resurrect a dead reader.
        gate.resume();
        assert_eq!(*gate.lock(), ReaderState::Dead);
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

    // ── RFC-0012 P3: remote file edit — download / conflict-check / write-back ───────────────

    /// Wraps a real [`Vfs`] but strips `Caps::RENAME` (mirrors Docker/K8s-style backends with no
    /// atomic rename), so tests can exercise [`write_temp_to_remote`]'s direct-overwrite branch
    /// without a real non-local backend. `rename`/`local_path` are deliberately left at their
    /// trait defaults (`Unsupported`/`None`) — consistent with `caps()` no longer advertising it.
    struct NoRenameVfs(Arc<dyn Vfs>);

    impl cairn_vfs::CapabilityProvider for NoRenameVfs {
        fn caps(&self) -> Caps {
            self.0.caps().difference(Caps::RENAME)
        }
        fn caps_at(&self, path: &VfsPath) -> Caps {
            self.0.caps_at(path).difference(Caps::RENAME)
        }
    }

    #[async_trait::async_trait]
    impl Vfs for NoRenameVfs {
        fn scheme(&self) -> cairn_types::Scheme {
            self.0.scheme()
        }
        fn connection(&self) -> ConnectionId {
            self.0.connection()
        }
        fn list<'a>(
            &'a self,
            dir: &VfsPath,
            opts: ListOpts,
        ) -> futures::stream::BoxStream<'a, Result<ListPage, VfsError>> {
            self.0.list(dir, opts)
        }
        async fn stat(&self, path: &VfsPath) -> Result<cairn_types::Entry, VfsError> {
            self.0.stat(path).await
        }
        async fn open_read(
            &self,
            path: &VfsPath,
            range: Option<ByteRange>,
        ) -> Result<cairn_vfs::ReadHandle, VfsError> {
            self.0.open_read(path, range).await
        }
        async fn open_write(
            &self,
            path: &VfsPath,
            opts: cairn_vfs::WriteOpts,
        ) -> Result<cairn_vfs::WriteHandle, VfsError> {
            self.0.open_write(path, opts).await
        }
        async fn remove(&self, path: &VfsPath, recurse: Recurse) -> Result<(), VfsError> {
            self.0.remove(path, recurse).await
        }
    }

    #[cfg(unix)]
    #[test]
    fn new_remote_edit_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = new_remote_edit_dir().unwrap();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "remote-edit temp dir must be owner-only");
    }

    #[cfg(unix)]
    #[test]
    fn create_temp_edit_file_is_0600_and_excl() {
        use std::os::unix::fs::PermissionsExt;
        let dir = new_remote_edit_dir().unwrap();
        let path = create_temp_edit_file(dir.path(), "notes.txt").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "remote-edit temp file must be owner-only");
        // O_EXCL: a second attempt at the same name must fail rather than silently truncate.
        assert!(
            create_temp_edit_file(dir.path(), "notes.txt").is_err(),
            "create_new must refuse a colliding name"
        );
    }

    #[test]
    fn create_temp_edit_file_sanitizes_a_path_like_leaf() {
        let dir = new_remote_edit_dir().unwrap();
        // Defensive: even though `VfsPath::file_name()` never contains a separator, a
        // path-shaped leaf must still resolve to a plain filename *inside* `dir`, not escape it.
        let path = create_temp_edit_file(dir.path(), "../../etc/passwd").unwrap();
        assert_eq!(path.parent(), Some(dir.path()));
        assert_eq!(path.file_name().unwrap(), "passwd");
    }

    #[tokio::test]
    async fn hash_file_detects_content_change() {
        let dir = tempfile_dir();
        let path = dir.path().join("f");
        std::fs::write(&path, b"hello").unwrap();
        let h1 = hash_file(&path).await.unwrap();
        let h2 = hash_file(&path).await.unwrap();
        assert_eq!(h1, h2, "hashing twice with no change must be stable");
        std::fs::write(&path, b"hello!").unwrap();
        let h3 = hash_file(&path).await.unwrap();
        assert_ne!(h1, h3, "a content change must change the hash");
    }

    // ── `decide_remote_edit_outcome` (RFC-0012 P3 gate-fix regression) ───────────────────────

    const H0: ContentHash = [0u8; 32];
    const H1: ContentHash = [1u8; 32];
    const H2: ContentHash = [2u8; 32];

    #[test]
    fn decide_outcome_true_no_op_first_round() {
        // First edit round: `hash` and `download_hash` coincide (nothing has diverged yet). The
        // editor changes nothing at all.
        assert_eq!(
            decide_remote_edit_outcome(H0, H0, H0),
            RemoteEditOutcome::NoChange
        );
    }

    #[test]
    fn decide_outcome_modified_first_round() {
        // First edit round, content changed: download_hash == hash == H0, new content is H1.
        assert_eq!(
            decide_remote_edit_outcome(H1, H0, H0),
            RemoteEditOutcome::Modified {
                unchanged_since_reopen: false
            }
        );
    }

    /// Regression for the bug-bot HIGH finding: after a conflict, `KeepEditing` re-opens the temp
    /// file with `hash` set to the conflict-time content (H1, which already differs from the
    /// original download H0). If the user quits the editor **without making any further changes**
    /// (`new_hash` is still H1), the outcome must be `Modified` (so the edit is written back), never
    /// `NoChange` — the old, buggy comparison (`new_hash == hash`) would have said `NoChange` here
    /// and silently discarded an edit that was never written back.
    #[test]
    fn decide_outcome_keep_editing_unchanged_since_reopen_is_still_modified() {
        let outcome = decide_remote_edit_outcome(H1, H0, H1);
        assert_eq!(
            outcome,
            RemoteEditOutcome::Modified {
                unchanged_since_reopen: true
            },
            "an edit that stayed unchanged across a KeepEditing reopen, but still differs from the \
             original download, must be Modified (written back), never silently discarded as NoChange"
        );
    }

    /// A `KeepEditing` round where the user edits *further* (new content H2, differing from both
    /// the original download H0 and the conflict-time reopen baseline H1) must also be `Modified`,
    /// with `unchanged_since_reopen: false`.
    #[test]
    fn decide_outcome_keep_editing_further_change_is_modified() {
        let outcome = decide_remote_edit_outcome(H2, H0, H1);
        assert_eq!(
            outcome,
            RemoteEditOutcome::Modified {
                unchanged_since_reopen: false
            }
        );
    }

    /// If, across any number of `KeepEditing` rounds, the content is eventually restored to exactly
    /// the original download's bytes, that is a genuine (if unusual) no-op — nothing to write back.
    #[test]
    fn decide_outcome_restored_to_original_is_no_op() {
        let outcome = decide_remote_edit_outcome(H0, H0, H1);
        assert_eq!(outcome, RemoteEditOutcome::NoChange);
    }

    #[tokio::test]
    async fn begin_remote_edit_refuses_read_only_backend() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("f.txt"), b"data").unwrap();
        let inner: Arc<dyn Vfs> = Arc::new(LocalVfs::new(LEFT, dir.path()));
        struct ReadOnlyVfs(Arc<dyn Vfs>);
        impl cairn_vfs::CapabilityProvider for ReadOnlyVfs {
            fn caps(&self) -> Caps {
                self.0.caps().difference(Caps::WRITE)
            }
        }
        #[async_trait::async_trait]
        impl Vfs for ReadOnlyVfs {
            fn scheme(&self) -> cairn_types::Scheme {
                self.0.scheme()
            }
            fn connection(&self) -> ConnectionId {
                self.0.connection()
            }
            fn list<'a>(
                &'a self,
                dir: &VfsPath,
                opts: ListOpts,
            ) -> futures::stream::BoxStream<'a, Result<ListPage, VfsError>> {
                self.0.list(dir, opts)
            }
            async fn stat(&self, path: &VfsPath) -> Result<cairn_types::Entry, VfsError> {
                self.0.stat(path).await
            }
            async fn open_read(
                &self,
                path: &VfsPath,
                range: Option<ByteRange>,
            ) -> Result<cairn_vfs::ReadHandle, VfsError> {
                self.0.open_read(path, range).await
            }
            async fn open_write(
                &self,
                path: &VfsPath,
                opts: cairn_vfs::WriteOpts,
            ) -> Result<cairn_vfs::WriteHandle, VfsError> {
                self.0.open_write(path, opts).await
            }
        }
        let ro: Arc<dyn Vfs> = Arc::new(ReadOnlyVfs(inner));
        let (tx, mut rx) = mpsc::channel(4);
        begin_remote_edit(&ro, LEFT, VfsPath::parse("/f.txt").unwrap(), &tx).await;
        let ev = rx.recv().await.unwrap();
        assert!(
            matches!(ev, AppEvent::EditFinished { error: true, ref status } if status.contains("read-only")),
            "unexpected event"
        );
    }

    #[tokio::test]
    async fn begin_remote_edit_refuses_oversized_file() {
        let dir = tempfile_dir();
        let big = vec![0u8; usize::try_from(REMOTE_EDIT_MAX_BYTES).unwrap() + 1];
        std::fs::write(dir.path().join("big.bin"), &big).unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(LocalVfs::new(LEFT, dir.path()));
        let (tx, mut rx) = mpsc::channel(4);
        begin_remote_edit(&vfs, LEFT, VfsPath::parse("/big.bin").unwrap(), &tx).await;
        let ev = rx.recv().await.unwrap();
        assert!(
            matches!(ev, AppEvent::EditFinished { error: true, ref status } if status.contains("exceeds")),
            "unexpected event"
        );
    }

    #[tokio::test]
    async fn begin_remote_edit_succeeds_within_cap() {
        // `begin_remote_edit` fails fast if no editor resolves. On Windows `$EDITOR`/`$VISUAL`
        // unset yields an error (there's no `vi` fallback), so pin a resolvable editor to reach the
        // success path on every platform. Serialize against the other env-mutating editor tests.
        let _serialize = env_test_lock().lock().await;
        let _env = EnvVarGuard::set(&[("VISUAL", ""), ("EDITOR", "vi")]);
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("f.txt"), b"small").unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(LocalVfs::new(LEFT, dir.path()));
        let (tx, mut rx) = mpsc::channel(4);
        begin_remote_edit(&vfs, LEFT, VfsPath::parse("/f.txt").unwrap(), &tx).await;
        let ev = rx.recv().await.unwrap();
        assert!(
            matches!(ev, AppEvent::RemoteEditNeedsDownload { size: 5, .. }),
            "unexpected event"
        );
    }

    #[tokio::test]
    async fn unique_sibling_path_increments_on_collision() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("notes.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("notes (edited).txt"), b"b").unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(LocalVfs::new(LEFT, dir.path()));
        let sibling = unique_sibling_path(&vfs, &VfsPath::parse("/notes.txt").unwrap())
            .await
            .unwrap();
        assert_eq!(sibling.as_str(), "/notes (edited) (1).txt");
    }

    #[tokio::test]
    async fn write_temp_to_remote_stages_and_renames_when_backend_supports_rename() {
        let temp_dir = tempfile_dir();
        std::fs::write(temp_dir.path().join("f.txt"), b"edited content").unwrap();
        let temp_vfs: Arc<dyn Vfs> =
            Arc::new(LocalVfs::new(REMOTE_EDIT_TEMP_CONN_ID, temp_dir.path()));
        let remote_dir = tempfile_dir();
        std::fs::write(remote_dir.path().join("f.txt"), b"original").unwrap();
        let remote_vfs: Arc<dyn Vfs> = Arc::new(LocalVfs::new(RIGHT, remote_dir.path()));
        write_temp_to_remote(
            &remote_vfs,
            &temp_vfs,
            &VfsPath::parse("/f.txt").unwrap(),
            &VfsPath::parse("/f.txt").unwrap(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(remote_dir.path().join("f.txt")).unwrap(),
            b"edited content"
        );
        // No staging file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(remote_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".cairn-edit-tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging file left behind: {leftovers:?}"
        );
    }

    /// Regression (bug-bot MEDIUM): the RENAME path stages a *new* inode and renames it over the
    /// target, which silently loses the original file's mode (umask-default on the staged copy) —
    /// a 0600 secret would come back world-readable after one remote edit. `orig_perms` must be
    /// restored on the target after the rename.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_temp_to_remote_restores_original_mode_on_rename_path() {
        use std::os::unix::fs::PermissionsExt;
        let temp_dir = tempfile_dir();
        std::fs::write(temp_dir.path().join("f.txt"), b"edited content").unwrap();
        let temp_vfs: Arc<dyn Vfs> =
            Arc::new(LocalVfs::new(REMOTE_EDIT_TEMP_CONN_ID, temp_dir.path()));
        let remote_dir = tempfile_dir();
        let remote_file = remote_dir.path().join("f.txt");
        std::fs::write(&remote_file, b"original").unwrap();
        std::fs::set_permissions(&remote_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let remote_vfs: Arc<dyn Vfs> = Arc::new(LocalVfs::new(RIGHT, remote_dir.path()));
        write_temp_to_remote(
            &remote_vfs,
            &temp_vfs,
            &VfsPath::parse("/f.txt").unwrap(),
            &VfsPath::parse("/f.txt").unwrap(),
            Some(UnixPerms::from_mode(0o600)),
        )
        .await
        .unwrap();
        let mode = std::fs::metadata(&remote_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the original 0600 mode must survive a staging-rename write-back"
        );
    }

    #[tokio::test]
    async fn write_temp_to_remote_direct_overwrite_when_backend_lacks_rename() {
        let temp_dir = tempfile_dir();
        std::fs::write(temp_dir.path().join("f.txt"), b"edited content").unwrap();
        let temp_vfs: Arc<dyn Vfs> =
            Arc::new(LocalVfs::new(REMOTE_EDIT_TEMP_CONN_ID, temp_dir.path()));
        let remote_dir = tempfile_dir();
        std::fs::write(remote_dir.path().join("f.txt"), b"original").unwrap();
        let remote_vfs: Arc<dyn Vfs> = Arc::new(NoRenameVfs(Arc::new(LocalVfs::new(
            RIGHT,
            remote_dir.path(),
        ))));
        write_temp_to_remote(
            &remote_vfs,
            &temp_vfs,
            &VfsPath::parse("/f.txt").unwrap(),
            &VfsPath::parse("/f.txt").unwrap(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(remote_dir.path().join("f.txt")).unwrap(),
            b"edited content"
        );
    }

    #[tokio::test]
    async fn run_download_for_edit_downloads_and_hashes() {
        let remote_dir = tempfile_dir();
        std::fs::write(remote_dir.path().join("f.txt"), b"remote content").unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, remote_dir.path())))
            .await;
        let temp_dir = new_remote_edit_dir().unwrap();
        let temp_path = create_temp_edit_file(temp_dir.path(), "f.txt").unwrap();
        let ev = run_download_for_edit(
            &registry,
            1,
            LEFT,
            VfsPath::parse("/f.txt").unwrap(),
            RemoteVersion::Unknown,
            14,
            None,
            temp_path.clone(),
        )
        .await;
        let AppEvent::RemoteEditDownloaded {
            id,
            temp_path: tp,
            orig_size,
            download_hash,
            ..
        } = ev
        else {
            panic!("expected RemoteEditDownloaded");
        };
        assert_eq!(id, 1);
        assert_eq!(tp, temp_path);
        assert_eq!(orig_size, 14);
        assert_eq!(download_hash, hash_file(&temp_path).await.unwrap());
        assert_eq!(std::fs::read(&temp_path).unwrap(), b"remote content");
    }

    /// A fresh registry/temp file pair for the `run_writeback` tests below: a `LocalVfs` "remote"
    /// with `f.txt` = `orig_content`, and a temp file already containing `temp_content`.
    ///
    /// Returns the two `TempDir` RAII guards alongside the registry/path — the caller must bind
    /// (not `let _ =`) both for the whole test, or the directory they own is deleted the moment
    /// this function returns, out from under the test that hasn't run yet.
    async fn writeback_fixture(
        orig_content: &[u8],
        temp_content: &[u8],
    ) -> (
        VfsRegistry,
        PathBuf,
        cairn_types::Entry,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let remote_dir = tempfile_dir();
        std::fs::write(remote_dir.path().join("f.txt"), orig_content).unwrap();
        let registry = VfsRegistry::new();
        registry
            .insert(LEFT, Arc::new(LocalVfs::new(LEFT, remote_dir.path())))
            .await;
        let entry = registry
            .get(LEFT)
            .await
            .unwrap()
            .stat(&VfsPath::parse("/f.txt").unwrap())
            .await
            .unwrap();
        let temp_dir = new_remote_edit_dir().unwrap();
        let temp_path = create_temp_edit_file(temp_dir.path(), "f.txt").unwrap();
        std::fs::write(&temp_path, temp_content).unwrap();
        (registry, temp_path, entry, remote_dir, temp_dir)
    }

    #[tokio::test]
    async fn run_writeback_check_then_write_succeeds_when_unchanged() {
        let (registry, temp_path, entry, _remote_dir, _temp_dir) =
            writeback_fixture(b"original", b"edited").await;
        let v0 = RemoteVersion::from_entry(&entry);
        let ev = run_writeback(
            &registry,
            1,
            LEFT,
            VfsPath::parse("/f.txt").unwrap(),
            temp_path,
            v0,
            8,
            None,
            [0u8; 32],
            WriteBackMode::CheckThenWrite,
        )
        .await;
        assert!(
            matches!(ev, AppEvent::WriteBackDone { id: 1, .. }),
            "unexpected event"
        );
        let vfs = registry.get(LEFT).await.unwrap();
        let mut reader = vfs
            .open_read(&VfsPath::parse("/f.txt").unwrap(), None)
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, b"edited");
    }

    #[tokio::test]
    async fn run_writeback_check_then_write_detects_remote_drift() {
        let (registry, temp_path, _entry, _remote_dir, _temp_dir) =
            writeback_fixture(b"original", b"edited").await;
        // Deliberately stale baseline: the real remote reports `MTimeSize`, but this `v0` will
        // never `confirmed_equal` it (mismatched variant) — simulating "the remote changed".
        let v0 = RemoteVersion::ETag("stale-etag".to_owned());
        let ev = run_writeback(
            &registry,
            1,
            LEFT,
            VfsPath::parse("/f.txt").unwrap(),
            temp_path,
            v0,
            8,
            None,
            [0u8; 32],
            WriteBackMode::CheckThenWrite,
        )
        .await;
        assert!(
            matches!(
                ev,
                AppEvent::WriteBackConflict {
                    id: 1,
                    reason: cairn_core::WritebackConflictReason::RemoteChanged,
                    ..
                }
            ),
            "unexpected event"
        );
        // Nothing was written — the original content is untouched.
        let vfs = registry.get(LEFT).await.unwrap();
        let mut reader = vfs
            .open_read(&VfsPath::parse("/f.txt").unwrap(), None)
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, b"original");
    }

    #[tokio::test]
    async fn run_writeback_zero_length_guard_trips() {
        let (registry, temp_path, entry, _remote_dir, _temp_dir) =
            writeback_fixture(b"original", b"").await;
        let v0 = RemoteVersion::from_entry(&entry);
        let ev = run_writeback(
            &registry,
            1,
            LEFT,
            VfsPath::parse("/f.txt").unwrap(),
            temp_path,
            v0,
            8, // orig_size > 0, current temp content is empty
            None,
            [0u8; 32],
            WriteBackMode::CheckThenWrite,
        )
        .await;
        assert!(
            matches!(
                ev,
                AppEvent::WriteBackConflict {
                    reason: cairn_core::WritebackConflictReason::ZeroLengthGuard,
                    ..
                }
            ),
            "unexpected event"
        );
    }

    #[tokio::test]
    async fn run_writeback_force_overwrite_skips_both_guards() {
        let (registry, temp_path, _entry, _remote_dir, _temp_dir) =
            writeback_fixture(b"original", b"").await;
        // A stale/mismatched v0 AND a zero-length temp file — both guards would normally trip,
        // but `ForceOverwrite` (the user already confirmed) must skip them and just write.
        let ev = run_writeback(
            &registry,
            1,
            LEFT,
            VfsPath::parse("/f.txt").unwrap(),
            temp_path,
            RemoteVersion::Unknown,
            8,
            None,
            [0u8; 32],
            WriteBackMode::ForceOverwrite,
        )
        .await;
        assert!(
            matches!(ev, AppEvent::WriteBackDone { .. }),
            "unexpected event"
        );
        let vfs = registry.get(LEFT).await.unwrap();
        let stat = vfs.stat(&VfsPath::parse("/f.txt").unwrap()).await.unwrap();
        assert_eq!(stat.size, Some(0));
    }

    #[tokio::test]
    async fn run_writeback_save_as_sibling_leaves_original_untouched() {
        let (registry, temp_path, _entry, _remote_dir, _temp_dir) =
            writeback_fixture(b"original", b"edited").await;
        let ev = run_writeback(
            &registry,
            1,
            LEFT,
            VfsPath::parse("/f.txt").unwrap(),
            temp_path,
            RemoteVersion::Unknown,
            8,
            None,
            [0u8; 32],
            WriteBackMode::SaveAsSibling,
        )
        .await;
        let AppEvent::WriteBackDone { name, .. } = ev else {
            panic!("expected WriteBackDone");
        };
        assert_eq!(name, "f (edited).txt");
        let vfs = registry.get(LEFT).await.unwrap();
        // Original untouched.
        let mut reader = vfs
            .open_read(&VfsPath::parse("/f.txt").unwrap(), None)
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, b"original");
        // Sibling has the edited content.
        let mut reader = vfs
            .open_read(&VfsPath::parse("/f (edited).txt").unwrap(), None)
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, b"edited");
    }

    #[tokio::test]
    async fn run_writeback_missing_connection_reports_failure() {
        let registry = VfsRegistry::new();
        let ev = run_writeback(
            &registry,
            1,
            ConnectionId(999),
            VfsPath::parse("/f.txt").unwrap(),
            PathBuf::from("/tmp/does-not-matter"),
            RemoteVersion::Unknown,
            0,
            None,
            [0u8; 32],
            WriteBackMode::ForceOverwrite,
        )
        .await;
        assert!(
            matches!(ev, AppEvent::RemoteEditFailed { id: 1, ref status } if status.contains("connection")),
            "unexpected event"
        );
    }

    // ── RFC-0011 P6: test-connection + pin/hide persistence ────────────────────────────────────

    use crate::connect::descriptor::{ConnectionKey, DescriptorProvenance, Reachability};

    fn locked_opener() -> crate::connect::ConnectionOpener {
        crate::connect::ConnectionOpener::new(Arc::new(Broker::locked()))
    }

    fn profile_descriptor(scheme: &str) -> ConnectionDescriptor {
        ConnectionDescriptor {
            id: ConnectionId(42),
            key: ConnectionKey::Saved(uuid::Uuid::nil()),
            provenance: DescriptorProvenance::Saved {
                profile_id: uuid::Uuid::nil(),
            },
            scheme: scheme.to_owned(),
            display_name: "test-conn".to_owned(),
            target: OpenTarget::Profile(cairn_config::ConnectionProfile::new(scheme, "test-conn")),
            reachability: Reachability::Ready,
        }
    }

    #[tokio::test]
    async fn test_connection_effect_on_local_root_is_always_ok() {
        let opener = locked_opener();
        let desc = ConnectionDescriptor {
            id: ConnectionId(1),
            key: ConnectionKey::Builtin(PathBuf::from("/")),
            provenance: DescriptorProvenance::Builtin,
            scheme: "local".to_owned(),
            display_name: "local: /".to_owned(),
            target: OpenTarget::LocalRoot(PathBuf::from("/")),
            reachability: Reachability::Ready,
        };
        let result = run_test_connection_effect(&opener, ConnectionId(1), &desc).await;
        assert!(result.is_ok(), "a LocalRoot probe must always succeed");
    }

    #[tokio::test]
    async fn test_connection_effect_reuses_the_opener_and_never_registers_anything() {
        // "ftp" is a known-unsupported scheme for the opener (see connect::mod's own dispatch
        // tests) — deterministic in every feature configuration (lean or all-features), so this
        // test doesn't need to be gated on any backend feature.
        let opener = locked_opener();
        let desc = profile_descriptor("ftp");
        let result = run_test_connection_effect(&opener, ConnectionId(42), &desc).await;
        assert!(
            result.is_err(),
            "an unsupported scheme must surface as a probe failure, not silently succeed"
        );
        assert!(
            result.unwrap_err().contains("ftp"),
            "the error should name the scheme it dispatched on"
        );
    }

    #[test]
    fn save_theme_at_path_persists_and_preserves_other_settings() {
        let dir = tempfile_dir();
        let path = dir.path().join("cairn.toml");
        // Seed a config with an unrelated setting to confirm the round-trip leaves it untouched.
        let mut seed = cairn_config::Config::default();
        seed.connections
            .push(cairn_config::ConnectionProfile::new("local", "work"));
        seed.ui.theme = "dark".to_owned();
        seed.save(&path).unwrap();

        save_theme_at_path(&path, "mc").unwrap();
        let after = cairn_config::Config::load(&path).unwrap();
        assert_eq!(after.ui.theme, "mc", "the theme was persisted");
        assert_eq!(
            after.connections.len(),
            1,
            "an unrelated section must round-trip untouched"
        );

        // Switching again overwrites the previous value (not appended/duplicated).
        save_theme_at_path(&path, "nord").unwrap();
        assert_eq!(cairn_config::Config::load(&path).unwrap().ui.theme, "nord");
    }

    #[test]
    fn discovery_flag_at_path_sets_then_unsets_without_touching_unrelated_fields() {
        let dir = tempfile_dir();
        let path = dir.path().join("cairn.toml");
        // Seed a pre-existing profile so the round-trip can confirm it survives untouched.
        let mut seed = cairn_config::Config::default();
        seed.connections
            .push(cairn_config::ConnectionProfile::new("local", "work"));
        seed.save(&path).unwrap();

        let key = "docker:socket:default";
        set_discovery_flag_at_path(&path, DiscoveryFlag::Pinned, key, true).unwrap();
        let after_set = cairn_config::Config::load(&path).unwrap();
        assert_eq!(after_set.discovery.pinned, vec![key.to_owned()]);
        assert_eq!(
            after_set.connections.len(),
            1,
            "an unrelated section must round-trip untouched"
        );

        // Setting the same key again must not duplicate it.
        set_discovery_flag_at_path(&path, DiscoveryFlag::Pinned, key, true).unwrap();
        let after_dup = cairn_config::Config::load(&path).unwrap();
        assert_eq!(after_dup.discovery.pinned, vec![key.to_owned()]);

        // Unset removes it cleanly.
        set_discovery_flag_at_path(&path, DiscoveryFlag::Pinned, key, false).unwrap();
        let after_unset = cairn_config::Config::load(&path).unwrap();
        assert!(after_unset.discovery.pinned.is_empty());

        // Hidden is a completely independent list.
        set_discovery_flag_at_path(&path, DiscoveryFlag::Hidden, key, true).unwrap();
        let after_hide = cairn_config::Config::load(&path).unwrap();
        assert_eq!(after_hide.discovery.hidden, vec![key.to_owned()]);
        assert!(after_hide.discovery.pinned.is_empty());
    }

    #[test]
    fn discovery_flag_at_path_write_is_atomic() {
        let dir = tempfile_dir();
        let path = dir.path().join("cairn.toml");
        cairn_config::Config::default().save(&path).unwrap();
        set_discovery_flag_at_path(&path, DiscoveryFlag::Hidden, "kube:kubeconfig", true).unwrap();
        // Only the target file must remain — no stray temp file from the atomic rename.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("cairn.toml")]);
    }

    /// P6 gate fix: two config-flag writes racing on the same file must not lose an update. Before
    /// `CONFIG_WRITE_LOCK`, `Config::load → mutate → save` had no coordination, so two concurrent
    /// pin/hide toggles could interleave (both load the pre-change file, each save clobbers the
    /// other's in-memory mutation) even though each individual `save` is atomic — atomicity only
    /// guards against a *torn* file, not against *two* writers racing each other. Runs on a real
    /// multi-thread runtime so the two tasks can genuinely execute concurrently, not just be
    /// interleaved cooperatively on one thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_discovery_flag_writes_do_not_lose_an_update() {
        let dir = tempfile_dir();
        let path = dir.path().join("cairn.toml");
        cairn_config::Config::default().save(&path).unwrap();

        let path_a = path.clone();
        let path_b = path.clone();
        let a = tokio::spawn(async move {
            set_discovery_flag_at_path_locked(
                &path_a,
                DiscoveryFlag::Pinned,
                "docker:socket:default",
                true,
            )
            .await
        });
        let b = tokio::spawn(async move {
            set_discovery_flag_at_path_locked(
                &path_b,
                DiscoveryFlag::Hidden,
                "kube:kubeconfig",
                true,
            )
            .await
        });
        let (ra, rb) = tokio::join!(a, b);
        ra.unwrap().unwrap();
        rb.unwrap().unwrap();

        let loaded = cairn_config::Config::load(&path).unwrap();
        assert_eq!(
            loaded.discovery.pinned,
            vec!["docker:socket:default".to_owned()],
            "the first task's write must have survived the race"
        );
        assert_eq!(
            loaded.discovery.hidden,
            vec!["kube:kubeconfig".to_owned()],
            "the second task's write must have survived the race too — neither may clobber the other"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_theme_and_discovery_writes_do_not_lose_an_update() {
        // A theme save and a pin/hide save share `CONFIG_WRITE_LOCK`, so racing them must not clobber
        // either — parity with `concurrent_discovery_flag_writes_do_not_lose_an_update`.
        let dir = tempfile_dir();
        let path = dir.path().join("cairn.toml");
        cairn_config::Config::default().save(&path).unwrap();

        let path_a = path.clone();
        let path_b = path.clone();
        let a = tokio::spawn(async move { save_theme_at_path_locked(&path_a, "mc").await });
        let b = tokio::spawn(async move {
            set_discovery_flag_at_path_locked(&path_b, DiscoveryFlag::Pinned, "saved:x", true).await
        });
        let (ra, rb) = tokio::join!(a, b);
        ra.unwrap().unwrap();
        rb.unwrap().unwrap();

        let loaded = cairn_config::Config::load(&path).unwrap();
        assert_eq!(loaded.ui.theme, "mc", "the theme write survived the race");
        assert_eq!(
            loaded.discovery.pinned,
            vec!["saved:x".to_owned()],
            "the discovery-flag write survived the race too"
        );
    }
}
