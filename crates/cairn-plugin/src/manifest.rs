//! `plugin.toml` manifest: schema, parsing, and validation (RFC-0010 §5.2).
//!
//! A `PluginManifest` is parsed from the `plugin.toml` file found alongside `component.wasm`
//! in a plugin directory. It declares:
//!
//! - `[plugin]` — identity: name, version, `api` major, description, optional metadata.
//! - `[capabilities]` — what the plugin wants: log, network hostnames, credential handles.
//! - `[network]` — optional HTTP fetch limits / HTTP-allow override.
//! - `[limits]` — optional per-call resource limits (memory, fuel, ticks, stream bytes).
//!
//! Unknown fields are **rejected** (`#[serde(deny_unknown_fields)]` on every struct) so a
//! future version's unrecognised key is surfaced as a `PluginError::Compile` rather than
//! silently granted under a host that doesn't understand it (RFC-0010 §5.2).
//!
//! Validation rules that `serde` cannot express (charset, lengths, SemVer) are checked in
//! [`PluginManifest::validate`].

use crate::PluginError;
use serde::{Deserialize, Serialize};

// ── Default value helpers ──────────────────────────────────────────────────────────────────────

fn default_max_memory_bytes() -> u64 {
    64 * 1024 * 1024 // 64 MiB
}

fn default_fuel() -> u64 {
    1_000_000_000 // 1 × 10^9
}

fn default_max_call_ticks() -> u64 {
    50 // 50 × 100 ms ≈ 5 s per call
}

fn default_max_stream_bytes() -> u64 {
    4 * 1024 * 1024 * 1024 // 4 GiB
}

fn default_max_response_bytes() -> u64 {
    8 * 1024 * 1024 // 8 MiB
}

fn default_connect_timeout_secs() -> u64 {
    4 // below the epoch ceiling (5 s) to leave DNS headroom
}

fn default_request_timeout_secs() -> u64 {
    4
}

// ── Schema ─────────────────────────────────────────────────────────────────────────────────────

/// The full `plugin.toml` manifest.
///
/// Parsed with `#[serde(deny_unknown_fields)]` so any key not in this schema is rejected
/// immediately (the error propagates as [`PluginError::Compile`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// `[plugin]` — identity block. Required.
    pub plugin: PluginMeta,
    /// `[capabilities]` — what the plugin requests. Defaults to empty (no capabilities).
    #[serde(default)]
    pub capabilities: CapabilitiesConfig,
    /// `[network]` — HTTP fetch configuration overrides. All optional; documented defaults apply.
    #[serde(default)]
    pub network: NetworkConfig,
    /// `[limits]` — per-call resource limits. All optional; safe defaults apply.
    #[serde(default)]
    pub limits: LimitsConfig,
}

/// `[plugin]` — identity and versioning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMeta {
    /// Machine identifier. Only `[a-z0-9\-_]`, max 64 chars. Used as the plugin's directory
    /// name prefix and in audit journal entries.
    pub name: String,
    /// SemVer version string (`MAJOR.MINOR.PATCH`).
    pub version: String,
    /// WIT world major version. Must match the host's `HOST_API_MAJOR` or instantiation is
    /// rejected with [`PluginError::Compile`] before any bytes are loaded.
    pub api: String,
    /// User-visible description. Max 256 chars; not secret.
    pub description: String,
    /// Optional homepage URL.
    #[serde(default)]
    pub homepage: Option<String>,
    /// Optional author list.
    #[serde(default)]
    pub authors: Option<Vec<String>>,
}

/// `[capabilities]` — what the plugin requests. All fields default to empty/false.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesConfig {
    /// Whether the plugin may write to Cairn's log via `host::log`.
    #[serde(default)]
    pub log: bool,
    /// Hostnames the plugin may contact via `host::http-fetch` (HTTPS by default).
    /// Exact-match only (no wildcards in v1).
    #[serde(default)]
    pub network: Vec<String>,
    /// Credential handle labels the plugin may use via `host::use-credential`.
    #[serde(default)]
    pub credentials: Vec<String>,
}

