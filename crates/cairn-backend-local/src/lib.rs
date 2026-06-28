//! The local filesystem backend.
//!
//! [`LocalVfs`] implements [`Vfs`] over `tokio::fs`. It is rooted at a base directory: a [`VfsPath`]
//! is resolved relative to that base, and because `VfsPath` rejects `..` at parse time, a request
//! can never escape the root. The base is typically the OS filesystem root, but rooting at an
//! arbitrary directory keeps the backend testable and contained.

use async_trait::async_trait;
use bytes::Bytes;
use cairn_types::{Caps, ConnectionId, Entry, EntryKind, Scheme, UnixPerms, VfsPath};
use cairn_vfs::{
    ByteRange, CapabilityProvider, ListOpts, ListPage, ReadHandle, Recurse, Vfs, VfsError,
    WriteHandle, WriteOpts, WriteSink,
};
use futures::stream::{self, BoxStream, StreamExt};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Capabilities supported on the current platform.
const fn platform_caps() -> Caps {
    let base = Caps::LIST
        .union(Caps::READ)
        .union(Caps::WRITE)
        .union(Caps::CREATE_DIR)
        .union(Caps::DELETE)
        .union(Caps::RENAME)
        .union(Caps::RENAME_ATOMIC)
        .union(Caps::RANDOM_READ)
        .union(Caps::APPEND);
    #[cfg(unix)]
    {
        base.union(Caps::CHMOD).union(Caps::SYMLINK)
    }
    #[cfg(not(unix))]
    {
        base
    }
}

/// The local filesystem backend, rooted at a base directory.
pub struct LocalVfs {
    conn: ConnectionId,
    root: PathBuf,
}

impl LocalVfs {
    /// Create a backend rooted at `root`. Paths are resolved relative to it.
    #[must_use]
    pub fn new(conn: ConnectionId, root: impl Into<PathBuf>) -> Self {
        Self {
            conn,
            root: root.into(),
        }
    }

    /// Resolve a [`VfsPath`] to an absolute OS path under the root. Safe because `VfsPath` contains
    /// no `..` segments.
    fn resolve(&self, path: &VfsPath) -> PathBuf {
        let mut pb = self.root.clone();
        for seg in path.segments() {
            pb.push(seg.as_str());
        }
        pb
    }
}

/// Map a `std::io::Error` to a [`VfsError`], attaching the logical path where useful.
fn map_io(e: std::io::Error, path: &VfsPath) -> VfsError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => VfsError::NotFound(path.clone()),
        ErrorKind::PermissionDenied => VfsError::Forbidden(path.clone()),
        ErrorKind::AlreadyExists => VfsError::AlreadyExists(path.clone()),
        _ => VfsError::Io(e),
    }
}

fn entry_from_meta(name: &str, meta: &std::fs::Metadata) -> Entry {
    let ft = meta.file_type();
    let kind = if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Special
    };
    let mut entry = Entry::new(name, kind);
    if kind == EntryKind::File {
        entry.size = Some(meta.len());
    }
    entry.modified = meta.modified().ok();
    entry.perms = unix_perms(meta);
    entry
}

#[cfg(unix)]
fn unix_perms(meta: &std::fs::Metadata) -> Option<UnixPerms> {
    use std::os::unix::fs::MetadataExt;
    Some(UnixPerms {
        mode: meta.mode(),
        uid: Some(meta.uid()),
        gid: Some(meta.gid()),
    })
}

#[cfg(not(unix))]
fn unix_perms(_meta: &std::fs::Metadata) -> Option<UnixPerms> {
    None
}

impl CapabilityProvider for LocalVfs {
    fn caps(&self) -> Caps {
        platform_caps()
    }
}

#[async_trait]
impl Vfs for LocalVfs {
    fn scheme(&self) -> Scheme {
        Scheme::Local
    }

    fn connection(&self) -> ConnectionId {
        self.conn
    }

