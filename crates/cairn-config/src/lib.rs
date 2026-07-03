//! Cairn's human-editable configuration.
//!
//! Stored as TOML. Holds UI preferences and **connection profiles**. By construction a
//! [`ConnectionProfile`] cannot hold a secret — it carries only non-secret endpoint fields and an
//! optional [`secret_ref`](ConnectionProfile::secret_ref) (a vault credential id). Credentials live
//! only in the encrypted vault. See `docs/LLD.md` §13.
//!
//! SECURITY: the optional [`shell_actions`](Config::shell_actions) section defines programs Cairn can
//! execute, so a config carrying it is **no longer safe to import from an untrusted source** — a
//! hostile file would otherwise run arbitrary commands. [`Config::secure_shell_actions`] gates loading
//! that section on file ownership/permissions (Unix), and the binary confirms before each run. The
//! rest of the config (UI, connections) remains secret-free and safe to share.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

mod error;
pub use error::ConfigError;

/// The current config schema version.
pub const SCHEMA_VERSION: u32 = 1;

// ── Plugin capability grants ───────────────────────────────────────────────────────────────────

/// The user's approved capability grants for one plugin version (RFC-0010 §5.4).
///
/// Stored in `cairn.toml` under `[plugins."<name>@<version>".grants]`. Not secret — this is
/// the user's *intent record* (what they approved at install time), not a credential. Declined
/// capabilities are simply absent (empty list or `false`).
///
/// Written by the approval UI (PR-C2) and read by `PluginLoader` (`cairn-plugin`) at mount time.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PluginGrantsRecord {
    /// Whether the plugin is approved to write to Cairn's log via `host::log`.
    pub log: bool,
    /// Hostnames this plugin is approved to contact via `host::http-fetch`.
    /// May be a strict subset of what the manifest requested (user narrowed the grant).
    pub network: Vec<String>,
    /// Credential handle labels this plugin is approved to use via `host::use-credential`.
    pub credentials: Vec<String>,
}

/// One entry in the `[plugins]` table for a specific plugin version.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginEntry {
    /// The approved capability grants for this plugin version.
    pub grants: PluginGrantsRecord,
}

/// A reference to a credential stored in the vault. Safe to serialize; reveals nothing.
pub type SecretRef = Uuid;

/// Auto-discovery preferences (`[discovery]`).
///
/// Controls which environment sources the coordinator probes for connections automatically.
/// All flags default to `true` (discovery is on by default); users can opt out per-source, and
/// specific discovered entries can be hidden or pinned via their stable key strings.
///
/// This section is additive and backward-compatible: a config that predates P3 loads with all
/// fields at their defaults (thanks to `#[serde(default)]`).
///
/// ## Key string format
///
/// Used in `hidden` and `pinned`:
/// - Docker socket (default): `"docker:socket:default"`
/// - Docker socket (explicit path): `"docker:socket:/run/user/1000/docker.sock"`
/// - Kubeconfig cluster: `"kube:kubeconfig"`
/// - In-cluster Kubernetes: `"kube:in-cluster"`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Whether to auto-discover Docker daemon sockets (default `true`).
    pub docker: bool,
    /// Whether to auto-discover Kubernetes clusters from kubeconfig / in-cluster (default `true`).
    pub kubernetes: bool,
    /// Discovered entries to hide from the connection switcher, identified by their key strings.
    /// A hidden entry is never shown even if discovered and reachable.
    pub hidden: Vec<String>,
    /// Discovered entries to float to the top of the connection list, in stated order.
    /// A pinned key that is not discovered is silently ignored.
    pub pinned: Vec<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            docker: true,
            kubernetes: true,
            hidden: Vec::new(),
            pinned: Vec::new(),
        }
    }
}

