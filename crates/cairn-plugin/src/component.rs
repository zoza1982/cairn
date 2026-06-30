//! Component-model plugin **backend** bridge (M8-3, foundation).
//!
//! Loads a WASM **component** that exports the `cairn:plugin/backend` interface (see RFC-0006 and
//! `wit/plugin.wit`) and exposes its non-streaming introspection/metadata calls (`scheme`,
//! `backend-caps`, `caps-at`, `stat`, `list-page`) behind a small host wrapper. Capabilities are
//! mapped to [`cairn_types::Caps`] so the eventual `PluginVfsBackend: Vfs` reads naturally.
//!
//! The granted `host` interface (log/now-secs; brokered fns deny-stubbed) is linked here, and
//! [`crate::backend::PluginVfsBackend`] exposes this as an async `Vfs` over a dedicated thread. The
//! streaming `read-stream`/`write-sink` resources and mutation calls are wrapped too (PR2/PR3).
//!
//! WASI is linked via the narrowed allow-list from [`crate::wasi_narrowing`] (RFC-0010 §1): only
//! `wasi:io/*`, `wasi:clocks/*`, and `wasi:random/*` are registered; sockets, filesystem, and CLI
//! are absent.  The poll and monotonic-clock stubs close the epoch-vs-blocking evasion gap
//! (RFC-0010 §2).  Deferred (M8-4): the real brokered host functions. Verified hermetically
//! against a committed guest fixture (CI needs no WASM toolchain).

use crate::{Limits, PluginError};
use cairn_broker_api::{CredentialAction, CredentialBroker};
use cairn_types::Caps;
use std::sync::Arc;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "backend-plugin",
    path: "wit",
});

use cairn::plugin::types::{Caps as Caps0, Entry, VfsError};
use exports::cairn::plugin::backend::ListPageResult;

// Re-exports so the `backend`/`bridge`/`http_fetch` modules can name the generated WIT types
// without re-running `bindgen!`.
// `WitHttpRequest`/`WitHttpResponse` are only used by `http_fetch.rs` (plugin-network feature).
#[cfg(feature = "plugin-network")]
pub(crate) use cairn::plugin::host::{
    HttpRequest as WitHttpRequest, HttpResponse as WitHttpResponse,
};
pub(crate) use cairn::plugin::types::{
    ByteRange as WitByteRange, Entry as WitEntry, EntryKind as WitEntryKind,
    VfsError as WitVfsError,
};
pub(crate) use exports::cairn::plugin::backend::ListPageResult as WitListPageResult;
pub(crate) use wasmtime::component::ResourceAny;

/// Store state for a component instance: the memory limiter plus an **ambient-authority-free** WASI
/// context, plus the per-plugin capability grants and brokered host function state.
///
/// A guest built against `std` references the `wasi:*` interfaces, so a subset must be linkable for
/// instantiation.  The context is **empty** — no preopened directories, no environment, null stdio,
/// no network — so the plugin has no ambient filesystem/secret/network access.  Only the RFC-0010
/// §1.2 allow-list is registered: `wasi:io/*`, `wasi:clocks/*`, and `wasi:random/*`.  Sockets,
/// filesystem, and CLI are absent from the linker; a component importing any of those fails
/// instantiation (default-deny).  The poll and monotonic-clock stubs close the epoch-evasion gap
/// (RFC-0010 §2).
///
/// The brokered host function fields (`network_grants`, `credential_grants`, `credential_broker`,
/// and the `plugin-network`-gated HTTP fields) are populated by
/// [`PluginComponent::instantiate_with_grants`] and default to empty/`None` in the no-grant path.
struct CompState {
    limits: StoreLimits,
    wasi: WasiCtx,
    table: ResourceTable,

    // ── Capability grants (RFC-0010 §3/§4) ─────────────────────────────────────────────────
    /// Hostnames this plugin may reach via `host::http-fetch`. Compared case-insensitively
    /// at call time; not normalized at storage time. Empty → deny-stub.
    network_grants: Vec<String>,
    /// Credential handle labels this plugin may use via `host::use-credential`. Empty → deny-stub.
    credential_grants: Vec<String>,
    /// The plugin's canonical name, recorded in audit-journal entries.
    plugin_name: String,
    /// Credential broker. `None` → use-credential is a deny-stub even with grants.
    credential_broker: Option<Arc<dyn CredentialBroker>>,

