//! [`MockVfs`]: an in-memory backend for hermetic tests, in this crate and in dependents (enable the
//! `test-utils` feature). It implements [`Vfs`] over a simple in-memory tree.

use crate::action::{ActionCtx, ActionId, ActionOutcome};
use crate::error::VfsError;
use crate::handle::{CommitMode, ReadHandle, WriteHandle, WriteSink};
use crate::vfs::{ByteRange, CapabilityProvider, ListOpts, ListPage, Recurse, Vfs, WriteOpts};
use bytes::Bytes;
use cairn_types::{Caps, ConnectionId, Entry, EntryKind, Scheme, VfsPath};
use futures::stream::{self, BoxStream, StreamExt};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt as _;

type Tree = Arc<Mutex<BTreeMap<String, Node>>>;

#[derive(Debug, Clone)]
enum Node {
    Dir,
    File(Vec<u8>),
}

/// An in-memory [`Vfs`] implementation for tests.
pub struct MockVfs {
    conn: ConnectionId,
    /// Map of canonical path string → node. Always contains the root `/`.
    nodes: Tree,
    /// Fault injection: a read of this path yields `after` bytes and then an I/O error, so a caller's
    /// mid-stream error handling (e.g. the transfer engine aborting the destination) is testable.
    read_fault: Option<(String, usize)>,
    /// Fault injection: every `write_chunk` to this path fails.
    write_fault: Option<String>,
    /// Paths whose write sink was `abort`ed, in order — the observable side of an error path.
    aborted: Arc<Mutex<Vec<String>>>,
    /// Fault injection: run this when a reader of `path` reports EOF — i.e. *during* the final
    /// `read()` that returns 0, the one await point a caller cannot observe from between chunks.
    on_eof: Option<(String, Arc<dyn Fn() + Send + Sync>)>,
    /// What the write sinks declare (default `Streamed`); `Buffered` models an object store.
    commit_mode: CommitMode,
    /// Artificial delay inside `finish`, so a test can observe what a caller does while a slow
    /// commit (a big single-shot upload, a remote fsync) is in flight.
    finish_delay: Option<std::time::Duration>,
}

impl MockVfs {
    /// Create an empty mock backend (containing only the root directory).
    #[must_use]
    pub fn new(conn: ConnectionId) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert("/".to_owned(), Node::Dir);
        Self {
            conn,
            nodes: Arc::new(Mutex::new(nodes)),
            read_fault: None,
            write_fault: None,
            aborted: Arc::new(Mutex::new(Vec::new())),
            on_eof: None,
            commit_mode: CommitMode::Streamed,
            finish_delay: None,
        }
    }

    /// Builder: run `f` when a reader of `path` reports EOF — i.e. *during* the final `read()` that
    /// returns 0, the one await point a caller cannot observe from between chunks.
    #[must_use]
    pub fn with_on_eof(mut self, path: &str, f: impl Fn() + Send + Sync + 'static) -> Self {
        let p = VfsPath::parse(path).expect("valid test path");
        self.on_eof = Some((p.as_str(), Arc::new(f)));
        self
    }

    /// Builder: make write sinks declare [`CommitMode::Buffered`] (the object-store shape: chunks
    /// are staged and the real transfer happens in `finish`).
    #[must_use]
    pub fn with_buffered_writes(mut self) -> Self {
        self.commit_mode = CommitMode::Buffered;
        self
    }

    /// Builder: make every write sink's `finish` sleep for `delay` before committing.
    #[must_use]
    pub fn with_finish_delay(mut self, delay: std::time::Duration) -> Self {
        self.finish_delay = Some(delay);
        self
    }

    /// Builder: make reads of `path` fail with an I/O error after yielding `after` bytes.
    #[must_use]
    pub fn with_read_fault(mut self, path: &str, after: usize) -> Self {
        let p = VfsPath::parse(path).expect("valid test path");
        self.read_fault = Some((p.as_str(), after));
        self
    }

    /// Builder: make every `write_chunk` to `path` fail.
    #[must_use]
    pub fn with_write_fault(mut self, path: &str) -> Self {
        let p = VfsPath::parse(path).expect("valid test path");
        self.write_fault = Some(p.as_str());
        self
    }

    /// Paths whose write sink was aborted (see [`WriteSink::abort`]), in order.
    #[must_use]
    pub fn aborted_writes(&self) -> Vec<String> {
        self.aborted
            .lock()
            .expect("mock vfs mutex poisoned")
            .clone()
    }

    /// Builder: add a directory (and ensure it exists).
    #[must_use]
    pub fn with_dir(self, path: &str) -> Self {
        let p = VfsPath::parse(path).expect("valid test path");
        self.lock().insert(p.as_str(), Node::Dir);
        self
    }

    /// Builder: add a file with the given contents.
    #[must_use]
    pub fn with_file(self, path: &str, contents: &[u8]) -> Self {
        let p = VfsPath::parse(path).expect("valid test path");
        self.lock()
            .insert(p.as_str(), Node::File(contents.to_vec()));
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Node>> {
        self.nodes.lock().expect("mock vfs mutex poisoned")
    }

    fn children(&self, dir: &VfsPath) -> Vec<Entry> {
        let nodes = self.lock();
        let prefix = if dir.is_root() {
            "/".to_owned()
        } else {
            format!("{}/", dir.as_str())
        };
        let mut out = Vec::new();
        for (path, node) in nodes.iter() {
            if path == "/" {
                continue;
            }
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() || rest.contains('/') {
                continue; // not a direct child
            }
            let kind = match node {
                Node::Dir => EntryKind::Dir,
                Node::File(_) => EntryKind::File,
            };
            let size = match node {
                Node::File(b) => Some(b.len() as u64),
                Node::Dir => None,
            };
            let mut entry = Entry::new(rest, kind);
            entry.size = size;
            out.push(entry);
        }
        out
    }
}

