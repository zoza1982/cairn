//! [`ZipOps`]: indexes and reads a `.zip` file (RFC-0013 P4).
//!
//! Zip carries a central directory, so — unlike tar — the whole member list (names, compressed and
//! uncompressed sizes, unix mode) is available from a single parse (`zip::ZipArchive::new`) without
//! touching any member's content. Content is only decoded lazily, per read, via `by_index`.

use crate::index::{ArchiveIndex, IndexBuilder, StoredEntry};
use crate::security::{
    check_entry_count, validate_member_name, zip_ratio_is_bomb, ARCHIVE_SYMLINK_TARGET_CAP,
};
use crate::ArchiveOps;
use cairn_types::VfsPath;
use cairn_vfs::VfsError;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

/// Unix file-type mask/values from `<sys/stat.h>` `S_IFMT`, applied to `unix_mode()`'s upper bits.
const S_IFMT: u32 = 0o170_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// A [`Vfs`](cairn_vfs::Vfs)-facing view over one open, indexed `.zip` file.
pub(crate) struct ZipOps {
    index: ArchiveIndex<usize>,
    archive: Mutex<zip::ZipArchive<File>>,
}

fn zip_err(e: zip::result::ZipError) -> VfsError {
    VfsError::Backend {
        code: "zip_error".to_owned(),
        msg: e.to_string(),
        retryable: false,
    }
}

impl ZipOps {
    /// Open and index `path`. Synchronous — callers must run this inside
    /// `tokio::task::spawn_blocking` (see [`crate::ArchiveVfs::open`]).
    pub(crate) fn build(path: &Path) -> Result<Self, VfsError> {
        let file = File::open(path).map_err(VfsError::Io)?;
        let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
        let len = archive.len();
        // The central directory gives us the full count up front, so the cap can be checked once
        // rather than incrementally — but we still route it through the shared helper so tar and
        // zip enforce the exact same threshold.
        check_entry_count(len)?;
        let mut builder: IndexBuilder<usize> = IndexBuilder::new();
        for i in 0..len {
            let mut zf = match archive.by_index(i) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, "skipping unreadable zip entry");
                    continue;
                }
            };
            let name = match zf.enclosed_name() {
                Some(p) => p.to_string_lossy().into_owned(),
                None => {
                    tracing::warn!("skipping zip entry with an unsafe/unenclosable name");
                    continue;
                }
            };
            let Some(vpath) = validate_member_name(&name) else {
                tracing::warn!("skipping zip entry with unsafe/invalid path");
                continue;
            };
            let compressed = zf.compressed_size();
            let uncompressed = zf.size();
            if zip_ratio_is_bomb(compressed, uncompressed) {
                tracing::warn!(
                    name = %name,
                    compressed,
                    uncompressed,
                    "skipping zip entry: compression ratio exceeds the bomb-detection threshold"
                );
                continue;
            }
            let mode = zf.unix_mode();
            let file_type = mode.map(|m| m & S_IFMT);
            // setuid/setgid/sticky bits: skip regardless of file type (mirrors the tar guard).
            if let Some(m) = mode {
                if m & 0o7000 != 0 {
                    continue;
                }
            }
            let is_dir = zf.is_dir() || file_type == Some(S_IFDIR);
            let is_symlink = file_type == Some(S_IFLNK);
            // Any other unix file type bit present (char/block/fifo/socket) is a special file:
            // never materialized. A `None`/`S_IFREG` mode is treated as an ordinary file/dir.
            if let Some(ft) = file_type {
                if !is_dir && !is_symlink && ft != S_IFREG {
                    continue;
                }
            }
            let stored = if is_dir {
                StoredEntry::Dir
            } else if is_symlink {
                // The zip format stores a symlink's target as the entry's *content*, not a header
                // field — read it now (bounded; never a general decompression) purely for display.
                let mut buf = Vec::new();
                let target = match zf
                    .by_ref()
                    .take(ARCHIVE_SYMLINK_TARGET_CAP)
                    .read_to_end(&mut buf)
                {
                    Ok(_) => {
                        let raw = String::from_utf8_lossy(&buf);
                        validate_member_name(&raw)
                    }
                    Err(_) => None,
                };
                StoredEntry::Symlink { target }
            } else {
                StoredEntry::File {
                    size: uncompressed,
                    locator: i,
                }
            };
            builder.insert(vpath, stored);
        }
        Ok(Self {
            index: builder.finish(),
            archive: Mutex::new(archive),
        })
    }
}

impl ArchiveOps for ZipOps {
    fn list_children(&self, dir: &VfsPath) -> Result<Vec<cairn_types::Entry>, VfsError> {
        self.index.list_children(dir)
    }

    fn entry_meta(&self, path: &VfsPath) -> Result<cairn_types::Entry, VfsError> {
        self.index.entry_meta(path)
    }

