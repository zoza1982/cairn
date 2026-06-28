//! Logging/tracing initialization.
//!
//! Logs go to stderr and honor the `CAIRN_LOG` environment variable (an [`tracing_subscriber`]
//! `EnvFilter` directive, e.g. `CAIRN_LOG=debug`). Default level is `warn`.
//!
//! A secret-redaction layer (per `docs/LLD.md` §9.5) is added once `cairn-secrets` lands in the M3
//! milestone; until then no credentials flow through the app, so plain logging is safe.

use tracing_subscriber::{fmt, EnvFilter};

/// The environment variable controlling log verbosity.
pub(crate) const LOG_ENV: &str = "CAIRN_LOG";

/// Initialize the global tracing subscriber. Safe to call once at startup.
pub(crate) fn init() {
    let filter = EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new("warn"));
    // `try_init` returns Err if a subscriber is already set; ignore so tests/embedders are tolerant.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
