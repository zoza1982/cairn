//! WASI subset narrowing for the plugin sandbox (RFC-0010 §1 and §2).
//!
//! Replaces the blanket `wasmtime_wasi::p2::add_to_linker_sync` with an explicit
//! **allow-list** of only the WASI interfaces a backend plugin needs, and substitutes
//! non-blocking stub implementations for the two interfaces that would otherwise allow a
//! guest to park the plugin thread indefinitely:
//!
//! - `wasi:io/poll` — the poll stub returns every pollable as immediately ready without
//!   suspending the host thread or entering the Tokio scheduler.
//! - `wasi:clocks/monotonic-clock` — `subscribe-duration` and `subscribe-instant` return
//!   immediately-ready pollables; `now` and `resolution` still read the real clock.
//!
//! All socket, filesystem, and CLI interfaces are **absent** from the linker:
//!
//! | Dropped group | Rationale |
//! |---|---|
//! | `wasi:sockets/*` | No raw network access; brokered `host::http-fetch` (PR-B) replaces it. |
//! | `wasi:filesystem/*` | Plugins access storage via the VFS ABI, not the host filesystem. |
//! | `wasi:cli/*` | `exit` would let a guest call `proc_exit`; `stdio` is replaced by `host::log`. |
//!
//! A component that imports any excluded interface fails instantiation with a clear
//! "unknown import" message — the **default-deny** posture in action.
//!
//! # Blocking-evasion closure
//!
//! Wasmtime's epoch mechanism only fires at instrumented points in *guest WebAssembly*
//! (loop back-edges, function entries). It cannot interrupt control that is parked inside
//! a **native host frame**. Before this module existed, a malicious guest could call
//! `monotonic-clock::subscribe-duration(far_future)` followed by `io/poll::poll` and
//! park the plugin thread inside a Tokio `sleep_until` call for an arbitrary duration —
//! invisible to both epoch and fuel. This stub closes that gap: `subscribe-duration`
//! (and `subscribe-instant`) now push an `ImmediatelyReady` pollable into the resource
//! table (no Tokio sleep), and `poll` returns all pollables as ready on the first call
//! (no `PollList` future, no Tokio scheduling). The epoch deadline now reliably bounds
//! the worst-case wall-clock time a plugin call can consume.

use async_trait::async_trait;
use wasmtime::component::{HasData, Linker, Resource, ResourceTable};
use wasmtime_wasi::clocks::{WasiClocks, WasiClocksCtxView, WasiClocksView};
use wasmtime_wasi::p2::bindings::clocks::monotonic_clock;
use wasmtime_wasi::p2::bindings::sync::io as sync_io;
use wasmtime_wasi::p2::{subscribe, DynPollable, Pollable};
use wasmtime_wasi::random::{WasiRandom, WasiRandomView};
use wasmtime_wasi::WasiView;

use crate::PluginError;

// ── Immediately-ready pollable ────────────────────────────────────────────────

/// A `Pollable` whose `ready()` future resolves without suspending.
///
/// Both the monotonic-clock stub and the poll stub back their pollables with this
/// type.  A guest that calls `monotonic-clock::subscribe-duration(∞)` + `poll`
/// gets a spurious-wakeup instead of an indefinite host-thread sleep.
struct ImmediatelyReady;

#[async_trait]
impl Pollable for ImmediatelyReady {
    async fn ready(&mut self) {
        // Intentionally empty: resolves immediately without any `.await` point.
        // This is the mechanism that prevents the blocking-evasion attack described
        // in the module-level docs.
    }
}

// ── Non-blocking monotonic-clock stub ─────────────────────────────────────────

/// Wraps `WasiClocksCtxView` and overrides only the subscribe methods to return
/// immediately-ready pollables (RFC-0010 §1.4).
///
/// `now` and `resolution` delegate to the real monotonic clock so guests that use
/// the clock for timing (e.g. exponential back-off between retries) still get
/// accurate values.
pub(crate) struct NbMonotonics<'a>(WasiClocksCtxView<'a>);

struct NbMonotonicClock;

impl HasData for NbMonotonicClock {
    type Data<'a> = NbMonotonics<'a>;
}