/// The whole configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Schema version for forward-compatible migration.
    pub version: u32,
    /// UI preferences.
    pub ui: UiConfig,
    /// Saved connection profiles.
    pub connections: Vec<ConnectionProfile>,
    /// User-defined shell-command actions (M8-7). **Security-sensitive**: each entry can run a local
    /// program. Gated by [`secure_shell_actions`](Config::secure_shell_actions) (file-trust) and an
    /// on-run confirm. Empty by default.
    #[serde(default)]
    pub shell_actions: Vec<ShellActionDef>,
    /// Transfer-engine preferences.
    #[serde(default)]
    pub transfers: TransfersConfig,
    /// Secrets-vault preferences.
    #[serde(default)]
    pub vault: VaultConfig,
    /// Auto-discovery preferences (RFC-0011 P3). Controls which environment sources are probed
    /// for connections at startup and on re-enumeration.
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// Installed plugin entries, keyed by `"<name>@<version>"` (RFC-0010 §5.4).
    ///
    /// Written by the plugin approval UI (PR-C2); read by `PluginLoader` at mount time.
    /// Not secret — this is the user's capability grant record.
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginEntry>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            ui: UiConfig::default(),
            connections: Vec::new(),
            shell_actions: Vec::new(),
            transfers: TransfersConfig::default(),
            vault: VaultConfig::default(),
            discovery: DiscoveryConfig::default(),
            plugins: BTreeMap::new(),
        }
    }
}

/// Secrets-vault preferences (`[vault]`).
///
/// Holds only the (non-secret) location of the encrypted vault file. The vault's contents — the
/// credentials — live exclusively inside that encrypted file, never in this config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VaultConfig {
    /// Path to the encrypted vault file. When unset, [`Config::vault_path`] falls back to the
    /// platform default (`…/cairn/vault.cvlt`, see [`default_vault_path`]).
    pub path: Option<PathBuf>,
}

impl Config {
    /// The resolved vault file path: the configured [`VaultConfig::path`] if set, otherwise the
    /// platform default (`…/cairn/vault.cvlt`). Returns `None` only when neither is available (no
    /// configured path *and* no determinable platform config directory).
    #[must_use]
    pub fn vault_path(&self) -> Option<PathBuf> {
        self.vault.path.clone().or_else(default_vault_path)
    }

    /// Return the approved grants record for the plugin identified by `key` (`"name@version"`).
    ///
    /// Returns `None` if the plugin has no entry in the config (not yet installed / approved).
    /// The loader treats `None` as "deny all" — no capabilities are granted.
    #[must_use]
    pub fn plugin_grants(&self, key: &str) -> Option<PluginGrantsRecord> {
        self.plugins.get(key).map(|e| e.grants.clone())
    }

    /// Write (or overwrite) the approved grants for the plugin identified by `key`.
    ///
    /// This is called by the approval UI (PR-C2) after the user approves capabilities at
    /// install time. Call [`save`](Config::save) afterwards to persist the change.
    pub fn set_plugin_grants(&mut self, key: &str, grants: PluginGrantsRecord) {
        self.plugins.entry(key.to_owned()).or_default().grants = grants;
    }

    /// Remove the grants record for a plugin (revoke all capabilities).
    ///
    /// The plugin directory is NOT removed; only the grants are cleared. On next load,
    /// the loader will see no grants and deny all capabilities (the plugin will fail
    /// to instantiate if it actually uses a brokered function).
    pub fn revoke_plugin_grants(&mut self, key: &str) {
        self.plugins.remove(key);
    }
}

/// Transfer-engine preferences (`[transfers]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TransfersConfig {
    /// How many transfers may run at once (the raw config value). Read it through
    /// [`effective_concurrency`](TransfersConfig::effective_concurrency), which clamps to `>= 1`;
    /// `1` is strict FIFO. Default 2.
    pub concurrency: usize,
}

impl Default for TransfersConfig {
    fn default() -> Self {
        Self { concurrency: 2 }
    }
}

impl TransfersConfig {
    /// The effective concurrency limit, clamped to at least 1 (a configured `0` would wedge the
    /// queue — every transfer would be queued and none would ever start).
    #[must_use]
    pub fn effective_concurrency(&self) -> usize {
        self.concurrency.max(1)
    }
}

