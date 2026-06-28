//! Cairn's sandboxed WASM plugin host.
//!
//! Built on `wasmtime`. This milestone delivers the security-bearing runtime core — instantiate an
//! untrusted module with a **memory cap** and an **execution-fuel limit**, expose only explicitly
//! linked **host functions** (capability-gated; default-deny), and run an export — all fully
//! offline and hermetically testable (WebAssembly executes without any external service).
//!
//! The Component Model / WIT interface (ADR-0004) and the `Vfs`-backend bridge layer on top of this
//! core in a later step; what is here is the part that proves a misbehaving plugin cannot hang the
//! host or exhaust memory, and that a guest reaches the host only through granted imports.

use thiserror::Error;
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, Trap};

/// Per-instance resource limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum linear-memory size in bytes.
    pub max_memory_bytes: usize,
    /// Execution fuel (roughly, instructions) before the guest is trapped.
    pub fuel: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 16 * 1024 * 1024,
            fuel: 100_000_000,
        }
    }
}

/// Plugin host errors. Never carries secret material.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PluginError {
    /// The module failed to compile/validate.
    #[error("plugin failed to compile: {0}")]
    Compile(String),
    /// Instantiation failed (e.g. an ungranted import).
    #[error("plugin failed to instantiate: {0}")]
    Instantiate(String),
    /// The requested export does not exist or has the wrong signature.
    #[error("export not found or wrong type: {0}")]
    Export(String),
    /// The guest exhausted its execution fuel.
    #[error("plugin exceeded its fuel limit")]
    OutOfFuel,
    /// The guest trapped during execution.
    #[error("plugin trapped: {0}")]
    Trap(String),
}

/// Per-instance store state: the resource limiter plus a capability-gated log buffer.
struct HostState {
    limits: StoreLimits,
    #[allow(dead_code)]
    log: Vec<String>,
}

/// A sandboxed WASM plugin host. One [`Engine`] is reused; each call gets a fresh [`Store`].
pub struct PluginHost {
    engine: Engine,
}

