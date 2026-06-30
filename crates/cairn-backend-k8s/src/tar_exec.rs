//! Pure helpers for parsing `tar` archives produced by in-container exec commands.
//!
//! The live adapter ([`crate::KubeRsOps`]) uses these helpers after collecting the stdout of
//! `tar cf - -C <dir> .` (for [`list_dir`](crate::KubeOps::list_dir)) or
//! `tar cf - -C <parent> <basename>` (for [`stat`](crate::KubeOps::stat) and
//! [`read`](crate::KubeOps::read)) over kube exec.  These functions are pure — they take bytes
//! and return typed results — so they are fully unit-testable without a cluster (see the `tests`
//! module below) and can be exercised hermetically in every `cargo test` run.

use crate::ops::{RemoteEntry, RemoteMeta};
use cairn_types::{Caps, EntryKind, VfsPath};
use cairn_vfs::VfsError;
use std::collections::BTreeSet;
use std::io::Read;

/// Extract the parent directory component of `path`.
///
/// The root `/` is its own parent.  Examples:
/// - `/etc/hostname` → `/etc`
/// - `/etc` → `/`
/// - `/` → `/`
pub(crate) fn tar_parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        // Trim leaves "" (the root was just "/") or points at the leading slash → parent is root.
        Some(0) | None => "/",
        Some(pos) => &trimmed[..pos],
    }
}

/// Extract the basename component of `path`.
///
/// Returns an empty string for the root (`/`).  Examples:
/// - `/etc/hostname` → `hostname`
/// - `/etc` → `etc`
/// - `/` → `` (empty)
pub(crate) fn tar_basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(pos) => &trimmed[pos + 1..],
        None => trimmed,
    }
}

/// Build a [`VfsError::NotFound`] for the given container-internal path.
pub(crate) fn not_found(path: &str) -> VfsError {
    VfsError::NotFound(VfsPath::parse(path).unwrap_or_else(|_| VfsPath::root()))
}

/// Build a generic [`VfsError::Backend`] for a `tar` I/O error.
fn tar_io_err(e: impl std::fmt::Display) -> VfsError {
    VfsError::Backend {
        code: "tar-io".to_owned(),
        msg: e.to_string(),
        retryable: false,
    }
}

/// Parse the stdout of `tar cf - -C <dir> .` into the immediate children of `<dir>`.
///
/// Entries produced by this tar invocation are named with a `./` prefix (e.g. `./`, `./file`,
/// `./subdir/`, `./subdir/nested`).  This function:
///
/// 1. Strips the `./` (or `/`) leader.
/// 2. Skips the self-entry (empty or `.`).
/// 3. For deeper descendants (e.g. `subdir/nested`), records only the first path component as a
///    directory, deduplicating across all entries.
/// 4. Returns directories first (sorted by name), then files.
///
/// An empty tar (no entries beyond the self-entry) returns `Ok(vec![])`, which is correct for an
/// empty or just-started container.
pub(crate) fn parse_list_dir(tar_bytes: &[u8]) -> Result<Vec<RemoteEntry>, VfsError> {
    let mut archive = tar::Archive::new(tar_bytes);
    let mut seen_dirs = BTreeSet::<String>::new();
    let mut files: Vec<RemoteEntry> = Vec::new();

    for entry_result in archive.entries().map_err(tar_io_err)? {
        let entry = entry_result.map_err(tar_io_err)?;
        let raw_path = entry.path().map_err(tar_io_err)?;
        let raw_str = raw_path.to_string_lossy();

        // Normalize: strip leading `./` (POSIX standard) or `/` (some non-standard tars).
        let stripped = raw_str.trim_start_matches("./").trim_start_matches('/');
        // Also strip trailing slash so we can analyse components uniformly.
        let relative = stripped.trim_end_matches('/');

        // Skip the self/root entry.
        if relative.is_empty() || relative == "." {
            continue;
        }

        if let Some(slash_pos) = relative.find('/') {
            // This entry is a deeper descendant — record only the first component as a dir.
            let dir_name = &relative[..slash_pos];
            if !dir_name.is_empty() {
                seen_dirs.insert(dir_name.to_owned());
            }
        } else if entry.header().entry_type().is_dir() {
            // Immediate directory child.
            seen_dirs.insert(relative.to_owned());
        } else if entry.header().entry_type().is_symlink()
            || entry.header().entry_type().is_hard_link()
        {
            // Symlinks and hard links: surface as File with unknown size (the header size field
            // is 0 for hard links and points to the target for symlinks, not the content size).
            // TODO(symlinks): expose as EntryKind::Symlink once the VFS surface adds that kind.
            files.push(RemoteEntry {
                name: relative.to_owned(),
                kind: EntryKind::File,
                size: None,
            });
        } else {
            // Regular file, device, FIFO, etc.
            let size = entry.header().size().map_err(tar_io_err)?;
            files.push(RemoteEntry {
                name: relative.to_owned(),
                kind: EntryKind::File,
                size: Some(size),
            });
        }
    }

    // Dirs first (sorted, stable — the UI can rely on this ordering), then files.
    let mut entries: Vec<RemoteEntry> = seen_dirs
        .into_iter()
        .map(|name| RemoteEntry {
            name,
            kind: EntryKind::Dir,
            size: None,
        })
        .collect();
    entries.extend(files);
    Ok(entries)
}

