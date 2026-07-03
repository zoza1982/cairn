//! [`TarOps`]: indexes and reads an **uncompressed** `.tar` file (RFC-0013 P4).
//!
//! `tar` is a sequential format with no central directory, so unlike zip we can't jump straight to
//! an index — we do one initial sequential scan (`tar::Archive::entries_with_seek`, which uses
//! `Seek` to skip over file *contents* rather than reading them) recording each kept member's
//! `raw_file_position()` (a byte offset into the file) and size. Later reads `seek` directly to that
//! offset and read the capped byte count — no re-scan, no temporary extraction to disk.

use crate::index::{ArchiveIndex, IndexBuilder, StoredEntry};
use crate::security::{check_entry_count, validate_member_name};
use crate::ArchiveOps;
use cairn_types::VfsPath;
use cairn_vfs::VfsError;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

/// A [`Vfs`](cairn_vfs::Vfs)-facing view over one open, indexed `.tar` file.
pub(crate) struct TarOps {
    index: ArchiveIndex<u64>,
    file: Mutex<File>,
}

fn io_err(e: std::io::Error) -> VfsError {
    VfsError::Io(e)
}

impl TarOps {
    /// Open and index `path`. Synchronous and potentially slow for a huge archive — callers must
    /// run this inside `tokio::task::spawn_blocking` (see [`crate::ArchiveVfs::open`]).
    pub(crate) fn build(path: &Path) -> Result<Self, VfsError> {
        let file = File::open(path).map_err(io_err)?;
        let mut archive = tar::Archive::new(file);
        let mut builder: IndexBuilder<u64> = IndexBuilder::new();
        let mut scanned: usize = 0;
        {
            let entries = archive.entries_with_seek().map_err(io_err)?;
            for entry in entries {
                scanned += 1;
                check_entry_count(scanned)?;
                let entry = match entry {
                    Ok(e) => e,
                    // A malformed individual header is skipped, not fatal to the whole archive —
                    // matches the "rejected member is skipped, never a panic" security guard.
                    Err(e) => {
                        tracing::warn!(error = %e, "skipping unreadable tar entry");
                        continue;
                    }
                };
                let header = entry.header();
                let entry_type = header.entry_type();
                let is_symlink =
                    matches!(entry_type, tar::EntryType::Symlink | tar::EntryType::Link);
                let is_file = matches!(
                    entry_type,
                    tar::EntryType::Regular | tar::EntryType::Continuous
                );
                let is_dir = entry_type == tar::EntryType::Directory;
                if !is_symlink && !is_file && !is_dir {
                    // Device/FIFO/socket/GNU-special/etc: never materialized.
                    continue;
                }
                // setuid/setgid/sticky bits: skip regardless of entry type (defense against
                // symlink+setuid style tricks; these bits are meaningless for a browse-only view).
                if let Ok(mode) = header.mode() {
                    if mode & 0o7000 != 0 {
                        continue;
                    }
                }
                let Ok(raw_path) = entry.path() else {
                    tracing::warn!("skipping tar entry with unrepresentable path bytes");
                    continue;
                };
                let name = raw_path.to_string_lossy().into_owned();
                let Some(vpath) = validate_member_name(&name) else {
                    tracing::warn!("skipping tar entry with unsafe/invalid path");
                    continue;
                };
                let stored = if is_dir {
                    StoredEntry::Dir
                } else if is_symlink {
                    let target = entry
                        .link_name()
                        .ok()
                        .flatten()
                        .and_then(|p| validate_member_name(&p.to_string_lossy()));
                    StoredEntry::Symlink { target }
                } else {
                    StoredEntry::File {
                        size: entry.size(),
                        locator: entry.raw_file_position(),
                    }
                };
                builder.insert(vpath, stored);
            }
        }
        let file = archive.into_inner();
        Ok(Self {
            index: builder.finish(),
            file: Mutex::new(file),
        })
    }
}

impl ArchiveOps for TarOps {
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
                // Recover the guard even if a prior access panicked while holding it — the file
                // handle itself is never left in an inconsistent state by a panic mid-read, so
                // recovering is safe and avoids unwrap/expect on a backend-reachable path.
                let mut file = self
                    .file
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                file.seek(SeekFrom::Start(*locator)).map_err(io_err)?;
                let mut buf = Vec::new();
                file.by_ref()
                    .take(take)
                    .read_to_end(&mut buf)
                    .map_err(io_err)?;
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

    /// Build a small in-memory-then-temp-file tar archive for tests: a top-level file, a nested
    /// directory with a file, an explicit directory entry, and a symlink.
    fn make_test_tar() -> tempfile::NamedTempFile {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut builder = tar::Builder::new(tmp.reopen().unwrap());
            let mut add_file = |name: &str, data: &[u8]| {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, data).unwrap();
            };
            add_file("top.txt", b"top");
            add_file("dir/sub/nested.txt", b"nested-data");

            let mut dir_header = tar::Header::new_gnu();
            dir_header.set_entry_type(tar::EntryType::Directory);
            dir_header.set_size(0);
            dir_header.set_mode(0o755);
            dir_header.set_cksum();
            builder
                .append_data(&mut dir_header, "explicit_dir", &b""[..])
                .unwrap();

