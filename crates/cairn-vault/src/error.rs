//! [`VaultError`].

/// Errors from vault operations. Never carries secret material.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VaultError {
    /// The target file already exists (on create).
    #[error("vault already exists")]
    AlreadyExists,
    /// Decryption failed — a wrong passphrase or a tampered/corrupt file.
    #[error("decryption failed (wrong passphrase or corrupt vault)")]
    Decrypt,
    /// The file is not a recognizable vault.
    #[error("invalid vault format")]
    Format,
    /// The vault format version is not supported by this build.
    #[error("unsupported vault version {0}")]
    Version(u16),
    /// Key derivation failed.
    #[error("key derivation failed")]
    Kdf,
    /// An underlying I/O error.
    #[error("io error")]
    Io(#[from] std::io::Error),
}
