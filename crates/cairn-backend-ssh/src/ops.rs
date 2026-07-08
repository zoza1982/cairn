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
    /// Fetch metadata for a path.
    async fn stat(&self, path: &str) -> Result<RemoteMeta, VfsError>;
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
    }

    /// In-memory SFTP transport for tests.
    pub(crate) struct MockSftp {
        nodes: Mutex<BTreeMap<String, Node>>,
        /// One-shot: the next `rename` whose destination equals this path fails (simulates a
        /// mid-operation server/network error), so the overwrite restore-on-failure path is testable.
        fail_rename_to: Mutex<Option<String>>,
    }

    impl MockSftp {
        pub(crate) fn new() -> Self {
            let mut nodes = BTreeMap::new();
            nodes.insert("/".to_owned(), Node::Dir);
            Self {
                nodes: Mutex::new(nodes),
                fail_rename_to: Mutex::new(None),
            }
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

        fn not_found(path: &str) -> VfsError {
            VfsError::NotFound(VfsPath::parse(path).unwrap_or_else(|_| VfsPath::root()))
        }
    }

    #[async_trait]
    impl SftpOps for MockSftp {
        async fn read_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, VfsError> {
            let nodes = self.nodes.lock().unwrap();
            if !matches!(nodes.get(path), Some(Node::Dir)) {
                return Err(Self::not_found(path));
            }
            let prefix = if path == "/" {
                "/".to_owned()
            } else {
                format!("{path}/")
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
                let (kind, size, mode) = match node {
                    Node::Dir => (EntryKind::Dir, None, Some(0o040_755)),
                    Node::File(b) => (EntryKind::File, Some(b.len() as u64), Some(0o100_644)),
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
                None => Err(Self::not_found(path)),
            }
        }

        async fn read(&self, path: &str, range: Option<ByteRange>) -> Result<Vec<u8>, VfsError> {
            let nodes = self.nodes.lock().unwrap();
            let Some(Node::File(b)) = nodes.get(path) else {
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
            self.nodes.lock().unwrap().remove(path);
            Ok(())
        }

        async fn remove_dir(&self, path: &str) -> Result<(), VfsError> {
            self.nodes.lock().unwrap().remove(path);
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
