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
use cairn_types::{Caps, ConnectionId, Entry, EntryKind, Scheme, UnixPerms, VfsPath};
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
            let mut kind = r.kind;
            // Some SFTP servers omit the type/permission attrs in READDIR responses, so every entry
            // arrives looking like a plain file (`mode == None`). Left unresolved, a directory would
            // be misclassified — and a recursive delete driven by these kinds would never descend
            // into it, stranding the directory (and its parent) instead of removing it. Recover the
            // true kind with an explicit stat. OpenSSH always sends the attrs, so this never fires
            // on the common path.
            if kind == EntryKind::File && r.mode.is_none() {
                if let Ok(child) = dir.join(&r.name) {
                    if let Ok(m) = self.ops.stat(&child.as_str()).await {
                        kind = m.kind;
                    }
                }
            }
            let mut e = Entry::new(r.name, kind);
            if kind == EntryKind::File {
                e.size = r.size;
            }
            e.modified = r.modified;
            e.perms = r.mode.map(UnixPerms::from_mode);
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
        // deepest-first. Enumerate via `list_dir` (not raw `read_dir`) so the stat-fallback for
        // type-less servers applies here too — otherwise a misclassified subdirectory would be
        // `remove_file`'d (and fail) instead of being recursed into.
        let mut to_visit = vec![dir.clone()];
        let mut dirs = Vec::new();
        while let Some(d) = to_visit.pop() {
            dirs.push(d.clone());
            for e in self.list_dir(d.clone()).await?.entries {
                let child = d.join(&e.name).map_err(|_| VfsError::NotFound(d.clone()))?;
                if e.is_dir() {
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
        // Not `RENAME_ATOMIC`: SFTP's `SSH_FXP_RENAME` can't atomically overwrite an existing
        // destination, so `SftpVfs::rename` emulates overwrite with a non-atomic move-aside +
        // rename (+ restore-on-failure). Plain `RENAME` (no existing destination) is atomic.
        Caps::LIST
            | Caps::READ
            | Caps::WRITE
            | Caps::CREATE_DIR
            | Caps::DELETE
            | Caps::RENAME
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
        e.perms = m.mode.map(UnixPerms::from_mode);
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
        // SFTP's `SSH_FXP_RENAME` fails when the destination already exists (OpenSSH's sftp-server),
        // unlike POSIX rename — and every other Cairn backend — which overwrites. To overwrite a
        // remote *file* safely, move the existing destination aside to a backup, then rename; on
        // success delete the backup, on failure restore it. This never deletes the old content until
        // the new rename has actually succeeded, so a mid-operation network drop can't leave *both*
        // the original and the new content gone (a real risk with a naive remove-then-rename, since
        // the edit → write-back caller deletes its staged temp when the final rename fails). Not
        // atomic — SFTP has no portable atomic overwrite. A directory destination is left in place so
        // the rename below rejects it (OpenSSH's SFTP refuses any existing destination).
        let to_str = to.as_str();
        let backup: Option<VfsPath> = match self.ops.stat(&to_str).await {
            Ok(meta) if meta.kind != EntryKind::Dir => {
                let bname = format!(".cairn-rename-bak-{}", to.file_name().unwrap_or("file"));
                // A `join` failure (an invalid backup name) just skips the backup — the rename below
                // then fails on the still-present destination, leaving it intact.
                match to.parent().unwrap_or_else(VfsPath::root).join(&bname) {
                    Ok(bpath) => {
                        // Clear a stale backup (from a prior interrupted rename) so moving the
                        // destination aside can't itself hit an existing-destination rejection.
                        let _ = self.ops.remove_file(&bpath.as_str()).await;
                        // If moving the destination aside fails, proceed with no backup.
                        if self.ops.rename(&to_str, &bpath.as_str()).await.is_ok() {
                            Some(bpath)
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                }
            }
            _ => None,
        };
        match self.ops.rename(&from.as_str(), &to_str).await {
            Ok(()) => {
                if let Some(b) = backup {
                    let _ = self.ops.remove_file(&b.as_str()).await; // best-effort cleanup post-success
                }
                Ok(())
            }
            Err(e) => {
                if let Some(b) = backup {
                    // Best-effort restore of the original (the destination slot is free again).
                    let _ = self.ops.rename(&b.as_str(), &to_str).await;
                }
                Err(e)
            }
        }
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
    async fn maps_remote_mode_to_entry_perms() {
        // Regression: the SFTP transport used to drop the server's mode bits, so a remote pane
        // rendered a blank permission column. Both `list` and `stat` must now carry them through to
        // `Entry.perms` (the full mode, type bits included) so remote files show `drwxr-xr-x` like
        // local ones. The mock reports dirs as 0o40755 and files as 0o100644.
        let vfs = backend();
        let mut s = vfs.list(&p("/"), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let by_name = |n: &str| page.entries.iter().find(|e| e.name == n).unwrap().clone();
        assert_eq!(by_name("top.txt").perms.map(|p| p.mode), Some(0o100_644));
        assert_eq!(by_name("d").perms.map(|p| p.mode), Some(0o040_755));
        // `stat` carries them too.
        assert_eq!(
            vfs.stat(&p("/top.txt"))
                .await
                .unwrap()
                .perms
                .map(|p| p.mode),
            Some(0o100_644)
        );
        assert_eq!(
            vfs.stat(&p("/d")).await.unwrap().perms.map(|p| p.mode),
            Some(0o040_755)
        );
    }

    #[tokio::test]
    async fn rename_overwrites_an_existing_destination() {
        // Regression: OpenSSH's plain SSH_FXP_RENAME refuses an existing destination, which broke
        // overwriting a remote file (notably the edit → write-back flow, which renames a staged temp
        // over the original). `SftpVfs::rename` must remove the existing file first and succeed.
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_file("/new.txt", b"new")
                .with_file("/existing.txt", b"old"),
        );
        vfs.rename(&p("/new.txt"), &p("/existing.txt"))
            .await
            .expect("rename must overwrite an existing destination");
        // The source is gone and the destination now holds the source's content.
        assert!(matches!(
            vfs.stat(&p("/new.txt")).await,
            Err(VfsError::NotFound(_))
        ));
        let mut rh = vfs.open_read(&p("/existing.txt"), None).await.unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "new", "destination has the source's bytes");
        // The move-aside backup was cleaned up on success (no debris left).
        assert!(matches!(
            vfs.stat(&p("/.cairn-rename-bak-existing.txt")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn rename_restores_the_original_when_the_rename_fails() {
        // Data-safety: if the rename fails *after* the existing destination was moved aside (a
        // mid-operation drop), the original must be restored — never leaving both copies gone.
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_file("/new.txt", b"new")
                .with_file("/existing.txt", b"old")
                .with_failing_rename_to("/existing.txt"),
        );
        let err = vfs.rename(&p("/new.txt"), &p("/existing.txt")).await;
        assert!(err.is_err(), "the injected failure must surface");
        // The original content is restored at the destination (not lost)…
        let mut rh = vfs.open_read(&p("/existing.txt"), None).await.unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(
            out, "old",
            "the original was restored after the failed rename"
        );
        // …the source is untouched, and no backup debris remains.
        assert!(vfs.stat(&p("/new.txt")).await.is_ok(), "source preserved");
        assert!(matches!(
            vfs.stat(&p("/.cairn-rename-bak-existing.txt")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn rename_onto_an_existing_directory_is_rejected_and_leaves_it() {
        // A file must not overwrite (or delete the contents of) a directory: the dir is left in place
        // and the rename is rejected (matching SFTP/POSIX).
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_file("/f.txt", b"f")
                .with_dir("/d")
                .with_file("/d/keep.txt", b"k"),
        );
        assert!(vfs.rename(&p("/f.txt"), &p("/d")).await.is_err());
        // The directory and its contents are intact; the source is untouched.
        assert!(vfs.stat(&p("/d")).await.unwrap().is_dir());
        assert!(vfs.stat(&p("/d/keep.txt")).await.is_ok());
        assert!(vfs.stat(&p("/f.txt")).await.is_ok());
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
    async fn list_recovers_dir_kind_when_readdir_omits_types() {
        // Some SFTP servers don't send type/permission attrs in READDIR, so every entry looks like a
        // plain file. `list` must stat those back to their true kind — otherwise a directory renders
        // (and, worse, deletes) as a file.
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_dir("/d")
                .with_dir("/d/sub")
                .with_file("/d/a.txt", b"a")
                .hiding_readdir_types(),
        );
        let mut s = vfs.list(&p("/d"), ListOpts { all: true });
        let page = s.next().await.unwrap().unwrap();
        let kind = |n: &str| page.entries.iter().find(|e| e.name == n).unwrap().is_dir();
        assert!(kind("sub"), "a subdir must be recovered as a directory");
        assert!(!kind("a.txt"), "a file stays a file");
    }

    #[tokio::test]
    async fn recursive_remove_survives_type_less_readdir() {
        // Regression: against a server that omits READDIR type bits, a recursive delete used to
        // `remove_file` its subdirectories (which fails), stranding the tree. It must now fully
        // remove a nested, hidden-only subtree.
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_dir("/d")
                .with_dir("/d/.git")
                .with_dir("/d/.git/objects")
                .with_file("/d/.git/config", b"c")
                .with_file("/d/.git/objects/x", b"x")
                .hiding_readdir_types(),
        );
        vfs.remove(&p("/d"), Recurse::Yes)
            .await
            .expect("recursive remove must clean a type-less tree");
        assert!(matches!(
            vfs.stat(&p("/d")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn walk_delete_survives_type_less_readdir() {
        // The app-edge delete walk (list with `all: true`, then `remove(Recurse::No)` per entry,
        // dirs deepest-first) must also fully remove a tree on a type-less server, since it drives
        // recursion off the listing's kinds.
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_dir("/d")
                .with_dir("/d/.hidden")
                .with_file("/d/.hidden/deep", b"x")
                .with_file("/d/visible.txt", b"v")
                .hiding_readdir_types(),
        );
        // Replicate the walk: DFS, files popped-and-removed, dirs recorded and removed deepest-first.
        let mut stack = vec![(p("/d"), vfs.stat(&p("/d")).await.unwrap().is_dir())];
        let mut dirs_post = Vec::new();
        while let Some((path, is_dir)) = stack.pop() {
            if is_dir {
                dirs_post.push(path.clone());
                let mut s = vfs.list(&path, ListOpts { all: true });
                while let Some(page) = s.next().await {
                    for e in page.unwrap().entries {
                        stack.push((path.join(&e.name).unwrap(), e.is_dir()));
                    }
                }
            } else {
                vfs.remove(&path, Recurse::No).await.unwrap();
            }
        }
        for d in dirs_post.iter().rev() {
            vfs.remove(d, Recurse::No).await.unwrap();
        }
        assert!(matches!(
            vfs.stat(&p("/d")).await,
            Err(VfsError::NotFound(_))
        ));
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