/// `[network]` — HTTP fetch overrides. All fields are optional; struct defaults to the
/// documented RFC-0010 values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// If `true`, the plugin may contact `http://` (plain HTTP) URLs for the granted
    /// hostnames. Requires an explicit user grant at install time. Default: `false` (HTTPS only).
    #[serde(default)]
    pub allow_http: bool,
    /// Maximum response body size in bytes. Default: 8 MiB.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: u64,
    /// TCP connect timeout in seconds. Default: 4 s.
    #[serde(default = "default_connect_timeout_secs")]
    pub http_connect_timeout_secs: u64,
    /// Total request timeout in seconds. Default: 4 s.
    #[serde(default = "default_request_timeout_secs")]
    pub http_request_timeout_secs: u64,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            allow_http: false,
            max_response_bytes: default_max_response_bytes(),
            http_connect_timeout_secs: default_connect_timeout_secs(),
            http_request_timeout_secs: default_request_timeout_secs(),
        }
    }
}

/// `[limits]` — per-call resource limits. All fields are optional with safe defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Maximum linear-memory size in bytes. Default: 64 MiB.
    #[serde(default = "default_max_memory_bytes")]
    pub max_memory_bytes: u64,
    /// Execution fuel (roughly, guest instructions) before the store is trapped. Default: 1 × 10^9.
    #[serde(default = "default_fuel")]
    pub fuel: u64,
    /// Maximum wall-clock ticks per call. Effective ceiling ≈ `max_call_ticks × 100 ms`.
    /// Default: 50 (≈ 5 s).
    #[serde(default = "default_max_call_ticks")]
    pub max_call_ticks: u64,
    /// Maximum total bytes a single read stream may yield before it is forcibly errored.
    /// Default: 4 GiB.
    #[serde(default = "default_stream_bytes")]
    pub max_stream_bytes: u64,
}

fn default_stream_bytes() -> u64 {
    default_max_stream_bytes()
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_memory_bytes: default_max_memory_bytes(),
            fuel: default_fuel(),
            max_call_ticks: default_max_call_ticks(),
            max_stream_bytes: default_max_stream_bytes(),
        }
    }
}

// ── Parsing and validation ─────────────────────────────────────────────────────────────────────

/// The WIT world major version this host implements. A manifest with a different `api` value is
/// rejected before any component bytes are loaded (RFC-0010 §5.3).
pub const HOST_API_MAJOR: &str = "1";

impl PluginManifest {
    /// Parse a `plugin.toml` document.
    ///
    /// Unknown fields are rejected by `serde` (deny-unknown-fields). Call [`Self::validate`] after
    /// parsing for the semantic checks that `serde` cannot express.
    ///
    /// # Errors
    /// [`PluginError::Compile`] for any parse error (TOML syntax, type mismatch, or an
    /// unrecognised field).
    pub fn from_toml(src: &str) -> Result<Self, PluginError> {
        toml::from_str(src)
            .map_err(|e| PluginError::Compile(format!("plugin.toml parse error: {e}")))
    }

