//! Security guards for parsing untrusted archive bytes (RFC-0013 §"Security guards").
//!
//! An archive is arbitrary, attacker-influenceable input (it may be downloaded, received as an
//! attachment, or copied from anywhere) — unlike the local filesystem it is browsed *through*,
//! every byte of its structure (paths, sizes, ratios, link targets) must be treated as hostile.
//! This module centralizes the caps and validation helpers both [`crate::tar_backend::TarOps`] and
//! [`crate::zip_backend::ZipOps`] apply during indexing and reads, so the two format-specific
//! scanners can't independently drift on policy.

use cairn_types::VfsPath;
use cairn_vfs::VfsError;

/// Hard cap on the number of raw headers/central-directory records a single archive may contain.
/// Enforced against every header *encountered* during the initial scan (not just the ones kept),
/// so a crafted archive with millions of tiny/invalid entries can't stall indexing or exhaust
/// memory before validation even gets a chance to reject them. 100k comfortably covers real-world
/// source trees / dependency archives while bounding worst-case scan time and index memory.
pub(crate) const ARCHIVE_MAX_ENTRIES: usize = 100_000;

/// Per-read byte cap: a single [`cairn_vfs::Vfs::open_read`] call never decodes more than this many
/// bytes for one member, regardless of the member's declared (and possibly forged) size.
pub(crate) const ARCHIVE_PER_MEMBER_CAP: u64 = 64 * 1024 * 1024;

/// Cumulative decoded-byte budget for one mounted archive (one [`crate::ArchiveVfs`] instance /
/// session). Once exhausted, further reads are refused with a "possible archive bomb" error
/// rather than silently continuing to decode — this is the backstop against a legitimate-looking
/// archive whose members are individually under [`ARCHIVE_PER_MEMBER_CAP`] but whose sum is not.
pub(crate) const ARCHIVE_SESSION_BYTE_CAP: u64 = 512 * 1024 * 1024;

/// Maximum tolerated uncompressed:compressed ratio for a zip member, checked from central-directory
/// metadata *before* any decompression is attempted. Ordinary text/data compresses well short of
/// this; a ratio beyond it is characteristic of a deliberately crafted decompression bomb (e.g. a
/// few KiB of repeated zeros expanding to gigabytes).
pub(crate) const ZIP_MAX_COMPRESSION_RATIO: u64 = 100;

/// Ratio-guard floor: members whose *uncompressed* size is below this are exempt from the ratio
/// check. A handful of bytes can legitimately compress at a huge ratio (e.g. an empty-ish file) and
/// numerically none of that is dangerous — the ratio only matters once the absolute size is large
/// enough to matter, and [`ARCHIVE_PER_MEMBER_CAP`]/[`ARCHIVE_SESSION_BYTE_CAP`] independently bound
/// the actual decode regardless.
pub(crate) const ZIP_RATIO_FLOOR_BYTES: u64 = 1024 * 1024;

/// Cap on a single path segment's length. Guards against a pathological name (megabytes of a single
/// "file name") reaching the TUI's listing renderer or a terminal-width calculation.
pub(crate) const ARCHIVE_MAX_NAME_LEN: usize = 4096;

/// Bound on how many bytes we'll read to resolve a zip symlink member's target (the target string
/// is the entry's *content* in the zip format, not a header field) — a legitimate symlink target is
/// at most a few hundred bytes; anything absurd is simply not treated as a resolvable target.
pub(crate) const ARCHIVE_SYMLINK_TARGET_CAP: u64 = 4 * 1024;

/// Validate and normalize one archive member's raw path string into a [`VfsPath`], or reject it.
///
/// Applies, in order:
/// - reject embedded NUL / other control characters (delegated to [`VfsPath::parse`]),
/// - normalize Windows-style `\` separators to `/` (many zips are authored with them),
/// - reject absolute paths (a leading `/` after normalization also catches UNC `\\server\share`,
///   which becomes `//server/share`),
/// - reject a Windows drive-letter prefix (`C:...`),
/// - reject `..` traversal (delegated to [`VfsPath::parse`]),
/// - reject a name whose length exceeds [`ARCHIVE_MAX_NAME_LEN`] (display-safety),
/// - trim a single trailing `/` (zip directory-entry convention) so a directory member and its
///   path as an ancestor-of-a-child both key to the *same* [`VfsPath`] (which otherwise
///   distinguishes `"foo"` from `"foo/"` for object-store prefix semantics — not meaningful here).
///
/// A rejected member should be skipped by the caller with a warning, never treated as fatal to the
/// whole archive.
#[must_use]
pub(crate) fn validate_member_name(raw: &str) -> Option<VfsPath> {
    if raw.len() > ARCHIVE_MAX_NAME_LEN {
        return None;
    }
    let normalized = raw.replace('\\', "/");
    let trimmed = normalized.strip_suffix('/').unwrap_or(&normalized);
    if trimmed.starts_with('/') {
        return None; // absolute path, or UNC (`\\server\share` -> `//server/share`)
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return None; // Windows drive letter, e.g. "C:/Windows/System32"
    }
    VfsPath::parse(trimmed).ok()
}