/// A user-defined shell-command action: bind a key to run a local program against the entry under the
/// cursor. **Security-sensitive** — see the module docs and [`validate`](ShellActionDef::validate).
///
/// Example:
/// ```toml
/// [[shell_actions]]
/// name = "Checksum"
/// key = "ctrl+h"
/// command = "/usr/bin/sha256sum"
/// args = ["--", "{path}"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellActionDef {
    /// Human-readable name, shown in the confirm prompt and status line.
    pub name: String,
    /// Key chord that triggers it (same syntax as `[ui.keybindings]`, e.g. `"ctrl+h"`).
    pub key: String,
    /// The program to run: an absolute path, or a bare name resolved via `PATH`. Relative paths with
    /// a separator (`./x`, `bin/x`) and `.bat`/`.cmd` are rejected.
    pub command: String,
    /// Argument templates. Each element is one argv element (never re-split / shell-parsed). The
    /// placeholders `{path}` (canonical OS path of the entry), `{dir}` (its parent), and `{name}`
    /// (file name) are expanded; any other `{...}` token is a config error.
    #[serde(default)]
    pub args: Vec<String>,
    /// Whether to confirm before running. Defaults to `true` (security default); set `false` per
    /// action to skip the prompt for a trusted, benign command.
    #[serde(default = "default_true")]
    pub confirm: bool,
}

fn default_true() -> bool {
    true
}

impl ShellActionDef {
    /// Validate the static (path-independent) shape of the action. Returns a human-readable reason on
    /// rejection. Called at startup so a bad entry is dropped with a warning rather than failing later.
    ///
    /// # Errors
    /// Empty name/command; a relative command containing a path separator; a `.bat`/`.cmd` command; or
    /// an arg containing an unknown `{...}` placeholder.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("action has an empty name".to_owned());
        }
        if self.command.is_empty() {
            return Err(format!("action '{}' has an empty command", self.name));
        }
        // Program must be absolute or a bare name (resolved via PATH). A relative path *with* a
        // separator (`./tool`, `bin/tool`) could execute a binary from the browsed directory.
        let p = Path::new(&self.command);
        let has_sep = self.command.contains('/') || self.command.contains('\\');
        if has_sep && !p.is_absolute() {
            return Err(format!(
                "action '{}': command must be an absolute path or a bare name, not a relative path",
                self.name
            ));
        }
        // Windows batch files route through cmd.exe with hazardous quoting — reject on every platform.
        let lower = self.command.to_ascii_lowercase();
        if lower.ends_with(".bat") || lower.ends_with(".cmd") {
            return Err(format!(
                "action '{}': .bat/.cmd commands are not allowed",
                self.name
            ));
        }
        for arg in &self.args {
            if let Some(bad) = unknown_placeholder(arg) {
                return Err(format!(
                    "action '{}': unknown placeholder '{{{bad}}}' in argument",
                    self.name
                ));
            }
        }
        Ok(())
    }
}

/// The placeholders an argument may contain. Anything else inside `{...}` is rejected at validation.
pub const SHELL_ACTION_PLACEHOLDERS: [&str; 3] = ["path", "dir", "name"];

/// Return the first invalid placeholder token inside `{...}` in `arg`, if any — either an unknown name
/// or an unbalanced `{` with no closing `}`. The string is reported back in the validation error.
fn unknown_placeholder(arg: &str) -> Option<String> {
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // Unbalanced `{` — a typo/truncated placeholder; reject rather than passing it literally.
            return Some(rest[open..].to_owned());
        };
        let name = &after[..close];
        if !SHELL_ACTION_PLACEHOLDERS.contains(&name) {
            return Some(name.to_owned());
        }
        rest = &after[close + 1..];
    }
    None
}