/// Stat the first meaningful entry in a tar produced by `tar cf - -C <parent> <basename>`.
///
/// The first entry in the archive is the target itself (a directory entry like `<basename>/` or a
/// file entry like `<basename>`).  Returns [`VfsError::NotFound`] if the archive is empty.
pub(crate) fn parse_stat_tar(tar_bytes: &[u8], path: &str) -> Result<RemoteMeta, VfsError> {
    let mut archive = tar::Archive::new(tar_bytes);
    let entry = archive
        .entries()
        .map_err(tar_io_err)?
        .next()
        .ok_or_else(|| not_found(path))?
        .map_err(tar_io_err)?;

    if entry.header().entry_type().is_dir() {
        Ok(RemoteMeta {
            kind: EntryKind::Dir,
            size: None,
        })
    } else {
        // Symlinks: report as File (size = 0 from the header; omit to avoid confusion).
        let size = if entry.header().entry_type().is_symlink()
            || entry.header().entry_type().is_hard_link()
        {
            None
        } else {
            Some(entry.header().size().map_err(tar_io_err)?)
        };
        Ok(RemoteMeta {
            kind: EntryKind::File,
            size,
        })
    }
}

/// Read the bytes of the first file entry in a tar produced by `tar cf - -C <parent> <basename>`.
///
/// Returns [`VfsError::Unsupported`] if the first entry is a directory (reading a directory path
/// does not make sense in a file-manager context), and [`VfsError::NotFound`] if the archive is
/// empty.
pub(crate) fn parse_read_tar(tar_bytes: &[u8], path: &str) -> Result<Vec<u8>, VfsError> {
    let mut archive = tar::Archive::new(tar_bytes);
    let mut entry = archive
        .entries()
        .map_err(tar_io_err)?
        .next()
        .ok_or_else(|| not_found(path))?
        .map_err(tar_io_err)?;

    if entry.header().entry_type().is_dir() {
        return Err(VfsError::Unsupported(Caps::READ));
    }
    let mut data = Vec::new();
    entry.read_to_end(&mut data).map_err(tar_io_err)?;
    Ok(data)
}

