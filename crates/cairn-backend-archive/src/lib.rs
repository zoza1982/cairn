//! Read-only archive backend for Cairn: browse a local `.tar`, `.zip`, or compressed-tar file as a
//! directory tree (RFC-0013 P4/P5, `docs/adr/0012-archive-mount-model.md`,
//! `docs/adr/0013-compressed-tar-decoder-selection.md`).
//!
//! [`ArchiveVfs`] implements [`Vfs`](cairn_vfs::Vfs) over an `ArchiveOps` built by
//! [`ArchiveVfs::open`], which sniffs the file's magic bytes (never its extension) to pick the tar,
//! zip, or compressed-tar indexer. All three (`tar_backend::TarOps`, `zip_backend::ZipOps`,
//! `compressed_tar::CompressedTarOps`) build their whole directory tree once, up front, inside
//! `tokio::task::spawn_blocking` (the `tar`/`zip` crates, and every compressed-tar decoder, are
//! sync-only — there is no maintained async fork; `tokio-tar` is abandoned and is deliberately not
//! used here) — see the crate-level rustdoc on `ArchiveOps` for why the trait is object-safe
//! (`dyn`) rather than generic like `cairn-backend-ssh`'s `SftpOps`.
//!
//! **Scope:** local archives only (an archive on a remote backend must be copied to a local pane
//! first — see `AppEffect::MountArchive`'s runner in `crates/cairn/src/app.rs`); `.tar`, `.zip`,
//! and — since RFC-0013 P5 — compressed tar (`.tgz`/`.tar.gz`, `.tbz2`/`.tar.bz2`, `.txz`/`.tar.xz`,
//! `.tzst`/`.tar.zst`, decompressed once to a private temp file and then indexed via the same
//! `tar_backend::TarOps` as a plain tar — see `compressed_tar`); no writing back into the archive,
//! and no auto-descent into a nested archive member (it is shown as a plain file). See
//! `docs/rfcs/0013-archive-backend.md` for the full design and what remains deferred.
//!
//! Every member path is validated (traversal/absolute/UNC/drive-letter/control-char rejection),
//! every read is bounded (per-member and cumulative-session byte caps, entry-count cap, a zip
//! compression-ratio guard), and symlink/hardlink members are presented inert — never followed —
//! per the threat model in `docs/rfcs/0013-archive-backend.md` §Security.

mod compressed_tar;
mod index;
mod security;
mod tar_backend;
mod vfs;
mod zip_backend;

pub use vfs::ArchiveVfs;

use cairn_types::{Entry, VfsPath};
use cairn_vfs::VfsError;

/// The subset of tar/zip-specific indexing and reading behavior [`ArchiveVfs`] needs, implemented
/// once for tar (`tar_backend::TarOps`) and once for zip (`zip_backend::ZipOps`) so
/// `impl Vfs for ArchiveVfs` (in `vfs.rs`) is written exactly once.
///
/// Object-safe (`dyn ArchiveOps`) rather than a generic type parameter (contrast
/// `cairn-backend-ssh`'s `SftpVfs<O: SftpOps>`): which concrete implementation to use is decided at
/// runtime from the file's magic bytes inside [`ArchiveVfs::open`], not at compile time, so a trait
/// object is the natural fit — there is no compile-time call site that names a concrete `TarOps` or
/// `ZipOps` type.
///
/// Every method here is synchronous: both `tar` and `zip` are sync-only crates, so
/// [`ArchiveVfs`]'s async `Vfs` methods invoke these inside `tokio::task::spawn_blocking` rather
/// than making the trait itself `async`.
pub(crate) trait ArchiveOps: Send + Sync + 'static {
    /// Immediate children of `dir` (already-validated members plus synthesized ancestor
    /// directories). `dir` must be a known directory (or the archive root); otherwise
    /// [`VfsError::NotFound`].
    fn list_children(&self, dir: &VfsPath) -> Result<Vec<Entry>, VfsError>;

    /// Metadata for one non-root path. The caller (`ArchiveVfs::stat`) handles the root itself.
    fn entry_meta(&self, path: &VfsPath) -> Result<Entry, VfsError>;

    /// Read up to `cap` bytes of `path`'s content, from the start of the member. `cap` is already
    /// the minimum of the per-member cap and the caller's remaining session byte budget — this
    /// method must never read more than `cap` bytes regardless of what the archive's own metadata
    /// declared for the member's size (a declared size is untrusted input).
    ///
    /// Implementations return [`VfsError::Unsupported`] for a directory or a symlink/hardlink
    /// (never read a link's target as if it were the member's content) and
    /// [`VfsError::NotFound`] for an unknown path.
    fn read_member(&self, path: &VfsPath, cap: u64) -> Result<Vec<u8>, VfsError>;
}
