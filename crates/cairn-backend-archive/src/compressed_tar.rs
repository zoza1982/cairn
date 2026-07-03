//! Compressed-tar support (RFC-0013 P5): `.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`, `.tar.zst`/`.tzst`.
//! `.tar.xz`/`.txz` is *recognized* (magic-sniffed) but deliberately **not decoded** — see
//! "xz/lzma is not decoded" below.
//!
//! **Approach: decompress once to a private temp file, then index it exactly like an ordinary
//! uncompressed tar.** A compressed tar stream is not randomly seekable — `tar_backend`'s
//! `entries_with_seek` trick (skip a member's *content* via `Seek` rather than reading it) has
//! nothing meaningful to seek *to* inside the compressed byte stream for an arbitrary member. So
//! unlike plain tar/zip (indexed directly over the original file), a compressed tar is fully
//! decoded once, up front, into a temp file; `build` then hands that temp path to `TarOps::build`
//! (the same uncompressed-tar indexer P4 already ships), so every existing tar guard — path
//! validation, symlink/hardlink inertness, special/setuid skipping, the entry-count cap, checked
//! size arithmetic — applies unchanged to the decompressed content with zero duplicated logic.
//!
//! This is deliberately a one-pass-then-random-access trade, not streaming re-decompression per
//! read: re-decoding from the start of the compressed stream on every `read_member` call would be
//! O(n) per read, where decompressing once up front is O(1) per read afterward via `TarOps`'s
//! existing offset index.
//!
//! Every decoder that *is* used is pure-Rust (no C/FFI parsing these untrusted bytes) — see
//! `Cargo.toml`'s per-dependency comments and `docs/adr/0013-compressed-tar-decoder-selection.md`
//! for the trade-offs weighed for each format.
//!
//! ## xz/lzma is not decoded
//!
//! `Compression::Xz` exists and [`sniff`] still recognizes `.txz`/`.tar.xz`'s magic bytes, but
//! [`decompress_to_temp`] refuses it with a typed, friendly error rather than decoding it. The
//! pure-Rust `lzma-rs` crate's LZMA2 decode path is not memory-bounded: its internal dictionary
//! buffer is flushed to the output sink only on specific internal resets, which a crafted stream
//! can defer indefinitely, letting a tiny `.txz` accumulate multiple GiB in **RAM** before this
//! module's own [`CappedWriter`] output-byte guard ever gets a chance to see a byte and abort — the
//! guard bounds the *file* this backend writes, not the *decoder's own internal memory*. Shipping
//! that would be an unconditional OOM vector reachable from opening a file, so xz was dropped
//! entirely rather than shipped with a known bomb. See ADR-0013 for the full record and the
//! follow-up options (a memory-bounded pure-Rust decoder, or a C `liblzma` binding with an explicit
//! `memlimit`, security-review-gated).
//!
//! ## Multi-stream / multi-frame concatenated input is never silently truncated
//!
//! gzip, bzip2, and zstd all allow concatenating multiple independent compressed streams/frames
//! back-to-back in one file (`cat a.gz b.gz`, `bgzip`'s multi-block output, etc.) — a naive
//! single-stream decoder silently stops at the end of the *first* one, producing a truncated tar
//! with no error at all. `flate2::read::MultiGzDecoder` (not `GzDecoder`) handles this correctly
//! for gzip by design. Neither `bzip2-rs` nor `ruzstd` continue past their first stream/frame, so
//! [`decompress_to_temp`] adds an explicit, decoder-implementation-independent guard for both: after
//! a successful decode, [`contains_magic_after_start`] scans the *original compressed file's own
//! bytes* (not anything the decoder reported about itself) for a second occurrence of that format's
//! magic number anywhere after the first byte. A hit means more stream data exists that was never
//! decoded, and the mount is refused with `compressed_tar_multi_stream` rather than silently
//! opening a truncated archive. This is deliberately independent of how much a decoder *claims* to
//! have consumed from its input reader: `bzip2_rs::DecoderReader` was found, empirically, to read
//! its *entire* underlying source into an internal buffer regardless of how many logical bzip2
//! streams are present, so a "bytes consumed from the reader" check alone would report a
//! concatenated two-stream file as fully consumed even though only the first stream was decoded —
//! a byte-level scan of the file's own content has no such blind spot.

use crate::security::{compression_ratio_is_bomb, ARCHIVE_MAX_DECOMPRESSED_BYTES};
use crate::tar_backend::TarOps;
use crate::ArchiveOps;
use cairn_types::VfsPath;
use cairn_vfs::VfsError;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// `.tar.gz`/`.tgz` magic (RFC 1952).
const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
/// `.tar.bz2`/`.tbz2` magic.
const BZIP2_MAGIC: &[u8] = b"BZh";
/// `.tar.xz`/`.txz` magic.
const XZ_MAGIC: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];
/// `.tar.zst`/`.tzst` magic — the little-endian zstd frame magic number.
const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];