/// UI preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Keybinding preset: `"mc"`, `"vim"`, or `"custom"`.
    pub keymap: String,
    /// Theme name.
    pub theme: String,
    /// User keybinding overrides, applied on top of the preset: a map of key-chord → action name.
    /// Chords look like `"ctrl+a"`, `"j"`, `"f5"`, `"enter"`, `"space"`; action names are snake_case
    /// (`"cursor_down"`, `"copy"`, `"ai_propose"`, …). Unparseable entries are ignored with a
    /// warning rather than rejecting the whole config.
    #[serde(default)]
    pub keybindings: BTreeMap<String, String>,
    /// Theme color overrides on top of the [`theme`](UiConfig::theme) preset: a map of role →
    /// color. Roles are `focused_border`/`unfocused_border`/`dir`/`error`/`status`/`selection_bg`/
    /// `selection_fg`; color *values must be strings* — names (`"cyan"`, `"bright-blue"`) or
    /// `#rrggbb`. Unknown roles, unparseable colors, and an unknown `theme` preset are ignored with a
    /// warning (the dark preset is used).
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            keymap: "mc".to_owned(),
            theme: "dark".to_owned(),
            keybindings: BTreeMap::new(),
            colors: BTreeMap::new(),
        }
    }
}

/// A saved connection. Holds only non-secret endpoint fields plus an optional reference to a vault
/// credential — **never** a secret value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionProfile {
    /// Stable id.
    pub id: Uuid,
    /// URI scheme (`"ssh"`, `"s3"`, …).
    pub scheme: String,
    /// Human-readable name.
    pub display_name: String,
    /// Non-secret endpoint fields (host, bucket, region, context, …).
    #[serde(default)]
    pub endpoint: BTreeMap<String, String>,
    /// Optional reference to the vault credential used to connect.
    #[serde(default)]
    pub secret_ref: Option<SecretRef>,
}

impl ConnectionProfile {
    /// Create a new profile with a fresh id and no credential reference.
    #[must_use]
    pub fn new(scheme: &str, display_name: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            scheme: scheme.to_owned(),
            display_name: display_name.to_owned(),
            endpoint: BTreeMap::new(),
            secret_ref: None,
        }
    }
}

impl Config {
    /// Load config from `path`, returning the default config if the file does not exist.
    ///
    /// # Errors
    /// [`ConfigError::Parse`] for malformed TOML, [`ConfigError::Version`] if the file is newer than
    /// this build supports, or an I/O error.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(ConfigError::Io(e)),
        };
        let config: Config = toml::from_str(&text)?;
        if config.version > SCHEMA_VERSION {
            return Err(ConfigError::Version {
                found: config.version,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(config)
    }

    /// Gate the executable [`shell_actions`](Config::shell_actions) on the trust of the file they came
    /// from. Because that section can run programs, loading it from a file other users can modify would
    /// be a privilege-escalation vector. On Unix, if `path` is owned by another user or is group-/
    /// world-writable, the actions are dropped and a warning is returned (the rest of the config is
    /// kept). Returns `None` when the actions are trusted (or there are none). No-op on non-Unix for
    /// now (returns a warning if any actions are present so the gap is visible).
    ///
    /// Call this immediately after [`load`](Config::load), before using `shell_actions`.
    #[must_use]
    pub fn secure_shell_actions(&mut self, path: &Path) -> Option<String> {
        if self.shell_actions.is_empty() {
            return None;
        }
        match shell_actions_file_trusted(path) {
            Ok(true) => None,
            Ok(false) => {
                let n = self.shell_actions.len();
                self.shell_actions.clear();
                Some(format!(
                    "ignoring {n} shell action(s): {} is writable by other users or not owned by you",
                    path.display()
                ))
            }
            Err(reason) => {
                let n = self.shell_actions.len();
                self.shell_actions.clear();
                Some(format!("ignoring {n} shell action(s): {reason}"))
            }
        }
    }

    /// Serialize and write the config to `path` (creating parent directories), atomically.
    ///
    /// Writes to a temp file in the same directory, fsyncs it, then renames it into place —
    /// mirroring `cairn-vault`'s `atomic_write`. A reader (or a crash mid-write) never observes a
    /// partially-written `cairn.toml`; frequent small writes (RFC-0011 P6 pin/hide toggles,
    /// connection add/edit/delete) never risk corrupting the file a user hand-edits.
    ///
    /// # Errors
    /// Serialization or I/O errors.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        atomic_write(path, text.as_bytes())
    }
}