            let mut link_header = tar::Header::new_gnu();
            link_header.set_entry_type(tar::EntryType::Symlink);
            link_header.set_size(0);
            link_header.set_mode(0o777);
            link_header.set_link_name("top.txt").unwrap();
            link_header.set_cksum();
            builder
                .append_data(&mut link_header, "link_to_top", &b""[..])
                .unwrap();

            builder.finish().unwrap();
        }
        tmp
    }

    #[test]
    fn indexes_files_dirs_and_symlinks() {
        let tmp = make_test_tar();
        let ops = TarOps::build(tmp.path()).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        let mut names: Vec<_> = root.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["dir", "explicit_dir", "link_to_top", "top.txt"]);

        let link = root.iter().find(|e| e.name == "link_to_top").unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);

        let nested = ops
            .list_children(&VfsPath::parse("dir/sub").unwrap())
            .unwrap();
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].name, "nested.txt");
    }

    #[test]
    fn reads_member_bytes_by_seek() {
        let tmp = make_test_tar();
        let ops = TarOps::build(tmp.path()).unwrap();
        let data = ops
            .read_member(&VfsPath::parse("dir/sub/nested.txt").unwrap(), 1024)
            .unwrap();
        assert_eq!(data, b"nested-data");
    }

    #[test]
    fn symlink_read_is_unsupported() {
        let tmp = make_test_tar();
        let ops = TarOps::build(tmp.path()).unwrap();
        let err = ops
            .read_member(&VfsPath::parse("link_to_top").unwrap(), 1024)
            .unwrap_err();
        assert!(matches!(err, VfsError::Unsupported(_)));
    }

    #[test]
    fn per_member_cap_bounds_the_read() {
        let tmp = make_test_tar();
        let ops = TarOps::build(tmp.path()).unwrap();
        let data = ops
            .read_member(&VfsPath::parse("top.txt").unwrap(), 1)
            .unwrap();
        assert_eq!(data.len(), 1);
    }

    #[test]
    fn traversal_member_is_skipped_not_fatal() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut builder = tar::Builder::new(tmp.reopen().unwrap());
            // `Header::set_path` itself rejects `..` — exactly the validation we're testing our own
            // backstop against, so it can't be used to construct the adversarial case. Write the raw
            // name bytes directly instead (the classic tar header's first 100 bytes, NUL-padded).
            let mut header = tar::Header::new_gnu();
            let name = b"../evil.txt";
            header.as_mut_bytes()[..name.len()].copy_from_slice(name);
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, &b"bad"[..]).unwrap();
            // A legitimate entry after the bad one must still be indexed.
            let mut good = tar::Header::new_gnu();
            good.set_path("ok.txt").unwrap();
            good.set_size(2);
            good.set_mode(0o644);
            good.set_cksum();
            builder.append(&good, &b"ok"[..]).unwrap();
            builder.finish().unwrap();
        }
        let ops = TarOps::build(tmp.path()).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        let names: Vec<_> = root.iter().map(|e| e.name.to_string()).collect();
        assert_eq!(names, vec!["ok.txt"]);
    }

    #[test]
    fn special_and_setuid_members_are_skipped() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut builder = tar::Builder::new(tmp.reopen().unwrap());
            let mut fifo = tar::Header::new_gnu();
            fifo.set_entry_type(tar::EntryType::Fifo);
            fifo.set_path("a_fifo").unwrap();
            fifo.set_size(0);
            fifo.set_mode(0o644);
            fifo.set_cksum();
            builder.append(&fifo, &b""[..]).unwrap();

            let mut setuid = tar::Header::new_gnu();
            setuid.set_path("setuid_bin").unwrap();
            setuid.set_size(3);
            setuid.set_mode(0o4755);
            setuid.set_cksum();
            builder.append(&setuid, &b"bin"[..]).unwrap();

            builder.finish().unwrap();
        }
        let ops = TarOps::build(tmp.path()).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        assert!(root.is_empty());
    }

    #[test]
    fn huge_declared_size_does_not_overflow_or_over_allocate() {
        // `tar::Builder::append`/`append_data` insist the reader actually supply `size` bytes, so a
        // lying header (huge declared size, tiny real content) has to be written by hand: a raw
        // 512-byte header block followed by far fewer content bytes than it claims. This is exactly
        // the truncated/adversarial shape the per-member cap must defend against.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut f = tmp.reopen().unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_path("huge.bin").unwrap();
            header.set_size(u64::MAX / 2);
            header.set_mode(0o644);
            header.set_cksum();
            f.write_all(header.as_bytes()).unwrap();
            f.write_all(b"tiny").unwrap();
        }
        let ops = TarOps::build(tmp.path()).unwrap();
        // The read must not panic, allocate anywhere near the declared size, or hang; it returns
        // at most `cap` bytes (bounded by real EOF here, since the file is actually tiny).
        let data = ops
            .read_member(&VfsPath::parse("huge.bin").unwrap(), 64 * 1024 * 1024)
            .unwrap();
        assert!(data.len() < 1024);
    }
}