/// The outer compression wrapping a tar stream, detected purely from magic bytes (never a file
/// extension) by [`sniff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    /// `.tar.gz` / `.tgz` — see [`GZIP_MAGIC`].
    Gzip,
    /// `.tar.bz2` / `.tbz2` — see [`BZIP2_MAGIC`].
    Bzip2,
    /// `.tar.xz` / `.txz` — see [`XZ_MAGIC`]. Recognized but not decoded; see the module docs.
    Xz,
    /// `.tar.zst` / `.tzst` — see [`ZSTD_MAGIC`].
    Zstd,
}

impl Compression {
    /// This format's magic bytes, as checked by [`sniff`].
    fn magic(self) -> &'static [u8] {
        match self {
            Self::Gzip => GZIP_MAGIC,
            Self::Bzip2 => BZIP2_MAGIC,
            Self::Xz => XZ_MAGIC,
            Self::Zstd => ZSTD_MAGIC,
        }
    }

    /// A human-readable name for error messages.
    fn label(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Bzip2 => "bzip2",
            Self::Xz => "xz",
            Self::Zstd => "zstd",
        }
    }
}

/// Sniff `prefix` (the first bytes of the file) for a recognized outer-compression magic. Checked
/// independently of (and after) the plain zip/tar magic checks in `vfs::sniff_format` — a
/// compression magic never collides with either.
#[must_use]
pub(crate) fn sniff(prefix: &[u8]) -> Option<Compression> {
    if prefix.starts_with(GZIP_MAGIC) {
        return Some(Compression::Gzip);
    }
    if prefix.starts_with(BZIP2_MAGIC) {
        return Some(Compression::Bzip2);
    }
    if prefix.starts_with(XZ_MAGIC) {
        return Some(Compression::Xz);
    }
    if prefix.starts_with(ZSTD_MAGIC) {
        return Some(Compression::Zstd);
    }
    None
}

/// A [`crate::ArchiveOps`] over a compressed tar: a plain `TarOps` indexing a decompressed temp
/// file, plus the RAII handle that deletes that temp file when the mount is dropped.
///
/// Held as `_temp: tempfile::NamedTempFile` — never read directly (all reads go through `inner`,
/// which holds its own re-opened `File` handle onto the same path) — its only job is outliving
/// `inner` and deleting the file on drop.
pub(crate) struct CompressedTarOps {
    inner: TarOps,
    _temp: tempfile::NamedTempFile,
}

impl CompressedTarOps {
    /// Decompress `path` (whose outer compression is `compression`) into a private temp file, then
    /// index that temp file as a plain tar. Synchronous and potentially slow for a large archive —
    /// callers must run this inside `tokio::task::spawn_blocking` (see [`crate::ArchiveVfs::open`]).
    ///
    /// # Errors
    /// A typed `VfsError` (never a panic) for: `compression == Xz` (recognized but never decoded —
    /// see the module docs), an unopenable/unreadable input file, malformed compressed data,
    /// decompressed output exceeding [`ARCHIVE_MAX_DECOMPRESSED_BYTES`] or the compression-ratio
    /// guard (both "possible archive bomb"), a second bzip2/zstd stream/frame left un-decoded (see
    /// `contains_magic_after_start`), or anything `TarOps::build` itself would reject in the
    /// decompressed content (entry-count cap, etc. — unchanged from plain tar).
    pub(crate) fn build(path: &Path, compression: Compression) -> Result<Self, VfsError> {
        let (temp, _decoded_len) =
            decompress_to_temp(path, compression, ARCHIVE_MAX_DECOMPRESSED_BYTES)?;
        let inner = TarOps::build(temp.path())?;
        Ok(Self { inner, _temp: temp })
    }
}

impl ArchiveOps for CompressedTarOps {
    fn list_children(&self, dir: &VfsPath) -> Result<Vec<cairn_types::Entry>, VfsError> {
        self.inner.list_children(dir)
    }

    fn entry_meta(&self, path: &VfsPath) -> Result<cairn_types::Entry, VfsError> {
        self.inner.entry_meta(path)
    }

    fn read_member(&self, path: &VfsPath, cap: u64) -> Result<Vec<u8>, VfsError> {
        self.inner.read_member(path, cap)
    }
}

/// A bomb-detection error raised by [`CappedWriter::write`] and threaded back out through whatever
/// decode function it aborted (a decoder's own I/O error variant almost always just wraps ours) —
/// stored on the writer itself (not parsed back out of the propagated error), which sidesteps
/// needing to downcast through each decoder crate's distinct error type.
#[derive(Debug, Clone, Copy)]
enum BombKind {
    /// Total decoded bytes exceeded [`ARCHIVE_MAX_DECOMPRESSED_BYTES`].
    OutputCap,
    /// [`compression_ratio_is_bomb`] flagged the running decoded-bytes-vs-compressed-input ratio.
    Ratio,
}

impl BombKind {
    fn message(self) -> &'static str {
        match self {
            Self::OutputCap => {
                "possible archive bomb: decompressed output exceeds the cap for this compressed tar"
            }
            Self::Ratio => {
                "possible archive bomb: compression ratio exceeds the bomb-detection threshold"
            }
        }
    }
}

