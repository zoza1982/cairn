//! The [`SftpOps`] transport trait that [`SftpVfs`](crate::SftpVfs) is built on, plus an in-memory
//! mock for hermetic tests.

use async_trait::async_trait;
use cairn_types::EntryKind;
use cairn_vfs::{ByteRange, VfsError};
use std::time::SystemTime;

/// One remote directory entry (transport-level, before mapping to a `Vfs` [`Entry`](cairn_types::Entry)).
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    /// Leaf name.
    pub name: String,
    /// Entry kind.
    pub kind: EntryKind,
    /// Size in bytes (files).
    pub size: Option<u64>,
    /// Last-modified time.
    pub modified: Option<SystemTime>,
    /// Raw Unix mode bits (including the file-type `S_IFMT` bits), as reported by the server. `None`
    /// when the server omits them. Mapped to `Entry.perms` so a remote pane shows `drwxr-xr-x` like
    /// local. NOTE for a future SFTP `chmod`: since `Entry.perms` now carries the type bits, any
    /// `set_perms` must mask them off (`mode & 0o7777`) before a `SETSTAT` — the backend has no
    /// `Caps::CHMOD` today, so nothing consumes them yet.
    pub mode: Option<u32>,
}

/// Remote metadata for a single path.
#[derive(Debug, Clone)]
pub struct RemoteMeta {
    /// Entry kind.
    pub kind: EntryKind,
    /// Size in bytes (files).
    pub size: Option<u64>,
    /// Last-modified time.
    pub modified: Option<SystemTime>,
    /// Raw Unix mode bits (including the file-type bits); see [`RemoteEntry::mode`].
    pub mode: Option<u32>,
}

/// The minimal SFTP transport surface the backend needs. Implemented by the real `russh-sftp`
/// adapter and by the in-memory test mock.
#[async_trait]
pub trait SftpOps: Send + Sync + 'static {
    /// List a directory's direct children.
    async fn read_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, VfsError>;
    /// Fetch metadata for a path (follows symlinks, `SSH_FXP_STAT`).
    async fn stat(&self, path: &str) -> Result<RemoteMeta, VfsError>;
    /// Fetch metadata for a path **without following symlinks** (`SSH_FXP_LSTAT`). Used to classify
    /// entries for deletion: a symlink-to-directory must be treated as a symlink (and unlinked),
    /// never followed into and recursed — otherwise a delete would destroy data *outside* the
    /// requested tree, and a symlink cycle would loop forever.
    async fn lstat(&self, path: &str) -> Result<RemoteMeta, VfsError>;
    /// Read a file's bytes, optionally a range.
    async fn read(&self, path: &str, range: Option<ByteRange>) -> Result<Vec<u8>, VfsError>;
    /// Write (create/truncate) a file.
    async fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError>;
    /// Remove a file.
    async fn remove_file(&self, path: &str) -> Result<(), VfsError>;
    /// Remove an empty directory.
    async fn remove_dir(&self, path: &str) -> Result<(), VfsError>;
    /// Create a directory.
    async fn create_dir(&self, path: &str) -> Result<(), VfsError>;
    /// Rename/move a path.
    async fn rename(&self, from: &str, to: &str) -> Result<(), VfsError>;
}

#[cfg(test)]
pub(crate) mod mock {
    use super::{RemoteEntry, RemoteMeta, SftpOps};
    use async_trait::async_trait;
    use cairn_types::{EntryKind, VfsPath};
    use cairn_vfs::{ByteRange, VfsError};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    enum Node {
        Dir,
        File(Vec<u8>),
        /// A symbolic link to another path (which may or may not exist / may itself be a link).
        Symlink(String),
    }

    /// In-memory SFTP transport for tests.
    pub(crate) struct MockSftp {
        nodes: Mutex<BTreeMap<String, Node>>,
        /// One-shot: the next `rename` whose destination equals this path fails (simulates a
        /// mid-operation server/network error), so the overwrite restore-on-failure path is testable.
        fail_rename_to: Mutex<Option<String>>,
        /// When set, `read_dir` reports every entry as a plain file with no mode bits — mimicking
        /// SFTP servers that omit the type/permission attrs in READDIR responses. `stat` still
        /// returns the true kind, so the backend's stat-fallback can recover it.
        hide_readdir_types: bool,
    }

