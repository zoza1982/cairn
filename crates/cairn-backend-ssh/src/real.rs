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

/// Map a transport error to a `VfsError`, distinguishing not-found from other failures.
fn map_err(e: impl std::fmt::Display, path: &str) -> VfsError {
    let msg = e.to_string();
    let low = msg.to_lowercase();
    if low.contains("no such file") || low.contains("not found") || low.contains("nosuchfile") {
        VfsError::NotFound(VfsPath::parse(path).unwrap_or_else(|_| VfsPath::root()))
    } else {
        VfsError::Backend {
            code: "sftp".to_owned(),
            msg,
            retryable: false,
        }
    }
}

fn io_err(e: std::io::Error) -> VfsError {
    VfsError::Io(e)
}

#[async_trait]
impl SftpOps for RealSftp {
    async fn read_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, VfsError> {
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
            });
        }
        Ok(out)
    }

    async fn stat(&self, path: &str) -> Result<RemoteMeta, VfsError> {
        // `stat` is idempotent, so retry it with backoff on a transient transport failure. Mutating
        // ops (write/remove/rename) are intentionally NOT auto-retried — a retried partial mutation
        // could double-apply.
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
            })
        })
        .await
    }

    async fn read(&self, path: &str, range: Option<ByteRange>) -> Result<Vec<u8>, VfsError> {
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