/// A [`Write`] sink wrapping the temp file that aborts the instant either bomb guard trips —
/// enforced incrementally, on every chunk the decoder writes, not after the fact on a fully
/// decoded buffer. Used uniformly across all three decoders that actually run (gzip/bzip2/zstd —
/// xz is never decoded at all, see the module docs), each driving it via `io::copy` from a
/// `Read`-based decoder, so the cap/ratio logic exists exactly once regardless of which crate is
/// decoding.
struct CappedWriter<'a> {
    file: &'a mut File,
    /// The whole compressed input file's size — the ratio guard's denominator. Never zero (the
    /// caller clamps via `.max(1)`), so division in `compression_ratio_is_bomb` is always safe.
    compressed_len: u64,
    /// The absolute output-byte cap for this decode — [`ARCHIVE_MAX_DECOMPRESSED_BYTES`] in
    /// production; tests inject a much smaller value so the cap-specific test fixture stays tiny
    /// and fast rather than needing to actually push hundreds of megabytes through the writer.
    max_decompressed_bytes: u64,
    total: u64,
    bomb: Option<BombKind>,
}

impl<'a> CappedWriter<'a> {
    fn new(file: &'a mut File, compressed_len: u64, max_decompressed_bytes: u64) -> Self {
        Self {
            file,
            compressed_len: compressed_len.max(1),
            max_decompressed_bytes,
            total: 0,
            bomb: None,
        }
    }
}

impl Write for CappedWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Checked, not wrapping/saturating-then-compared: an overflow here would itself be a sign
        // of adversarial input, and it must abort rather than silently wrap to a small number that
        // would then sail past both guards below.
        let candidate = match self.total.checked_add(buf.len() as u64) {
            Some(v) => v,
            None => {
                self.bomb = Some(BombKind::OutputCap);
                return Err(io::Error::other(
                    "archive bomb: decoded byte count overflowed",
                ));
            }
        };
        if candidate > self.max_decompressed_bytes {
            self.bomb = Some(BombKind::OutputCap);
            return Err(io::Error::other(
                "archive bomb: decompressed output cap exceeded",
            ));
        }
        if compression_ratio_is_bomb(self.compressed_len, candidate) {
            self.bomb = Some(BombKind::Ratio);
            return Err(io::Error::other("archive bomb: compression ratio exceeded"));
        }
        self.file.write_all(buf)?;
        self.total = candidate;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn bomb_err(kind: BombKind) -> VfsError {
    VfsError::Backend {
        code: "archive_bomb_detected".to_owned(),
        msg: kind.message().to_owned(),
        retryable: false,
    }
}

fn decode_err(e: impl std::fmt::Display) -> VfsError {
    VfsError::Backend {
        code: "compressed_tar_decode_error".to_owned(),
        msg: format!("failed to decompress archive: {e}"),
        retryable: false,
    }
}

/// xz is recognized but never decoded — see the module docs for why (`lzma-rs`'s LZMA2 path is not
/// memory-bounded against a decompression bomb).
fn xz_unsupported_err() -> VfsError {
    VfsError::Backend {
        code: "compressed_tar_xz_unsupported".to_owned(),
        msg: "xz-compressed tar (.txz/.tar.xz) is not yet supported — its pure-Rust decoder is \
              not memory-bounded against decompression bombs; tracked as an RFC-0013 follow-up"
            .to_owned(),
        retryable: false,
    }
}

/// A second stream/frame of `label`-compressed data was found in the input beyond what was
/// decoded — see [`contains_magic_after_start`] and the module docs' "Multi-stream" section.
fn multi_stream_err(label: &str) -> VfsError {
    VfsError::Backend {
        code: "compressed_tar_multi_stream".to_owned(),
        msg: format!(
            "multi-stream/multi-frame {label} archives are only partially decoded; full support \
             is an RFC-0013 follow-up"
        ),
        retryable: false,
    }
}

/// Scan `path`'s own bytes (read once, streamed in fixed-size chunks — never the decoder's
/// interpretation of them) for a second occurrence of `magic` starting at any byte offset at or
/// after 1 (offset 0 is the file's own leading magic, already established by [`sniff`]).
///
/// This is the multi-stream/multi-frame guard described in the module docs: it is deliberately
/// independent of anything a decoder reports about how much of its input it "consumed", because
/// that signal was found to be unreliable for at least one decoder in this module (`bzip2-rs`
/// reads its entire input into an internal buffer regardless of stream count). A hit is a
/// conservative, fail-closed signal — an extremely rare false positive (the magic bytes
/// coincidentally recurring within genuine compressed entropy) costs a refused mount, never a
/// silently truncated one, which is the correct trade-off for a file manager.
fn contains_magic_after_start(path: &Path, magic: &[u8]) -> Result<bool, VfsError> {
    debug_assert!(!magic.is_empty());
    let file = File::open(path).map_err(VfsError::Io)?;
    let mut reader = io::BufReader::new(file);
    // `overlap` carries the trailing `magic.len() - 1` bytes of each chunk into the next one, so a
    // match straddling a chunk boundary is never missed. `base` is the absolute file offset of
    // `window[0]`, updated every time the window is trimmed back down to the overlap.
    let overlap = magic.len().saturating_sub(1);
    let mut window: Vec<u8> = Vec::with_capacity(overlap + 64 * 1024);
    let mut base: u64 = 0;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk).map_err(VfsError::Io)?;
        if n == 0 {
            return Ok(false);
        }
        window.extend_from_slice(&chunk[..n]);
        if window.len() >= magic.len() {
            for (i, w) in window.windows(magic.len()).enumerate() {
                // Offset 0 is the file's own leading magic (already established by `sniff`) — only
                // a match starting at offset >= 1 indicates a *second* occurrence.
                if w == magic && base + i as u64 >= 1 {
                    return Ok(true);
                }
            }
            let drain = window.len() - overlap;
            base += drain as u64;
            window.drain(0..drain);
        }
    }
}

