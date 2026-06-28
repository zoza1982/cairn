//! [`VfsError`]: the uniform error every backend maps its failures into.

use cairn_types::{Caps, VfsPath};
use std::time::Duration;

/// A boxed, thread-safe error source used to carry an underlying SDK/transport error without
/// committing to its concrete type. Implementors are responsible for scrubbing secrets first.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The uniform error type returned by every [`Vfs`](crate::Vfs) operation.
///
/// Backends translate their native/SDK errors into these variants. The set is `#[non_exhaustive]`
/// so new variants can be added without breaking downstream `match` arms (always include a `_`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VfsError {
    /// The path does not exist.
    #[error("not found: {0}")]
    NotFound(VfsPath),
    /// The operation was denied by permissions / access policy.
    #[error("permission denied: {0}")]
    Forbidden(VfsPath),
    /// The target already exists and the operation required it not to.
    #[error("already exists: {0}")]
    AlreadyExists(VfsPath),
    /// The backend does not support the requested operation. The flags name the missing capability.
    #[error("operation not supported by backend ({0:?})")]
    Unsupported(Caps),
    /// The operation timed out.
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    /// Establishing or using the connection failed. The source never embeds secrets.
    #[error("connection failed")]
    Connection(#[source] BoxError),
    /// Authentication failed. No credential material is included.
    #[error("authentication failed")]
    Auth,
    /// A conflict or failed precondition (e.g. a conditional write returned 412).
    #[error("conflict / precondition failed")]
    Conflict,
    /// A backend-specific error with a stable code and a (secret-free) message.
    #[error("backend error [{code}]: {msg}")]
    Backend {
        /// A short, stable, machine-friendly code (e.g. `"NoSuchBucket"`).
        code: String,
        /// A human-readable, secret-free message.
        msg: String,
        /// Whether retrying the operation might succeed.
        retryable: bool,
    },
    /// The operation was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// An underlying I/O error.
    #[error("io error")]
    Io(#[source] std::io::Error),
}

impl VfsError {
    /// Whether retrying this operation might succeed (timeouts, transient backend errors, dropped
    /// connections). Drives the transfer engine's retry logic and a UI "retry" affordance.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout(_) | Self::Connection(_) => true,
            Self::Backend { retryable, .. } => *retryable,
            Self::Io(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::TimedOut
            ),
            _ => false,
        }
    }

    /// A `Display` wrapper that is safe to log: it never reveals paths, hosts, or source detail that
    /// might correlate to credentials. Apply before logging or before any value could reach the AI.
    #[must_use]
    pub fn redacted(&self) -> RedactedError<'_> {
        RedactedError(self)
    }
}

/// A redacting `Display` wrapper around a [`VfsError`] (see [`VfsError::redacted`]).
#[derive(Debug)]
pub struct RedactedError<'a>(&'a VfsError);

impl std::fmt::Display for RedactedError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.0 {
            VfsError::NotFound(_) => "not found",
            VfsError::Forbidden(_) => "permission denied",
            VfsError::AlreadyExists(_) => "already exists",
            VfsError::Unsupported(_) => "operation not supported",
            VfsError::Timeout(_) => "timed out",
            VfsError::Connection(_) => "connection failed",
            VfsError::Auth => "authentication failed",
            VfsError::Conflict => "conflict",
            VfsError::Backend { code, .. } => return write!(f, "backend error [{code}]"),
            VfsError::Cancelled => "cancelled",
            VfsError::Io(_) => "io error",
        };
        f.write_str(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification() {
        assert!(VfsError::Timeout(Duration::from_secs(1)).is_retryable());
        assert!(VfsError::Backend {
            code: "Throttled".into(),
            msg: "slow down".into(),
            retryable: true
        }
        .is_retryable());
        assert!(!VfsError::NotFound(VfsPath::root()).is_retryable());
        assert!(!VfsError::Forbidden(VfsPath::root()).is_retryable());
    }

    #[test]
    fn redacted_hides_path() {
        let e = VfsError::NotFound(VfsPath::parse("/secret/path").unwrap());
        let red = e.redacted().to_string();
        assert!(!red.contains("secret"));
        assert_eq!(red, "not found");
    }

    #[test]
    fn redacted_keeps_backend_code_only() {
        let e = VfsError::Backend {
            code: "NoSuchBucket".into(),
            msg: "bucket my-secret-bucket missing".into(),
            retryable: false,
        };
        let red = e.redacted().to_string();
        assert!(red.contains("NoSuchBucket"));
        assert!(!red.contains("my-secret-bucket"));
    }
}
