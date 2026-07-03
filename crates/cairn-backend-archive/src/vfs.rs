//! [`ArchiveVfs`]: maps an [`ArchiveOps`] onto the [`Vfs`] trait, plus the magic-byte format sniff
//! that picks tar vs zip at [`ArchiveVfs::open`] time.

use crate::security::{ARCHIVE_PER_MEMBER_CAP, ARCHIVE_SESSION_BYTE_CAP};
use crate::tar_backend::TarOps;
use crate::zip_backend::ZipOps;
use crate::ArchiveOps;
use async_trait::async_trait;
use cairn_types::{Caps, ConnectionId, Entry, EntryKind, Scheme, VfsPath};
use cairn_vfs::{
    apply_byte_range, ByteRange, CapabilityProvider, ListOpts, ListPage, ReadHandle, Vfs, VfsError,
};
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A [`Vfs`] over a single mounted (read-only) `.tar` or `.zip` file.
///
/// Constructed by [`ArchiveVfs::open`], which builds the whole member index up front inside
/// `tokio::task::spawn_blocking`. Advertises `Caps::LIST | Caps::READ | Caps::RANDOM_READ` only —
/// every mutating trait method keeps the [`Vfs`] default (`Unsupported`), and
/// [`Vfs::local_path`] keeps the default `None` (archive members are not real OS paths).
pub struct ArchiveVfs {
    conn: ConnectionId,
    ops: Arc<dyn ArchiveOps>,
    /// Cumulative decoded bytes served by `open_read` so far this session — see
    /// [`crate::security::ARCHIVE_SESSION_BYTE_CAP`].
    cumulative_bytes: Arc<AtomicU64>,
}

/// Detected archive container format, decided purely from magic bytes (never a file extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Zip,
    Tar,
}

/// Sniff `prefix` (the first bytes of the file) for a recognized archive magic:
/// - zip: `PK\x03\x04` at offset 0 (the local file header signature).
/// - tar: the POSIX `ustar` magic at byte offset 257.
fn sniff_format(prefix: &[u8]) -> Option<Format> {
    if prefix.len() >= 4 && &prefix[0..4] == b"PK\x03\x04" {
        return Some(Format::Zip);
    }
    if prefix.len() >= 262 && &prefix[257..262] == b"ustar" {
        return Some(Format::Tar);
    }
    None
}

/// Read enough of the front of the file to sniff the format, then build the matching index.
/// Synchronous top to bottom — run inside `spawn_blocking` by [`ArchiveVfs::open`].
fn open_sync(path: &Path) -> Result<Arc<dyn ArchiveOps>, VfsError> {
    let mut probe = std::fs::File::open(path).map_err(VfsError::Io)?;
    let mut prefix = [0u8; 262];
    // A short read (a tiny or empty file) is fine — `sniff_format` only trusts what it actually got.
    let n = read_prefix_best_effort(&mut probe, &mut prefix)?;
    match sniff_format(&prefix[..n]) {
        Some(Format::Zip) => Ok(Arc::new(ZipOps::build(path)?)),
        Some(Format::Tar) => Ok(Arc::new(TarOps::build(path)?)),
        None => Err(VfsError::Backend {
            code: "unrecognized_archive".to_owned(),
            msg: "not a recognized tar or zip archive".to_owned(),
            retryable: false,
        }),
    }
}

fn read_prefix_best_effort(f: &mut std::fs::File, buf: &mut [u8]) -> Result<usize, VfsError> {
    let mut total = 0;
    loop {
        match f.read(&mut buf[total..]) {
            Ok(0) => return Ok(total),
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(VfsError::Io(e)),
        }
        if total == buf.len() {
            return Ok(total);
        }
    }
}

