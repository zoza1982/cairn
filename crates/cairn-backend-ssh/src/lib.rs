//! SSH/SFTP backend.
//!
//! The product logic — mapping SFTP operations onto the [`Vfs`] trait — lives in [`SftpVfs`], which
//! is generic over an [`SftpOps`] transport so it is fully unit-testable against an in-memory mock.
//! The real transport (`russh` + `russh-sftp`) is the thin [`RealSftp`] adapter; establishing the
//! SSH connection (`ssh_connect`) lives in `connect.rs` behind the `ssh` feature (it pulls `russh`),
//! while `RealSftp` (over `russh-sftp`, TLS-free) stays unconditional. See `docs/LLD.md` §3.6,
//! RFC-0003, ADR-0006.

#[cfg(feature = "ssh")]
mod connect;
mod ops;
mod real;

#[cfg(feature = "ssh")]
pub use connect::{ssh_connect, HostKeyPolicy, SshConnectParams};
pub use ops::{RemoteEntry, RemoteMeta, SftpOps};
pub use real::RealSftp;

use async_trait::async_trait;
use cairn_types::{Caps, ConnectionId, Entry, EntryKind, Scheme, VfsPath};
use cairn_vfs::{
    ByteRange, CapabilityProvider, ListOpts, ListPage, ReadHandle, Recurse, Vfs, VfsError,
    WriteHandle, WriteOpts, WriteSink,
};
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use std::sync::Arc;

/// A [`Vfs`] over any [`SftpOps`] transport.
pub struct SftpVfs<O: SftpOps> {
    conn: ConnectionId,
    ops: Arc<O>,
}

impl<O: SftpOps> SftpVfs<O> {
    /// Create a backend over the given SFTP transport.
    pub fn new(conn: ConnectionId, ops: O) -> Self {
        Self {
            conn,
            ops: Arc::new(ops),
        }
    }

    async fn list_dir(&self, dir: VfsPath) -> Result<ListPage, VfsError> {
        let remote = self.ops.read_dir(&dir.as_str()).await?;
        let mut entries = Vec::with_capacity(remote.len());
        for r in remote {
            if r.name == "." || r.name == ".." {
                continue;
            }
            let mut e = Entry::new(r.name, r.kind);
            if r.kind == EntryKind::File {
                e.size = r.size;
            }
            e.modified = r.modified;
            entries.push(e);
        }
        Ok(ListPage {
            entries,
            cursor: None,
            done: true,
        })
    }

    async fn remove_recursive(&self, dir: &VfsPath) -> Result<(), VfsError> {
        // Post-order: discover the whole subtree, delete files as we go, then remove directories
        // deepest-first.
        let mut to_visit = vec![dir.clone()];
        let mut dirs = Vec::new();
        while let Some(d) = to_visit.pop() {
            dirs.push(d.clone());
            for r in self.ops.read_dir(&d.as_str()).await? {
                if r.name == "." || r.name == ".." {
                    continue;
                }
                let child = d.join(&r.name).map_err(|_| VfsError::NotFound(d.clone()))?;
                if r.kind == EntryKind::Dir {
                    to_visit.push(child);
                } else {
                    self.ops.remove_file(&child.as_str()).await?;
                }
            }
        }
        for d in dirs.iter().rev() {
            self.ops.remove_dir(&d.as_str()).await?;
        }
        Ok(())
    }
}

impl<O: SftpOps> CapabilityProvider for SftpVfs<O> {
    fn caps(&self) -> Caps {
        Caps::LIST
            | Caps::READ
            | Caps::WRITE
            | Caps::CREATE_DIR
            | Caps::DELETE
            | Caps::RENAME
            | Caps::RENAME_ATOMIC
            | Caps::RANDOM_READ
            | Caps::SYMLINK
    }
}

#[async_trait]
impl<O: SftpOps> Vfs for SftpVfs<O> {
    fn scheme(&self) -> Scheme {
        Scheme::Ssh
    }

    fn connection(&self) -> ConnectionId {
        self.conn
    }

