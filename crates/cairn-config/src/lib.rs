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

/// A reference to a credential stored in the vault. Safe to serialize; reveals nothing.
pub type SecretRef = Uuid;

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            ui: UiConfig::default(),
            connections: Vec::new(),
            shell_actions: Vec::new(),
        }
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

    /// Serialize and write the config to `path` (creating parent directories).
    ///
    /// # Errors
    /// Serialization or I/O errors.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
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
        assert!(action("ok", "/usr/bin/sha256sum", &["--", "{path}"])
            .validate()
            .is_ok());
        assert!(action("ok-bare", "sha256sum", &["{name}"])
            .validate()
            .is_ok());
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
    fn secure_shell_actions_noop_without_actions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        assert!(cfg.secure_shell_actions(&path).is_none());
    }
}