    impl MockSftp {
        pub(crate) fn new() -> Self {
            let mut nodes = BTreeMap::new();
            nodes.insert("/".to_owned(), Node::Dir);
            Self {
                nodes: Mutex::new(nodes),
                fail_rename_to: Mutex::new(None),
                hide_readdir_types: false,
            }
        }

        /// Simulate a server that doesn't send type/permission bits in directory listings.
        #[must_use]
        pub(crate) fn hiding_readdir_types(mut self) -> Self {
            self.hide_readdir_types = true;
            self
        }

        /// Make the next `rename` with the given destination fail once.
        #[must_use]
        pub(crate) fn with_failing_rename_to(self, path: &str) -> Self {
            *self.fail_rename_to.lock().unwrap() = Some(path.to_owned());
            self
        }

        #[must_use]
        pub(crate) fn with_dir(self, path: &str) -> Self {
            self.nodes
                .lock()
                .unwrap()
                .insert(path.to_owned(), Node::Dir);
            self
        }

        #[must_use]
        pub(crate) fn with_file(self, path: &str, data: &[u8]) -> Self {
            self.nodes
                .lock()
                .unwrap()
                .insert(path.to_owned(), Node::File(data.to_vec()));
            self
        }

        /// Add a symlink at `path` pointing at `target` (an absolute path in this mock's namespace).
        #[must_use]
        pub(crate) fn with_symlink(self, path: &str, target: &str) -> Self {
            self.nodes
                .lock()
                .unwrap()
                .insert(path.to_owned(), Node::Symlink(target.to_owned()));
            self
        }

        fn not_found(path: &str) -> VfsError {
            VfsError::NotFound(VfsPath::parse(path).unwrap_or_else(|_| VfsPath::root()))
        }

        /// Follow symlinks from `path` to the final non-link node key, mimicking a server that
        /// resolves links on `stat`/`opendir`/`open`. Returns `None` for a dangling or cyclic link
        /// (a bounded hop count stands in for `ELOOP`), so callers surface not-found rather than spin.
        fn resolve(nodes: &BTreeMap<String, Node>, path: &str) -> Option<String> {
            let mut cur = path.to_owned();
            for _ in 0..40 {
                match nodes.get(&cur) {
                    Some(Node::Symlink(target)) => cur = target.clone(),
                    Some(_) => return Some(cur),
                    None => return None,
                }
            }
            None
        }
    }

    #[async_trait]
    impl SftpOps for MockSftp {
        async fn read_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, VfsError> {
            let nodes = self.nodes.lock().unwrap();
            // Opening a directory follows symlinks (a symlink-to-dir lists the target's children).
            let dir_key = Self::resolve(&nodes, path).ok_or_else(|| Self::not_found(path))?;
            if !matches!(nodes.get(&dir_key), Some(Node::Dir)) {
                return Err(Self::not_found(path));
            }
            let prefix = if dir_key == "/" {
                "/".to_owned()
            } else {
                format!("{dir_key}/")
            };
            let mut out = Vec::new();
            for (key, node) in nodes.iter() {
                if key == "/" {
                    continue;
                }
                let Some(rest) = key.strip_prefix(&prefix) else {
                    continue;
                };
                if rest.is_empty() || rest.contains('/') {
                    continue;
                }
                // READDIR is lstat-like: an entry that is itself a symlink reports as a symlink, not
                // its target's kind.
                let (kind, size, mode) = match node {
                    Node::Dir => (EntryKind::Dir, None, Some(0o040_755)),
                    Node::File(b) => (EntryKind::File, Some(b.len() as u64), Some(0o100_644)),
                    Node::Symlink(_) => (EntryKind::Symlink, None, Some(0o120_777)),
                };
                // A type-less server reports everything as a plain file with no mode bits.
                let (kind, mode) = if self.hide_readdir_types {
                    (EntryKind::File, None)
                } else {
                    (kind, mode)
                };
                out.push(RemoteEntry {
                    name: rest.to_owned(),
                    kind,
                    size,
                    modified: None,
                    mode,
                });
            }
            Ok(out)
        }

        async fn stat(&self, path: &str) -> Result<RemoteMeta, VfsError> {
            // `stat` follows symlinks: resolve to the final target, then report the target's kind.
            let nodes = self.nodes.lock().unwrap();
            let resolved = Self::resolve(&nodes, path).ok_or_else(|| Self::not_found(path))?;
            match nodes.get(&resolved) {
                Some(Node::Dir) => Ok(RemoteMeta {
                    kind: EntryKind::Dir,
                    size: None,
                    modified: None,
                    mode: Some(0o040_755),
                }),
                Some(Node::File(b)) => Ok(RemoteMeta {
                    kind: EntryKind::File,
                    size: Some(b.len() as u64),
                    modified: None,
                    mode: Some(0o100_644),
                }),
                // `resolve` only returns a non-symlink key; a missing target is not-found.
                _ => Err(Self::not_found(path)),
            }
        }