/// Write `bytes` to `path` atomically: a temp file created in the same directory (so the final
/// rename is same-filesystem and therefore atomic), fsynced, then persisted over `path`.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| ConfigError::Io(e.error))?;
    Ok(())
}

/// Whether `path` is trusted to define executable shell actions: owned by the current user and not
/// writable by group or others. Unix only; on other platforms returns `Err` so the caller surfaces
/// that the gate is not yet enforced there.
#[cfg(unix)]
fn shell_actions_file_trusted(path: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot stat config: {e}"))?;
    let mode = meta.mode();
    // Reject group- or world-writable (0o022). Owner-writable (0o200) is expected.
    if mode & 0o022 != 0 {
        return Ok(false);
    }
    // Reject files not owned by the current user.
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(not(unix))]
fn shell_actions_file_trusted(_path: &Path) -> Result<bool, String> {
    Err("shell-action file-trust check is not implemented on this platform".to_owned())
}

/// The default config file path for this platform (`…/cairn/config.toml`), if it can be determined.
#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "Cairn", "cairn")
        .map(|d| d.config_dir().join("config.toml"))
}

/// The default vault file path for this platform (`…/cairn/vault.cvlt`), if it can be determined.
///
/// Lives alongside the config file in the platform config directory; the file itself is encrypted,
/// so storing it there is safe.
#[must_use]
pub fn default_vault_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "Cairn", "cairn")
        .map(|d| d.config_dir().join("vault.cvlt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg.version, SCHEMA_VERSION);
        assert_eq!(cfg.ui.keymap, "mc");
        assert_eq!(cfg.ui.theme, "dark"); // load-bearing: the resolver falls back to dark
        assert!(cfg.connections.is_empty());
    }

    #[test]
    fn roundtrip_with_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        let mut prof = ConnectionProfile::new("s3", "prod-backups");
        prof.endpoint.insert("bucket".into(), "prod-backups".into());
        prof.endpoint.insert("region".into(), "eu-west-1".into());
        prof.secret_ref = Some(Uuid::new_v4());
        cfg.connections.push(prof);
        cfg.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.connections.len(), 1);
        let p = &loaded.connections[0];
        assert_eq!(p.scheme, "s3");
        assert_eq!(
            p.endpoint.get("region").map(String::as_str),
            Some("eu-west-1")
        );
        assert!(p.secret_ref.is_some());
    }

    /// `Config::save` writes atomically (temp file + rename): after a successful save, only the
    /// target file exists in the directory — the temp file used for the rename must never be
    /// left behind.
    #[test]
    fn save_is_atomic_no_stray_temp_files_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cairn.toml");
        Config::default().save(&path).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("cairn.toml")]);
    }

    /// A second `save` over an existing file (as every pin/hide toggle and connection
    /// add/edit/delete does) must fully replace its contents, not merge or append.
    #[test]
    fn save_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cairn.toml");
        let mut cfg = Config::default();
        cfg.discovery.pinned = vec!["a".to_owned()];
        cfg.save(&path).unwrap();

        cfg.discovery.pinned = vec!["b".to_owned()];
        cfg.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.discovery.pinned, vec!["b".to_owned()]);
    }

    #[test]
    fn serialized_config_contains_no_secret_values() {
        // A profile cannot hold a secret (no such field); only a reference id. Confirm the on-disk
        // form contains the reference but nothing that looks like a credential value.
        let mut cfg = Config::default();
        let mut prof = ConnectionProfile::new("ssh", "bastion");
        prof.endpoint
            .insert("host".into(), "bastion.example".into());
        prof.secret_ref = Some(Uuid::new_v4());
        cfg.connections.push(prof);
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("secret_ref"));
        assert!(text.contains("bastion.example"));
        assert!(!text.to_lowercase().contains("password"));
        assert!(!text.to_lowercase().contains("private_key"));
    }

    #[test]
    fn theme_colors_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.ui.theme = "dark".into();
        cfg.ui
            .colors
            .insert("focused_border".into(), "magenta".into());
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded.ui.colors.get("focused_border").map(String::as_str),
            Some("magenta")
        );
    }

    #[test]
    fn keybindings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.ui
            .keybindings
            .insert("ctrl+a".into(), "ai_propose".into());
        cfg.ui.keybindings.insert("x".into(), "delete".into());
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded.ui.keybindings.get("ctrl+a").map(String::as_str),
            Some("ai_propose")
        );
        assert_eq!(
            loaded.ui.keybindings.get("x").map(String::as_str),
            Some("delete")
        );
    }

    #[test]
    fn config_without_keybindings_section_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n[ui]\nkeymap = \"vim\"\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.ui.keymap, "vim");
        assert!(cfg.ui.keybindings.is_empty());
        assert!(cfg.ui.colors.is_empty());
    }

    #[test]
    fn future_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 999\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Version { .. })
        ));
    }

    fn action(name: &str, command: &str, args: &[&str]) -> ShellActionDef {
        ShellActionDef {
            name: name.to_owned(),
            key: "ctrl+h".to_owned(),
            command: command.to_owned(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            confirm: true,
        }
    }

    #[test]
    fn shell_action_validation() {
        // A bare program name (PATH-resolved) is valid on every platform.
        assert!(action("ok-bare", "sha256sum", &["{name}", "--", "{path}"])
            .validate()
            .is_ok());
        // A platform-appropriate *absolute* path is valid (Unix `/usr/...` is not absolute on Windows).
        #[cfg(unix)]
        assert!(action("ok-abs", "/usr/bin/sha256sum", &["{path}"])
            .validate()
            .is_ok());
        #[cfg(windows)]
        assert!(
            action("ok-abs", r"C:\Windows\System32\where.exe", &["{path}"])
                .validate()
                .is_ok()
        );
        // empty name / command
        assert!(action("", "x", &[]).validate().is_err());
        assert!(action("n", "", &[]).validate().is_err());
        // relative-with-separator program
        assert!(action("n", "./tool", &[]).validate().is_err());
        assert!(action("n", "bin/tool", &[]).validate().is_err());
        // batch files
        assert!(action("n", "evil.bat", &[]).validate().is_err());
        assert!(action("n", "EVIL.CMD", &[]).validate().is_err());
        // unknown placeholder
        assert!(action("n", "echo", &["{oops}"]).validate().is_err());
        // unbalanced brace (no closing `}`) is rejected, not silently accepted
        assert!(action("n", "echo", &["{path"]).validate().is_err());
        // known placeholders embedded in text are fine
        assert!(action("n", "tar", &["{name}.tgz", "{dir}"])
            .validate()
            .is_ok());
    }

    #[test]
    fn shell_actions_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.shell_actions
            .push(action("Checksum", "/usr/bin/sha256sum", &["--", "{path}"]));
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.shell_actions.len(), 1);
        assert_eq!(loaded.shell_actions[0].name, "Checksum");
        assert!(loaded.shell_actions[0].confirm, "confirm defaults to true");
    }

    #[test]
    fn confirm_defaults_to_true_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "version = 1\n[[shell_actions]]\nname = \"x\"\nkey = \"f9\"\ncommand = \"true\"\n",
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.shell_actions[0].confirm);
    }

    #[cfg(unix)]
    #[test]
    fn secure_shell_actions_drops_world_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.shell_actions.push(action("x", "true", &[]));
        cfg.save(&path).unwrap();
        // World-writable config → actions dropped with a warning.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let warn = cfg.secure_shell_actions(&path);
        assert!(warn.is_some(), "expected a warning");
        assert!(cfg.shell_actions.is_empty(), "actions must be dropped");
    }

    #[cfg(unix)]
    #[test]
    fn secure_shell_actions_keeps_owner_only_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.shell_actions.push(action("x", "true", &[]));
        cfg.save(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let warn = cfg.secure_shell_actions(&path);
        assert!(warn.is_none(), "owner-only config is trusted: {warn:?}");
        assert_eq!(cfg.shell_actions.len(), 1);
    }

    #[test]
    fn transfers_concurrency_defaults_and_clamps() {
        assert_eq!(Config::default().transfers.effective_concurrency(), 2);
        assert_eq!(
            TransfersConfig { concurrency: 4 }.effective_concurrency(),
            4
        );
        // A configured 0 is clamped to 1 (else the queue would never drain).
        assert_eq!(
            TransfersConfig { concurrency: 0 }.effective_concurrency(),
            1
        );
    }

    #[test]
    fn transfers_config_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.transfers.concurrency = 3;
        cfg.save(&path).unwrap();
        assert_eq!(
            Config::load(&path)
                .unwrap()
                .transfers
                .effective_concurrency(),
            3
        );
    }

    #[test]
    fn vault_path_defaults_when_unset_and_uses_config_when_set() {
        // Unset → falls back to the platform default (which may be None in a sandbox with no config
        // dir, but must equal `default_vault_path()` either way).
        let cfg = Config::default();
        assert_eq!(cfg.vault_path(), default_vault_path());
        // An explicit path wins and is returned verbatim.
        let mut cfg = Config::default();
        cfg.vault.path = Some(PathBuf::from("/custom/vault.cvlt"));
        assert_eq!(cfg.vault_path(), Some(PathBuf::from("/custom/vault.cvlt")));
    }

    #[test]
    fn vault_path_roundtrips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.vault.path = Some(PathBuf::from("/srv/secrets/cairn.cvlt"));
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded.vault.path.as_deref(),
            Some(Path::new("/srv/secrets/cairn.cvlt"))
        );
    }

    #[test]
    fn config_without_vault_section_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n[ui]\nkeymap = \"mc\"\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        // A config predating the [vault] section loads with the field absent.
        assert!(cfg.vault.path.is_none());
    }

    #[test]
    fn secure_shell_actions_noop_without_actions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        assert!(cfg.secure_shell_actions(&path).is_none());
    }

    // ── Plugin grants (RFC-0010 §5.4) ─────────────────────────────────────────────────────

    #[test]
    fn plugin_grants_roundtrip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.set_plugin_grants(
            "my-cloud@0.2.1",
            PluginGrantsRecord {
                log: true,
                network: vec!["api.mycloud.example.com".to_owned()],
                credentials: vec!["my-cloud-key".to_owned()],
            },
        );
        cfg.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        let grants = loaded
            .plugin_grants("my-cloud@0.2.1")
            .expect("grants must be present");
        assert!(grants.log);
        assert_eq!(grants.network, vec!["api.mycloud.example.com"]);
        assert_eq!(grants.credentials, vec!["my-cloud-key"]);
    }

    #[test]
    fn plugin_grants_absent_returns_none() {
        let cfg = Config::default();
        assert!(cfg.plugin_grants("nonexistent@1.0.0").is_none());
    }

    #[test]
    fn revoke_plugin_grants_removes_entry() {
        let mut cfg = Config::default();
        cfg.set_plugin_grants(
            "my-cloud@0.2.1",
            PluginGrantsRecord {
                log: true,
                network: vec!["api.mycloud.example.com".to_owned()],
                credentials: vec![],
            },
        );
        assert!(cfg.plugin_grants("my-cloud@0.2.1").is_some());
        cfg.revoke_plugin_grants("my-cloud@0.2.1");
        assert!(
            cfg.plugin_grants("my-cloud@0.2.1").is_none(),
            "grants must be removed after revoke"
        );
    }

    #[test]
    fn config_without_plugins_section_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n[ui]\nkeymap = \"mc\"\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.plugins.is_empty(), "plugins section absent → empty map");
    }

    #[test]
    fn multiple_plugin_grants_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.set_plugin_grants(
            "plugin-a@1.0.0",
            PluginGrantsRecord {
                log: true,
                network: vec![],
                credentials: vec![],
            },
        );
        cfg.set_plugin_grants(
            "plugin-b@2.0.0",
            PluginGrantsRecord {
                log: false,
                network: vec!["api.b.example.com".to_owned()],
                credentials: vec!["key-b".to_owned()],
            },
        );
        cfg.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        let a = loaded.plugin_grants("plugin-a@1.0.0").unwrap();
        assert!(a.log);
        assert!(a.network.is_empty());
        let b = loaded.plugin_grants("plugin-b@2.0.0").unwrap();
        assert!(!b.log);
        assert_eq!(b.network, vec!["api.b.example.com"]);
        assert_eq!(b.credentials, vec!["key-b"]);
    }

    #[test]
    fn plugin_grants_not_secret_in_serialized_form() {
        // Verify the serialized config contains the grants as plain text (not encrypted).
        // Plugin grants are NOT secret — they are the user's intent record.
        let mut cfg = Config::default();
        cfg.set_plugin_grants(
            "my-cloud@0.2.1",
            PluginGrantsRecord {
                log: true,
                network: vec!["api.example.com".to_owned()],
                credentials: vec!["my-key".to_owned()],
            },
        );
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            text.contains("api.example.com"),
            "network grant must be visible in TOML"
        );
        assert!(
            text.contains("my-key"),
            "credential label must be visible in TOML"
        );
        assert!(
            !text.to_lowercase().contains("password"),
            "must contain no secret values"
        );
    }

    // ── DiscoveryConfig ──────────────────────────────────────────────────────────────────────

    /// A config file without a `[discovery]` section must load cleanly with all defaults.
    #[test]
    fn discovery_config_defaults_when_section_absent() {
        let toml = r#"version = 1"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.discovery.docker, "docker discovery defaults to true");
        assert!(
            cfg.discovery.kubernetes,
            "kubernetes discovery defaults to true"
        );
        assert!(cfg.discovery.hidden.is_empty(), "hidden defaults to empty");
        assert!(cfg.discovery.pinned.is_empty(), "pinned defaults to empty");
    }

    /// Round-trip: serialize a custom `DiscoveryConfig` to TOML and parse it back.
    #[test]
    fn discovery_config_roundtrips_through_toml() {
        let mut cfg = Config::default();
        cfg.discovery.docker = false;
        cfg.discovery.kubernetes = true;
        cfg.discovery.hidden = vec!["docker:socket:/run/docker.sock".to_owned()];
        cfg.discovery.pinned = vec!["kube:kubeconfig".to_owned()];

        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert!(
            !parsed.discovery.docker,
            "docker=false must survive round-trip"
        );
        assert!(
            parsed.discovery.kubernetes,
            "kubernetes=true must survive round-trip"
        );
        assert_eq!(
            parsed.discovery.hidden,
            vec!["docker:socket:/run/docker.sock"]
        );
        assert_eq!(parsed.discovery.pinned, vec!["kube:kubeconfig"]);
    }

    /// The `hidden` and `pinned` lists accept arbitrary key strings without parse errors.
    #[test]
    fn discovery_config_accepts_arbitrary_key_strings() {
        let toml = r#"
version = 1

[discovery]
docker = true
kubernetes = false
hidden = ["builtin:/", "saved:00000000-0000-0000-0000-000000000000"]
pinned = ["kube:in-cluster"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.discovery.docker);
        assert!(!cfg.discovery.kubernetes);
        assert_eq!(cfg.discovery.hidden.len(), 2);
        assert_eq!(cfg.discovery.pinned.len(), 1);
    }
}