    /// Semantic validation of the parsed manifest.
    ///
    /// Checks that `serde` cannot enforce:
    /// - `name` charset (`[a-z0-9-_]`), non-empty, max 64 chars.
    /// - `version` parses as SemVer (`MAJOR.MINOR.PATCH`, pre-release/build ignored).
    /// - `api` major matches [`HOST_API_MAJOR`] (§5.3).
    /// - `description` max 256 chars.
    ///
    /// # Errors
    /// [`PluginError::Compile`] with a human-readable message.
    pub fn validate(&self) -> Result<(), PluginError> {
        // name: [a-z0-9-_], non-empty, max 64
        let name = &self.plugin.name;
        if name.is_empty() {
            return Err(PluginError::Compile(
                "plugin.toml: [plugin].name must not be empty".to_owned(),
            ));
        }
        if name.len() > 64 {
            return Err(PluginError::Compile(format!(
                "plugin.toml: [plugin].name '{}' exceeds 64 characters",
                name
            )));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(PluginError::Compile(format!(
                "plugin.toml: [plugin].name '{}' contains invalid characters \
                 (only [a-z0-9-_] are allowed)",
                name
            )));
        }

        // version: must be parseable as SemVer (MAJOR.MINOR.PATCH with optional pre/build)
        validate_semver(&self.plugin.version)?;

        // api major: must match host
        let api = self.plugin.api.trim();
        if api != HOST_API_MAJOR {
            return Err(PluginError::Compile(format!(
                "plugin.toml: [plugin].api '{}' does not match host API major '{}'; \
                 this plugin was built for a different version of the Cairn plugin ABI",
                api, HOST_API_MAJOR
            )));
        }

        // description: max 256 chars
        if self.plugin.description.len() > 256 {
            return Err(PluginError::Compile(format!(
                "plugin.toml: [plugin].description exceeds 256 characters ({} chars)",
                self.plugin.description.len()
            )));
        }

        Ok(())
    }
}

