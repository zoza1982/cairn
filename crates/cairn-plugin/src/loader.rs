//! Plugin loader: directory discovery, manifest parsing, ABI check, grants-from-config, and
//! component instantiation (RFC-0010 §5.1–§5.6).
//!
//! [`PluginLoader`] is the production entry point for loading a named plugin from the plugins
//! directory. Its responsibility is:
//!
//! 1. **Discovery (§5.1):** locate `<name>-<version>/` under the configured plugins directory.
//! 2. **Manifest parse (§5.2):** read and validate `plugin.toml`.
//! 3. **ABI check (§5.3):** verify `api` major matches [`HOST_API_MAJOR`].
//! 4. **Grants from config (§5.4):** read approved grants from `cairn-config`.
//! 5. **Capability-gated instantiation (§5.6):** build a [`PluginGrants`] from approved grants
//!    and call [`PluginComponent::instantiate_with_grants`].
//! 6. **Backend construction:** read `scheme()`/`caps()` and construct a [`PluginVfsBackend`].
//!
//! The loader does NOT implement the approval UI (PR-C2) or the install subcommand (PR-C3) —
//! those are follow-up items. Grants are read from config that the approval UI will later
//! populate; for tests they can be seeded directly into the config.

use crate::manifest::{LimitsConfig, NetworkConfig, PluginManifest, HOST_API_MAJOR};
use crate::{
    Limits, PluginComponent, PluginError, PluginGrants, PluginHostConfig, PluginVfsBackend,
};
use cairn_broker_api::CredentialBroker;
use cairn_config::Config;
use cairn_types::ConnectionId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasmtime::Engine;

/// The maximum wall-clock epoch ceiling in seconds. Timeouts from the manifest are clamped
/// to this value so a plugin cannot park the thread past the epoch deadline.
/// Default epoch: 50 ticks × 100 ms = 5 s.
const EPOCH_CEILING_SECS: u64 = 5;

/// Discovers and loads plugin components from the plugins directory.
///
/// Create one [`PluginLoader`] at startup and reuse it for every `load` call; the
/// wasmtime [`Engine`] is shared and reused across all loaded plugins.
pub struct PluginLoader {
    /// Root of the plugins directory (`${cairn_data_dir}/plugins/` by default).
    dir: PathBuf,
    /// Shared wasmtime engine (component model + fuel + epoch interruption enabled).
    /// Must be built from [`crate::engine_config`].
    engine: Arc<Engine>,
}

impl PluginLoader {
    /// Create a loader that reads plugins from `dir`.
    ///
    /// `engine` must be built from [`crate::engine_config`] (component model, fuel metering,
    /// epoch interruption).
    pub fn new(dir: PathBuf, engine: Arc<Engine>) -> Self {
        Self { dir, engine }
    }

    /// Locate the directory for `<name>-<version>` under the loader's root.
    ///
    /// Returns the path if the directory exists and contains both required files
    /// (`plugin.toml` + `component.wasm`), or a [`PluginError::Compile`] otherwise.
    fn locate(&self, name: &str, version: &str) -> Result<PathBuf, PluginError> {
        // SECURITY: validate name and version BEFORE the path join. A `/` or `..` in
        // either argument would traverse out of the plugins directory (path injection).
        // Reuse the manifest name charset for `name`; apply a path-safe check for `version`.
        validate_plugin_name_arg(name)?;
        validate_plugin_version_arg(version)?;
        let dir = self.dir.join(format!("{name}-{version}"));
        if !dir.is_dir() {
            return Err(PluginError::Compile(format!(
                "plugin directory '{dir}' not found",
                dir = dir.display()
            )));
        }
        if !dir.join("plugin.toml").is_file() {
            return Err(PluginError::Compile(format!(
                "plugin '{name}@{version}': missing 'plugin.toml' in '{dir}'",
                dir = dir.display()
            )));
        }
        if !dir.join("component.wasm").is_file() {
            return Err(PluginError::Compile(format!(
                "plugin '{name}@{version}': missing 'component.wasm' in '{dir}'",
                dir = dir.display()
            )));
        }
        Ok(dir)
    }