impl CapabilityProvider for MockVfs {
    fn caps(&self) -> Caps {
        Caps::LIST
            | Caps::READ
            | Caps::WRITE
            | Caps::CREATE_DIR
            | Caps::DELETE
            | Caps::RENAME
            | Caps::RENAME_ATOMIC
            | Caps::RANDOM_READ
    }
}

#[async_trait::async_trait]
impl Vfs for MockVfs {
    fn scheme(&self) -> Scheme {
        Scheme::Local
    }

    fn connection(&self) -> ConnectionId {
        self.conn
    }

    fn list<'a>(
        &'a self,
        dir: &VfsPath,
        _opts: ListOpts,
    ) -> BoxStream<'a, Result<ListPage, VfsError>> {
        let exists = {
            let nodes = self.lock();
            matches!(nodes.get(&dir.as_str()), Some(Node::Dir))
        };
        if !exists {
            let err = VfsError::NotFound(dir.clone());
            return stream::once(async move { Err(err) }).boxed();
        }
        let entries = self.children(dir);
        let page = ListPage {
            entries,
            cursor: None,
            done: true,
        };
        stream::once(async move { Ok(page) }).boxed()
    }

    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError> {
        if path.is_root() {
            return Ok(Entry::new("", EntryKind::Dir));
        }
        let nodes = self.lock();
        match nodes.get(&path.as_str()) {
            Some(Node::Dir) => Ok(Entry::new(path.file_name().unwrap_or(""), EntryKind::Dir)),
            Some(Node::File(b)) => {
                let mut e = Entry::new(path.file_name().unwrap_or(""), EntryKind::File);
                e.size = Some(b.len() as u64);
                Ok(e)
            }
            None => Err(VfsError::NotFound(path.clone())),
        }
    }

    async fn open_read(
        &self,
        path: &VfsPath,
        range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError> {
        let bytes = {
            let nodes = self.lock();
            match nodes.get(&path.as_str()) {
                Some(Node::File(b)) => b.clone(),
                Some(Node::Dir) => return Err(VfsError::Unsupported(Caps::READ)),
                None => return Err(VfsError::NotFound(path.clone())),
            }
        };
        let total = bytes.len() as u64;
        let sliced = match range {
            None => bytes,
            // Saturating clamp — the old open-coded `offset + len` overflowed on a hostile range.
            Some(r) => crate::vfs::apply_byte_range(&bytes, r).to_vec(),
        };
        let reader: Box<dyn tokio::io::AsyncRead + Send + Unpin> = match &self.read_fault {
            Some((fault_path, after)) if *fault_path == path.as_str() => {
                // Serve the first `after` bytes, then fail: `Cursor` over the prefix chained with a
                // reader that errors on its first poll.
                let good = sliced[..(*after).min(sliced.len())].to_vec();
                Box::new(std::io::Cursor::new(good).chain(FailingReader))
            }
            _ => Box::new(std::io::Cursor::new(sliced)),
        };
        let reader: Box<dyn tokio::io::AsyncRead + Send + Unpin> = match &self.on_eof {
            Some((hook_path, hook)) if *hook_path == path.as_str() => Box::new(EofHook {
                inner: reader,
                hook: hook.clone(),
            }),
            _ => reader,
        };
        Ok(ReadHandle::new(reader, Some(total)))
    }

    async fn open_write(&self, path: &VfsPath, _opts: WriteOpts) -> Result<WriteHandle, VfsError> {
        Ok(WriteHandle::new(Box::new(MockWriteSink {
            fail_writes: self.write_fault.as_deref() == Some(&*path.as_str()),
            path: path.clone(),
            buf: Vec::new(),
            nodes: self.nodes.clone(),
            aborted: self.aborted.clone(),
            commit_mode: self.commit_mode,
            finish_delay: self.finish_delay,
        })))
    }

    async fn create_dir(&self, path: &VfsPath) -> Result<(), VfsError> {
        self.lock().insert(path.as_str(), Node::Dir);
        Ok(())
    }

    async fn remove(&self, path: &VfsPath, recurse: Recurse) -> Result<(), VfsError> {
        let key = path.as_str();
        let mut nodes = self.lock();
        if !nodes.contains_key(&key) {
            return Err(VfsError::NotFound(path.clone()));
        }
        if recurse == Recurse::Yes {
            let prefix = format!("{key}/");
            nodes.retain(|k, _| k != &key && !k.starts_with(&prefix));
        } else {
            nodes.remove(&key);
        }
        Ok(())
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError> {
        let mut nodes = self.lock();
        let node = nodes
            .remove(&from.as_str())
            .ok_or_else(|| VfsError::NotFound(from.clone()))?;
        nodes.insert(to.as_str(), node);
        Ok(())
    }

    async fn invoke(
        &self,
        _path: &VfsPath,
        _action: ActionId,
        _ctx: ActionCtx,
    ) -> Result<ActionOutcome, VfsError> {
        // Unconditional `Done` — sufficient for VFS-path tests; action-outcome tests use the
        // concrete backends with MockDocker/MockKube.
        Ok(ActionOutcome::Done)
    }
}

/// A reader that runs a hook the moment the inner reader reports EOF (a poll that fills 0 bytes).
struct EofHook {
    inner: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    hook: Arc<dyn Fn() + Send + Sync>,
}

impl tokio::io::AsyncRead for EofHook {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(poll, std::task::Poll::Ready(Ok(()))) && buf.filled().len() == before {
            (self.hook)();
        }
        poll
    }
}