/// Validate that `s` is a parseable SemVer string (`MAJOR.MINOR.PATCH`, with optional
/// pre-release and build-metadata suffixes as per semver.org).
///
/// We implement a minimal parser rather than pulling in the `semver` crate (which is not yet
/// in the workspace dep tree). Only syntactic validity is checked; semantics (compatibility,
/// ordering) are handled by the caller.
fn validate_semver(s: &str) -> Result<(), PluginError> {
    // Strip optional pre-release (`-alpha.1`) and build metadata (`+build.123`).
    let base = s.split('+').next().unwrap_or(s);
    let base = base.split('-').next().unwrap_or(base);
    let parts: Vec<&str> = base.split('.').collect();
    if parts.len() < 3 {
        return Err(PluginError::Compile(format!(
            "plugin.toml: [plugin].version '{s}' is not a valid SemVer string \
             (expected MAJOR.MINOR.PATCH)"
        )));
    }
    for part in &parts[..3] {
        part.parse::<u64>().map_err(|_| {
            PluginError::Compile(format!(
                "plugin.toml: [plugin].version '{s}' contains non-numeric component '{part}'"
            ))
        })?;
    }
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml(name: &str, version: &str, api: &str, description: &str) -> String {
        format!(
            r#"
[plugin]
name        = "{name}"
version     = "{version}"
api         = "{api}"
description = "{description}"
"#
        )
    }

    #[test]
    fn valid_manifest_round_trips() {
        let src = r#"
[plugin]
name        = "my-cloud"
version     = "0.2.1"
api         = "1"
description = "My Cloud storage backend"
homepage    = "https://example.com/my-cloud"
authors     = ["Alice <a@b.com>"]

[capabilities]
log         = true
network     = ["api.mycloud.example.com", "auth.mycloud.example.com"]
credentials = ["my-cloud-key"]

[network]
allow_http            = false
max_response_bytes    = 8388608
http_connect_timeout_secs = 4
http_request_timeout_secs = 4

[limits]
max_memory_bytes = 67108864
fuel             = 1000000000
max_call_ticks   = 50
max_stream_bytes = 4294967296
"#;
        let m = PluginManifest::from_toml(src).expect("parse");
        m.validate().expect("validate");
        assert_eq!(m.plugin.name, "my-cloud");
        assert_eq!(m.plugin.version, "0.2.1");
        assert_eq!(m.plugin.api, "1");
        assert_eq!(m.capabilities.network.len(), 2);
        assert!(m.capabilities.log);
        assert_eq!(m.limits.fuel, 1_000_000_000);
    }

    #[test]
    fn unknown_field_is_rejected() {
        // A key the current schema does not recognise must be rejected (deny_unknown_fields).
        let src = r#"
[plugin]
name        = "test"
version     = "1.0.0"
api         = "1"
description = "test"

[network]
allow_http = false
mystery_future_key = "oops"
"#;
        let err = PluginManifest::from_toml(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown") || msg.contains("parse error"),
            "expected unknown-field rejection, got: {msg}"
        );
    }

    #[test]
    fn api_major_mismatch_is_rejected() {
        let src = minimal_toml("ok", "1.0.0", "2", "test");
        let m = PluginManifest::from_toml(&src).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("api"), "err = {err}");
        assert!(err.to_string().contains("2"), "err = {err}");
    }

    #[test]
    fn invalid_name_charset_is_rejected() {
        // uppercase
        let src = minimal_toml("MyCloud", "1.0.0", "1", "test");
        let m = PluginManifest::from_toml(&src).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("name"), "err = {err}");
        // space
        let src2 = minimal_toml("my cloud", "1.0.0", "1", "test");
        let m2 = PluginManifest::from_toml(&src2).expect("parse");
        assert!(m2.validate().is_err());
    }

    #[test]
    fn name_too_long_is_rejected() {
        let long = "a".repeat(65);
        let src = minimal_toml(&long, "1.0.0", "1", "test");
        let m = PluginManifest::from_toml(&src).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("64"), "err = {err}");
    }

    #[test]
    fn description_too_long_is_rejected() {
        let desc = "x".repeat(257);
        let src = minimal_toml("ok", "1.0.0", "1", &desc);
        let m = PluginManifest::from_toml(&src).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("description"), "err = {err}");
    }

    #[test]
    fn invalid_semver_is_rejected() {
        for bad in &["1.0", "1", "abc", "1.x.0", ""] {
            let src = minimal_toml("ok", bad, "1", "test");
            let m = PluginManifest::from_toml(&src).expect("parse");
            let err = m.validate().unwrap_err();
            assert!(
                err.to_string().contains("version") || err.to_string().contains("SemVer"),
                "bad version '{bad}' should be rejected, got: {err}"
            );
        }
    }

    #[test]
    fn valid_semver_with_prerelease_accepted() {
        let src = minimal_toml("ok", "1.2.3-alpha.1+build.001", "1", "desc");
        let m = PluginManifest::from_toml(&src).expect("parse");
        m.validate().expect("pre-release SemVer should be accepted");
    }

    #[test]
    fn empty_capabilities_defaults_to_no_grants() {
        let src = minimal_toml("ok", "1.0.0", "1", "desc");
        let m = PluginManifest::from_toml(&src).expect("parse");
        m.validate().expect("valid");
        assert!(!m.capabilities.log);
        assert!(m.capabilities.network.is_empty());
        assert!(m.capabilities.credentials.is_empty());
    }

    #[test]
    fn limits_defaults_match_rfc_spec() {
        let src = minimal_toml("ok", "1.0.0", "1", "desc");
        let m = PluginManifest::from_toml(&src).expect("parse");
        assert_eq!(m.limits.max_memory_bytes, 64 * 1024 * 1024);
        assert_eq!(m.limits.fuel, 1_000_000_000);
        assert_eq!(m.limits.max_call_ticks, 50);
        assert_eq!(m.limits.max_stream_bytes, 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn network_defaults_match_rfc_spec() {
        let src = minimal_toml("ok", "1.0.0", "1", "desc");
        let m = PluginManifest::from_toml(&src).expect("parse");
        assert!(!m.network.allow_http);
        assert_eq!(m.network.max_response_bytes, 8 * 1024 * 1024);
        assert_eq!(m.network.http_connect_timeout_secs, 4);
        assert_eq!(m.network.http_request_timeout_secs, 4);
    }

    #[test]
    fn name_with_hyphen_and_underscore_is_valid() {
        let src = minimal_toml("my-cloud_v2", "1.0.0", "1", "desc");
        let m = PluginManifest::from_toml(&src).expect("parse");
        m.validate().expect("hyphens and underscores are valid");
    }

    #[test]
    fn empty_name_is_rejected() {
        let src = minimal_toml("", "1.0.0", "1", "desc");
        let m = PluginManifest::from_toml(&src).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("name"), "err = {err}");
    }
}
