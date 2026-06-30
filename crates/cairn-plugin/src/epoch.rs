//! Wall-clock interruption for sandboxed guests via wasmtime's **epoch** mechanism.
//!
//! Execution **fuel** bounds the number of *instructions* a guest runs, but fuel does not advance
//! while the guest spins. Epoch interruption is the wall-clock companion: a guest traps once the
//! engine's epoch counter has been incremented past the store's deadline.
//!
//! Wasmtime only increments the epoch when asked; [`EpochTicker`] is the background thread that does
//! so on a fixed interval. A store arms a deadline with `set_epoch_deadline(ticks)` before each call
//! (see [`PluginComponent`](crate::PluginComponent)); after `ticks` increments it traps with
//! [`wasmtime::Trap::Interrupt`]. The ticker holds only a [`wasmtime::EngineWeak`], so it never keeps
//! an engine alive on its own and exits cleanly once the engine is dropped (or on `Drop`).
//!
//! **Scope / SECURITY (important):** epoch checks are only emitted at instrumented points in *guest
//! WebAssembly* (loop back-edges, function entries). Neither epoch nor fuel is observed while control
//! is inside a **native host frame**, so a guest blocked *inside* a host or WASI call is **not**
//! interrupted by this mechanism. The granted `host` imports are non-blocking (`log`/`now-secs`) or
//! immediate deny-stubs. The WASI surface is limited to the RFC-0010 §1 allow-list
//! (`crate::wasi_narrowing`), which replaces the blocking `wasi:io/poll` and
//! `wasi:clocks/monotonic-clock` implementations with non-blocking stubs. A guest calling
//! `subscribe-duration(far_future)` receives an immediately-ready pollable that returns without
//! suspending the host thread; subsequent `poll` calls likewise return all indices immediately.
//! Epoch now reliably bounds *both* spinning and WASI-clock-evasion attacks. The remaining hazard is
//! `wasi:io/streams` blocking methods, which are mitigated by the empty `WasiCtx` (no accessible
//! streams today); PR-B (M8-5) will close that gap with stub implementations.
//!
//! Each [`EpochTicker`] increments the engine-global epoch. **One ticker per `Engine`**: the
//! `PluginVfsBackend` bridge currently builds one engine per instance and spawns one ticker for it.
//! If an engine is ever shared across instances, share a single ticker too — N tickers on one engine
//! advance it N× and shrink the effective deadline to `1/N` of the configured value.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use wasmtime::Engine;

/// A background thread that increments an [`Engine`]'s epoch on a fixed interval, enabling wall-clock
/// deadlines for guests on that engine. Stops (and joins) the thread on drop.
pub struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EpochTicker {
    /// Spawn a ticker incrementing `engine`'s epoch every `interval`.
    ///
    /// The thread holds a weak engine handle, so it does not keep the engine alive; it exits when the
    /// engine is dropped or when this `EpochTicker` is dropped. If the OS refuses the thread, epoch
    /// deadlines simply never fire (the guest is still fuel-bounded) rather than failing the caller.
    #[must_use]
    pub fn spawn(engine: &Engine, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let weak = engine.weak();
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("cairn-plugin-epoch".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    // `park_timeout` so `Drop` can wake us immediately via `unpark`.
                    std::thread::park_timeout(interval);
                    match weak.upgrade() {
                        Some(engine) => engine.increment_epoch(),
                        None => break, // engine gone — nothing left to interrupt
                    }
                }
            })
            .ok();
        if handle.is_none() {
            // Fail open to fuel-only protection, but make the lost backstop observable.
            tracing::warn!(target: "plugin", "epoch ticker thread failed to spawn; guests are fuel-bounded only");
        }
        Self { stop, handle }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        // `Relaxed` is sufficient: the flag guards only the thread's loop predicate; no other data is
        // ordered through it, and `join()` below provides the happens-before for anything after.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Config, Engine, Instance, Module, Store, Trap};

    #[test]
    fn epoch_ticker_traps_a_spinning_guest() {
        // Epoch is independent of fuel: enable only epoch interruption (no fuel) so a trap here can
        // *only* be the wall-clock deadline, proving the ticker advances the epoch and the deadline
        // fires.
        let mut cfg = Config::new();
        cfg.epoch_interruption(true);
        let engine = Engine::new(&cfg).unwrap();
        let ticker = EpochTicker::spawn(&engine, Duration::from_millis(1));
        // Guard: with no fuel limit, only the epoch stops the spin — if the ticker thread didn't
        // spawn, fail fast here rather than hang forever.
        assert!(
            ticker.handle.is_some(),
            "ticker thread must spawn for this test"
        );

        let module =
            Module::new(&engine, r#"(module (func (export "spin") (loop br 0)))"#).unwrap();
        let mut store = Store::new(&engine, ());
        store.set_epoch_deadline(2);
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .unwrap();

        let err = spin.call(&mut store, ()).unwrap_err();
        let trap = err.downcast_ref::<Trap>().expect("a trap");
        assert_eq!(
            *trap,
            Trap::Interrupt,
            "epoch deadline must trap as Interrupt"
        );
    }

    #[test]
    fn dropping_the_ticker_stops_the_thread() {
        // A finite work loop completes when the deadline is generous and the ticker is dropped — i.e.
        // dropping the ticker is clean (no panic, no hang on join).
        let mut cfg = Config::new();
        cfg.epoch_interruption(true);
        let engine = Engine::new(&cfg).unwrap();
        {
            let _ticker = EpochTicker::spawn(&engine, Duration::from_millis(1));
        } // dropped here — join must return promptly
        let mut store = Store::new(&engine, ());
        store.set_epoch_deadline(1_000_000); // effectively no deadline (epoch no longer advances)
        let module = Module::new(&engine, r#"(module (func (export "noop")))"#).unwrap();
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let noop = instance
            .get_typed_func::<(), ()>(&mut store, "noop")
            .unwrap();
        noop.call(&mut store, ())
            .expect("no trap after ticker dropped");
    }
}