    // ── plugin-network: brokered HTTP client ────────────────────────────────────────────────
    /// A shared `reqwest::Client` configured with the plugin's redirect policy.
    /// `None` when the `plugin-network` feature is disabled or network grants are empty.
    #[cfg(feature = "plugin-network")]
    http_client: Option<Arc<reqwest::Client>>,
    /// Per-call HTTP resource limits (response size cap, timeouts).
    #[cfg(feature = "plugin-network")]
    http_limits: crate::http_fetch::HttpLimits,
    /// The tokio runtime handle captured at instantiation time, used to drive async reqwest
    /// futures from the synchronous plugin thread (`std::thread`, not a tokio task).
    #[cfg(feature = "plugin-network")]
    tokio_handle: Option<tokio::runtime::Handle>,
}

impl WasiView for CompState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// The granted `host` interface. `log`/`now-secs` are always safe (no I/O, no secrets).
// `http-fetch` and `use-credential` are capability-gated and broker-backed (RFC-0010 §3/§4).
// When the plugin has no matching grant, or when the broker is absent, the function returns
// an error string — the plugin instantiates but the call is denied at runtime.
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
                // `unwrap_or(0)` is unreachable: the range `0..=MAX_LOG` always includes `0`,
                // and `0` is always a UTF-8 char boundary, so `find` never returns `None`.
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

    /// Brokered HTTP fetch (RFC-0010 §3).
    ///
    /// The host performs the HTTP request; the guest never touches a socket. The call is
    /// rejected if `network_grants` is empty (no network grant), if the URL's hostname is not
    /// in the grant list, or if SSRF guards trigger. Requires the `plugin-network` feature.
    fn http_fetch(
        &mut self,
        req: cairn::plugin::host::HttpRequest,
    ) -> Result<cairn::plugin::host::HttpResponse, String> {
        // Gate: no network grant → deny without touching any network path.
        if self.network_grants.is_empty() {
            return Err("http-fetch: plugin has no network grant".to_owned());
        }

        #[cfg(feature = "plugin-network")]
        {
            // The HTTP client is `None` either because:
            //  a) network grants are non-empty but the tokio runtime wasn't present at
            //     instantiation time (should be rare — the binary always has one), OR
            //  b) `build_client` failed at instantiation.
            let client =
                match self.http_client.as_ref() {
                    Some(c) => c.clone(),
                    None => return Err(
                        "http-fetch: HTTP client unavailable (no tokio runtime at instantiation)"
                            .to_owned(),
                    ),
                };
            let handle = match self.tokio_handle.as_ref() {
                Some(h) => h.clone(),
                None => return Err("http-fetch: no tokio runtime handle".to_owned()),
            };
            let grants = self.network_grants.clone();
            let limits = self.http_limits;
            // Drive the async fetch from this synchronous plugin thread.
            // Sanitize error strings (RFC §3.4): strip control chars + cap length before
            // the string crosses the WIT ABI back to the guest, consistent with other host fns.
            handle
                .block_on(crate::http_fetch::do_http_fetch(
                    &req, &grants, &client, limits,
                ))
                .map_err(crate::bridge::sanitize_msg)
        }

        #[cfg(not(feature = "plugin-network"))]
        {
            let _ = req;
            Err("http-fetch: the plugin-network feature is not enabled in this build".to_owned())
        }
    }

    /// Brokered credential use (RFC-0010 §4).
    ///
    /// The plugin names a credential by opaque handle (a vault label). The host:
    /// 1. Verifies the handle is in the plugin's `credentials` grant.
    /// 2. Parses the action string into the closed vocabulary (`bearer-token`,
    ///    `basic-auth-header`). An unrecognised string is rejected before touching the vault.
    /// 3. Calls the broker, which resolves the secret internally, derives the ephemeral
    ///    artifact, zeroizes the secret, and returns the artifact.
    ///
    /// The raw `CredentialSecret` never reaches this method — it stays in the broker stack
    /// frame. This method returns only the artifact string or a secret-free error.
    fn use_credential(&mut self, handle: String, action: String) -> Result<String, String> {
        // Gate 1: credential not in approved grant list.
        if !self.credential_grants.contains(&handle) {
            // Do NOT reveal the grant list in the error.
            return Err("use-credential: handle not in credentials grant".to_owned());
        }

        // Gate 2: unknown action — closed vocabulary. Reject before touching the vault.
        let cred_action = CredentialAction::parse(&action).ok_or_else(|| {
            format!(
                "use-credential: unknown action '{action}' \
                 (accepted: 'bearer-token', 'basic-auth-header')"
            )
        })?;

        // Gate 3: no broker wired in (vault locked at instantiation, or not injected in tests).
        let broker = self.credential_broker.as_ref().ok_or_else(|| {
            "use-credential: no credential broker available (vault may be locked)".to_owned()
        })?;

        // Delegate to the broker. The broker resolves the secret, performs the action,
        // zeroizes the secret, journals the event (no secret material), and returns the
        // ephemeral artifact. `CredentialBrokerError` is secret-free by design.
        //
        // NOTE (SEC-10): credential handle grant lookup is case-sensitive (exact opaque-label
        // match). The manifest grant label must exactly match the vault entry label — the
        // comparison intentionally fails closed for any case variant.
        broker
            .use_credential(&self.plugin_name, &handle, &cred_action)
            .map_err(|e| crate::bridge::sanitize_msg(format!("use-credential failed: {e}")))
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
        // Zero-grant path: no network grant, no credentials, no broker. Used for fixture tests and
        // untrusted-plugin sandbox verification; brokered host fns return deny errors.
        Self::instantiate_with_grants(
            engine,
            bytes,
            limits,
            crate::PluginHostConfig {
                grants: crate::PluginGrants::default(),
                plugin_name: String::new(),
                credential_broker: None,
            },
        )
    }

    /// Instantiate a component with explicit capability grants and a credential broker
    /// (RFC-0010 §3/§4).
    ///
    /// This is the production entry point. `config.grants.network` controls which hostnames
    /// `http-fetch` may contact; `config.grants.credentials` controls which vault handles
    /// `use-credential` may resolve. Both default to empty (deny-all) so callers must
    /// explicitly grant capabilities.
    ///
    /// # Errors
    /// [`PluginError::Compile`] if the bytes are not a valid component, or
    /// [`PluginError::Instantiate`] if an unsatisfied import exists or fuel/component-model
    /// is not enabled on `engine`.
    pub fn instantiate_with_grants(
        engine: &Engine,
        bytes: &[u8],
        limits: Limits,
        config: crate::PluginHostConfig,
    ) -> Result<Self, PluginError> {
        let component = Component::from_binary(engine, bytes)
            .map_err(|e| PluginError::Compile(e.to_string()))?;
        let mut linker: Linker<CompState> = Linker::new(engine);
        // The dedicated-thread `Vfs` bridge makes synchronous guest calls, so the sync WASI linker
        // is correct (no async linker needed).
        //
        // SECURITY (RFC-0010 §1 + §2): Only the RFC-0010 allow-list is registered here.
        // Sockets, filesystem, and CLI are absent from the linker (default-deny).  The
        // `wasi:io/poll` and `wasi:clocks/monotonic-clock` stubs return immediately, so a
        // malicious guest cannot park this thread inside a native Tokio frame that epoch
        // cannot interrupt.  The epoch deadline now reliably bounds wall-clock time even
        // for a guest that tries the blocking-evasion attack described in `crate::epoch`.
        crate::wasi_narrowing::add_narrowed_wasi_to_linker(&mut linker)?;
        // The granted `host` interface: log/now-secs always; http-fetch/use-credential
        // capability-gated (by grants in CompState, not by linker presence — the guest
        // always instantiates but gets a runtime deny if grants are absent).
        cairn::plugin::host::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
            &mut linker,
            |s| s,
        )
        .map_err(|e| PluginError::Instantiate(e.to_string()))?;

        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes)
            .build();

        // SECURITY (release-gating for M8-5 / PR-C): before the plugin loader (PR-C) makes
        // this function reachable from the binary with untrusted plugins and grant-bearing
        // config, the DNS rebinding TOCTOU window in `check_ssrf_via_dns` MUST be closed by
        // pinning each connection to the `SocketAddr` set validated at pre-flight time via a
        // custom `reqwest::dns::Resolve` override that re-validates the pinned IP at connect.
        // See `http_fetch::check_ssrf_via_dns` for the full security note.
        // Bundle SEC-8 (6to4/Teredo/NAT64 embedded-v4 prefix blocks) with that work.
        //
        // Build the HTTP client when the plugin-network feature is enabled and network grants
        // are non-empty. The tokio runtime handle is captured here (on the calling async task
        // or thread); it is later used by the synchronous plugin thread to drive async reqwest
        // futures via `Handle::block_on`.
        #[cfg(feature = "plugin-network")]
        let (http_client, http_limits, tokio_handle) = {
            let h_limits = crate::http_fetch::HttpLimits::default();
            let handle = tokio::runtime::Handle::try_current().ok();
            let client = if !config.grants.network.is_empty() && handle.is_some() {
                match crate::http_fetch::build_client(h_limits) {
                    Ok(c) => Some(Arc::new(c)),
                    Err(e) => {
                        // Log and degrade gracefully — plugin instantiates but http-fetch will
                        // return an error. The plugin author will see the error at call time.
                        tracing::warn!(
                            target: "cairn_plugin",
                            plugin = %config.plugin_name,
                            "failed to build HTTP client for plugin: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };
            (client, h_limits, handle)
        };

        let mut store = Store::new(
            engine,
            CompState {
                limits: store_limits,
                // Empty context: no preopened dirs, env, stdio, or network.
                // HAZARD (PR-B): `wasi:io/streams` blocking methods (blocking-read,
                // blocking-write-and-flush, …) can still park the thread inside
                // a native Tokio frame that epoch cannot interrupt, if a guest
                // obtains a stream via a host-brokered resource.  With an empty
                // context, no streams are accessible to a plugin today.  PR-B
                // (M8-5) will stub the blocking methods with ImmediatelyReady semantics.
                wasi: WasiCtx::builder().build(),
                table: ResourceTable::new(),
                network_grants: config.grants.network,
                credential_grants: config.grants.credentials,
                plugin_name: config.plugin_name,
                credential_broker: config.credential_broker,
                #[cfg(feature = "plugin-network")]
                http_client,
                #[cfg(feature = "plugin-network")]
                http_limits,
                #[cfg(feature = "plugin-network")]
                tokio_handle,
            },
        );
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        // Epoch interruption is enabled on the engine (see `engine_config`), so a deadline MUST be
        // armed or the guest traps on its first instruction. The bridge re-arms it before every op;
        // this initial arm covers direct (non-bridge) calls too. `.max(1)`: 0 would trap immediately.
        store.set_epoch_deadline(limits.max_call_ticks.max(1));
        let bindings = BackendPlugin::instantiate(&mut store, &component, &linker)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        Ok(Self { store, bindings })
    }

    /// Re-arm both per-call budgets (fuel + the wall-clock epoch deadline) before a guest op, so each
    /// op gets its own budget rather than sharing one cumulative pool across the instance lifetime.
    /// The epoch deadline only bites while an [`EpochTicker`](crate::EpochTicker) advances the
    /// engine's epoch.
    pub(crate) fn arm(&mut self, limits: Limits) {
        // `set_fuel` fails only when fuel metering is disabled on the engine. `engine_config`
        // enables it unconditionally, so this should never fire. Log a warning rather than
        // silently discarding the error — a misconfigured engine is a programming error.
        if let Err(e) = self.store.set_fuel(limits.fuel) {
            tracing::warn!(
                target: "cairn_plugin",
                "arm: set_fuel failed (engine misconfiguration?): {e}"
            );
        }
        // `.max(1)`: a 0-tick deadline would trap the very next op (deadline == current epoch).
        self.store.set_epoch_deadline(limits.max_call_ticks.max(1));
    }

    /// The engine backing this instance — used to spawn an [`EpochTicker`](crate::EpochTicker).
    pub(crate) fn engine(&self) -> &Engine {
        self.store.engine()
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

    /// Open a write sink; returns the owned guest resource handle on success.
    pub(crate) fn open_write(
        &mut self,
        path: &str,
        overwrite: bool,
        size_hint: Option<u64>,
    ) -> Result<Result<ResourceAny, WitVfsError>, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .call_open_write(&mut self.store, path, overwrite, size_hint)
            .map_err(trap)
    }

    /// Write the next chunk to a write sink.
    pub(crate) fn write_chunk(
        &mut self,
        sink: ResourceAny,
        chunk: &[u8],
    ) -> Result<Result<(), WitVfsError>, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .write_sink()
            .call_write_chunk(&mut self.store, sink, chunk)
            .map_err(trap)
    }

    /// Commit a write sink and free the guest resource, returning the resulting entry.
    pub(crate) fn finish_write(
        &mut self,
        sink: ResourceAny,
    ) -> Result<Result<Entry, WitVfsError>, PluginError> {
        let r = self
            .bindings
            .cairn_plugin_backend()
            .write_sink()
            .call_finish(&mut self.store, sink)
            .map_err(trap);
        let _ = sink.resource_drop(&mut self.store);
        r
    }

    /// Abort a write sink (discard partial data) and free the guest resource.
    pub(crate) fn abort_write(&mut self, sink: ResourceAny) {
        let _ = self
            .bindings
            .cairn_plugin_backend()
            .write_sink()
            .call_abort(&mut self.store, sink);
        let _ = sink.resource_drop(&mut self.store);
    }

    /// Create a directory.
    pub(crate) fn create_dir(
        &mut self,
        path: &str,
    ) -> Result<Result<(), WitVfsError>, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .call_create_dir(&mut self.store, path)
            .map_err(trap)
    }

    /// Remove an entry (optionally recursively).
    pub(crate) fn remove(
        &mut self,
        path: &str,
        recursive: bool,
    ) -> Result<Result<(), WitVfsError>, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .call_remove(&mut self.store, path, recursive)
            .map_err(trap)
    }

    /// Rename/move an entry.
    pub(crate) fn rename(
        &mut self,
        src: &str,
        dst: &str,
    ) -> Result<Result<(), WitVfsError>, PluginError> {
        self.bindings
            .cairn_plugin_backend()
            .call_rename(&mut self.store, src, dst)
            .map_err(trap)
    }
}

