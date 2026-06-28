//! The [`Vfs`] trait — the abstraction every backend implements — and its supporting types.

use crate::action::{ActionCtx, ActionDescriptor, ActionId, ActionOutcome};
use crate::error::VfsError;
use crate::handle::{ReadHandle, WriteHandle};
use cairn_types::{Caps, ConnectionId, Entry, Scheme, VfsPath};
use futures::stream::BoxStream;
use smol_str::SmolStr;

/// Provides a backend's capability set, optionally refined per path (k8s/docker vary by depth).
pub trait CapabilityProvider {
    /// The backend-wide baseline capabilities.
    fn caps(&self) -> Caps;
    /// Capabilities at a specific path (defaults to the baseline).
    fn caps_at(&self, _path: &VfsPath) -> Caps {
        self.caps()
    }
}

/// Options controlling a listing.
#[derive(Debug, Clone, Default)]
pub struct ListOpts {
    /// Include hidden entries (dotfiles, etc.).
    pub all: bool,
}

/// An opaque pagination cursor (e.g. an object-store continuation token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCursor(pub SmolStr);

/// One page of a listing stream.
#[derive(Debug, Clone, Default)]
pub struct ListPage {
    /// Entries in this page.
    pub entries: Vec<Entry>,
    /// Cursor to fetch the next page, if any.
    pub cursor: Option<PageCursor>,
    /// Whether this is the final page.
    pub done: bool,
}

/// A byte range for ranged reads.
#[derive(Debug, Clone, Copy)]
pub struct ByteRange {
    /// Starting byte offset.
    pub offset: u64,
    /// Number of bytes, or `None` for "to end".
    pub len: Option<u64>,
}

/// Apply a [`ByteRange`] to an in-memory buffer, clamping to the buffer's bounds.
///
/// Used by backends that buffer a whole object and slice in memory (no transport-level seek).
/// The arithmetic is saturating, so an arbitrary caller-controlled `offset`/`len` (e.g. `u64::MAX`)
/// clamps to an empty slice instead of overflowing or panicking.
#[must_use]
pub fn apply_byte_range(data: &[u8], range: ByteRange) -> &[u8] {
    let total = data.len() as u64;
    let start = range.offset.min(total) as usize;
    let end = match range.len {
        Some(l) => range.offset.saturating_add(l).min(total) as usize,
        None => data.len(),
    };
    &data[start..end]
}

/// Build an absolute path from VFS path segments below some root (e.g. a container/pod filesystem).
///
/// An empty slice yields `"/"` (the root itself). Segments have already passed [`VfsPath`] parsing,
/// which rejects `..` and control characters, so the result cannot traverse out of the root. Shared
/// by the container backends (Docker, Kubernetes) that map a subtree onto a remote filesystem.
#[must_use]
pub fn join_abs_path(rest: &[&str]) -> String {
    if rest.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", rest.join("/"))
    }
}

/// Options controlling a write.
#[derive(Debug, Clone, Default)]
pub struct WriteOpts {
    /// Overwrite an existing entry if present.
    pub overwrite: bool,
    /// Approximate total size, if known (lets the backend pick single-shot vs multipart).
    pub size_hint: Option<u64>,
}

/// Whether a removal should recurse into directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recurse {
    /// Remove a single entry only; fail on a non-empty directory.
    No,
    /// Remove recursively.
    Yes,
}

/// The virtual filesystem trait implemented by every backend.
///
/// Held as `Arc<dyn Vfs>`, so it must be object-safe — hence `#[async_trait]` (boxed futures) rather
/// than native `async fn` in traits. `list` is intentionally **not** `async`: it returns a stream
/// synchronously (an `async fn` returning a borrowing stream is an unsatisfiable lifetime). Mutations
/// default to [`VfsError::Unsupported`] so a backend only implements what it actually supports.
#[async_trait::async_trait]
pub trait Vfs: CapabilityProvider + Send + Sync + 'static {
    /// The URI scheme this backend serves.
    fn scheme(&self) -> Scheme;

    /// The connection this backend instance belongs to.
    fn connection(&self) -> ConnectionId;

    /// List a directory as a stream of pages. The first page should arrive before the listing
    /// completes so the UI can paint immediately.
    ///
    /// The returned stream borrows only `&self`, not `dir`: implementations consume `dir`
    /// synchronously (cloning what they need into the stream), so callers may pass a temporary.
    fn list<'a>(
        &'a self,
        dir: &VfsPath,
        opts: ListOpts,
    ) -> BoxStream<'a, Result<ListPage, VfsError>>;

    /// Fetch metadata for a single path.
    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError>;

    /// Open a streaming reader, optionally for a byte range (requires [`Caps::RANDOM_READ`]).
    async fn open_read(
        &self,
        path: &VfsPath,
        range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError>;

    /// Open a streaming writer.
    async fn open_write(&self, path: &VfsPath, opts: WriteOpts) -> Result<WriteHandle, VfsError>;

    /// Create a directory. Defaults to unsupported.
    async fn create_dir(&self, _path: &VfsPath) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::CREATE_DIR))
    }

    /// Remove an entry. Defaults to unsupported.
    async fn remove(&self, _path: &VfsPath, _recurse: Recurse) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::DELETE))
    }

    /// Rename/move an entry within the backend. Defaults to unsupported.
    async fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::RENAME))
    }

    /// Set Unix permissions. Defaults to unsupported.
    async fn set_perms(
        &self,
        _path: &VfsPath,
        _perms: cairn_types::UnixPerms,
    ) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::CHMOD))
    }

    /// Server-side copy within this backend (e.g. S3 `CopyObject`). Defaults to unsupported, which
    /// makes the transfer engine fall back to a stream-through copy.
    async fn copy_within(&self, _from: &VfsPath, _to: &VfsPath) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::COPY_SERVER))
    }

    /// Discover backend-specific actions available at a path. Defaults to none.
    fn actions_at(&self, _path: &VfsPath) -> Vec<ActionDescriptor> {
        Vec::new()
    }

    /// Invoke a discovered action. Defaults to unsupported.
    async fn invoke(&self, _action: ActionId, _ctx: ActionCtx) -> Result<ActionOutcome, VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_byte_range, join_abs_path, ByteRange};

    #[test]
    fn join_abs_path_builds_rooted_paths() {
        assert_eq!(join_abs_path(&[]), "/");
        assert_eq!(join_abs_path(&["etc"]), "/etc");
        assert_eq!(join_abs_path(&["etc", "hostname"]), "/etc/hostname");
    }

    #[test]
    fn byte_range_clamps_and_saturates() {
        let data = b"web-1\n"; // 6 bytes
                               // Normal sub-range.
        assert_eq!(
            apply_byte_range(
                data,
                ByteRange {
                    offset: 0,
                    len: Some(3)
                }
            ),
            b"web"
        );
        // Offset past EOF -> empty.
        assert_eq!(
            apply_byte_range(
                data,
                ByteRange {
                    offset: 99,
                    len: Some(3)
                }
            ),
            b""
        );
        // len running past EOF clamps to the tail.
        assert_eq!(
            apply_byte_range(
                data,
                ByteRange {
                    offset: 4,
                    len: Some(100)
                }
            ),
            b"1\n"
        );
        // None len reads to the end.
        assert_eq!(
            apply_byte_range(
                data,
                ByteRange {
                    offset: 4,
                    len: None
                }
            ),
            b"1\n"
        );
        // Pathological offset+len must not overflow/panic -> empty.
        assert_eq!(
            apply_byte_range(
                data,
                ByteRange {
                    offset: u64::MAX,
                    len: Some(1)
                }
            ),
            b""
        );
    }
}
