//! [`ArchiveVfs`]: maps an `ArchiveOps` onto the [`Vfs`] trait, plus the magic-byte format sniff
//! that picks tar vs zip at [`ArchiveVfs::open`] time.

use crate::compressed_tar::{CompressedTarOps, Compression};
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
    /// A tar stream wrapped in an outer compression (RFC-0013 P5) — `.tgz`/`.tbz2`/`.txz`/`.tzst`.
    CompressedTar(Compression),
}

/// Sniff `prefix` (the first bytes of the file) for a recognized archive magic:
/// - zip: `PK\x03\x04` at offset 0 (the local file header signature).
/// - tar: the POSIX `ustar` magic at byte offset 257.
/// - compressed tar: one of the four outer-compression magics (see `compressed_tar::sniff`), all
///   at offset 0 and none colliding with the zip/tar signatures above.
///
/// Checked in this order deliberately: the zip and plain-tar signatures are structural markers of
/// an *uncompressed* container and take priority; a compression magic is only meaningful once
/// those two have already been ruled out.
fn sniff_format(prefix: &[u8]) -> Option<Format> {
    if prefix.len() >= 4 && &prefix[0..4] == b"PK\x03\x04" {
        return Some(Format::Zip);
    }
    if prefix.len() >= 262 && &prefix[257..262] == b"ustar" {
        return Some(Format::Tar);
    }
    if let Some(compression) = crate::compressed_tar::sniff(prefix) {
        return Some(Format::CompressedTar(compression));
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
        Some(Format::CompressedTar(compression)) => {
            Ok(Arc::new(CompressedTarOps::build(path, compression)?))
        }
        None => Err(VfsError::Backend {
            code: "unrecognized_archive".to_owned(),
            msg: "not a recognized tar, zip, or compressed-tar archive".to_owned(),
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
        let declared = meta.size.unwrap_or(0);
        // How many bytes we need decoded from the *start* of the member to satisfy this request:
        // `read_member` always decodes from byte 0 and `apply_byte_range` slices afterward, so a
        // ranged read must decode `offset + len` bytes; a read-to-end (`len: None`) or a full read
        // needs the whole member. Clamped to the member's real length so we never try to decode past
        // its end (fixes the earlier `offset + declared` over-estimate for `len: None`).
        let wanted = match range {
            Some(r) => match r.len {
                Some(len) => r.offset.saturating_add(len),
                None => declared,
            },
            None => declared,
        }
        .min(declared);
        let remaining = self.remaining_session_budget();
        // Only the session cap being exhausted for a *non-empty* request is an error; a zero-byte
        // read (empty member, or a zero-length range) trivially succeeds even at the cap.
        if remaining == 0 && wanted > 0 {
            return Err(VfsError::Backend {
                code: "archive_session_cap_reached".to_owned(),
                msg:
                    "possible archive bomb: cumulative archive read limit reached for this session"
                        .to_owned(),
                retryable: false,
            });
        }
        let cap = wanted.min(ARCHIVE_PER_MEMBER_CAP).min(remaining);
        // A *full-member* read (`range: None`, e.g. a copy-out through the transfer engine) that the
        // caps would truncate must fail loudly rather than silently return a short buffer: otherwise
        // extracting an over-cap member would produce a truncated file that even `VerifyPolicy::Size`
        // accepts (it compares bytes-written to bytes-read, both short — see the transfer engine),
        // i.e. silent data loss. `declared` is the member's authoritative content length by both the
        // tar and zip formats, so `declared > cap` is exactly the truncation condition. A *ranged*
        // read is a deliberate bounded window (the sniff's ~8 KiB prefix, a pager preview) and is
        // honored up to the cap without erroring.
        if range.is_none() && declared > cap {
            return Err(VfsError::Backend {
                code: "archive_member_too_large".to_owned(),
                msg:
                    "archive member exceeds the per-member/session read limit; extracting members \
                      this large is a follow-up (streaming extraction, RFC-0013)"
                        .to_owned(),
                retryable: false,
            });
        }
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

    /// The zip/tar structural signatures win over a compression-magic guess, and each of the four
    /// compression magics is recognized when neither of those two matched (RFC-0013 P5).
    #[test]
    fn sniffs_every_compressed_tar_magic() {
        assert_eq!(
            sniff_format(&[0x1f, 0x8b, 0x08, 0x00]),
            Some(Format::CompressedTar(Compression::Gzip))
        );
        assert_eq!(
            sniff_format(b"BZh91AY&SY"),
            Some(Format::CompressedTar(Compression::Bzip2))
        );
        assert_eq!(
            sniff_format(&[0xfd, b'7', b'z', b'X', b'Z', 0x00, 0x00]),
            Some(Format::CompressedTar(Compression::Xz))
        );
        assert_eq!(
            sniff_format(&[0x28, 0xb5, 0x2f, 0xfd, 0x00]),
            Some(Format::CompressedTar(Compression::Zstd))
        );
    }

    /// End-to-end through `ArchiveVfs::open`: a real `.tar.gz` is decompressed, indexed, and
    /// browsable exactly like a plain tar — the whole point of P5 being additive to P4's dispatch.
    #[tokio::test]
    async fn open_mounts_a_real_gzip_compressed_tar() {
        let tar_bytes = {
            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "hi.txt", &b"hey"[..])
                .unwrap();
            builder.into_inner().unwrap()
        };
        let gz_bytes = {
            use std::io::Write as _;
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&tar_bytes).unwrap();
            enc.finish().unwrap()
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &gz_bytes).unwrap();

        let vfs = ArchiveVfs::open(ConnectionId(1), tmp.path().to_path_buf())
            .await
            .unwrap();
        let entries = read_all(&vfs, "hi.txt", None).await;
        assert_eq!(entries, b"hey");
    }

    /// End-to-end: `.txz` is sniffed correctly (routed to `Format::CompressedTar(Compression::Xz)`)
    /// but `ArchiveVfs::open` must surface a typed, friendly refusal rather than attempt to decode
    /// it or panic — xz decoding is deliberately not shipped (RFC-0013 P5, ADR-0013: `lzma-rs`'s
    /// LZMA2 path is not memory-bounded against a decompression bomb).
    #[tokio::test]
    async fn open_txz_gives_a_friendly_unsupported_error_not_panic() {
        let mut bytes = vec![0xfd, b'7', b'z', b'X', b'Z', 0x00];
        bytes.extend_from_slice(&[0u8; 16]);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &bytes).unwrap();

        match ArchiveVfs::open(ConnectionId(1), tmp.path().to_path_buf()).await {
            Err(VfsError::Backend { code, .. }) => {
                assert_eq!(code, "compressed_tar_xz_unsupported");
            }
            other => panic!(
                "expected compressed_tar_xz_unsupported, got ok={}",
                other.is_ok()
            ),
        }
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

    /// Build a single-file zip and return an opened `ArchiveVfs` over it (kept alive by the returned
    /// tempfile).
    async fn zip_with(name: &str, content: &[u8]) -> (ArchiveVfs, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let file = tmp.reopen().unwrap();
            let mut zw = zip::ZipWriter::new(file);
            zw.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            use std::io::Write as _;
            zw.write_all(content).unwrap();
            zw.finish().unwrap();
        }
        let vfs = ArchiveVfs::open(ConnectionId(1), tmp.path().to_path_buf())
            .await
            .unwrap();
        (vfs, tmp)
    }

    async fn read_all(vfs: &ArchiveVfs, path: &str, range: Option<ByteRange>) -> Vec<u8> {
        use tokio::io::AsyncReadExt as _;
        let mut handle = vfs
            .open_read(&VfsPath::parse(path).unwrap(), range)
            .await
            .unwrap();
        let mut out = Vec::new();
        handle.read_to_end(&mut out).await.unwrap();
        out
    }

    /// A full read (`range: None`) of a normal member returns its entire content.
    #[tokio::test]
    async fn open_read_full_member_returns_all_bytes() {
        let (vfs, _tmp) = zip_with("a.txt", b"hello, cairn archive").await;
        assert_eq!(read_all(&vfs, "a.txt", None).await, b"hello, cairn archive");
    }

    /// A ranged read returns exactly the requested window — exercising `offset`+`len`, and `len:
    /// None` (read-to-end) at a non-zero offset (the case whose `wanted` formula was fixed).
    #[tokio::test]
    async fn open_read_ranged_returns_the_window() {
        let (vfs, _tmp) = zip_with("a.txt", b"0123456789").await;
        // [3, 3+4) = "3456"
        let mid = read_all(
            &vfs,
            "a.txt",
            Some(ByteRange {
                offset: 3,
                len: Some(4),
            }),
        )
        .await;
        assert_eq!(mid, b"3456");
        // offset 7, read to end = "789"
        let tail = read_all(
            &vfs,
            "a.txt",
            Some(ByteRange {
                offset: 7,
                len: None,
            }),
        )
        .await;
        assert_eq!(tail, b"789");
    }

    /// A *full* read of a member whose declared size exceeds the per-member cap must fail loudly with
    /// `archive_member_too_large` rather than silently return a truncated buffer — otherwise a
    /// copy-out through the transfer engine would produce a truncated file that size-verify accepts
    /// (regression test for the silent-truncation data-loss bug). A hand-written tar header lets us
    /// declare a huge size without writing the bytes.
    #[tokio::test]
    async fn open_read_full_over_cap_member_errors_not_truncates() {
        use std::io::Write as _;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut f = tmp.reopen().unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("huge.bin").unwrap();
            header.set_size(ARCHIVE_PER_MEMBER_CAP + 1); // just over the per-member cap
            header.set_mode(0o644);
            header.set_cksum();
            f.write_all(header.as_bytes()).unwrap();
            f.write_all(b"tiny").unwrap();
        }
        let vfs = ArchiveVfs::open(ConnectionId(1), tmp.path().to_path_buf())
            .await
            .unwrap();
        // Full read: must error, not truncate.
        match vfs
            .open_read(&VfsPath::parse("huge.bin").unwrap(), None)
            .await
        {
            Err(VfsError::Backend { code, .. }) => assert_eq!(code, "archive_member_too_large"),
            other => panic!("expected archive_member_too_large, got {:?}", other.is_ok()),
        }
        // A bounded ranged read of the same member is still honored (a deliberate window).
        let head = read_all(
            &vfs,
            "huge.bin",
            Some(ByteRange {
                offset: 0,
                len: Some(4),
            }),
        )
        .await;
        assert_eq!(head, b"tiny");
    }
}