    fn list<'a>(
        &'a self,
        dir: &VfsPath,
        opts: ListOpts,
    ) -> BoxStream<'a, Result<ListPage, VfsError>> {
        let base = self.resolve(dir);
        let dir = dir.clone();
        stream::once(async move { read_dir_page(&base, &dir, opts.all).await }).boxed()
    }

    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError> {
        let full = self.resolve(path);
        let meta = tokio::fs::symlink_metadata(&full)
            .await
            .map_err(|e| map_io(e, path))?;
        let name = path.file_name().unwrap_or("");
        Ok(entry_from_meta(name, &meta))
    }

    async fn open_read(
        &self,
        path: &VfsPath,
        range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError> {
        let full = self.resolve(path);
        let mut file = tokio::fs::File::open(&full)
            .await
            .map_err(|e| map_io(e, path))?;
        let total = file.metadata().await.map_err(|e| map_io(e, path))?.len();
        match range {
            None => Ok(ReadHandle::new(Box::new(file), Some(total))),
            Some(r) => {
                file.seek(std::io::SeekFrom::Start(r.offset))
                    .await
                    .map_err(|e| map_io(e, path))?;
                match r.len {
                    Some(len) => Ok(ReadHandle::new(Box::new(file.take(len)), Some(total))),
                    None => Ok(ReadHandle::new(Box::new(file), Some(total))),
                }
            }
        }
    }

    async fn open_write(&self, path: &VfsPath, opts: WriteOpts) -> Result<WriteHandle, VfsError> {
        let full = self.resolve(path);
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .create_new(!opts.overwrite)
            .truncate(opts.overwrite)
            .open(&full)
            .await
            .map_err(|e| map_io(e, path))?;
        Ok(WriteHandle::new(Box::new(LocalWriteSink {
            file,
            full,
            path: path.clone(),
            written: 0,
        })))
    }

    async fn create_dir(&self, path: &VfsPath) -> Result<(), VfsError> {
        tokio::fs::create_dir(self.resolve(path))
            .await
            .map_err(|e| map_io(e, path))
    }

    async fn remove(&self, path: &VfsPath, recurse: Recurse) -> Result<(), VfsError> {
        let full = self.resolve(path);
        let meta = tokio::fs::symlink_metadata(&full)
            .await
            .map_err(|e| map_io(e, path))?;
        let result = if meta.is_dir() {
            match recurse {
                Recurse::Yes => tokio::fs::remove_dir_all(&full).await,
                Recurse::No => tokio::fs::remove_dir(&full).await,
            }
        } else {
            tokio::fs::remove_file(&full).await
        };
        result.map_err(|e| map_io(e, path))
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError> {
        tokio::fs::rename(self.resolve(from), self.resolve(to))
            .await
            .map_err(|e| map_io(e, from))
    }

    async fn set_perms(&self, path: &VfsPath, perms: UnixPerms) -> Result<(), VfsError> {
        set_perms_impl(&self.resolve(path), path, perms).await
    }
}

#[cfg(unix)]
async fn set_perms_impl(full: &Path, path: &VfsPath, perms: UnixPerms) -> Result<(), VfsError> {
    use std::os::unix::fs::PermissionsExt;
    let p = std::fs::Permissions::from_mode(perms.mode);
    tokio::fs::set_permissions(full, p)
        .await
        .map_err(|e| map_io(e, path))
}

#[cfg(not(unix))]
async fn set_perms_impl(_full: &Path, _path: &VfsPath, _perms: UnixPerms) -> Result<(), VfsError> {
    Err(VfsError::Unsupported(Caps::CHMOD))
}

async fn read_dir_page(base: &Path, dir: &VfsPath, all: bool) -> Result<ListPage, VfsError> {
    let mut rd = tokio::fs::read_dir(base)
        .await
        .map_err(|e| map_io(e, dir))?;
    let mut entries = Vec::new();
    while let Some(de) = rd.next_entry().await.map_err(|e| map_io(e, dir))? {
        let name = de.file_name().to_string_lossy().into_owned();
        if !all && name.starts_with('.') {
            continue;
        }
        let meta = match de.metadata().await {
            Ok(m) => m,
            Err(_) => continue, // entry vanished between readdir and stat; skip
        };
        entries.push(entry_from_meta(&name, &meta));
    }
    Ok(ListPage {
        entries,
        cursor: None,
        done: true,
    })
}

/// A [`WriteSink`] that writes directly to a local file, syncing on `finish` and removing the
/// partial file on `abort`.
struct LocalWriteSink {
    file: tokio::fs::File,
    full: PathBuf,
    path: VfsPath,
    written: u64,
}

#[async_trait]
impl WriteSink for LocalWriteSink {
    async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), VfsError> {
        self.file
            .write_all(&chunk)
            .await
            .map_err(|e| map_io(e, &self.path))?;
        self.written += chunk.len() as u64;
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<Entry, VfsError> {
        self.file.flush().await.map_err(|e| map_io(e, &self.path))?;
        self.file
            .sync_all()
            .await
            .map_err(|e| map_io(e, &self.path))?;
        let mut entry = Entry::new(self.path.file_name().unwrap_or(""), EntryKind::File);
        entry.size = Some(self.written);
        Ok(entry)
    }

    async fn abort(self: Box<Self>) {
        drop(self.file);
        let _ = tokio::fs::remove_file(&self.full).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    fn backend() -> (tempfile::TempDir, LocalVfs) {
        let dir = tempfile::tempdir().unwrap();
        let vfs = LocalVfs::new(ConnectionId(1), dir.path());
        (dir, vfs)
    }

    #[tokio::test]
    async fn list_reads_directory() {
        let (dir, vfs) = backend();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join(".hidden"), b"x").unwrap();

        let mut s = vfs.list(&p("/"), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let mut names: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["a.txt", "sub"]); // .hidden excluded by default

        let mut s = vfs.list(&p("/"), ListOpts { all: true });
        let page = s.next().await.unwrap().unwrap();
        assert_eq!(page.entries.len(), 3);
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let (_dir, vfs) = backend();
        let mut wh = vfs
            .open_write(&p("/out.txt"), WriteOpts::default())
            .await
            .unwrap();
        wh.write_chunk(Bytes::from_static(b"hello ")).await.unwrap();
        wh.write_chunk(Bytes::from_static(b"world")).await.unwrap();
        let e = wh.finish().await.unwrap();
        assert_eq!(e.size, Some(11));

        let mut rh = vfs.open_read(&p("/out.txt"), None).await.unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn ranged_read() {
        let (dir, vfs) = backend();
        std::fs::write(dir.path().join("f"), b"0123456789").unwrap();
        let mut rh = vfs
            .open_read(
                &p("/f"),
                Some(ByteRange {
                    offset: 3,
                    len: Some(4),
                }),
            )
            .await
            .unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "3456");
    }

    #[tokio::test]
    async fn mkdir_rename_remove() {
        let (_dir, vfs) = backend();
        vfs.create_dir(&p("/d")).await.unwrap();
        assert!(vfs.stat(&p("/d")).await.unwrap().is_dir());
        vfs.rename(&p("/d"), &p("/d2")).await.unwrap();
        assert!(matches!(
            vfs.stat(&p("/d")).await,
            Err(VfsError::NotFound(_))
        ));
        vfs.remove(&p("/d2"), Recurse::No).await.unwrap();
        assert!(matches!(
            vfs.stat(&p("/d2")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn stat_missing_is_not_found() {
        let (_dir, vfs) = backend();
        assert!(matches!(
            vfs.stat(&p("/nope")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn no_overwrite_without_flag() {
        let (dir, vfs) = backend();
        std::fs::write(dir.path().join("exists"), b"x").unwrap();
        let res = vfs.open_write(&p("/exists"), WriteOpts::default()).await;
        assert!(matches!(res, Err(VfsError::AlreadyExists(_))));
    }
}