impl monotonic_clock::Host for NbMonotonics<'_> {
    fn now(&mut self) -> wasmtime::Result<monotonic_clock::Instant> {
        // Delegate to the real clock — reading a timestamp is always safe.
        monotonic_clock::Host::now(&mut self.0)
    }

    fn resolution(&mut self) -> wasmtime::Result<monotonic_clock::Instant> {
        monotonic_clock::Host::resolution(&mut self.0)
    }

    /// Return an immediately-ready pollable regardless of `when`.
    ///
    /// The stock implementation would create a Tokio `sleep_until` future for
    /// `when`, which epoch cannot interrupt.  This stub replaces that with an
    /// `ImmediatelyReady` resource that resolves on the first poll, so a guest
    /// calling `poll([subscribe_instant(far_future)])` returns promptly.
    fn subscribe_instant(
        &mut self,
        _when: monotonic_clock::Instant,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        let r = self.0.table.push(ImmediatelyReady)?;
        subscribe(self.0.table, r)
    }

    /// Return an immediately-ready pollable regardless of `duration`.
    ///
    /// The stock implementation would call `tokio::time::sleep_until`, parking
    /// the host thread for up to `duration` nanoseconds inside a native frame that
    /// epoch cannot interrupt.  This stub closes that gap.
    fn subscribe_duration(
        &mut self,
        _duration: monotonic_clock::Duration,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        let r = self.0.table.push(ImmediatelyReady)?;
        subscribe(self.0.table, r)
    }
}

// ── Non-blocking poll stub ────────────────────────────────────────────────────

/// Wraps `&mut ResourceTable` and implements the sync `wasi:io/poll` host interface
/// with non-blocking semantics:
///
/// - `poll` returns **all** pollable indices as immediately ready without entering
///   the Tokio scheduler or sleeping on any I/O handle.
/// - `block` returns immediately (no-op).
/// - `ready` always returns `true`.
/// - `drop` delegates to `ResourceTable`'s implementation, which correctly handles
///   the `DynPollable` cleanup and the optional removal of the underlying resource
///   that was registered via `subscribe`.
pub(crate) struct NbPollTable<'a>(&'a mut ResourceTable);

struct NbPollHasData;

impl HasData for NbPollHasData {
    type Data<'a> = NbPollTable<'a>;
}

impl sync_io::poll::Host for NbPollTable<'_> {
    fn poll(&mut self, pollables: Vec<Resource<DynPollable>>) -> wasmtime::Result<Vec<u32>> {
        // Return all indices as ready immediately — never suspend the host thread.
        let n = u32::try_from(pollables.len())
            .map_err(|_| wasmtime::Error::msg("poll list too large (> u32::MAX)"))?;
        Ok((0..n).collect())
    }
}

impl sync_io::poll::HostPollable for NbPollTable<'_> {
    fn ready(&mut self, _pollable: Resource<DynPollable>) -> wasmtime::Result<bool> {
        // All pollables are immediately ready.
        Ok(true)
    }

    fn block(&mut self, _pollable: Resource<DynPollable>) -> wasmtime::Result<()> {
        // No-op: return immediately rather than blocking the host thread.
        Ok(())
    }

    fn drop(&mut self, pollable: Resource<DynPollable>) -> wasmtime::Result<()> {
        // Delegate to ResourceTable's HostPollable::drop, which deletes the
        // DynPollable from the table and (when the resource was owned) also
        // removes the underlying ImmediatelyReady resource via
        // remove_index_on_delete.  This is the only method we cannot stub
        // non-trivially — proper cleanup is required to avoid table leaks.
        use sync_io::poll::HostPollable;
        <ResourceTable as HostPollable>::drop(self.0, pollable)
    }
}

// ── HasData bridge for &mut ResourceTable ─────────────────────────────────────

/// `HasData` implementation that projects `&mut T` → `&mut ResourceTable`,
/// used for `wasi:io/error` and `wasi:io/streams`.
///
/// Both interfaces have their host traits implemented for `ResourceTable` (and
/// hence `&mut ResourceTable`) in `wasmtime-wasi` and `wasmtime-wasi-io`, so no
/// stub is needed — they carry no ambient authority.
struct HasIoData;