/// Enforce [`ARCHIVE_MAX_ENTRIES`] against `scanned`, the count of raw headers/central-directory
/// records seen *so far* (including ones later rejected by validation). Called once per header by
/// both [`crate::tar_backend::TarOps::build`] and [`crate::zip_backend::ZipOps::build`], so the two
/// formats can't drift on where the line is drawn.
pub(crate) fn check_entry_count(scanned: usize) -> Result<(), VfsError> {
    if scanned > ARCHIVE_MAX_ENTRIES {
        return Err(VfsError::Backend {
            code: "archive_too_many_entries".to_owned(),
            msg: "possible archive bomb: too many entries".to_owned(),
            retryable: false,
        });
    }
    Ok(())
}

/// Whether a zip member's declared compressed/uncompressed sizes look like a decompression bomb,
/// checked from central-directory metadata alone (before any bytes are decoded). `uncompressed`
/// below [`ZIP_RATIO_FLOOR_BYTES`] is always accepted regardless of ratio.
#[must_use]
pub(crate) fn zip_ratio_is_bomb(compressed: u64, uncompressed: u64) -> bool {
    if uncompressed < ZIP_RATIO_FLOOR_BYTES {
        return false;
    }
    // A declared-zero (or tiny) compressed size for a large uncompressed size is the degenerate
    // case of an absurd ratio; treat it as a bomb rather than divide-by-near-zero.
    let divisor = compressed.max(1);
    uncompressed / divisor > ZIP_MAX_COMPRESSION_RATIO
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_absolute_and_traversal_and_unc_and_drive() {
        assert!(validate_member_name("/etc/passwd").is_none());
        assert!(validate_member_name("../../etc/passwd").is_none());
        assert!(validate_member_name("a/../../b").is_none());
        assert!(validate_member_name("\\\\server\\share\\file").is_none());
        assert!(validate_member_name("C:/Windows/System32/x").is_none());
        assert!(validate_member_name("C:\\Windows\\System32\\x").is_none());
    }

    #[test]
    fn rejects_control_chars_and_nul() {
        assert!(validate_member_name("a\0b").is_none());
        assert!(validate_member_name("a\nb").is_none());
    }

    #[test]
    fn rejects_overlong_names() {
        let long = "a".repeat(ARCHIVE_MAX_NAME_LEN + 1);
        assert!(validate_member_name(&long).is_none());
    }

    #[test]
    fn accepts_and_normalizes_ordinary_paths() {
        let p = validate_member_name("dir/sub/file.txt").unwrap();
        assert_eq!(p.as_str(), "/dir/sub/file.txt");
        // Windows-style separators normalize to `/`.
        let p2 = validate_member_name("dir\\sub\\file.txt").unwrap();
        assert_eq!(p2, p);
        // A trailing slash (zip directory convention) keys the same as the bare name.
        let d = validate_member_name("dir/sub/").unwrap();
        let d2 = validate_member_name("dir/sub").unwrap();
        assert_eq!(d, d2);
    }

    #[test]
    fn entry_count_cap_boundary() {
        assert!(check_entry_count(ARCHIVE_MAX_ENTRIES).is_ok());
        assert!(matches!(
            check_entry_count(ARCHIVE_MAX_ENTRIES + 1),
            Err(VfsError::Backend { .. })
        ));
    }

    #[test]
    fn ratio_guard_floor_exempts_small_members() {
        // Absurd ratio, but tiny absolute size -> not flagged.
        assert!(!zip_ratio_is_bomb(1, 10_000));
    }

    #[test]
    fn ratio_guard_flags_large_absurd_ratios() {
        // 1 byte compressed expanding to 200 MiB uncompressed.
        assert!(zip_ratio_is_bomb(1, 200 * 1024 * 1024));
        // A realistic compression ratio for compressible text is not flagged.
        assert!(!zip_ratio_is_bomb(2 * 1024 * 1024, 10 * 1024 * 1024));
    }
}
