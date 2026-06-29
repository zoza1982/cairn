//! Component-model plugin **backend** bridge (M8-3, foundation).
//!
//! Loads a WASM **component** that exports the `cairn:plugin/backend` interface (see RFC-0006 and
//! `wit/plugin.wit`) and exposes its non-streaming introspection/metadata calls (`scheme`,
//! `backend-caps`, `caps-at`, `stat`, `list-page`) behind a small host wrapper. Capabilities are
//! mapped to [`cairn_types::Caps`] so the eventual `PluginVfsBackend: Vfs` reads naturally.
//!
//! The granted `host` interface (log/now-secs; brokered fns deny-stubbed) is linked here, and
//! [`crate::backend::PluginVfsBackend`] exposes this as an async `Vfs` over a dedicated thread.
//! Deferred (M8-3b PR2/PR3): the streaming `read-stream`/`write-sink` resources and mutations, then
//! an epoch deadline + the real brokered host functions (M8-4). Verified hermetically against a
//! committed guest fixture (CI needs no WASM toolchain).

use crate::{Limits, PluginError};
use cairn_types::Caps;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "backend-plugin",
    path: "wit",
});

use cairn::plugin::types::{Caps as Caps0, Entry, VfsError};
use exports::cairn::plugin::backend::ListPageResult;

// Re-exports so the `backend`/`bridge` modules can name the generated WIT types without re-running
// `bindgen!`.
pub(crate) use cairn::plugin::types::{
    ByteRange as WitByteRange, Entry as WitEntry, EntryKind as WitEntryKind,
    VfsError as WitVfsError,
};
pub(crate) use exports::cairn::plugin::backend::ListPageResult as WitListPageResult;
pub(crate) use wasmtime::component::ResourceAny;

/// Store state for a component instance: the memory limiter plus an **ambient-authority-free** WASI
/// context.
///
/// A guest built against `std` references the `wasi:*` interfaces, so they must be linkable for
/// instantiation. The context is **empty** — no preopened directories, no environment, null stdio,
/// no network — so the plugin has no ambient filesystem/secret/network access (clocks and entropy
/// remain functional; harmless without a network). It is not "no WASI": the interfaces are linked but
/// grant nothing. Capability-gating which WASI a plugin may use, per its manifest grants — and
/// narrowing the linked subset — is future work (M8-4/M8-5).
struct CompState {
    limits: StoreLimits,
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for CompState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// The granted `host` interface. `log`/`now-secs` are always safe (no I/O, no secrets). The brokered
// `http-fetch`/`use-credential` are deny-stubs here — a guest importing them instantiates but the call
// returns an error; the real broker-backed implementations (grant-gated) are M8-4/M8-5.
impl cairn::plugin::host::Host for CompState {
    fn log(&mut self, level: u8, msg: String) {
        let level = match level {
            0 => tracing::Level::ERROR,
            1 => tracing::Level::WARN,
            2 => tracing::Level::INFO,
            _ => tracing::Level::DEBUG,
        };
        // Cap the untrusted guest message: a guest could pass a multi-MiB string (bounded only by
        // its 16 MiB memory), forcing a large host copy + log record per call.
        const MAX_LOG: usize = 4096;
        let msg = if msg.len() > MAX_LOG {
            // Truncate at a UTF-8 char boundary at or below the cap (plain `truncate` would panic).
            let end = (0..=MAX_LOG)
                .rev()
                .find(|&i| msg.is_char_boundary(i))
                .unwrap_or(0);
            format!("{}…[truncated]", &msg[..end])
        } else {
            msg
        };
        // The guest message is untrusted; logged as data, never interpolated as a format string.
        match level {
            tracing::Level::ERROR => tracing::error!(target: "plugin", "{msg}"),
            tracing::Level::WARN => tracing::warn!(target: "plugin", "{msg}"),
            tracing::Level::INFO => tracing::info!(target: "plugin", "{msg}"),
            _ => tracing::debug!(target: "plugin", "{msg}"),
        }
    }

