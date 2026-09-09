//! Logging/tracing initialization.
//!
//! Logs go to stderr and honor the `CAIRN_LOG` environment variable (an [`tracing_subscriber`]
//! `EnvFilter` directive, e.g. `CAIRN_LOG=debug`). Default level is `warn`.
//!
//! **The secret-redaction layer specified by `docs/LLD.md` §9.5 is NOT installed here yet.** The
//! comment this replaces claimed it would arrive with `cairn-secrets` in M3 and that "no credentials
//! flow through the app" until then; both halves are now stale — `cairn-secrets` landed, and the
//! vault/broker/connect paths do carry credentials. What protects logs today is per-error redaction
//! at the boundaries (`VfsError::redacted()`, `ConnectError`'s `Display`/`Debug`), not a subscriber
//! layer, so a *dependency* that logs a secret itself is not covered — see the RUSTSEC-2026-0275
//! note in `deny.toml` for a live example (`azure_core` 0.21 logs the `authorization` header at
//! DEBUG). `cairn_secrets::redact` exists but currently has no caller.

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
