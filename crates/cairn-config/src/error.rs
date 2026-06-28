//! [`ConfigError`].

/// Errors from loading or saving configuration.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The TOML could not be parsed.
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    /// The config could not be serialized.
    #[error("config serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
    /// The file's schema version is newer than this build supports.
    #[error("unsupported config version {found}; this build supports up to {supported}")]
    Version {
        /// The version found in the file.
        found: u32,
        /// The newest version this build understands.
        supported: u32,
    },
    /// An underlying I/O error.
    #[error("io error")]
    Io(#[from] std::io::Error),
}