// ---------------------------------------------------------------------------
// Unit tests — hermetic, no cluster.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Path helpers ---------------------------------------------------------

    #[test]
    fn tar_parent_root_is_self() {
        assert_eq!(tar_parent("/"), "/");
        assert_eq!(tar_parent("///"), "/"); // degenerate but safe
    }

    #[test]
    fn tar_parent_direct_child() {
        assert_eq!(tar_parent("/etc"), "/");
        assert_eq!(tar_parent("/foo"), "/");
    }

    #[test]
    fn tar_parent_nested() {
        assert_eq!(tar_parent("/etc/hostname"), "/etc");
        assert_eq!(tar_parent("/a/b/c"), "/a/b");
        assert_eq!(tar_parent("/a/b/c/"), "/a/b"); // trailing slash stripped
    }

    #[test]
    fn tar_basename_root_is_empty() {
        assert_eq!(tar_basename("/"), "");
    }

    #[test]
    fn tar_basename_direct_child() {
        assert_eq!(tar_basename("/etc"), "etc");
        assert_eq!(tar_basename("/etc/"), "etc");
    }

    #[test]
    fn tar_basename_nested() {
        assert_eq!(tar_basename("/etc/hostname"), "hostname");
        assert_eq!(tar_basename("/a/b/c"), "c");
    }

    // -- Tar archive builder (helper for the parse tests) --------------------

    /// Build a minimal in-memory tar archive containing the specified entries.
    ///
    /// Each tuple is `(name, is_dir, content)`.  Directories carry a trailing `/` in the
    /// archive (as POSIX tar requires) and their content is ignored.
    fn build_tar(entries: &[(&str, bool, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            for (name, is_dir, content) in entries {
                let mut header = tar::Header::new_gnu();
                if *is_dir {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    // Directories must end with `/` in the archive name for standard POSIX tars.
                    let dir_name = if name.ends_with('/') {
                        (*name).to_owned()
                    } else {
                        format!("{name}/")
                    };
                    header.set_path(&dir_name).unwrap();
                    header.set_cksum();
                    builder.append(&header, &b""[..]).unwrap();
                } else {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(content.len() as u64);
                    header.set_path(name).unwrap();
                    header.set_cksum();
                    builder.append(&header, *content).unwrap();
                }
            }
            builder.finish().unwrap();
        }
        buf
    }

    // -- parse_list_dir -------------------------------------------------------

    #[test]
    fn list_dir_empty_tar_returns_empty_vec() {
        // An empty tar (just end-of-archive blocks) must return Ok(vec![]) — not NotFound.
        let tar = build_tar(&[("./", true, b"")]);
        let entries = parse_list_dir(&tar).unwrap();
        assert!(entries.is_empty(), "expected empty, got {entries:?}");
    }

    #[test]
    fn list_dir_flat_directory() {
        // `tar cf - -C /dir .` produces: `./`, `./file1`, `./file2`, `./subdir/`.
        let tar = build_tar(&[
            ("./", true, b""),
            ("./file1", false, b"hello"),
            ("./file2", false, b"world"),
            ("./subdir/", true, b""),
        ]);
        let entries = parse_list_dir(&tar).unwrap();
        // Dirs first, then files.
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["subdir", "file1", "file2"]);
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[1].kind, EntryKind::File);
        assert_eq!(entries[1].size, Some(5)); // "hello"
    }

    #[test]
    fn list_dir_deduplicates_deep_descendants() {
        // A deep tree: only the top-level directory names should appear.
        let tar = build_tar(&[
            ("./", true, b""),
            ("./etc/", true, b""),
            ("./etc/hostname", false, b"pod\n"),
            ("./etc/subdir/", true, b""),
            ("./etc/subdir/nested", false, b"x"),
            ("./top_file", false, b"y"),
        ]);
        let entries = parse_list_dir(&tar).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // `etc` appears once (dir), `top_file` appears once (file), no nested entries.
        assert_eq!(names, vec!["etc", "top_file"]);
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[1].kind, EntryKind::File);
    }

    #[test]
    fn list_dir_no_leading_dot_slash_variant() {
        // Some non-POSIX tar implementations omit the `./` leader, emitting bare names such as
        // `file` and `subdir/`.  `parse_list_dir` must still produce the correct result.
        // (Absolute-path archives like `/file` cannot be constructed with `tar::Builder` per the
        // POSIX spec, so we test the bare-name variant here; the `trim_start_matches('/')` guard
        // in the parsing code handles the defensive case for truly non-standard producers.)
        let tar = build_tar(&[("file1", false, b"data"), ("subdir/", true, b"")]);
        let entries = parse_list_dir(&tar).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // BTreeSet for dirs puts "subdir" before "file1".
        assert_eq!(names, vec!["subdir", "file1"]);
    }

    // -- parse_stat_tar -------------------------------------------------------

    #[test]
    fn stat_tar_file_returns_file_meta() {
        let tar = build_tar(&[("hostname", false, b"pod\n")]);
        let meta = parse_stat_tar(&tar, "/etc/hostname").unwrap();
        assert_eq!(meta.kind, EntryKind::File);
        assert_eq!(meta.size, Some(4));
    }

    #[test]
    fn stat_tar_directory_returns_dir_meta() {
        let tar = build_tar(&[("etc/", true, b"")]);
        let meta = parse_stat_tar(&tar, "/etc").unwrap();
        assert_eq!(meta.kind, EntryKind::Dir);
        assert!(meta.size.is_none());
    }

    #[test]
    fn stat_tar_empty_archive_is_not_found() {
        // Empty archive bytes (all zeros / no entries) must map to NotFound.
        let tar = build_tar(&[]);
        let result = parse_stat_tar(&tar, "/missing");
        assert!(
            matches!(result, Err(VfsError::NotFound(_))),
            "expected NotFound, got {result:?}"
        );
    }

    // -- parse_read_tar -------------------------------------------------------

    #[test]
    fn read_tar_file_returns_bytes() {
        let content = b"127.0.0.1 localhost\n";
        let tar = build_tar(&[("hosts", false, content)]);
        let data = parse_read_tar(&tar, "/etc/hosts").unwrap();
        assert_eq!(data, content);
    }

    #[test]
    fn read_tar_directory_is_unsupported() {
        let tar = build_tar(&[("etc/", true, b"")]);
        let result = parse_read_tar(&tar, "/etc");
        assert!(
            matches!(result, Err(VfsError::Unsupported(_))),
            "expected Unsupported, got {result:?}"
        );
    }

    #[test]
    fn read_tar_empty_archive_is_not_found() {
        let tar = build_tar(&[]);
        let result = parse_read_tar(&tar, "/missing");
        assert!(
            matches!(result, Err(VfsError::NotFound(_))),
            "expected NotFound, got {result:?}"
        );
    }

    #[test]
    fn read_tar_empty_file_returns_empty_bytes() {
        let tar = build_tar(&[("empty", false, b"")]);
        let data = parse_read_tar(&tar, "/empty").unwrap();
        assert_eq!(data, b"");
    }
}