impl HasData for HasIoData {
    type Data<'a> = &'a mut ResourceTable;
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Add the RFC-0010 §1.2 WASI allow-list to `linker`.
///
/// This is the replacement for the blanket `wasmtime_wasi::p2::add_to_linker_sync`
/// call.  Only the interfaces listed in the table below are defined; every other
/// WASI interface (sockets, filesystem, CLI) is absent by design.  A component that
/// imports an excluded interface fails instantiation with a "unknown import" error —
/// the default-deny security posture.
///
/// | Interface | Stub? | Notes |
/// |---|---|---|
/// | `wasi:io/error@0.2.x` | no | Foundational error type; no attack surface. |
/// | `wasi:io/streams@0.2.x` | no | Byte-stream primitives; empty context, no real resources. |
/// | `wasi:io/poll@0.2.x` | **yes** | Returns all pollables ready immediately; never parks the thread. |
/// | `wasi:clocks/wall-clock@0.2.x` | no | Non-blocking; used by `std::time::SystemTime`. |
/// | `wasi:clocks/monotonic-clock@0.2.x` | **yes** | `subscribe-*` returns immediately-ready pollables. |
/// | `wasi:random/random@0.2.x` | no | Cryptographic entropy; safe. |
/// | `wasi:random/insecure@0.2.x` | no | Non-cryptographic randomness; safe. |
/// | `wasi:random/insecure-seed@0.2.x` | no | Hash-map seeding; safe. |
///
/// # Errors
///
/// Returns `PluginError::Instantiate` if any interface registration fails (e.g. the
/// same interface is registered twice — call this function at most once per `Linker`).
pub(crate) fn add_narrowed_wasi_to_linker<T>(linker: &mut Linker<T>) -> Result<(), PluginError>
where
    T: WasiView + Send + 'static,
{
    use wasmtime_wasi::p2::bindings::sync::io;
    use wasmtime_wasi::p2::bindings::{clocks, random};

    let e = |err: wasmtime::Error| PluginError::Instantiate(err.to_string());

    // ── wasi:io — foundational stream/poll/error primitives ──────────────────
    //
    // io/error and io/streams use the stock `ResourceTable` impl (non-blocking,
    // no ambient access).  io/poll uses the non-blocking stub above.

    io::error::add_to_linker::<T, HasIoData>(linker, |t| t.ctx().table).map_err(e)?;
    io::streams::add_to_linker::<T, HasIoData>(linker, |t| t.ctx().table).map_err(e)?;
    io::poll::add_to_linker::<T, NbPollHasData>(linker, |t| NbPollTable(t.ctx().table))
        .map_err(e)?;

    // ── wasi:clocks — wall clock real; monotonic subscribe non-blocking ────────

    clocks::wall_clock::add_to_linker::<T, WasiClocks>(linker, T::clocks).map_err(e)?;
    clocks::monotonic_clock::add_to_linker::<T, NbMonotonicClock>(linker, |s| {
        NbMonotonics(s.clocks())
    })
    .map_err(e)?;

    // ── wasi:random — entropy; no blocking, no secrets ───────────────────────

    random::random::add_to_linker::<T, WasiRandom>(linker, |t| t.random()).map_err(e)?;
    random::insecure::add_to_linker::<T, WasiRandom>(linker, |t| t.random()).map_err(e)?;
    random::insecure_seed::add_to_linker::<T, WasiRandom>(linker, |t| t.random()).map_err(e)?;

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::component::{Component, Linker, ResourceTable};
    use wasmtime::Store;
    use wasmtime_wasi::clocks::{WasiClocksCtx, WasiClocksCtxView};
    use wasmtime_wasi::{WasiCtx, WasiCtxView};

    // ── Minimal store-data type for tests ─────────────────────────────────────

    struct TestState {
        wasi: WasiCtx,
        table: ResourceTable,
    }

    impl WasiView for TestState {
        fn ctx(&mut self) -> WasiCtxView<'_> {
            WasiCtxView {
                ctx: &mut self.wasi,
                table: &mut self.table,
            }
        }
    }

    fn test_state() -> TestState {
        TestState {
            wasi: WasiCtx::builder().build(),
            table: ResourceTable::new(),
        }
    }

    // ── ImmediatelyReady ──────────────────────────────────────────────────────

    #[test]
    fn immediately_ready_resolves_without_suspending() {
        // Drive the async future synchronously via tokio's current_thread runtime.
        // If `ready()` suspended on any future (e.g. sleep), this test would hang;
        // instead it must return on the first poll.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build current_thread runtime");
        rt.block_on(async {
            let mut r = ImmediatelyReady;
            // Safety: no timeout here — if this hangs, the CI test runner kills
            // the process, which is the intended failure signal.
            r.ready().await;
        });
    }