impl ArchiveVfs {
    /// Open and index the archive at `archive_path`, detecting tar vs zip from its magic bytes.
    ///
    /// Indexing is CPU/IO-bound sync work (the `tar`/`zip` crates have no maintained async API),
    /// so it runs inside `tokio::task::spawn_blocking` — this method never blocks the calling task.
    ///
    /// # Errors
    /// Returns a [`VfsError`] if the file can't be opened, isn't a recognized tar/zip archive, or
    /// exceeds the entry-count cap (`docs/rfcs/0013-archive-backend.md` §Security).
    pub async fn open(conn: ConnectionId, archive_path: PathBuf) -> Result<Self, VfsError> {
        let ops = tokio::task::spawn_blocking(move || open_sync(&archive_path))
            .await
            .map_err(|_| VfsError::Backend {
                code: "archive_index_task_failed".to_owned(),
                msg: "archive indexing task did not complete".to_owned(),
                retryable: false,
            })??;
        Ok(Self {
            conn,
            ops,
            cumulative_bytes: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Bytes still available under the per-session cumulative cap.
    fn remaining_session_budget(&self) -> u64 {
        ARCHIVE_SESSION_BYTE_CAP.saturating_sub(self.cumulative_bytes.load(Ordering::Relaxed))
    }
}

impl CapabilityProvider for ArchiveVfs {
    fn caps(&self) -> Caps {
        Caps::LIST | Caps::READ | Caps::RANDOM_READ
    }
}

#[async_trait]
impl Vfs for ArchiveVfs {
    fn scheme(&self) -> Scheme {
        Scheme::Archive
    }

    fn connection(&self) -> ConnectionId {
        self.conn
    }

    fn list<'a>(
        &'a self,
        dir: &VfsPath,
        _opts: ListOpts,
    ) -> BoxStream<'a, Result<ListPage, VfsError>> {
        let dir = dir.clone();
        let ops = self.ops.clone();
        stream::once(async move {
            let entries = ops.list_children(&dir)?;
            Ok(ListPage {
                entries,
                cursor: None,
                done: true,
            })
        })
        .boxed()
    }

    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError> {
        if path.is_root() {
            return Ok(Entry::new("", EntryKind::Dir));
        }
        self.ops.entry_meta(path)
    }

    async fn open_read(
        &self,
        path: &VfsPath,
        range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError> {
        let meta = self.ops.entry_meta(path)?;
        if !matches!(meta.kind, EntryKind::File) {
            // Directories have no content; symlinks/hardlinks are presented inert and are never
            // followed or read as if they were the linked-to content (RFC-0013 §Security).
            return Err(VfsError::Unsupported(Caps::READ));
        }
        let remaining = self.remaining_session_budget();
        if remaining == 0 {
            return Err(VfsError::Backend {
                code: "archive_session_cap_reached".to_owned(),
                msg:
                    "possible archive bomb: cumulative archive read limit reached for this session"
                        .to_owned(),
                retryable: false,
            });
        }
        let declared = meta.size.unwrap_or(0);
        // How many bytes we need decoded from the start of the member to satisfy this request.
        let wanted = match range {
            Some(r) => r.offset.saturating_add(r.len.unwrap_or(declared)),
            None => declared,
        };
        let cap = wanted.min(ARCHIVE_PER_MEMBER_CAP).min(remaining);
        let ops = self.ops.clone();
        let path_owned = path.clone();
        let data = tokio::task::spawn_blocking(move || ops.read_member(&path_owned, cap))
            .await
            .map_err(|_| VfsError::Backend {
                code: "archive_read_task_failed".to_owned(),
                msg: "archive read task did not complete".to_owned(),
                retryable: false,
            })??;
        self.cumulative_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        let sliced = match range {
            Some(r) => apply_byte_range(&data, r).to_vec(),
            None => data,
        };
        let len = sliced.len() as u64;
        Ok(ReadHandle::new(
            Box::new(std::io::Cursor::new(sliced)),
            Some(len),
        ))
    }

    async fn open_write(
        &self,
        _path: &VfsPath,
        _opts: cairn_vfs::WriteOpts,
    ) -> Result<cairn_vfs::WriteHandle, VfsError> {
        // Read-only backend: writing into an archive is out of scope for all of RFC-0013.
        Err(VfsError::Unsupported(Caps::WRITE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_zip_and_tar_and_rejects_neither() {
        let mut zip_prefix = [0u8; 262];
        zip_prefix[0..4].copy_from_slice(b"PK\x03\x04");
        assert_eq!(sniff_format(&zip_prefix), Some(Format::Zip));

        let mut tar_prefix = [0u8; 262];
        tar_prefix[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff_format(&tar_prefix), Some(Format::Tar));

        assert_eq!(sniff_format(b"not an archive"), None);
        assert_eq!(sniff_format(&[]), None);
    }

    #[tokio::test]
    async fn open_rejects_a_non_archive_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hello, world\n").unwrap();
        match ArchiveVfs::open(ConnectionId(1), tmp.path().to_path_buf()).await {
            Err(VfsError::Backend { .. }) => {}
            other => panic!(
                "expected a Backend error rejecting a non-archive file, got {}",
                other.is_ok()
            ),
        }
    }

    #[tokio::test]
    async fn open_caps_are_list_read_random_read_only() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let file = tmp.reopen().unwrap();
            let mut zw = zip::ZipWriter::new(file);
            zw.start_file("a.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            use std::io::Write as _;
            zw.write_all(b"hi").unwrap();
            zw.finish().unwrap();
        }
        let vfs = ArchiveVfs::open(ConnectionId(1), tmp.path().to_path_buf())
            .await
            .unwrap();
        let caps = vfs.caps();
        assert!(caps.contains(Caps::LIST));
        assert!(caps.contains(Caps::READ));
        assert!(caps.contains(Caps::RANDOM_READ));
        assert!(!caps.contains(Caps::WRITE));
        assert!(!caps.contains(Caps::DELETE));
        assert!(vfs.local_path(&VfsPath::root()).is_none());
    }
}
