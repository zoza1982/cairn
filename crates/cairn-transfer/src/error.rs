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