    fn now_secs(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn http_fetch(
        &mut self,
        _req: cairn::plugin::host::HttpRequest,
    ) -> Result<cairn::plugin::host::HttpResponse, String> {
        Err("http-fetch requires a network grant (M8-4)".to_owned())
    }

    fn use_credential(&mut self, _handle: String, _action: String) -> Result<String, String> {
        Err("use-credential requires a credentials grant (M8-4)".to_owned())
    }
}

/// A loaded backend-plugin component plus its store. Not `Sync`; the eventual `Vfs` bridge will own
/// this on a dedicated thread (a `wasmtime::Store` is `!Sync`).
pub struct PluginComponent {
    store: Store<CompState>,
    bindings: BackendPlugin,
}

impl PluginComponent {
    /// Instantiate a backend-plugin component from its raw bytes, under the given [`Limits`].
    ///
    /// `engine` must be built from [`engine_config`] (component model + fuel metering enabled). Only
    /// WASI (with an empty context) is linked; the granted `host` imports are wired in the follow-up,
    /// so a fixture/plugin that imports no `host` function instantiates here.
    ///
    /// # Errors
    /// [`PluginError::Compile`] if the bytes aren't a valid component, or [`PluginError::Instantiate`]
    /// if instantiation fails (e.g. an unsatisfied import or fuel/component-model not enabled on
    /// `engine`).
    pub fn instantiate(engine: &Engine, bytes: &[u8], limits: Limits) -> Result<Self, PluginError> {
        let component = Component::from_binary(engine, bytes)
            .map_err(|e| PluginError::Compile(e.to_string()))?;
        let mut linker: Linker<CompState> = Linker::new(engine);
        // The dedicated-thread `Vfs` bridge makes synchronous guest calls, so the sync WASI linker
        // is correct (no async linker needed).
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        // The granted `host` interface (log/now-secs real; brokered fns deny-stubbed — see `Host`).
        cairn::plugin::host::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes)
            .build();
        let mut store = Store::new(
            engine,
            CompState {
                limits: store_limits,
                wasi: WasiCtx::builder().build(),
                table: ResourceTable::new(),
            },
        );
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        let bindings = BackendPlugin::instantiate(&mut store, &component, &linker)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        Ok(Self { store, bindings })
    }

    /// Reset the fuel budget. The dedicated-thread bridge calls this before each `Vfs` op so every
    /// call gets its own budget rather than sharing one cumulative pool across the instance lifetime.
    pub(crate) fn refuel(&mut self, fuel: u64) {
        let _ = self.store.set_fuel(fuel);
    }

    /// The URI scheme this backend serves (e.g. `"mycloud"`).
    ///
    /// # Errors
    /// [`PluginError::Trap`] if the guest traps.
    pub fn scheme(&mut self) -> Result<String, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .call_scheme(&mut self.store)
            .map_err(trap)
    }

    /// The backend-wide capabilities, mapped to [`cairn_types::Caps`].
    ///
    /// # Errors
    /// [`PluginError::Trap`] if the guest traps.
    pub fn caps(&mut self) -> Result<Caps, PluginError> {
        let caps = self
            .bindings
            .cairn_plugin_backend()
            .call_backend_caps(&mut self.store)
            .map_err(trap)?;
        Ok(map_caps(caps))
    }

    /// Capabilities at a specific path.
    ///
    /// # Errors
    /// [`PluginError::Trap`] if the guest traps.
    pub fn caps_at(&mut self, path: &str) -> Result<Caps, PluginError> {
        let caps = self
            .bindings
            .cairn_plugin_backend()
            .call_caps_at(&mut self.store, path)
            .map_err(trap)?;
        Ok(map_caps(caps))
    }

    /// Fetch metadata for a single path. The inner `Result` is the guest's `vfs-error`.
    ///
    /// # Errors
    /// [`PluginError::Trap`] if the guest traps.
    pub fn stat(&mut self, path: &str) -> Result<Result<Entry, VfsError>, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .call_stat(&mut self.store, path)
            .map_err(trap)
    }

    /// List one page of a directory.
    ///
    /// # Errors
    /// [`PluginError::Trap`] if the guest traps.
    pub fn list_page(
        &mut self,
        dir: &str,
        cursor: Option<&str>,
        include_hidden: bool,
    ) -> Result<Result<ListPageResult, VfsError>, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .call_list_page(&mut self.store, dir, cursor, include_hidden)
            .map_err(trap)
    }

    /// Open a read stream; returns the owned guest resource handle on success.
    pub(crate) fn open_read(
        &mut self,
        path: &str,
        range: Option<WitByteRange>,
    ) -> Result<Result<ResourceAny, WitVfsError>, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .call_open_read(&mut self.store, path, range)
            .map_err(trap)
    }

    /// Read the next chunk from a read stream (empty = EOF).
    pub(crate) fn read_chunk(
        &mut self,
        stream: ResourceAny,
        max_bytes: u32,
    ) -> Result<Result<Vec<u8>, WitVfsError>, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .read_stream()
            .call_read_chunk(&mut self.store, stream, max_bytes)
            .map_err(trap)
    }

    /// Close a read stream and free the guest resource.
    pub(crate) fn close_read(&mut self, stream: ResourceAny) {
        let _ = self
            .bindings
            .cairn_plugin_backend()
            .read_stream()
            .call_close(&mut self.store, stream);
        let _ = stream.resource_drop(&mut self.store);
    }
}

/// A [`Config`] with the settings [`PluginComponent::instantiate`] requires: the component model and
/// fuel metering. Pass it to [`Engine::new`].
#[must_use]
pub fn engine_config() -> Config {
    let mut cfg = Config::new();
    cfg.consume_fuel(true);
    cfg.wasm_component_model(true);
    cfg
}

/// Map a guest call error into a [`PluginError`], distinguishing fuel exhaustion from a crash (mirrors
/// the core-module path's `map_call_err`) so callers can tell "resource-killed" from "trapped".
fn trap(e: wasmtime::Error) -> PluginError {
    if let Some(trap) = e.downcast_ref::<Trap>() {
        if *trap == Trap::OutOfFuel {
            return PluginError::OutOfFuel;
        }
        return PluginError::Trap(trap.to_string());
    }
    PluginError::Trap(e.to_string())
}