/// A reader that fails on its first poll — the tail of an injected mid-stream read fault.
struct FailingReader;

impl tokio::io::AsyncRead for FailingReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("injected read fault")))
    }
}

/// A [`WriteSink`] that buffers bytes and commits them into the mock tree on `finish`.
struct MockWriteSink {
    path: VfsPath,
    buf: Vec<u8>,
    nodes: Tree,
    fail_writes: bool,
    aborted: Arc<Mutex<Vec<String>>>,
    commit_mode: CommitMode,
    finish_delay: Option<std::time::Duration>,
}

#[async_trait::async_trait]
impl WriteSink for MockWriteSink {
    fn commit_mode(&self) -> CommitMode {
        self.commit_mode
    }

    async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), VfsError> {
        if self.fail_writes {
            return Err(VfsError::Io(std::io::Error::other("injected write fault")));
        }
        self.buf.extend_from_slice(&chunk);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<Entry, VfsError> {
        if let Some(d) = self.finish_delay {
            tokio::time::sleep(d).await;
        }
        let len = self.buf.len() as u64;
        self.nodes
            .lock()
            .expect("mock vfs mutex poisoned")
            .insert(self.path.as_str(), Node::File(self.buf));
        let mut e = Entry::new(self.path.file_name().unwrap_or(""), EntryKind::File);
        e.size = Some(len);
        Ok(e)
    }

    async fn abort(self: Box<Self>) {
        self.aborted
            .lock()
            .expect("mock vfs mutex poisoned")
            .push(self.path.as_str());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    #[tokio::test]
    async fn list_returns_direct_children() {
        let vfs = MockVfs::new(ConnectionId(1))
            .with_dir("/src")
            .with_file("/README.md", b"hi")
            .with_file("/src/main.rs", b"fn main() {}");
        let mut s = vfs.list(&p("/"), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let mut names: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["README.md", "src"]);
    }

    #[tokio::test]
    async fn read_roundtrip_and_range() {
        let vfs = MockVfs::new(ConnectionId(1)).with_file("/f", b"hello world");
        let mut rh = vfs.open_read(&p("/f"), None).await.unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "hello world");

        let mut rh = vfs
            .open_read(
                &p("/f"),
                Some(ByteRange {
                    offset: 6,
                    len: Some(5),
                }),
            )
            .await
            .unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "world");
    }

    #[tokio::test]
    async fn write_then_read() {
        let vfs = MockVfs::new(ConnectionId(1));
        let mut wh = vfs
            .open_write(&p("/new.txt"), WriteOpts::default())
            .await
            .unwrap();
        wh.write_chunk(Bytes::from_static(b"abc")).await.unwrap();
        wh.write_chunk(Bytes::from_static(b"def")).await.unwrap();
        let entry = wh.finish().await.unwrap();
        assert_eq!(entry.size, Some(6));
        let mut rh = vfs.open_read(&p("/new.txt"), None).await.unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "abcdef");
    }

    #[tokio::test]
    async fn stat_and_not_found() {
        let vfs = MockVfs::new(ConnectionId(1)).with_file("/f", b"x");
        assert_eq!(vfs.stat(&p("/f")).await.unwrap().size, Some(1));
        assert!(matches!(
            vfs.stat(&p("/missing")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn remove_recursive() {
        let vfs = MockVfs::new(ConnectionId(1))
            .with_dir("/d")
            .with_file("/d/a", b"1")
            .with_file("/d/b", b"2");
        vfs.remove(&p("/d"), Recurse::Yes).await.unwrap();
        assert!(matches!(
            vfs.stat(&p("/d/a")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[test]
    fn local_path_defaults_to_none_for_a_non_local_backend() {
        // A backend with no local filesystem identity must deny `local_path` by default, so features
        // that shell out can never reach a remote backend.
        let vfs = MockVfs::new(ConnectionId(1)).with_file("/f", b"x");
        assert!(vfs.local_path(&p("/f")).is_none());
    }
}