    /// Load and instantiate the plugin `<name>@<version>` from the plugins directory.
    ///
    /// Reads the manifest, validates the ABI major, reads approved grants from `config`,
    /// and instantiates the component with only the approved capabilities wired in.
    ///
    /// `connection` is assigned to the resulting [`PluginVfsBackend`] for logging and
    /// identification. `credential_broker` is wired in only when the plugin has a `credentials`
    /// grant; pass `None` to keep `use-credential` as a deny-stub (e.g. when the vault is
    /// locked at load time).
    ///
    /// # Errors
    ///
    /// - [`PluginError::Compile`]: plugin directory / manifest not found, TOML parse error,
    ///   ABI major mismatch, or manifest validation failure.
    /// - [`PluginError::Instantiate`]: the component bytes are invalid, a required import is
    ///   missing (e.g. the plugin imported a socket interface that is not in the linker), or
    ///   a missing grant causes the `http-fetch` / `use-credential` import to be absent.
    ///   The error message names the missing import so it is actionable.
    pub fn load(
        &self,
        name: &str,
        version: &str,
        config: &Config,
        connection: ConnectionId,
        credential_broker: Option<Arc<dyn CredentialBroker>>,
    ) -> Result<PluginVfsBackend, PluginError> {
        // 1. Locate directory.
        let dir = self.locate(name, version)?;

        // 2. Parse and validate the manifest.
        let manifest = read_manifest(&dir)?;

        // Sanity-check: name and version in the manifest must match the directory name.
        if manifest.plugin.name != name {
            return Err(PluginError::Compile(format!(
                "plugin directory is named '{name}' but plugin.toml declares name '{}'",
                manifest.plugin.name
            )));
        }
        if manifest.plugin.version != version {
            return Err(PluginError::Compile(format!(
                "plugin directory is named '{version}' but plugin.toml declares version '{}'",
                manifest.plugin.version
            )));
        }

        // 3. ABI check (§5.3): reject before loading component bytes.
        let api = manifest.plugin.api.trim();
        if api != HOST_API_MAJOR {
            return Err(PluginError::Compile(format!(
                "plugin '{name}@{version}' declares api = '{api}' but this host supports \
                 api = '{HOST_API_MAJOR}'; re-build the plugin against the current ABI"
            )));
        }

        // 4. Grants from config (§5.4).
        let key = format!("{name}@{version}");
        let stored = config.plugin_grants(&key);
        let grants = grants_from_config(&manifest, stored.as_ref());

        // 5. Limits from manifest.
        let limits = limits_from_manifest(&manifest.limits);

        // 6. Network limits from manifest `[network]`, with epoch-ceiling clamp.
        let (max_response_bytes, http_connect_timeout_secs, http_request_timeout_secs, allow_http) =
            http_limits_from_manifest_network(&manifest.network);

        // 7. Read component bytes.
        let wasm_bytes = read_wasm(&dir)?;

        // 8. Build PluginHostConfig: only wire credential_broker if credentials are granted.
        let credential_broker = if grants.credentials.is_empty() {
            None
        } else {
            credential_broker
        };

        let host_config = PluginHostConfig {
            grants,
            plugin_name: name.to_owned(),
            credential_broker,
            max_response_bytes,
            http_connect_timeout_secs,
            http_request_timeout_secs,
            allow_http,
        };

        // 9. Instantiate.
        let component = PluginComponent::instantiate_with_grants(
            &self.engine,
            &wasm_bytes,
            limits,
            host_config,
        )?;

        // 10. Construct the async backend.
        PluginVfsBackend::new(component, limits, connection)
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────────────────────

/// Validate that `name` is a safe plugin name argument before a path join.
///
/// Uses the same charset as the manifest (`[a-z0-9-_]`, non-empty, max 64) so a `/`, `..`,
/// or uppercase letter is caught before `self.dir.join(name-version)` can traverse out of the
/// plugins directory (path injection). This is a defence-in-depth check; the manifest's
/// `validate()` enforces the same rule on the TOML value, but the `load()`/`locate()` args
/// could theoretically arrive from a caller other than the loader flow.
fn validate_plugin_name_arg(name: &str) -> Result<(), PluginError> {
    if name.is_empty() || name.len() > 64 {
        return Err(PluginError::Compile(format!(
            "plugin name argument '{name}' is invalid (must be 1–64 chars of [a-z0-9-_])"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(PluginError::Compile(format!(
            "plugin name argument '{name}' contains characters outside [a-z0-9-_]"
        )));
    }
    Ok(())
}

/// Validate that `version` is path-safe before a path join.
///
/// Permits SemVer characters (`[a-zA-Z0-9._+\-]`) and rejects anything containing `/`, `\`,
/// or `..` that could escape the plugins directory. The version is not validated as a full
/// SemVer string here — that check runs on the manifest value via `validate_semver`.
fn validate_plugin_version_arg(version: &str) -> Result<(), PluginError> {
    if version.is_empty() {
        return Err(PluginError::Compile(
            "plugin version argument must not be empty".to_owned(),
        ));
    }
    // Reject any character that could contribute to path traversal.
    let has_unsafe = version
        .chars()
        .any(|c| c == '/' || c == '\\' || c == '\0' || c.is_control());
    if has_unsafe || version.contains("..") {
        return Err(PluginError::Compile(format!(
            "plugin version argument '{version}' contains unsafe path characters"
        )));
    }
    Ok(())
}

/// Compute the effective HTTP resource limits from the manifest's `[network]` section.
///
/// Timeouts are clamped to [`EPOCH_CEILING_SECS`] so a plugin cannot configure a timeout
/// that outlasts the epoch deadline and parks the plugin thread indefinitely.
///
/// Returns `(max_response_bytes, connect_timeout_secs, request_timeout_secs, allow_http)`.
pub(crate) fn http_limits_from_manifest_network(net: &NetworkConfig) -> (usize, u64, u64, bool) {
    let max_response_bytes = net.max_response_bytes.min(usize::MAX as u64) as usize;
    let connect_timeout = net.http_connect_timeout_secs.min(EPOCH_CEILING_SECS);
    let request_timeout = net.http_request_timeout_secs.min(EPOCH_CEILING_SECS);
    (
        max_response_bytes,
        connect_timeout,
        request_timeout,
        net.allow_http,
    )
}

/// Read and parse `plugin.toml` from `dir`.
fn read_manifest(dir: &Path) -> Result<PluginManifest, PluginError> {
    let path = dir.join("plugin.toml");
    let src = std::fs::read_to_string(&path).map_err(|e| {
        // Intentionally not including the full path in the error: callers already know the
        // plugin name/version; including an FS path in the error is unnecessary and could
        // leak unintended information in sandboxed contexts.
        PluginError::Compile(format!("could not read plugin.toml: {e}"))
    })?;
    let manifest = PluginManifest::from_toml(&src)?;
    manifest.validate()?;
    Ok(manifest)
}

/// Read `component.wasm` from `dir`.
fn read_wasm(dir: &Path) -> Result<Vec<u8>, PluginError> {
    std::fs::read(dir.join("component.wasm"))
        .map_err(|e| PluginError::Compile(format!("could not read component.wasm: {e}")))
}

/// Build [`PluginGrants`] from the manifest-requested capabilities filtered to the stored
/// approved grants for this plugin instance.
///
/// A capability requested by the manifest but **not** in `stored` is dropped silently —
/// the import is absent from the linker, causing instantiation to fail with a clear error
/// naming the missing import if the plugin actually calls that function. This is §5.4's
/// "a manifest-requested capability NOT in the stored grants is dropped" rule.
///
/// If `stored` is `None` (no grants record exists for this plugin, e.g. it was never
/// installed via the approval flow), all capabilities are denied.
fn grants_from_config(
    manifest: &PluginManifest,
    stored: Option<&cairn_config::PluginGrantsRecord>,
) -> PluginGrants {
    let Some(stored) = stored else {
        // No approval record → deny all.
        return PluginGrants::default();
    };

    PluginGrants {
        // `log` is granted only when BOTH the manifest requests it AND the user approved it.
        // Declining `log` makes guest `host::log` calls silently no-ops — the plugin
        // instantiates normally, its log output simply doesn't reach Cairn's log stream.
        log: manifest.capabilities.log && stored.log,
        // Intersect: only hostnames present in BOTH the manifest request AND the stored grants.
        network: manifest
            .capabilities
            .network
            .iter()
            .filter(|h| stored.network.iter().any(|g| g.eq_ignore_ascii_case(h)))
            .cloned()
            .collect(),
        // Same for credentials.
        credentials: manifest
            .capabilities
            .credentials
            .iter()
            .filter(|h| stored.credentials.contains(*h))
            .cloned()
            .collect(),
    }
}

/// Convert the manifest's `[limits]` section to the runtime [`Limits`] type.
fn limits_from_manifest(cfg: &LimitsConfig) -> Limits {
    Limits {
        // Clamp `as usize` — on 32-bit platforms `usize` is 4 GiB max, but the
        // default 64 MiB is well within range. Oversized values saturate to `usize::MAX`.
        max_memory_bytes: cfg.max_memory_bytes.min(usize::MAX as u64) as usize,
        fuel: cfg.fuel,
        max_stream_bytes: cfg.max_stream_bytes,
        max_call_ticks: cfg.max_call_ticks,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::CapabilitiesConfig;
    use cairn_config::{Config, PluginGrantsRecord};
    use tempfile::TempDir;

    /// Write a minimal plugin directory into `root` for the given name/version.
    /// Copies the committed fixture component.wasm so instantiation works.
    fn write_plugin_dir(
        root: &Path,
        name: &str,
        version: &str,
        api: &str,
        has_wasm: bool,
    ) -> PathBuf {
        let dir = root.join(format!("{name}-{version}"));
        std::fs::create_dir_all(&dir).unwrap();

        let toml_src = format!(
            r#"
[plugin]
name        = "{name}"
version     = "{version}"
api         = "{api}"
description = "Test plugin"
"#
        );
        std::fs::write(dir.join("plugin.toml"), &toml_src).unwrap();

        if has_wasm {
            // Copy the committed fixture component so the loader can actually instantiate it.
            let fixture = include_bytes!("../tests/fixtures/backend.wasm");
            std::fs::write(dir.join("component.wasm"), fixture).unwrap();
        }
        dir
    }

    fn engine() -> Arc<Engine> {
        Arc::new(Engine::new(&crate::component::engine_config()).unwrap())
    }

    // ── Discovery tests ────────────────────────────────────────────────────────────────────

    #[test]
    fn missing_dir_is_compile_error() {
        let tmp = TempDir::new().unwrap();
        let loader = PluginLoader::new(tmp.path().to_owned(), engine());
        let err = loader.locate("nope", "1.0.0").unwrap_err();
        assert!(matches!(err, PluginError::Compile(_)), "err = {err:?}");
    }

    #[test]
    fn dir_without_wasm_is_compile_error() {
        let tmp = TempDir::new().unwrap();
        write_plugin_dir(tmp.path(), "test", "1.0.0", "1", false);
        let loader = PluginLoader::new(tmp.path().to_owned(), engine());
        let err = loader.locate("test", "1.0.0").unwrap_err();
        assert!(matches!(err, PluginError::Compile(_)), "err = {err:?}");
    }

    #[test]
    fn dir_with_both_files_is_found() {
        let tmp = TempDir::new().unwrap();
        write_plugin_dir(tmp.path(), "test", "1.0.0", "1", true);
        let loader = PluginLoader::new(tmp.path().to_owned(), engine());
        let dir = loader.locate("test", "1.0.0").expect("should find dir");
        assert!(dir.is_dir());
    }

    // ── ABI check test ─────────────────────────────────────────────────────────────────────

    #[test]
    fn api_major_mismatch_is_compile_error() {
        let tmp = TempDir::new().unwrap();
        // Write a dir with api = "2" (host supports "1").
        write_plugin_dir(tmp.path(), "bad-api", "1.0.0", "2", true);
        let loader = PluginLoader::new(tmp.path().to_owned(), engine());
        let mut config = Config::default();
        // Seed a full grant so the ABI check fires (not a grants-absent early-exit).
        config.set_plugin_grants(
            "bad-api@1.0.0",
            PluginGrantsRecord {
                log: true,
                network: vec![],
                credentials: vec![],
            },
        );
        let result = loader.load("bad-api", "1.0.0", &config, ConnectionId(99), None);
        assert!(result.is_err(), "ABI mismatch must be an error");
        let err = result.err().unwrap();
        assert!(matches!(err, PluginError::Compile(_)), "err = {err:?}");
        assert!(
            err.to_string().contains("api") || err.to_string().contains("ABI"),
            "err = {err}"
        );
    }

    // ── Grants-from-config test ────────────────────────────────────────────────────────────

    #[test]
    fn grants_from_config_intersects_correctly() {
        let manifest_caps = CapabilitiesConfig {
            log: true,
            network: vec!["api.example.com".to_owned(), "auth.example.com".to_owned()],
            credentials: vec!["key-a".to_owned(), "key-b".to_owned()],
        };
        // Stored grants: only one network hostname and one credential label approved.
        let stored = PluginGrantsRecord {
            log: true,
            network: vec!["api.example.com".to_owned()],
            credentials: vec!["key-a".to_owned()],
        };
        let fake_manifest = PluginManifest {
            plugin: crate::manifest::PluginMeta {
                name: "test".to_owned(),
                version: "1.0.0".to_owned(),
                api: "1".to_owned(),
                description: "test".to_owned(),
                homepage: None,
                authors: None,
            },
            capabilities: manifest_caps,
            network: crate::manifest::NetworkConfig::default(),
            limits: crate::manifest::LimitsConfig::default(),
        };
        let grants = grants_from_config(&fake_manifest, Some(&stored));
        // Only the intersection is granted.
        assert!(
            grants.log,
            "log must be granted when both manifest and stored approve it"
        );
        assert_eq!(grants.network, vec!["api.example.com"]);
        assert_eq!(grants.credentials, vec!["key-a"]);
    }

    #[test]
    fn no_stored_grants_means_deny_all() {
        let fake_manifest = PluginManifest {
            plugin: crate::manifest::PluginMeta {
                name: "test".to_owned(),
                version: "1.0.0".to_owned(),
                api: "1".to_owned(),
                description: "test".to_owned(),
                homepage: None,
                authors: None,
            },
            capabilities: CapabilitiesConfig {
                log: true,
                network: vec!["api.example.com".to_owned()],
                credentials: vec!["key".to_owned()],
            },
            network: crate::manifest::NetworkConfig::default(),
            limits: crate::manifest::LimitsConfig::default(),
        };
        let grants = grants_from_config(&fake_manifest, None);
        assert!(grants.network.is_empty());
        assert!(grants.credentials.is_empty());
    }

    // ── Full load test (fixture component, no grants) ──────────────────────────────────────

    #[test]
    fn load_fixture_with_no_grants_succeeds() {
        let tmp = TempDir::new().unwrap();
        write_plugin_dir(tmp.path(), "fixture", "1.0.0", "1", true);
        let loader = PluginLoader::new(tmp.path().to_owned(), engine());
        let config = Config::default(); // No grants seeded → deny all.
        let result = loader.load("fixture", "1.0.0", &config, ConnectionId(1), None);
        // The fixture guest requests no network/credentials, so deny-all is fine.
        assert!(result.is_ok(), "fixture should load without grants");
    }

    // ── Security: charset validation before path join ──────────────────────────────────────

    #[test]
    fn path_traversal_in_name_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let loader = PluginLoader::new(tmp.path().to_owned(), engine());
        // A `/` in the name would escape the plugins directory.
        let err = loader.locate("../../etc/shadow", "1.0.0").unwrap_err();
        assert!(matches!(err, PluginError::Compile(_)), "err = {err:?}");
        // Uppercase — rejected by name charset.
        let err2 = loader.locate("BadPlugin", "1.0.0").unwrap_err();
        assert!(matches!(err2, PluginError::Compile(_)), "err = {err2:?}");
    }

    #[test]
    fn path_traversal_in_version_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let loader = PluginLoader::new(tmp.path().to_owned(), engine());
        let err = loader.locate("myplugin", "../../1.0.0").unwrap_err();
        assert!(matches!(err, PluginError::Compile(_)), "err = {err:?}");
        let err2 = loader.locate("myplugin", "1.0.0/evil").unwrap_err();
        assert!(matches!(err2, PluginError::Compile(_)), "err = {err2:?}");
    }

    // ── Manifest name/version mismatch ─────────────────────────────────────────────────────

    #[test]
    fn manifest_name_mismatch_is_compile_error() {
        let tmp = TempDir::new().unwrap();
        // Directory is named "real-plugin-1.0.0" but plugin.toml declares name = "other".
        let dir = tmp.path().join("real-plugin-1.0.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            r#"
[plugin]
name        = "other"
version     = "1.0.0"
api         = "1"
description = "Mismatched name"
"#,
        )
        .unwrap();
        let fixture = include_bytes!("../tests/fixtures/backend.wasm");
        std::fs::write(dir.join("component.wasm"), fixture).unwrap();

        let loader = PluginLoader::new(tmp.path().to_owned(), engine());
        let config = Config::default();
        let result = loader.load("real-plugin", "1.0.0", &config, ConnectionId(1), None);
        assert!(result.is_err(), "name mismatch must be an error");
        let err = result.err().unwrap();
        assert!(matches!(err, PluginError::Compile(_)), "err = {err:?}");
    }

    // ── Network limits threading ────────────────────────────────────────────────────────────

    #[test]
    fn network_limits_from_manifest_are_clamped_to_epoch_ceiling() {
        use crate::manifest::NetworkConfig;
        let net = NetworkConfig {
            max_response_bytes: 1024,
            http_connect_timeout_secs: 600, // must be clamped to EPOCH_CEILING_SECS
            http_request_timeout_secs: 600,
            allow_http: true,
        };
        let (max_resp, conn, req, http) = http_limits_from_manifest_network(&net);
        assert_eq!(
            max_resp, 1024,
            "max_response_bytes must be threaded through"
        );
        assert_eq!(conn, EPOCH_CEILING_SECS, "connect_timeout must be clamped");
        assert_eq!(req, EPOCH_CEILING_SECS, "request_timeout must be clamped");
        assert!(http, "allow_http must be threaded through");
    }

    #[test]
    fn network_limits_below_ceiling_are_not_clamped() {
        use crate::manifest::NetworkConfig;
        let net = NetworkConfig {
            max_response_bytes: 4 * 1024 * 1024,
            http_connect_timeout_secs: 3,
            http_request_timeout_secs: 2,
            allow_http: false,
        };
        let (_, conn, req, http) = http_limits_from_manifest_network(&net);
        assert_eq!(conn, 3, "timeout below ceiling must not be modified");
        assert_eq!(req, 2, "timeout below ceiling must not be modified");
        assert!(!http);
    }

    // ── Log grant propagation ───────────────────────────────────────────────────────────────

    #[test]
    fn log_denied_when_manifest_denies_even_if_stored_approves() {
        // manifest requests log = false; stored approves log = true → intersection = false
        let fake_manifest = PluginManifest {
            plugin: crate::manifest::PluginMeta {
                name: "test".to_owned(),
                version: "1.0.0".to_owned(),
                api: "1".to_owned(),
                description: "test".to_owned(),
                homepage: None,
                authors: None,
            },
            capabilities: CapabilitiesConfig {
                log: false, // manifest does NOT request log
                network: vec![],
                credentials: vec![],
            },
            network: crate::manifest::NetworkConfig::default(),
            limits: crate::manifest::LimitsConfig::default(),
        };
        let stored = PluginGrantsRecord {
            log: true, // user approved log (from a previous manifest version?)
            network: vec![],
            credentials: vec![],
        };
        let grants = grants_from_config(&fake_manifest, Some(&stored));
        assert!(
            !grants.log,
            "log must be false when manifest does not request it, even if stored approves"
        );
    }

    #[test]
    fn log_denied_when_stored_denies_even_if_manifest_requests() {
        // manifest requests log = true; stored approves log = false → intersection = false
        let fake_manifest = PluginManifest {
            plugin: crate::manifest::PluginMeta {
                name: "test".to_owned(),
                version: "1.0.0".to_owned(),
                api: "1".to_owned(),
                description: "test".to_owned(),
                homepage: None,
                authors: None,
            },
            capabilities: CapabilitiesConfig {
                log: true, // manifest requests log
                network: vec![],
                credentials: vec![],
            },
            network: crate::manifest::NetworkConfig::default(),
            limits: crate::manifest::LimitsConfig::default(),
        };
        let stored = PluginGrantsRecord {
            log: false, // user did NOT approve log
            network: vec![],
            credentials: vec![],
        };
        let grants = grants_from_config(&fake_manifest, Some(&stored));
        assert!(
            !grants.log,
            "log must be false when stored record does not approve it"
        );
    }
}
