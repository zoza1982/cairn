//! The real SFTP transport: an [`SftpOps`] adapter over a `russh-sftp` client session.
//!
//! This adapter is compiled and type-checked against the `russh-sftp` client API. Establishing the
//! underlying SSH connection (a `russh` channel whose stream feeds `SftpSession::new`) is the
//! integration step wired up with a live-server test per the M4 CI design; this type accepts an
//! already-opened [`SftpSession`].

use crate::ops::{RemoteEntry, RemoteMeta, SftpOps, SftpWriteStream};
use async_trait::async_trait;
use cairn_types::{EntryKind, VfsPath};
use cairn_vfs::{ByteRange, RetryPolicy, VfsError};
use russh_sftp::client::fs::File;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// An [`SftpOps`] implementation backed by a live `russh-sftp` client session.
pub struct RealSftp {
    /// Shared with the write streams handed out by `open_write`, which need the session again at
    /// `abort` time to remove the partial file. `SftpSession` is not `Clone`, hence the `Arc`.
    session: Arc<SftpSession>,
}

impl RealSftp {
    /// Wrap an already-connected SFTP session.
    #[must_use]
    pub fn new(session: SftpSession) -> Self {
        Self {
            session: Arc::new(session),
        }
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

    async fn open_read(
        &self,
        path: &str,
        range: Option<ByteRange>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, VfsError> {
        // Only the *open* (+ the local, round-trip-free seek) is retried: it is idempotent and cheap.
        // Once the reader is handed back nothing retries mid-stream — a dropped connection surfaces
        // as an `io::Error` from `poll_read` and fails that file, which the transfer engine handles.
        // Resuming a broken stream is a ranged re-open, i.e. an engine-level concern, not this seam's.
        cairn_vfs::retry(RetryPolicy::default(), || async {
            let mut file = self
                .session
                .open_with_flags(path, OpenFlags::READ)
                .await
                .map_err(|e| map_err(e, path))?;
            match range {
                None => Ok(Box::new(file) as Box<dyn AsyncRead + Send + Unpin>),
                Some(r) => {
                    file.seek(std::io::SeekFrom::Start(r.offset))
                        .await
                        .map_err(|e| map_err(e, path))?;
                    Ok(match r.len {
                        // `File::poll_read` issues one `SSH_FXP_READ` per poll (≤ the negotiated
                        // packet size), so neither branch buffers beyond a single packet.
                        Some(l) => Box::new(file.take(l)) as Box<dyn AsyncRead + Send + Unpin>,
                        None => Box::new(file),
                    })
                }
            }
        })
        .await
    }

    async fn open_write(&self, path: &str) -> Result<Box<dyn SftpWriteStream>, VfsError> {
        // Not retried: like the other mutating ops (see `stat`), a retried open could truncate a
        // file a previous attempt had already started writing.
        let file = self
            .session
            .open_with_flags(
                path,
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
            )
            .await
            .map_err(|e| map_err(e, path))?;
        Ok(Box::new(RealSftpWriteStream {
            file,
            path: path.to_owned(),
            session: self.session.clone(),
        }))
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

/// An open remote file being streamed to, over `russh-sftp`'s [`File`] (`AsyncWrite`).
///
/// `File::poll_write` sends one `SSH_FXP_WRITE` per call and only blocks once
/// `max_concurrent_writes` (8) acks are outstanding, so `write_all` on a 1 MiB chunk queues ~4
/// packets and returns while the previous window drains — the wire stays busy while the engine reads
/// the next source chunk, and progress tracks bytes the server has actually been handed.
struct RealSftpWriteStream {
    file: File,
    path: String,
    session: Arc<SftpSession>,
}

#[async_trait]
impl SftpWriteStream for RealSftpWriteStream {
    async fn write_all(&mut self, data: &[u8]) -> Result<(), VfsError> {
        // A `WRITE`'s failure surfaces on a *later* poll (when its ack is drained), so an error here
        // may belong to an earlier chunk — either way the file is bad and the caller aborts.
        self.file
            .write_all(data)
            .await
            .map_err(|e| map_err(e, &self.path))
    }

    async fn finish(self: Box<Self>) -> Result<(), VfsError> {
        let Self { mut file, path, .. } = *self;
        // `flush` drains every outstanding ack (and fsyncs where the server supports it); `shutdown`
        // sends `CLOSE` and awaits its status. That status is the commit point — a server that fails
        // the close has NOT durably written the file, so it must propagate, never be discarded. The
        // caller (`SftpWriteSink::finish`) removes the temp on either error.
        file.flush().await.map_err(|e| map_err(e, &path))?;
        file.shutdown().await.map_err(|e| map_err(e, &path))
    }

    async fn abort(self: Box<Self>) {
        let Self {
            file,
            path,
            session,
        } = *self;
        // Dropping the handle sends a best-effort, unawaited `CLOSE`; then remove what the open
        // created. The file exists (empty at minimum) from the moment `open_write` returned, so this
        // must run even if no chunk was ever written. Best-effort: there is nothing useful to do
        // with a failure here, and the caller is already on an error/cancel path.
        drop(file);
        let _ = session.remove_file(&path).await;
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