    fn list<'a>(
        &'a self,
        dir: &VfsPath,
        _opts: ListOpts,
    ) -> BoxStream<'a, Result<ListPage, VfsError>> {
        let dir = dir.clone();
        stream::once(async move { self.list_dir(dir).await }).boxed()
    }

    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError> {
        if path.is_root() {
            return Ok(Entry::new("", EntryKind::Dir));
        }
        let m = self.ops.stat(&path.as_str()).await?;
        let mut e = Entry::new(path.file_name().unwrap_or(""), m.kind);
        if m.kind == EntryKind::File {
            e.size = m.size;
        }
        e.modified = m.modified;
        Ok(e)
    }

    async fn open_read(
        &self,
        path: &VfsPath,
        range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError> {
        let data = self.ops.read(&path.as_str(), range).await?;
        let len = data.len() as u64;
        Ok(ReadHandle::new(
            Box::new(std::io::Cursor::new(data)),
            Some(len),
        ))
    }

    async fn open_write(&self, path: &VfsPath, _opts: WriteOpts) -> Result<WriteHandle, VfsError> {
        Ok(WriteHandle::new(Box::new(SftpWriteSink {
            ops: self.ops.clone(),
            path: path.as_str(),
            name: path.file_name().unwrap_or("").to_owned(),
            buf: Vec::new(),
        })))
    }

    async fn create_dir(&self, path: &VfsPath) -> Result<(), VfsError> {
        self.ops.create_dir(&path.as_str()).await
    }

    async fn remove(&self, path: &VfsPath, recurse: Recurse) -> Result<(), VfsError> {
        let m = self.ops.stat(&path.as_str()).await?;
        match (m.kind, recurse) {
            (EntryKind::Dir, Recurse::Yes) => self.remove_recursive(path).await,
            (EntryKind::Dir, Recurse::No) => self.ops.remove_dir(&path.as_str()).await,
            _ => self.ops.remove_file(&path.as_str()).await,
        }
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError> {
        self.ops.rename(&from.as_str(), &to.as_str()).await
    }
}

struct SftpWriteSink<O: SftpOps> {
    ops: Arc<O>,
    path: String,
    name: String,
    buf: Vec<u8>,
}

#[async_trait]
impl<O: SftpOps> WriteSink for SftpWriteSink<O> {
    async fn write_chunk(&mut self, chunk: bytes::Bytes) -> Result<(), VfsError> {
        self.buf.extend_from_slice(&chunk);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<Entry, VfsError> {
        self.ops.write(&self.path, &self.buf).await?;
        let mut e = Entry::new(self.name, EntryKind::File);
        e.size = Some(self.buf.len() as u64);
        Ok(e)
    }

    async fn abort(self: Box<Self>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use ops::mock::MockSftp;
    use tokio::io::AsyncReadExt;

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    fn backend() -> SftpVfs<MockSftp> {
        let mock = MockSftp::new()
            .with_file("/top.txt", b"top")
            .with_dir("/d")
            .with_file("/d/a.txt", b"aaa")
            .with_dir("/d/sub")
            .with_file("/d/sub/b.txt", b"bbbb");
        SftpVfs::new(ConnectionId(1), mock)
    }

    /// `SftpVfs` must be `Send + Sync` to live behind `Arc<dyn Vfs>`. Guards against russh's
    /// historically `!Send` futures regressing the real adapter (R7 in the implementation plan).
    #[test]
    fn vfs_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SftpVfs<MockSftp>>();
        // `RealSftp` (over russh-sftp) is compiled unconditionally, so guard it in the lean build too.
        assert_send_sync::<SftpVfs<RealSftp>>();
    }

    #[tokio::test]
    async fn lists_and_navigates() {
        let vfs = backend();
        let mut s = vfs.list(&p("/"), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let mut names: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["d", "top.txt"]);

        let mut s = vfs.list(&p("/d"), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let mut names: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["a.txt", "sub"]);
    }

    #[tokio::test]
    async fn stat_read_write_rename() {
        let vfs = backend();
        assert_eq!(vfs.stat(&p("/top.txt")).await.unwrap().size, Some(3));
        assert!(vfs.stat(&p("/d")).await.unwrap().is_dir());

        let mut rh = vfs.open_read(&p("/top.txt"), None).await.unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "top");

        let mut wh = vfs
            .open_write(&p("/new.txt"), WriteOpts::default())
            .await
            .unwrap();
        wh.write_chunk(bytes::Bytes::from_static(b"hi"))
            .await
            .unwrap();
        assert_eq!(wh.finish().await.unwrap().size, Some(2));
        let mut rh = vfs.open_read(&p("/new.txt"), None).await.unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "hi");

        vfs.rename(&p("/new.txt"), &p("/renamed.txt"))
            .await
            .unwrap();
        assert!(matches!(
            vfs.stat(&p("/new.txt")).await,
            Err(VfsError::NotFound(_))
        ));
        assert_eq!(vfs.stat(&p("/renamed.txt")).await.unwrap().size, Some(2));
    }

    #[tokio::test]
    async fn ranged_read() {
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new().with_file("/f", b"0123456789"),
        );
        let mut rh = vfs
            .open_read(
                &p("/f"),
                Some(ByteRange {
                    offset: 2,
                    len: Some(3),
                }),
            )
            .await
            .unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "234");
    }

    #[tokio::test]
    async fn recursive_remove() {
        let vfs = backend();
        vfs.remove(&p("/d"), Recurse::Yes).await.unwrap();
        assert!(matches!(
            vfs.stat(&p("/d/sub/b.txt")).await,
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            vfs.stat(&p("/d")).await,
            Err(VfsError::NotFound(_))
        ));
        assert_eq!(vfs.stat(&p("/top.txt")).await.unwrap().size, Some(3));
    }

    #[tokio::test]
    async fn missing_is_not_found() {
        let vfs = backend();
        assert!(matches!(
            vfs.stat(&p("/nope")).await,
            Err(VfsError::NotFound(_))
        ));
    }
}