    fn read_member(&self, path: &VfsPath, cap: u64) -> Result<Vec<u8>, VfsError> {
        match self.index.get(path) {
            Some(StoredEntry::File { size, locator }) => {
                let take = (*size).min(cap);
                let mut archive = self
                    .archive
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut zf = archive.by_index(*locator).map_err(zip_err)?;
                let mut buf = Vec::new();
                // Bounded regardless of what the central directory declared: even if `size` lied,
                // `take` caps the actual decompressed bytes read here.
                zf.by_ref()
                    .take(take)
                    .read_to_end(&mut buf)
                    .map_err(VfsError::Io)?;
                Ok(buf)
            }
            Some(StoredEntry::Dir) => Err(VfsError::Unsupported(cairn_types::Caps::READ)),
            Some(StoredEntry::Symlink { .. }) => {
                Err(VfsError::Unsupported(cairn_types::Caps::READ))
            }
            None => Err(VfsError::NotFound(path.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_types::EntryKind;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn make_test_zip() -> tempfile::NamedTempFile {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let file = tmp.reopen().unwrap();
            let mut zw = ZipWriter::new(file);
            let opts = SimpleFileOptions::default();
            zw.start_file("top.txt", opts).unwrap();
            zw.write_all(b"top").unwrap();
            zw.start_file("dir/sub/nested.txt", opts).unwrap();
            zw.write_all(b"nested-data").unwrap();
            // Explicit directory entry.
            zw.add_directory("explicit_dir", opts).unwrap();
            zw.finish().unwrap();
        }
        tmp
    }

    #[test]
    fn indexes_files_and_dirs() {
        let tmp = make_test_zip();
        let ops = ZipOps::build(tmp.path()).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        let mut names: Vec<_> = root.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["dir", "explicit_dir", "top.txt"]);

        let nested = ops
            .list_children(&VfsPath::parse("dir/sub").unwrap())
            .unwrap();
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].name, "nested.txt");
        assert_eq!(nested[0].size, Some(11));
    }

    #[test]
    fn reads_member_bytes() {
        let tmp = make_test_zip();
        let ops = ZipOps::build(tmp.path()).unwrap();
        let data = ops
            .read_member(&VfsPath::parse("dir/sub/nested.txt").unwrap(), 1024)
            .unwrap();
        assert_eq!(data, b"nested-data");
    }

    #[test]
    fn per_member_cap_bounds_the_read() {
        let tmp = make_test_zip();
        let ops = ZipOps::build(tmp.path()).unwrap();
        let data = ops
            .read_member(&VfsPath::parse("top.txt").unwrap(), 1)
            .unwrap();
        assert_eq!(data.len(), 1);
    }

    #[test]
    fn traversal_member_is_skipped_not_fatal() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let file = tmp.reopen().unwrap();
            let mut zw = ZipWriter::new(file);
            let opts = SimpleFileOptions::default();
            zw.start_file("../evil.txt", opts).unwrap();
            zw.write_all(b"bad").unwrap();
            zw.start_file("ok.txt", opts).unwrap();
            zw.write_all(b"ok").unwrap();
            zw.finish().unwrap();
        }
        let ops = ZipOps::build(tmp.path()).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        let names: Vec<_> = root.iter().map(|e| e.name.to_string()).collect();
        assert_eq!(names, vec!["ok.txt"]);
    }

    #[test]
    fn symlink_member_is_inert_with_target_shown() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let file = tmp.reopen().unwrap();
            let mut zw = ZipWriter::new(file);
            let opts = SimpleFileOptions::default();
            // `unix_permissions` only ever sets the permission bits (the writer forces the S_IFREG
            // file-type bits for a plain `start_file`); a real symlink entry needs the dedicated
            // `add_symlink` API, which sets the S_IFLNK type bits and stores the target as content.
            zw.add_symlink("link_to_top", "top.txt", opts).unwrap();
            zw.finish().unwrap();
        }
        let ops = ZipOps::build(tmp.path()).unwrap();
        let meta = ops
            .entry_meta(&VfsPath::parse("link_to_top").unwrap())
            .unwrap();
        assert_eq!(meta.kind, EntryKind::Symlink);
        assert_eq!(
            meta.symlink_target
                .as_ref()
                .map(cairn_types::VfsPath::as_str),
            Some("/top.txt".to_owned())
        );
        let err = ops
            .read_member(&VfsPath::parse("link_to_top").unwrap(), 1024)
            .unwrap_err();
        assert!(matches!(err, VfsError::Unsupported(_)));
    }

    #[test]
    fn absurd_ratio_member_is_rejected_before_decompression() {
        // A real (if modest) decompression bomb: a run of zeros deflates at a ratio far beyond
        // `ZIP_MAX_COMPRESSION_RATIO`. `ZipOps::build` must reject it purely from the central
        // directory's compressed/uncompressed sizes — the member never appears in the listing, and
        // building the index must not itself decompress the payload to notice.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let file = tmp.reopen().unwrap();
            let mut zw = ZipWriter::new(file);
            let opts = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .compression_level(Some(9));
            zw.start_file("zeros.bin", opts).unwrap();
            zw.write_all(&vec![0u8; 2 * 1024 * 1024]).unwrap(); // 2 MiB of zeros
            zw.start_file("ok.txt", SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"ok").unwrap();
            zw.finish().unwrap();
        }
        let ops = ZipOps::build(tmp.path()).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        let names: Vec<_> = root.iter().map(|e| e.name.to_string()).collect();
        assert_eq!(names, vec!["ok.txt"]);
    }

    #[test]
    fn entry_count_cap_rejects_an_oversized_archive() {
        // One more central-directory record than the cap allows. Directory entries have no content
        // to compress, so this stays fast and small even at cap-plus-one scale.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let file = tmp.reopen().unwrap();
            let mut zw = ZipWriter::new(file);
            let opts = SimpleFileOptions::default();
            for i in 0..=crate::security::ARCHIVE_MAX_ENTRIES {
                zw.add_directory(format!("d{i}"), opts).unwrap();
            }
            zw.finish().unwrap();
        }
        match ZipOps::build(tmp.path()) {
            Err(VfsError::Backend { code, .. }) => assert_eq!(code, "archive_too_many_entries"),
            other => panic!(
                "expected an entry-count-cap error, got ok={}",
                other.is_ok()
            ),
        }
    }
}