/// Map the component `caps` flags to [`cairn_types::Caps`]. Bit identity is guaranteed by RFC-0006
/// (the WIT `flags` order mirrors `Caps`), but we map field-by-field so a future reordering is caught
/// at compile time rather than silently mis-mapping.
pub(crate) fn map_caps(c: Caps0) -> Caps {
    let mut out = Caps::empty();
    // NB: `Caps::LOCAL_PATH` has no WIT counterpart by design — it is host-only (gates
    // `Vfs::local_path` for the local backend) and must never be grantable to a sandboxed plugin.
    // `map_caps_covers_every_wit_flag` asserts every WIT flag below is mapped.
    let pairs = [
        (Caps0::LIST_DIR, Caps::LIST),
        (Caps0::READ, Caps::READ),
        (Caps0::WRITE, Caps::WRITE),
        (Caps0::CREATE_DIR, Caps::CREATE_DIR),
        (Caps0::DELETE, Caps::DELETE),
        (Caps0::RENAME, Caps::RENAME),
        (Caps0::RENAME_ATOMIC, Caps::RENAME_ATOMIC),
        (Caps0::COPY_SERVER, Caps::COPY_SERVER),
        (Caps0::CHMOD, Caps::CHMOD),
        (Caps0::CHOWN, Caps::CHOWN),
        (Caps0::SYMLINK, Caps::SYMLINK),
        (Caps0::APPEND, Caps::APPEND),
        (Caps0::RANDOM_READ, Caps::RANDOM_READ),
        (Caps0::MULTIPART, Caps::MULTIPART),
        (Caps0::VERSIONS, Caps::VERSIONS),
        (Caps0::WATCH, Caps::WATCH),
        (Caps0::SEARCH_CONTENT, Caps::SEARCH_CONTENT),
    ];
    for (from, to) in pairs {
        if c.contains(from) {
            out |= to;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::cairn::plugin::types::EntryKind;
    use super::*;

    /// The guest fixture component, built once locally (`cargo component build`) and committed so CI
    /// needs no WASM toolchain. See `tests/fixture-guest/`.
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/backend.wasm");

    fn engine() -> Engine {
        Engine::new(&engine_config()).unwrap()
    }

    fn load() -> PluginComponent {
        PluginComponent::instantiate(&engine(), FIXTURE, Limits::default()).unwrap()
    }

    #[test]
    fn fixture_introspection() {
        let mut p = load();
        assert_eq!(p.scheme().unwrap(), "fixture");
        let caps = p.caps().unwrap();
        assert!(caps.contains(Caps::LIST) && caps.contains(Caps::READ));
        assert!(!caps.contains(Caps::WRITE));
    }

    #[test]
    fn fixture_stat_and_list() {
        let mut p = load();
        let entry = p.stat("/dir/a.txt").unwrap().expect("a.txt exists");
        assert_eq!(entry.name, "a.txt");
        assert!(matches!(entry.kind, EntryKind::File));
        assert_eq!(entry.size, Some(5));

        let page = p
            .list_page("/dir", None, false)
            .unwrap()
            .expect("/dir lists");
        assert!(page.done);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].name, "a.txt");
    }

    #[test]
    fn fixture_stat_missing_is_not_found() {
        let mut p = load();
        match p.stat("/nope").unwrap() {
            Err(VfsError::NotFound(path)) => assert_eq!(path, "/nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn fuel_exhaustion_maps_to_out_of_fuel() {
        // Fuel is cumulative across calls in one store and never refilled here, so repeated calls
        // deterministically exhaust a small budget — and the error must be `OutOfFuel`, not `Trap`.
        let mut p = PluginComponent::instantiate(
            &engine(),
            FIXTURE,
            Limits {
                max_memory_bytes: 16 * 1024 * 1024,
                fuel: 200_000,
                ..Limits::default()
            },
        )
        .unwrap();
        for _ in 0..100_000 {
            if let Err(e) = p.list_page("/dir", None, false) {
                assert!(
                    matches!(e, PluginError::OutOfFuel),
                    "fuel exhaustion must map to OutOfFuel, got {e:?}"
                );
                return;
            }
        }
        panic!("expected the guest to exhaust its fuel budget");
    }

    #[test]
    fn map_caps_covers_every_wit_flag() {
        // Every WIT `caps` flag must be translated; the host-only `LOCAL_PATH` has no WIT counterpart.
        // This breaks if either side grows a flag without updating `map_caps`.
        let mapped = map_caps(Caps0::all());
        assert_eq!(mapped, Caps::all() & !Caps::LOCAL_PATH);
    }

    #[test]
    fn fixture_wit_matches_the_host_wit() {
        // The fixture guest copies the host WIT (it's a detached workspace). Keep them byte-identical
        // so the fixture always tests against the same interface the host bindings are generated from.
        let host = include_str!("../wit/plugin.wit");
        let guest = include_str!("../tests/fixture-guest/wit/plugin.wit");
        assert_eq!(host, guest, "host and fixture-guest WIT must stay in sync");
    }
}