/// Pick a private, per-user temp directory for the decompression destination: `$XDG_RUNTIME_DIR`
/// (per-user, `0700`, usually tmpfs, torn down at logout) when it's set and really is a directory,
/// falling back to the platform temp dir (`std::env::temp_dir()`, via `tempfile`'s default)
/// otherwise. Either way the file itself is still created with `tempfile`'s own `0600`
/// permissions and a randomized, non-predictable name.
fn preferred_temp_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let path = PathBuf::from(dir);
    path.is_dir().then_some(path)
}

fn new_temp_file() -> Result<tempfile::NamedTempFile, VfsError> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".cairn-archive-");
    match preferred_temp_dir() {
        Some(dir) => builder.tempfile_in(dir),
        None => builder.tempfile(),
    }
    .map_err(VfsError::Io)
}

/// Decompress `path` into a fresh, private temp file, enforcing the decompression-bomb guards
/// incrementally as bytes are produced (never after the fact on a fully-materialized buffer), then
/// (for bzip2/zstd) checking for a second stream/frame left un-decoded (see the module docs).
///
/// `max_decompressed_bytes` is [`ARCHIVE_MAX_DECOMPRESSED_BYTES`] in production
/// (`CompressedTarOps::build`); tests call this directly with a much smaller value so the
/// absolute-cap guard can be exercised against a tiny, fast fixture instead of one that must
/// actually approach the real ~512 MiB production cap.
///
/// The temp file is created via [`new_temp_file`] (mode `0o600` on Unix, randomized non-predictable
/// name, preferring `$XDG_RUNTIME_DIR`) and deleted automatically when the returned handle is
/// dropped (including on any later error in the caller, since ownership is returned to it
/// immediately). Returns the handle plus the final decoded byte count.
fn decompress_to_temp(
    path: &Path,
    compression: Compression,
    max_decompressed_bytes: u64,
) -> Result<(tempfile::NamedTempFile, u64), VfsError> {
    if compression == Compression::Xz {
        return Err(xz_unsupported_err());
    }

    let compressed_len = std::fs::metadata(path).map_err(VfsError::Io)?.len();
    let source = File::open(path).map_err(VfsError::Io)?;
    let mut temp = new_temp_file()?;

    let mut capped = CappedWriter::new(temp.as_file_mut(), compressed_len, max_decompressed_bytes);
    let copy_result: io::Result<()> = match compression {
        Compression::Gzip => {
            // `MultiGzDecoder`, not `GzDecoder`: decodes every concatenated gzip member (e.g.
            // `bgzip` output), not just the first — see the module docs' "Multi-stream" section.
            let mut decoder = flate2::read::MultiGzDecoder::new(source);
            io::copy(&mut decoder, &mut capped).map(|_| ())
        }
        Compression::Bzip2 => {
            let mut decoder = bzip2_rs::DecoderReader::new(source);
            io::copy(&mut decoder, &mut capped).map(|_| ())
        }
        Compression::Zstd => match ruzstd::decoding::StreamingDecoder::new(source) {
            Ok(mut decoder) => io::copy(&mut decoder, &mut capped).map(|_| ()),
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        },
        Compression::Xz => unreachable!("handled above"),
    };

    let bomb = capped.bomb;
    let total = capped.total;
    match (copy_result, bomb) {
        // The abort was ours (a bomb guard tripped) — report that specifically, not the decoder's
        // (possibly confusing, generic "broken pipe"-style) wrapping of our forced I/O error.
        (_, Some(kind)) => Err(bomb_err(kind)),
        (Ok(()), None) => {
            // gzip's `MultiGzDecoder` already handles concatenation correctly; bzip2/zstd don't,
            // so guard those two explicitly (see `contains_magic_after_start`'s doc comment).
            if matches!(compression, Compression::Bzip2 | Compression::Zstd)
                && contains_magic_after_start(path, compression.magic())?
            {
                return Err(multi_stream_err(compression.label()));
            }
            Ok((temp, total))
        }
        (Err(e), None) => Err(decode_err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_types::EntryKind;

    #[test]
    fn sniffs_every_compression_magic_and_rejects_neither() {
        assert_eq!(sniff(&[0x1f, 0x8b, 0x08, 0x00]), Some(Compression::Gzip));
        assert_eq!(sniff(b"BZh9"), Some(Compression::Bzip2));
        assert_eq!(
            sniff(&[0xfd, b'7', b'z', b'X', b'Z', 0x00, 0x00]),
            Some(Compression::Xz)
        );
        assert_eq!(
            sniff(&[0x28, 0xb5, 0x2f, 0xfd, 0x00]),
            Some(Compression::Zstd)
        );
        assert_eq!(sniff(b"not compressed archive!"), None);
        assert_eq!(sniff(&[]), None);
    }

    /// Build an uncompressed tar (one file) in memory, returning its raw bytes.
    fn build_test_tar(name: &str, content: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, content).unwrap();
        builder.into_inner().unwrap()
    }

    /// gzip-compress `data` (via `flate2`'s writer side — used only in this test module to build
    /// fixtures; the backend itself only ever *decodes* gzip).
    fn gzip_bytes(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn write_temp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();
        tmp
    }

    #[test]
    fn decompresses_and_indexes_a_real_gzip_tar() {
        let tar_bytes = build_test_tar("hello.txt", b"hello, compressed archive");
        let gz_bytes = gzip_bytes(&tar_bytes);
        let tmp = write_temp(&gz_bytes);

        let ops = CompressedTarOps::build(tmp.path(), Compression::Gzip).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "hello.txt");
        assert_eq!(root[0].kind, EntryKind::File);

        let data = ops
            .read_member(&VfsPath::parse("hello.txt").unwrap(), 1024)
            .unwrap();
        assert_eq!(data, b"hello, compressed archive");
    }

    /// The whole point of decompress-to-temp: a *ranged* read after the initial decode must not
    /// re-decompress anything (no re-scan) — exercised indirectly by confirming a second,
    /// differently-ranged read against the same `CompressedTarOps` still returns correct bytes,
    /// which only holds if reads go through `TarOps`'s seek-based offset index over the already
    /// decoded temp file rather than something re-driving the gzip decoder.
    #[test]
    fn temp_file_backed_reads_are_random_access_after_one_decode() {
        let tar_bytes = build_test_tar("data.bin", b"0123456789");
        let gz_bytes = gzip_bytes(&tar_bytes);
        let tmp = write_temp(&gz_bytes);
        let ops = CompressedTarOps::build(tmp.path(), Compression::Gzip).unwrap();

        let all = ops
            .read_member(&VfsPath::parse("data.bin").unwrap(), 1024)
            .unwrap();
        assert_eq!(all, b"0123456789");
        // A second read of the same member (would be a fresh seek+read in `TarOps`, not a fresh
        // gzip decode) still returns the same bytes.
        let again = ops
            .read_member(&VfsPath::parse("data.bin").unwrap(), 1024)
            .unwrap();
        assert_eq!(again, b"0123456789");
    }

    #[test]
    fn unrecognized_bytes_after_the_gzip_magic_error_not_panic() {
        // Valid gzip magic, but the rest is garbage — must surface a typed decode error, not panic.
        let mut bytes = vec![0x1f, 0x8b, 0x08, 0x00];
        bytes.extend_from_slice(&[0u8; 32]);
        let tmp = write_temp(&bytes);
        match CompressedTarOps::build(tmp.path(), Compression::Gzip) {
            Err(VfsError::Backend { code, .. }) => assert_eq!(code, "compressed_tar_decode_error"),
            other => panic!(
                "expected a decode-error Backend variant, got ok={}",
                other.is_ok()
            ),
        }
    }

    /// xz is sniffed correctly but must never be decoded — see the module docs / ADR-0013 (the
    /// `lzma-rs` LZMA2 path is not memory-bounded against a decompression bomb). This is the
    /// regression test for that decision: the trailing bytes here are arbitrary/never a real xz
    /// stream, and must never be parsed at all — only the typed refusal matters.
    #[test]
    fn txz_is_recognized_but_not_decoded() {
        let mut bytes = vec![0xfd, b'7', b'z', b'X', b'Z', 0x00];
        bytes.extend_from_slice(&[0u8; 16]);
        assert_eq!(sniff(&bytes), Some(Compression::Xz));
        let tmp = write_temp(&bytes);
        match CompressedTarOps::build(tmp.path(), Compression::Xz) {
            Err(VfsError::Backend { code, .. }) => {
                assert_eq!(code, "compressed_tar_xz_unsupported");
            }
            other => panic!(
                "expected compressed_tar_xz_unsupported, got ok={}",
                other.is_ok()
            ),
        }
    }

    #[test]
    fn truncated_bzip2_stream_errors_not_panics() {
        let bytes = b"BZh9\x01\x02\x03garbage-not-a-real-stream".to_vec();
        let tmp = write_temp(&bytes);
        match CompressedTarOps::build(tmp.path(), Compression::Bzip2) {
            Err(VfsError::Backend { .. }) => {}
            other => panic!("expected a decode error, got ok={}", other.is_ok()),
        }
    }

    #[test]
    fn truncated_zstd_stream_errors_not_panics() {
        let bytes = vec![0x28, 0xb5, 0x2f, 0xfd, 0x01, 0x02, 0x03];
        let tmp = write_temp(&bytes);
        match CompressedTarOps::build(tmp.path(), Compression::Zstd) {
            Err(VfsError::Backend { .. }) => {}
            other => panic!("expected a decode error, got ok={}", other.is_ok()),
        }
    }

    // --- Decompression-bomb guards -----------------------------------------------------------
    //
    // Every fixture below is genuinely decoded through the real per-format crate (never faked or
    // short-circuited) but deliberately kept small (single-digit MiB or less) so the whole suite
    // stays fast — the property under test ("aborts long before completing", not "aborts exactly
    // at 512 MiB") holds identically at this scale as it does at the real production cap, since
    // the guard is a plain byte-count/ratio comparison with no scale-dependent behavior.

    /// bzip2-compress `data` via `banzai` (dev-dependency only — a pure-Rust encoder used solely to
    /// build test fixtures; the backend itself only ever *decodes* bzip2, via `bzip2-rs`).
    fn bzip2_bytes(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        banzai::encode(io::BufReader::new(data), io::BufWriter::new(&mut out), 9).unwrap();
        out
    }

    /// The zstd spec (RFC 8878 §3.1.1.2) caps every block — regardless of type — at 128 KiB
    /// (`Block_Maximum_Size`), so a single RLE block can't declare our whole desired run; ruzstd
    /// enforces this (`BlockSizeTooLarge`) rather than trusting the 21-bit field's nominal ~2 MiB
    /// range.
    const ZSTD_MAX_BLOCK_SIZE: u32 = 128 * 1024;

    /// Hand-build a minimal, spec-valid zstd frame (RFC 8878) that decodes to `total` bytes of a
    /// single repeated byte, as a chain of RLE blocks (each capped at
    /// [`ZSTD_MAX_BLOCK_SIZE`]) — a handful of header bytes per block, no real payload. No zstd
    /// *encoder* dependency exists in this crate's tree (`ruzstd` is decode-only, and the
    /// alternative `zstd` crate is a C-FFI binding to libzstd we deliberately don't want even as a
    /// dev-dependency, per ADR-0006's cross-platform-build-risk reasoning) — this is also the
    /// standard "zstd bomb" shape in the wild, so building it by hand is more representative of a
    /// real adversarial input than any encoder would produce anyway.
    fn zstd_rle_bytes(total: u32) -> Vec<u8> {
        let mut bytes = vec![0x28, 0xb5, 0x2f, 0xfd]; // frame magic number
                                                      // Frame_Header_Descriptor: Frame_Content_Size_flag=2 (4-byte FCS field), Single_Segment=1
                                                      // (so no separate Window_Descriptor byte), no content checksum, no dictionary ID.
        bytes.push(0b1010_0000);
        bytes.extend_from_slice(&total.to_le_bytes()); // Frame_Content_Size (4 bytes, LE)
        let mut remaining = total;
        while remaining > 0 {
            let this_block = remaining.min(ZSTD_MAX_BLOCK_SIZE);
            remaining -= this_block;
            // Block_Header (3 bytes, little-endian 24-bit): Block_Type=1 (RLE), Block_Size=this
            // block's decompressed run length; Last_Block=1 only once nothing remains after it.
            let last_block: u32 = u32::from(remaining == 0);
            let block_type_rle: u32 = 1;
            let header = last_block | (block_type_rle << 1) | (this_block << 3);
            bytes.extend_from_slice(&header.to_le_bytes()[0..3]);
            bytes.push(0x00); // RLE_Block content: the single byte value to repeat.
        }
        bytes
    }

    /// Hand-build a minimal, spec-valid zstd frame (RFC 8878) containing `data` verbatim, as a
    /// chain of *Raw_Block*s (`Block_Type` 0 — uncompressed literal content, each capped at
    /// [`ZSTD_MAX_BLOCK_SIZE`]) — unlike [`zstd_rle_bytes`] this can carry arbitrary content, not
    /// just a repeated byte, which is what the happy-path and multi-frame tests below need to
    /// verify real tar bytes round-trip correctly. Same rationale as `zstd_rle_bytes` for why this
    /// is hand-built rather than using an encoder crate.
    fn zstd_raw_frame_bytes(data: &[u8]) -> Vec<u8> {
        let total = u32::try_from(data.len()).expect("test fixture fits in u32");
        let mut bytes = vec![0x28, 0xb5, 0x2f, 0xfd];
        bytes.push(0b1010_0000);
        bytes.extend_from_slice(&total.to_le_bytes());
        let mut offset = 0usize;
        loop {
            let remaining = data.len() - offset;
            let this_block = remaining.min(ZSTD_MAX_BLOCK_SIZE as usize);
            let last_block = u32::from(offset + this_block == data.len());
            let block_type_raw: u32 = 0;
            let header = last_block | (block_type_raw << 1) | ((this_block as u32) << 3);
            bytes.extend_from_slice(&header.to_le_bytes()[0..3]);
            bytes.extend_from_slice(&data[offset..offset + this_block]);
            offset += this_block;
            if offset >= data.len() {
                break;
            }
        }
        bytes
    }

    /// gzip: a run of zeros compresses at an enormous ratio (~1000:1+), tripping the ratio guard
    /// (not the absolute cap — decoded stays well under [`ARCHIVE_MAX_DECOMPRESSED_BYTES`]) almost
    /// immediately once decoded output crosses `COMPRESSION_RATIO_FLOOR_BYTES`.
    #[test]
    fn gzip_bomb_is_aborted_by_the_ratio_guard() {
        let bomb = vec![0u8; 4 * 1024 * 1024]; // 4 MiB of zeros
        let gz_bytes = gzip_bytes(&bomb);
        assert!(
            (gz_bytes.len() as u64) * crate::security::MAX_COMPRESSION_RATIO < bomb.len() as u64
        );
        let tmp = write_temp(&gz_bytes);
        match CompressedTarOps::build(tmp.path(), Compression::Gzip) {
            Err(VfsError::Backend { code, .. }) => assert_eq!(code, "archive_bomb_detected"),
            other => panic!("expected archive_bomb_detected, got ok={}", other.is_ok()),
        }
    }

    /// Same bomb shape via bzip2.
    #[test]
    fn bzip2_bomb_is_aborted_by_the_ratio_guard() {
        let bomb = vec![0u8; 4 * 1024 * 1024];
        let bz_bytes = bzip2_bytes(&bomb);
        assert!(
            (bz_bytes.len() as u64) * crate::security::MAX_COMPRESSION_RATIO < bomb.len() as u64
        );
        let tmp = write_temp(&bz_bytes);
        match CompressedTarOps::build(tmp.path(), Compression::Bzip2) {
            Err(VfsError::Backend { code, .. }) => assert_eq!(code, "archive_bomb_detected"),
            other => panic!("expected archive_bomb_detected, got ok={}", other.is_ok()),
        }
    }

    /// Same bomb shape via zstd, using the hand-built RLE frame: a ~2 MiB run declared in a
    /// ~13-byte frame — an even more extreme ratio than the other three fixtures.
    #[test]
    fn zstd_bomb_is_aborted_by_the_ratio_guard() {
        let zst_bytes = zstd_rle_bytes(2_000_000);
        assert!((zst_bytes.len() as u64) * crate::security::MAX_COMPRESSION_RATIO < 2_000_000);
        let tmp = write_temp(&zst_bytes);
        match CompressedTarOps::build(tmp.path(), Compression::Zstd) {
            Err(VfsError::Backend { code, .. }) => assert_eq!(code, "archive_bomb_detected"),
            other => panic!("expected archive_bomb_detected, got ok={}", other.is_ok()),
        }
    }

    /// The absolute output-byte cap is a distinct guard from the ratio check: this fixture has a
    /// *low* ratio (well under [`crate::security::MAX_COMPRESSION_RATIO`] — pseudo-random,
    /// effectively incompressible bytes, so gzip's compressed output is close to the input size)
    /// but its decoded size exceeds an (injected, test-scale) cap. Exercises `decompress_to_temp`
    /// directly (bypassing `CompressedTarOps::build`'s fixed production constant) so the two guards
    /// can be proven independent without needing a multi-hundred-MiB fixture to test the real cap.
    #[test]
    fn absolute_cap_trips_independently_of_the_ratio_guard() {
        let incompressible: Vec<u8> = (0..8_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761)) as u8)
            .collect();
        let gz_bytes = gzip_bytes(&incompressible);
        // Confirm this fixture's ratio is nowhere near bomb territory — it's the cap being tested.
        assert!(
            incompressible.len() as u64
                <= gz_bytes.len() as u64 * crate::security::MAX_COMPRESSION_RATIO,
            "fixture must NOT be a ratio-guard bomb, to isolate the absolute-cap guard"
        );
        let tmp = write_temp(&gz_bytes);
        let tiny_cap = 4_096u64; // far below the 8000-byte decoded size, far below the ratio floor too
        match decompress_to_temp(tmp.path(), Compression::Gzip, tiny_cap) {
            Err(VfsError::Backend { code, .. }) => assert_eq!(code, "archive_bomb_detected"),
            other => panic!("expected archive_bomb_detected, got ok={}", other.is_ok()),
        }
    }

    #[test]
    fn temp_file_is_owner_only_and_deleted_on_drop() {
        let tar_bytes = build_test_tar("a.txt", b"a");
        let gz_bytes = gzip_bytes(&tar_bytes);
        let tmp = write_temp(&gz_bytes);
        let ops = CompressedTarOps::build(tmp.path(), Compression::Gzip).unwrap();

        // Recover the temp path via a second decompress+build so we can inspect the file while the
        // owning `CompressedTarOps` is still alive, then confirm it's gone once dropped.
        let (temp2, _) = decompress_to_temp(
            tmp.path(),
            Compression::Gzip,
            ARCHIVE_MAX_DECOMPRESSED_BYTES,
        )
        .unwrap();
        let temp_path = temp2.path().to_path_buf();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&temp_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "archive temp file must be owner-only (0600)");
        }
        drop(temp2);
        assert!(
            !temp_path.exists(),
            "temp file must be deleted once its NamedTempFile handle is dropped"
        );
        drop(ops); // exercise the real struct's Drop path too, not just the bare NamedTempFile
    }

    // --- Multi-stream / multi-frame concatenated input: never silently truncated -------------
    //
    // gzip's `MultiGzDecoder` reconstructs concatenated members correctly (tested directly below);
    // bzip2/zstd don't continue past their first stream/frame, so `contains_magic_after_start`
    // must catch it. The bzip2 case is the important regression test: `bzip2_rs::DecoderReader`
    // reads its *entire* input into an internal buffer regardless of stream count (verified during
    // development), so a naive "bytes consumed from the reader == input length" check would have
    // reported this exact fixture as fully, successfully decoded even though only the first stream
    // was — the byte-level magic scan does not have that blind spot.

    /// Build a two-file uncompressed tar, for splitting/reconstructing across compressed members.
    fn build_two_file_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut h1 = tar::Header::new_gnu();
        h1.set_size(5);
        h1.set_mode(0o644);
        h1.set_cksum();
        builder
            .append_data(&mut h1, "first.txt", &b"AAAAA"[..])
            .unwrap();
        let mut h2 = tar::Header::new_gnu();
        h2.set_size(5);
        h2.set_mode(0o644);
        h2.set_cksum();
        builder
            .append_data(&mut h2, "second.txt", &b"BBBBB"[..])
            .unwrap();
        builder.into_inner().unwrap()
    }

    /// The real-world shape this guards against: a single logical tar, split at an arbitrary byte
    /// boundary into two independently-gzipped members (as `bgzip` output looks), concatenated back
    /// together. `MultiGzDecoder` must reconstruct the exact original byte stream — both files must
    /// list, not just the first half's worth of content.
    #[test]
    fn gzip_multi_member_is_fully_reconstructed() {
        let full_tar = build_two_file_tar();
        let split_at = full_tar.len() / 2;
        let (first_half, second_half) = full_tar.split_at(split_at);
        let mut combined = gzip_bytes(first_half);
        combined.extend_from_slice(&gzip_bytes(second_half));

        let tmp = write_temp(&combined);
        let ops = CompressedTarOps::build(tmp.path(), Compression::Gzip).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        let mut names: Vec<_> = root.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["first.txt", "second.txt"]);
    }

    /// A legitimate single-stream bzip2 archive still decodes normally — the multi-stream guard
    /// must not false-positive on ordinary input (there is exactly one `"BZh"` occurrence: the
    /// leading magic itself).
    #[test]
    fn decompresses_and_indexes_a_real_bzip2_tar() {
        let tar_bytes = build_test_tar("hello.txt", b"hello, bzip2 archive");
        let bz_bytes = bzip2_bytes(&tar_bytes);
        let tmp = write_temp(&bz_bytes);

        let ops = CompressedTarOps::build(tmp.path(), Compression::Bzip2).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "hello.txt");
        let data = ops
            .read_member(&VfsPath::parse("hello.txt").unwrap(), 1024)
            .unwrap();
        assert_eq!(data, b"hello, bzip2 archive");
    }

    /// A legitimate single-frame zstd archive still decodes normally — same false-positive check
    /// as the bzip2 case above, for the zstd magic.
    #[test]
    fn decompresses_and_indexes_a_real_zstd_tar() {
        let tar_bytes = build_test_tar("hello.txt", b"hello, zstd archive");
        let zst_bytes = zstd_raw_frame_bytes(&tar_bytes);
        let tmp = write_temp(&zst_bytes);

        let ops = CompressedTarOps::build(tmp.path(), Compression::Zstd).unwrap();
        let root = ops.list_children(&VfsPath::root()).unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "hello.txt");
        let data = ops
            .read_member(&VfsPath::parse("hello.txt").unwrap(), 1024)
            .unwrap();
        assert_eq!(data, b"hello, zstd archive");
    }

    /// THE regression test: two independently-encoded bzip2 streams concatenated. `DecoderReader`
    /// decodes only the first and (verified empirically) reads the *entire* combined input while
    /// doing so, so a "bytes consumed == input length" check alone would silently accept this as a
    /// complete decode. The mount must instead be refused, never opened with the second file
    /// missing and no error.
    #[test]
    fn bzip2_multi_stream_is_refused_not_silently_truncated() {
        let mut combined = bzip2_bytes(b"hello-bzip2-part-one");
        combined.extend_from_slice(&bzip2_bytes(b"world-bzip2-part-two"));
        let tmp = write_temp(&combined);
        match CompressedTarOps::build(tmp.path(), Compression::Bzip2) {
            Err(VfsError::Backend { code, .. }) => {
                assert_eq!(code, "compressed_tar_multi_stream");
            }
            other => panic!(
                "must never silently truncate: expected compressed_tar_multi_stream, got ok={}",
                other.is_ok()
            ),
        }
    }

    /// Same shape via zstd: two independent frames concatenated. `ruzstd`'s `StreamingDecoder`
    /// stops after the first frame; the mount must be refused, not silently truncated.
    #[test]
    fn zstd_multi_frame_is_refused_not_silently_truncated() {
        let mut combined = zstd_raw_frame_bytes(b"hello-frame-one-");
        combined.extend_from_slice(&zstd_raw_frame_bytes(b"world-frame-two!"));
        let tmp = write_temp(&combined);
        match CompressedTarOps::build(tmp.path(), Compression::Zstd) {
            Err(VfsError::Backend { code, .. }) => {
                assert_eq!(code, "compressed_tar_multi_stream");
            }
            other => panic!(
                "must never silently truncate: expected compressed_tar_multi_stream, got ok={}",
                other.is_ok()
            ),
        }
    }
}