/// A [`Config`] with the settings [`PluginComponent::instantiate`] requires: the component model,
/// fuel metering, and **epoch interruption** (the wall-clock backstop; see
/// [`EpochTicker`](crate::EpochTicker)). Pass it to [`Engine::new`].
///
/// NB: because epoch interruption is on, every store from this engine traps immediately unless a
/// deadline is armed — `PluginComponent::instantiate` does so (and the bridge re-arms per op).
#[must_use]
pub fn engine_config() -> Config {
    let mut cfg = Config::new();
    cfg.consume_fuel(true);
    cfg.wasm_component_model(true);
    cfg.epoch_interruption(true);
    cfg
}

/// Map a guest call error into a [`PluginError`], distinguishing fuel exhaustion and the wall-clock
/// (epoch) deadline from a crash (mirrors the core-module path's `map_call_err`) so callers can tell
/// "resource-killed" from "trapped".
fn trap(e: wasmtime::Error) -> PluginError {
    if let Some(trap) = e.downcast_ref::<Trap>() {
        if *trap == Trap::OutOfFuel {
            return PluginError::OutOfFuel;
        }
        if *trap == Trap::Interrupt {
            return PluginError::Timeout;
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
        assert!(caps.contains(Caps::WRITE) && caps.contains(Caps::RENAME));
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