        async fn lstat(&self, path: &str) -> Result<RemoteMeta, VfsError> {
            // `lstat` does not follow symlinks: a symlink reports as a symlink.
            let nodes = self.nodes.lock().unwrap();
            match nodes.get(path) {
                Some(Node::Dir) => Ok(RemoteMeta {
                    kind: EntryKind::Dir,
                    size: None,
                    modified: None,
                    mode: Some(0o040_755),
                }),
                Some(Node::File(b)) => Ok(RemoteMeta {
                    kind: EntryKind::File,
                    size: Some(b.len() as u64),
                    modified: None,
                    mode: Some(0o100_644),
                }),
                Some(Node::Symlink(_)) => Ok(RemoteMeta {
                    kind: EntryKind::Symlink,
                    size: None,
                    modified: None,
                    mode: Some(0o120_777),
                }),
                None => Err(Self::not_found(path)),
            }
        }

        async fn read(&self, path: &str, range: Option<ByteRange>) -> Result<Vec<u8>, VfsError> {
            let nodes = self.nodes.lock().unwrap();
            let resolved = Self::resolve(&nodes, path).ok_or_else(|| Self::not_found(path))?;
            let Some(Node::File(b)) = nodes.get(&resolved) else {
                return Err(Self::not_found(path));
            };
            Ok(match range {
                None => b.clone(),
                Some(r) => cairn_vfs::apply_byte_range(b, r).to_vec(),
            })
        }

        async fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
            self.nodes
                .lock()
                .unwrap()
                .insert(path.to_owned(), Node::File(data.to_vec()));
            Ok(())
        }

        async fn remove_file(&self, path: &str) -> Result<(), VfsError> {
            let mut nodes = self.nodes.lock().unwrap();
            match nodes.get(path) {
                Some(Node::Dir) => Err(VfsError::Backend {
                    code: "sftp".to_owned(),
                    msg: "remove: is a directory".to_owned(),
                    retryable: false,
                }),
                // A symlink is unlinked by name (its target is untouched), like SSH_FXP_REMOVE.
                Some(Node::File(_) | Node::Symlink(_)) => {
                    nodes.remove(path);
                    Ok(())
                }
                None => Err(Self::not_found(path)),
            }
        }

        async fn remove_dir(&self, path: &str) -> Result<(), VfsError> {
            let mut nodes = self.nodes.lock().unwrap();
            let prefix = format!("{path}/");
            if nodes.keys().any(|k| k.starts_with(&prefix)) {
                return Err(VfsError::Backend {
                    code: "sftp".to_owned(),
                    msg: "rmdir: directory not empty".to_owned(),
                    retryable: false,
                });
            }
            nodes.remove(path);
            Ok(())
        }

        async fn create_dir(&self, path: &str) -> Result<(), VfsError> {
            self.nodes
                .lock()
                .unwrap()
                .insert(path.to_owned(), Node::Dir);
            Ok(())
        }

        async fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
            // Injected one-shot failure (a simulated mid-operation drop), checked before mutating any
            // state so the destination is left untouched — exercises the overwrite restore path.
            {
                let mut fail = self.fail_rename_to.lock().unwrap();
                if fail.as_deref() == Some(to) {
                    *fail = None;
                    return Err(VfsError::Backend {
                        code: "sftp".to_owned(),
                        msg: "rename: simulated failure".to_owned(),
                        retryable: false,
                    });
                }
            }
            let mut nodes = self.nodes.lock().unwrap();
            // Faithfully model OpenSSH's sftp-server: a plain SSH_FXP_RENAME fails if the destination
            // already exists (no overwrite). `SftpVfs::rename` works around this by moving an existing
            // file destination aside first — this rejection is what proves that fix.
            if nodes.contains_key(to) {
                return Err(VfsError::Backend {
                    code: "sftp".to_owned(),
                    msg: "rename: destination already exists".to_owned(),
                    retryable: false,
                });
            }
            let node = nodes.remove(from).ok_or_else(|| Self::not_found(from))?;
            nodes.insert(to.to_owned(), node);
            Ok(())
        }
    }
}
