//! The real SFTP transport: an [`SftpOps`] adapter over a `russh-sftp` client session.
//!
//! This adapter is compiled and type-checked against the `russh-sftp` client API. Establishing the
//! underlying SSH connection (a `russh` channel whose stream feeds `SftpSession::new`) is the
//! integration step wired up with a live-server test per the M4 CI design; this type accepts an
//! already-opened [`SftpSession`].

use crate::ops::{RemoteEntry, RemoteMeta, SftpOps};
use async_trait::async_trait;
use cairn_types::{EntryKind, VfsPath};
use cairn_vfs::{ByteRange, RetryPolicy, VfsError};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// An [`SftpOps`] implementation backed by a live `russh-sftp` client session.
pub struct RealSftp {
    session: SftpSession,
}

impl RealSftp {
    /// Wrap an already-connected SFTP session.
    #[must_use]
    pub fn new(session: SftpSession) -> Self {
        Self { session }
    }
}

fn kind_of(md: &russh_sftp::protocol::FileAttributes) -> EntryKind {
    if md.is_dir() {
        EntryKind::Dir
    } else if md.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::File
    }
}

/// Map a transport error to a `VfsError`: not-found, a *transient* (retryable) transport failure, or
/// a generic backend error. Classifying transient failures as retryable is what lets the read-only
/// ops actually benefit from [`cairn_vfs::retry`].
fn map_err(e: impl std::fmt::Display, path: &str) -> VfsError {
    let msg = e.to_string();
    let low = msg.to_lowercase();
    if low.contains("no such file") || low.contains("not found") || low.contains("nosuchfile") {
        VfsError::NotFound(VfsPath::parse(path).unwrap_or_else(|_| VfsPath::root()))
    } else {
        let transient = [
            "timeout",
            "timed out",
            "connection",
            "reset",
            "broken pipe",
            "eof",
        ]
        .iter()
        .any(|m| low.contains(m));
        VfsError::Backend {
            code: "sftp".to_owned(),
            msg,
            retryable: transient,
        }
    }
}

fn io_err(e: std::io::Error) -> VfsError {
    VfsError::Io(e)
}

#[async_trait]
impl SftpOps for RealSftp {
    async fn read_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, VfsError> {
        // Idempotent read: retried on a transient transport failure (see `stat` for the policy).
        cairn_vfs::retry(RetryPolicy::default(), || async {
            let dir = self
                .session
                .read_dir(path)
                .await
                .map_err(|e| map_err(e, path))?;
            let mut out = Vec::new();
            for entry in dir {
                let md = entry.metadata();
                let kind = kind_of(&md);
                out.push(RemoteEntry {
                    name: entry.file_name(),
                    kind,
                    size: if kind == EntryKind::File {
                        Some(md.len())
                    } else {
                        None
                    },
                    modified: md.modified().ok(),
                    mode: md.permissions,
                });
            }
            Ok(out)
        })
        .await
    }

    async fn stat(&self, path: &str) -> Result<RemoteMeta, VfsError> {
        // `stat` is idempotent, so retry it with backoff on a *transient* failure (server-side
        // timeout/throttle, classified retryable by `map_err`). This does not re-establish a dropped
        // session — connection recovery is a higher-level concern (M4-4 keepalive/reconnect).
        // Mutating ops (write/remove/rename) are intentionally NOT auto-retried — a retried partial
        // mutation could double-apply.
        cairn_vfs::retry(RetryPolicy::default(), || async {
            let md = self
                .session
                .metadata(path)
                .await
                .map_err(|e| map_err(e, path))?;
            let kind = kind_of(&md);
            Ok(RemoteMeta {
                kind,
                size: if kind == EntryKind::File {
                    Some(md.len())
                } else {
                    None
                },
                modified: md.modified().ok(),
                mode: md.permissions,
            })
        })
        .await
    }

    async fn lstat(&self, path: &str) -> Result<RemoteMeta, VfsError> {
        // Like `stat`, but `symlink_metadata` (SSH_FXP_LSTAT) does not follow symlinks, so a symlink
        // reports as `EntryKind::Symlink` rather than its target's kind. Same idempotent-retry policy.
        cairn_vfs::retry(RetryPolicy::default(), || async {
            let md = self
                .session
                .symlink_metadata(path)
                .await
                .map_err(|e| map_err(e, path))?;
            let kind = kind_of(&md);
            Ok(RemoteMeta {
                kind,
                size: if kind == EntryKind::File {
                    Some(md.len())
                } else {
                    None
                },
                modified: md.modified().ok(),
                mode: md.permissions,
            })
        })
        .await
    }

    async fn read(&self, path: &str, range: Option<ByteRange>) -> Result<Vec<u8>, VfsError> {
        // Idempotent read: retried on a transient failure. A retry re-opens and re-reads the whole
        // range from the start (the returned bytes are correct; only the in-flight read is repeated).
        cairn_vfs::retry(RetryPolicy::default(), || async {
            let mut file = self
                .session
                .open_with_flags(path, OpenFlags::READ)
                .await
                .map_err(|e| map_err(e, path))?;
            let mut buf = Vec::new();
            match range {
                None => {
                    file.read_to_end(&mut buf).await.map_err(io_err)?;
                }
                Some(r) => {
                    file.seek(std::io::SeekFrom::Start(r.offset))
                        .await
                        .map_err(io_err)?;
                    match r.len {
                        Some(l) => {
                            file.take(l).read_to_end(&mut buf).await.map_err(io_err)?;
                        }
                        None => {
                            file.read_to_end(&mut buf).await.map_err(io_err)?;
                        }
                    }
                }
            }
            Ok(buf)
        })
        .await
    }

    async fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let mut file = self
            .session
            .open_with_flags(
                path,
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
            )
            .await
            .map_err(|e| map_err(e, path))?;
        file.write_all(data).await.map_err(io_err)?;
        file.flush().await.map_err(io_err)?;
        let _ = file.shutdown().await;
        Ok(())
    }

    async fn remove_file(&self, path: &str) -> Result<(), VfsError> {
        self.session
            .remove_file(path)
            .await
            .map_err(|e| map_err(e, path))
    }

    async fn remove_dir(&self, path: &str) -> Result<(), VfsError> {
        self.session
            .remove_dir(path)
            .await
            .map_err(|e| map_err(e, path))
    }

    async fn create_dir(&self, path: &str) -> Result<(), VfsError> {
        self.session
            .create_dir(path)
            .await
            .map_err(|e| map_err(e, path))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.session
            .rename(from, to)
            .await
            .map_err(|e| map_err(e, from))
    }
}

#[cfg(test)]
mod tests {
    use super::map_err;
    use cairn_vfs::VfsError;

    #[test]
    fn map_err_classifies_transient_failures_as_retryable() {
        // A transport timeout/connection drop is retryable…
        assert!(map_err("Connection reset by peer", "/x").is_retryable());
        assert!(map_err("operation timed out", "/x").is_retryable());
        // …a not-found is a distinct, non-retryable error…
        assert!(matches!(
            map_err("No such file", "/x"),
            VfsError::NotFound(_)
        ));
        // …and a generic protocol error is not retried.
        assert!(!map_err("permission denied", "/x").is_retryable());
    }
}
