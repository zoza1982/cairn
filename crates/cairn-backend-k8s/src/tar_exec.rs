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
/// The `tar` argv for listing one directory's children.
///
/// `--no-recursion` is what keeps this a *listing*: without it `tar` walks the whole subtree and
/// streams every file's contents, so drawing one pane of a pod's `/` pulled the entire container
/// filesystem through the exec stream. `--` keeps a path beginning with `-` an operand, not a flag.
pub(crate) fn list_dir_argv(path: &str) -> [&str; 8] {
    ["tar", "cf", "-", "--no-recursion", "-C", path, "--", "."]
}

/// The `tar` argv for stat-ing or reading a single path.
///
/// `-h` dereferences symlinks: a symlink's tar header carries size 0 and no body, so without it
/// reading `/bin/sh` or `/etc/resolv.conf` — symlinks in most images — yielded an *empty file*
/// instead of the target's contents, silently.
pub(crate) fn stat_read_argv<'a>(parent: &'a str, basename: &'a str) -> [&'a str; 8] {
    ["tar", "cf", "-", "-h", "-C", parent, "--", basename]
}

/// Entries recoverable from the output of a `tar` run that exited non-zero.
///
/// `tar` reports failure when it could not read *any* member — one unreadable subdirectory in a
/// container running as non-root is enough — while still writing a complete archive of everything
/// it could read. Treating that as a hard error turned "a few entries are not readable by this
/// user" into "the directory does not exist". `None` when nothing usable came back, so the caller
/// can fall through to real error classification.
pub(crate) fn salvage_partial_listing(stdout: &[u8]) -> Option<Vec<RemoteEntry>> {
    if stdout.is_empty() {
        return None;
    }
    match parse_list_dir(stdout) {
        Ok(entries) if !entries.is_empty() => Some(entries),
        _ => None,
    }
}

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

    let kind = entry.header().entry_type();
    if kind.is_dir() {
        return Err(VfsError::Unsupported(Caps::READ));
    }
    // A link header carries size 0 and no body, so reading it yields an *empty file* — silent
    // corruption of exactly the files most likely to be links (`/bin/sh`, `/etc/resolv.conf`).
    // `stat_read_argv` passes `-h` so tar dereferences and we never see one; if a tar without that
    // support ever does emit one, say so rather than hand back nothing.
    if kind.is_symlink() || kind.is_hard_link() {
        return Err(VfsError::Backend {
            code: "tar-symlink".to_owned(),
            msg: format!("{path} is a link the container's tar did not dereference"),
            retryable: false,
        });
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

    /// Build a tar archive of `(path, kind, contents)` entries, for the parser tests below.
    fn tar_of(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, kind, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_entry_type(*kind);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h.clone(), path, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    /// Regression: `tar` exits non-zero if it could not read *any* member — one unreadable
    /// subdirectory is enough, which is the norm for a container running as non-root — but still
    /// writes everything it could read. Discarding that turned "some entries are not readable" into
    /// "the directory does not exist", so the pane came up empty with a NotFound error.
    #[test]
    fn a_partial_archive_from_a_failed_tar_is_salvaged() {
        let bytes = tar_of(&[
            ("./readable.txt", tar::EntryType::Regular, b"hi"),
            ("./sub/", tar::EntryType::Directory, b""),
        ]);
        let salvaged = salvage_partial_listing(&bytes).expect("a readable archive is salvageable");
        let mut names: Vec<_> = salvaged.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["readable.txt", "sub"]);
    }

    /// …but an empty or unparseable archive is not "an empty directory": the caller must fall
    /// through to real error classification (NotFound / exec_unavailable) rather than show a
    /// successful, empty pane.
    #[test]
    fn nothing_is_salvaged_from_empty_or_unusable_output() {
        assert!(salvage_partial_listing(b"").is_none());
        assert!(salvage_partial_listing(b"not a tar archive at all").is_none());
        assert!(
            salvage_partial_listing(&tar_of(&[])).is_none(),
            "an empty archive carries no entries to show"
        );
    }

    /// Listing must not recurse. Without `--no-recursion`, `tar` walks the whole subtree *and
    /// streams every file's body*, so drawing one pane of a pod's `/` pulled the entire container
    /// filesystem through the exec stream.
    #[test]
    fn the_listing_command_is_depth_one_and_guards_its_operand() {
        let argv = list_dir_argv("/var/log");
        assert!(argv.contains(&"--no-recursion"), "{argv:?}");
        // `--` immediately before the operand, so a path starting with `-` cannot become a flag.
        let dashdash = argv.iter().position(|a| *a == "--").expect("`--` present");
        assert_eq!(argv[dashdash + 1], ".");
        assert_eq!(argv.last(), Some(&"."));
    }

    /// Reading must dereference: a symlink's header has size 0 and no body, so `/bin/sh` and
    /// `/etc/resolv.conf` — symlinks in most images — read back as empty files.
    #[test]
    fn the_stat_read_command_dereferences_and_guards_its_operand() {
        let argv = stat_read_argv("/etc", "resolv.conf");
        assert!(argv.contains(&"-h"), "{argv:?}");
        let dashdash = argv.iter().position(|a| *a == "--").expect("`--` present");
        assert_eq!(argv[dashdash + 1], "resolv.conf");
        // A basename that looks like a flag stays an operand.
        let argv = stat_read_argv("/tmp", "--checkpoint-action=exec=sh");
        let dashdash = argv.iter().position(|a| *a == "--").unwrap();
        assert_eq!(argv[dashdash + 1], "--checkpoint-action=exec=sh");
        assert_eq!(argv.len(), dashdash + 2, "the operand is last");
    }

    /// A symlink entry must not be served as an empty file. With `-h` tar emits the target's
    /// regular-file header instead, which is what `parse_read_tar` then sees.
    #[test]
    fn a_symlink_entry_is_not_read_as_empty_content() {
        let link = tar_of(&[("resolv.conf", tar::EntryType::Symlink, b"")]);
        assert!(
            parse_read_tar(&link, "/etc/resolv.conf").is_err(),
            "a symlink header must not yield an empty file"
        );
        // What `-h` actually produces: the target's content under the requested name.
        let dereferenced = tar_of(&[(
            "resolv.conf",
            tar::EntryType::Regular,
            b"nameserver 1.1.1.1",
        )]);
        assert_eq!(
            parse_read_tar(&dereferenced, "/etc/resolv.conf").unwrap(),
            b"nameserver 1.1.1.1"
        );
    }

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
