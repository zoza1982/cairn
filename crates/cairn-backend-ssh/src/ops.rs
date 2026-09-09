//! The [`SftpOps`] transport trait that [`SftpVfs`](crate::SftpVfs) is built on, plus an in-memory
//! mock for hermetic tests.

use async_trait::async_trait;
use cairn_types::EntryKind;
use cairn_vfs::{ByteRange, VfsError};
use std::time::SystemTime;
use tokio::io::AsyncRead;

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
    /// Open a file for a **streaming** read, already positioned and bounded per `range`. The
    /// returned reader must yield bytes as they arrive from the server (one `SSH_FXP_READ` per
    /// poll, or thereabouts) — never a whole-file buffer — so a download's progress reflects the
    /// wire, not a memcpy. `range.len == None` reads to end.
    async fn open_read(
        &self,
        path: &str,
        range: Option<ByteRange>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, VfsError>;
    /// Open a file for a **streaming** write (create/truncate is issued here, so the remote file
    /// exists — empty — once this returns). Each [`SftpWriteStream::write_all`] must hand its bytes
    /// to the transport before returning (the transport's own ack window is the backpressure);
    /// buffering the file to upload in `finish` is exactly the lie this seam exists to prevent.
    async fn open_write(&self, path: &str) -> Result<Box<dyn SftpWriteStream>, VfsError>;
    /// Remove a file.
    async fn remove_file(&self, path: &str) -> Result<(), VfsError>;
    /// Remove an empty directory.
    async fn remove_dir(&self, path: &str) -> Result<(), VfsError>;
    /// Create a directory.
    async fn create_dir(&self, path: &str) -> Result<(), VfsError>;
    /// Rename/move a path.
    async fn rename(&self, from: &str, to: &str) -> Result<(), VfsError>;
}

/// One open remote file being written through [`SftpOps::open_write`].
///
/// A small purpose-built trait rather than raw [`tokio::io::AsyncWrite`]: the mock stays a plain
/// `async fn` impl, `finish` can surface the SFTP `CLOSE` status (the commit point — a failed close is
/// a failed write), and `abort` owns the "remove the partial remote file" policy using the path it
/// was opened with, so the `Vfs` mapping never has to reach back into the transport to clean up.
#[async_trait]
pub trait SftpWriteStream: Send {
    /// Send one chunk. Returns once the bytes are queued on the transport — under `russh-sftp` that
    /// is a bounded window of in-flight `WRITE`s, so awaiting here *is* the backpressure.
    async fn write_all(&mut self, data: &[u8]) -> Result<(), VfsError>;
    /// Flush every outstanding write and `CLOSE` the handle, surfacing the server's status.
    async fn finish(self: Box<Self>) -> Result<(), VfsError>;
    /// Drop the handle uncommitted and best-effort remove the partial remote file. Never fails
    /// outward (mirrors [`cairn_vfs::WriteSink::abort`]).
    async fn abort(self: Box<Self>);
}

#[cfg(test)]
pub(crate) mod mock {
    use super::{RemoteEntry, RemoteMeta, SftpOps, SftpWriteStream};
    use async_trait::async_trait;
    use cairn_types::{EntryKind, VfsPath};
    use cairn_vfs::{ByteRange, VfsError};
    use std::collections::BTreeMap;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    /// The largest slice [`MockReader`] hands out per `poll_read`, regardless of the caller's
    /// buffer. Small on purpose: a test that reads a multi-KiB file through a 1 MiB buffer (the
    /// transfer engine's real chunk) can prove the mock never served the file as one buffer.
    pub(crate) const MOCK_READ_CHUNK: usize = 4096;

