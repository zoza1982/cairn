//! Cairn's human-editable configuration.
//!
//! Stored as TOML. Holds UI preferences and **connection profiles**. By construction a
//! [`ConnectionProfile`] cannot hold a secret — it carries only non-secret endpoint fields and an
//! optional [`secret_ref`](ConnectionProfile::secret_ref) (a vault credential id). Credentials live
//! only in the encrypted vault; config files, bookmarks, and session state are therefore always
//! safe to read or share. See `docs/LLD.md` §13.

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            ui: UiConfig::default(),
            connections: Vec::new(),
        }
    }
}

/// UI preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Keybinding preset: `"mc"`, `"vim"`, or `"custom"`.
    pub keymap: String,
    /// Theme name.
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            keymap: "mc".to_owned(),
            theme: "dark".to_owned(),
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
    fn future_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 999\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Version { .. })
        ));
    }
}
