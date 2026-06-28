//! [`TransferError`]: errors from the transfer engine.

use cairn_types::VfsPath;
use cairn_vfs::VfsError;

/// An error from a transfer operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransferError {
    /// A backend operation failed.
    #[error(transparent)]
    Vfs(#[from] VfsError),
    /// A path could not be constructed (e.g. an illegal name).
    #[error("invalid path")]
    Path(#[from] cairn_types::PathError),
    /// The destination exists and the conflict policy deferred to the caller.
    #[error("destination conflict: {0}")]
    Conflict(VfsPath),
    /// Post-transfer verification failed for the destination.
    #[error("verification failed: {0}")]
    VerifyFailed(VfsPath),
    /// The transfer was cancelled.
    #[error("cancelled")]
    Cancelled,
}

impl TransferError {
    /// A secret-free, path-free message safe to show in the UI or feed back to the AI layer. The
    /// `Display` impls embed `VfsPath`s and backend messages; this drops them (delegating to
    /// [`VfsError::redacted`] for the backend arm).
    #[must_use]
    pub fn redacted(&self) -> String {
        match self {
            TransferError::Vfs(e) => e.redacted().to_string(),
            TransferError::Path(_) => "invalid path".to_owned(),
            TransferError::Conflict(_) => "destination already exists".to_owned(),
            TransferError::VerifyFailed(_) => "verification failed".to_owned(),
            TransferError::Cancelled => "cancelled".to_owned(),
        }
    }
}