impl PluginHost {
    /// Create a host (fuel metering enabled).
    ///
    /// # Errors
    /// If the engine cannot be created.
    pub fn new() -> Result<Self, PluginError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|e| PluginError::Compile(e.to_string()))?;
        Ok(Self { engine })
    }

    fn store(&self, limits: Limits) -> Result<Store<HostState>, PluginError> {
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes)
            .build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                limits: store_limits,
                log: Vec::new(),
            },
        );
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        Ok(store)
    }

    /// Build the capability-gated linker. Only the host functions granted here are reachable by a
    /// guest; everything else is default-deny (an ungranted import fails instantiation).
    fn linker(&self) -> Result<Linker<HostState>, PluginError> {
        let mut linker = Linker::new(&self.engine);
        // A single demonstrative, always-granted host function: `host.add1`.
        linker
            .func_wrap(
                "host",
                "add1",
                |_caller: wasmtime::Caller<'_, HostState>, x: i32| x.wrapping_add(1),
            )
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        Ok(linker)
    }

    fn compile(&self, wasm: &[u8]) -> Result<Module, PluginError> {
        Module::new(&self.engine, wasm).map_err(|e| PluginError::Compile(e.to_string()))
    }

    fn map_call_err(e: wasmtime::Error) -> PluginError {
        if let Some(trap) = e.downcast_ref::<Trap>() {
            if *trap == Trap::OutOfFuel {
                return PluginError::OutOfFuel;
            }
            return PluginError::Trap(trap.to_string());
        }
        PluginError::Trap(e.to_string())
    }

    /// Run an exported `(i32) -> i32` function in a fresh, limited store.
    ///
    /// # Errors
    /// [`PluginError`] for compile/instantiate/export failures, fuel exhaustion, or a trap.
    pub fn run_i32(
        &self,
        wasm: &[u8],
        func: &str,
        arg: i32,
        limits: Limits,
    ) -> Result<i32, PluginError> {
        let module = self.compile(wasm)?;
        let linker = self.linker()?;
        let mut store = self.store(limits)?;
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        let f = instance
            .get_typed_func::<i32, i32>(&mut store, func)
            .map_err(|e| PluginError::Export(e.to_string()))?;
        f.call(&mut store, arg).map_err(Self::map_call_err)
    }

    /// Run an exported `() -> ()` function in a fresh, limited store (e.g. to verify a runaway guest
    /// is trapped by the fuel limit rather than hanging the host).
    ///
    /// # Errors
    /// As [`PluginHost::run_i32`].
    pub fn run_void(&self, wasm: &[u8], func: &str, limits: Limits) -> Result<(), PluginError> {
        let module = self.compile(wasm)?;
        let linker = self.linker()?;
        let mut store = self.store(limits)?;
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;
        let f = instance
            .get_typed_func::<(), ()>(&mut store, func)
            .map_err(|e| PluginError::Export(e.to_string()))?;
        f.call(&mut store, ()).map_err(Self::map_call_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOUBLE: &str = r#"(module (func (export "double") (param i32) (result i32) local.get 0 i32.const 2 i32.mul))"#;
    const SPIN: &str = r#"(module (func (export "spin") (loop br 0)))"#;
    const GROW: &str = r#"(module
        (memory 1)
        (func (export "grow") (param i32) (result i32) local.get 0 memory.grow))"#;
    const USES_HOST: &str = r#"(module
        (import "host" "add1" (func $add1 (param i32) (result i32)))
        (func (export "use") (param i32) (result i32) local.get 0 call $add1))"#;
    const NEEDS_UNGRANTED: &str = r#"(module
        (import "host" "danger" (func $d (param i32) (result i32)))
        (func (export "use") (param i32) (result i32) local.get 0 call $d))"#;

    fn host() -> PluginHost {
        PluginHost::new().unwrap()
    }

    #[test]
    fn runs_a_simple_export() {
        let out = host()
            .run_i32(DOUBLE.as_bytes(), "double", 21, Limits::default())
            .unwrap();
        assert_eq!(out, 42);
    }

    #[test]
    fn runaway_guest_is_trapped_by_fuel_not_hung() {
        let limits = Limits {
            max_memory_bytes: 1 << 20,
            fuel: 10_000,
        };
        let err = host()
            .run_void(SPIN.as_bytes(), "spin", limits)
            .unwrap_err();
        assert!(matches!(err, PluginError::OutOfFuel), "got {err:?}");
    }

    #[test]
    fn memory_growth_is_capped() {
        // Cap at exactly one 64 KiB page: initial memory fits, growth is denied (memory.grow → -1).
        let limits = Limits {
            max_memory_bytes: 64 * 1024,
            fuel: 1_000_000,
        };
        let r = host().run_i32(GROW.as_bytes(), "grow", 1, limits).unwrap();
        assert_eq!(r, -1, "growth past the cap must be denied");

        // With headroom, the same grow succeeds (returns the previous page count, 1).
        let limits = Limits {
            max_memory_bytes: 256 * 1024,
            fuel: 1_000_000,
        };
        let r = host().run_i32(GROW.as_bytes(), "grow", 1, limits).unwrap();
        assert_eq!(r, 1);
    }

    #[test]
    fn guest_reaches_host_only_through_granted_imports() {
        // Granted import works.
        let out = host()
            .run_i32(USES_HOST.as_bytes(), "use", 41, Limits::default())
            .unwrap();
        assert_eq!(out, 42);

        // An ungranted import fails at instantiation (default-deny).
        let err = host()
            .run_i32(NEEDS_UNGRANTED.as_bytes(), "use", 1, Limits::default())
            .unwrap_err();
        assert!(matches!(err, PluginError::Instantiate(_)), "got {err:?}");
    }

    #[test]
    fn invalid_module_is_a_compile_error() {
        let err = host()
            .run_i32(b"(module (this is not wat", "x", 0, Limits::default())
            .unwrap_err();
        assert!(matches!(err, PluginError::Compile(_)));
    }

    #[test]
    fn missing_export_is_an_error() {
        let err = host()
            .run_i32(DOUBLE.as_bytes(), "nonexistent", 0, Limits::default())
            .unwrap_err();
        assert!(matches!(err, PluginError::Export(_)));
    }
}
