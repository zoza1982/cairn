//! Archive container magic bytes, shared so the pure UI-side sniff and the archive backend can't
//! drift.
//!
//! The front-end file-kind sniff (`cairn_core::detect_file_kind`) decides whether `Enter` mounts
//! an archive, and the backend (`cairn-backend-archive`) decides how to actually open it. Those two
//! live in different crates (`cairn-core` has no backend dependency), but they must agree on exactly
//! which bytes identify an archive. Keeping the constants here — in the shared leaf crate both
//! already depend on — turns "must stay in sync" from a comment into a compile-time fact.

/// Zip local-file-header signature (`PK\x03\x04`) at offset 0.
pub const ZIP_MAGIC: &[u8] = b"PK\x03\x04";

/// POSIX tar `ustar` magic. Unlike the others this is not at offset 0 — it sits at
/// [`TAR_USTAR_OFFSET`] within the fixed 512-byte tar header.
pub const TAR_USTAR_MAGIC: &[u8] = b"ustar";

/// Byte offset of [`TAR_USTAR_MAGIC`] within a tar header.
pub const TAR_USTAR_OFFSET: usize = 257;

/// gzip magic (`.tar.gz`/`.tgz`, RFC 1952) at offset 0.
pub const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];

/// bzip2 magic (`.tar.bz2`/`.tbz2`) at offset 0.
pub const BZIP2_MAGIC: &[u8] = b"BZh";

/// xz magic (`.tar.xz`/`.txz`) at offset 0.
pub const XZ_MAGIC: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];

/// zstd little-endian frame magic (`.tar.zst`/`.tzst`) at offset 0.
pub const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];

/// The four outer-compression magics of a compressed tar, each checked at offset 0 (after the
/// structural zip/tar signatures, which take priority). Order matches the backend's sniff.
pub const COMPRESSED_TAR_MAGICS: &[&[u8]] = &[GZIP_MAGIC, BZIP2_MAGIC, XZ_MAGIC, ZSTD_MAGIC];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_tar_magics_matches_the_individual_consts() {
        assert_eq!(
            COMPRESSED_TAR_MAGICS,
            &[GZIP_MAGIC, BZIP2_MAGIC, XZ_MAGIC, ZSTD_MAGIC]
        );
    }

    #[test]
    fn magics_are_the_expected_bytes() {
        assert_eq!(ZIP_MAGIC, &[0x50, 0x4b, 0x03, 0x04]);
        assert_eq!(TAR_USTAR_MAGIC, b"ustar");
        assert_eq!(TAR_USTAR_OFFSET, 257);
        assert_eq!(GZIP_MAGIC, &[0x1f, 0x8b]);
        assert_eq!(BZIP2_MAGIC, &[0x42, 0x5a, 0x68]);
        assert_eq!(XZ_MAGIC, &[0xfd, b'7', b'z', b'X', b'Z', 0x00]);
        assert_eq!(ZSTD_MAGIC, &[0x28, 0xb5, 0x2f, 0xfd]);
    }
}