    // ── Non-blocking monotonic-clock stub ──────────────────────────────────────

    #[test]
    fn nb_clock_subscribe_duration_returns_immediately_ready_pollable() {
        // Creating a pollable via subscribe_duration must succeed and not touch
        // the Tokio scheduler (verified by running outside a Tokio runtime).
        use monotonic_clock::Host as MonoHost;
        use sync_io::poll::HostPollable;

        let mut clocks_ctx = WasiClocksCtx::default();
        let mut table = ResourceTable::new();
        let mut nb = NbMonotonics(WasiClocksCtxView {
            ctx: &mut clocks_ctx,
            table: &mut table,
        });

        // 10 seconds — would block for 10 s with the stock implementation.
        let pollable = nb
            .subscribe_duration(10_000_000_000)
            .expect("subscribe_duration");

        // Verify a valid pollable was created (rep is the resource-table key).
        let _ = pollable.rep(); // just ensure the field is accessible

        // Clean up: delete the DynPollable (which also removes ImmediatelyReady).
        <ResourceTable as HostPollable>::drop(&mut table, pollable).expect("drop pollable");
    }

    #[test]
    fn nb_clock_subscribe_instant_returns_immediately_ready_pollable() {
        use monotonic_clock::Host as MonoHost;
        use sync_io::poll::HostPollable;

        let mut clocks_ctx = WasiClocksCtx::default();
        let mut table = ResourceTable::new();
        let mut nb = NbMonotonics(WasiClocksCtxView {
            ctx: &mut clocks_ctx,
            table: &mut table,
        });

        // Year 2106 — far future.
        let pollable = nb.subscribe_instant(u64::MAX).expect("subscribe_instant");

        <ResourceTable as HostPollable>::drop(&mut table, pollable).expect("drop pollable");
    }

    // ── Non-blocking poll stub ──────────────────────────────────────────────

    #[test]
    fn nb_poll_returns_all_indices_immediately() {
        // NbPollTable::poll must return indices [0, 1, 2, …, n-1] immediately,
        // matching the "all ready" contract.
        use sync_io::poll::Host as PollHost;

        let mut table = ResourceTable::new();
        let mut poll_table = NbPollTable(&mut table);

        // poll is defined on `Vec<Resource<DynPollable>>`.  We pass an empty
        // list — all 0 indices are returned trivially.
        let ready = poll_table.poll(vec![]).expect("poll(empty)");
        assert!(ready.is_empty(), "empty poll list → empty ready list");
    }

    // ── Default-deny: sockets not in the narrowed linker ───────────────────────

    #[test]
    fn socket_import_fails_instantiation_with_narrowed_linker() {
        // A component that imports wasi:sockets/tcp@0.2.0 with a required named
        // export must fail to instantiate because the narrowed linker never registers
        // that interface.
        //
        // Note: an *empty* `(instance)` type (zero required exports) is synthesised
        // by wasmtime even when nothing is registered for that name.  We must
        // therefore require at least one named export so that wasmtime cannot
        // produce a trivially-satisfying stub.
        let socket_wat = r#"(component
  (import "wasi:sockets/tcp@0.2.0" (instance
    (export "start-connect" (func))
  ))
)"#;

        let engine = wasmtime::Engine::new(&crate::engine_config()).expect("engine");
        let component =
            Component::new(&engine, socket_wat.as_bytes()).expect("WAT component must parse");

        let mut linker: Linker<TestState> = Linker::new(&engine);
        add_narrowed_wasi_to_linker(&mut linker)
            .expect("narrowed WASI must register without error");

        let mut store = Store::new(&engine, test_state());
        // Set a generous epoch deadline so the missing-import check (which runs
        // before any guest code) is not preempted.
        store.set_epoch_deadline(u64::MAX / 2);
        let _ = store.set_fuel(u64::MAX);

        let err = linker
            .instantiate(&mut store, &component)
            .expect_err("component importing sockets must fail to instantiate");

        let msg = err.to_string();
        // The exact wording depends on wasmtime internals, but it must reference
        // the missing import name or the unknown-import situation.
        assert!(
            msg.contains("wasi:sockets") || msg.contains("unknown import") || msg.contains("tcp"),
            "error must identify the missing socket import, got: {msg}"
        );
    }
}