    /// A transport call the mock observed, in order. Lets tests assert *how* the `Vfs` mapping drove
    /// the transport (chunk-by-chunk, before `finish`), not just the end state.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum MockCall {
        /// One `write_all` on an open write stream.
        Write { path: String, len: usize },
        /// `finish` committed the stream.
        Finished(String),
        /// `abort` discarded the stream.
        Aborted(String),
    }

    enum Node {
        Dir,
        File(Vec<u8>),
        /// A symbolic link to another path (which may or may not exist / may itself be a link).
        Symlink(String),
    }

    /// In-memory SFTP transport for tests.
    pub(crate) struct MockSftp {
        /// Shared with the write streams handed out by `open_write`, which outlive the borrow.
        nodes: Arc<Mutex<BTreeMap<String, Node>>>,
        /// Ordered log of write-stream calls (see [`MockCall`]).
        calls: Arc<Mutex<Vec<MockCall>>>,
        /// Paths whose write stream's `finish` fails (simulates a failed flush / `CLOSE` status).
        fail_finish: Mutex<Vec<String>>,
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
                nodes: Arc::new(Mutex::new(nodes)),
                calls: Arc::new(Mutex::new(Vec::new())),
                fail_finish: Mutex::new(Vec::new()),
                fail_rename_to: Mutex::new(None),
                hide_readdir_types: false,
            }
        }

        /// Every write-stream call observed so far, in order.
        pub(crate) fn calls(&self) -> Vec<MockCall> {
            self.calls.lock().unwrap().clone()
        }

        /// Make `finish` fail for any write stream opened on a path with this suffix (the write
        /// sink streams into a generated temp name, so tests match on the target's file name).
        #[must_use]
        pub(crate) fn with_failing_finish_for(self, name_suffix: &str) -> Self {
            self.fail_finish
                .lock()
                .unwrap()
                .push(name_suffix.to_owned());
            self
        }

        /// Every path currently in the tree (for asserting no temp was left behind).
        pub(crate) fn paths(&self) -> Vec<String> {
            self.nodes.lock().unwrap().keys().cloned().collect()
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

        async fn open_read(
            &self,
            path: &str,
            range: Option<ByteRange>,
        ) -> Result<Box<dyn AsyncRead + Send + Unpin>, VfsError> {
            let nodes = self.nodes.lock().unwrap();
            let resolved = Self::resolve(&nodes, path).ok_or_else(|| Self::not_found(path))?;
            let Some(Node::File(b)) = nodes.get(&resolved) else {
                return Err(Self::not_found(path));
            };
            let data = match range {
                None => b.clone(),
                Some(r) => cairn_vfs::apply_byte_range(b, r).to_vec(),
            };
            Ok(Box::new(MockReader { data, pos: 0 }))
        }

        async fn open_write(&self, path: &str) -> Result<Box<dyn SftpWriteStream>, VfsError> {
            let fail_finish = self
                .fail_finish
                .lock()
                .unwrap()
                .iter()
                .any(|suffix| path.contains(suffix.as_str()));
            let mut nodes = self.nodes.lock().unwrap();
            // Opening a directory for write is a server failure, not a silent replace.
            if matches!(nodes.get(path), Some(Node::Dir)) {
                return Err(VfsError::Backend {
                    code: "sftp".to_owned(),
                    msg: "open: is a directory".to_owned(),
                    retryable: false,
                });
            }
            // Model create-on-open: the real server creates (and truncates) the file when the handle
            // is opened, before any bytes arrive — which is why `abort` must always remove it.
            nodes.insert(path.to_owned(), Node::File(Vec::new()));
            Ok(Box::new(MockWriteStream {
                path: path.to_owned(),
                nodes: self.nodes.clone(),
                calls: self.calls.clone(),
                pending: Vec::new(),
                fail_finish,
            }))
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
            let mut nodes = self.nodes.lock().unwrap();
            // Faithfully model OpenSSH's sftp-server: `mkdir` on an existing path fails with a generic
            // SSH_FX_FAILURE (no dedicated "already exists" code). `SftpVfs::create_dir` recovers by
            // stat-ing and reporting `AlreadyExists`, which is what this models for the merge-copy test.
            if nodes.contains_key(path) {
                return Err(VfsError::Backend {
                    code: "sftp".to_owned(),
                    msg: "Failure: Failure".to_owned(),
                    retryable: false,
                });
            }
            nodes.insert(path.to_owned(), Node::Dir);
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
    /// A reader that serves at most [`MOCK_READ_CHUNK`] bytes per poll, so tests can observe that a
    /// consumer really streams (many small reads) rather than receiving the file in one call.
    struct MockReader {
        data: Vec<u8>,
        pos: usize,
    }

    impl AsyncRead for MockReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let n = (self.data.len() - self.pos)
                .min(MOCK_READ_CHUNK)
                .min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.data[start..start + n]);
            self.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    /// The mock's open write stream: appends to `pending`, logs every call, and only commits the
    /// bytes to the node map on `finish` (or removes the pre-created node on `abort`).
    struct MockWriteStream {
        path: String,
        nodes: Arc<Mutex<BTreeMap<String, Node>>>,
        calls: Arc<Mutex<Vec<MockCall>>>,
        pending: Vec<u8>,
        fail_finish: bool,
    }

    #[async_trait]
    impl SftpWriteStream for MockWriteStream {
        async fn write_all(&mut self, data: &[u8]) -> Result<(), VfsError> {
            self.pending.extend_from_slice(data);
            self.calls.lock().unwrap().push(MockCall::Write {
                path: self.path.clone(),
                len: data.len(),
            });
            Ok(())
        }

        async fn finish(self: Box<Self>) -> Result<(), VfsError> {
            if self.fail_finish {
                // Like the real transport: the handle is gone, and what the server holds is suspect.
                // (The node stays — it is the caller's job to remove the temp.)
                return Err(VfsError::Backend {
                    code: "sftp".to_owned(),
                    msg: "close: simulated failure".to_owned(),
                    retryable: false,
                });
            }
            self.nodes
                .lock()
                .unwrap()
                .insert(self.path.clone(), Node::File(self.pending));
            self.calls
                .lock()
                .unwrap()
                .push(MockCall::Finished(self.path));
            Ok(())
        }

        async fn abort(self: Box<Self>) {
            self.nodes.lock().unwrap().remove(&self.path);
            self.calls
                .lock()
                .unwrap()
                .push(MockCall::Aborted(self.path));
        }
    }
}
