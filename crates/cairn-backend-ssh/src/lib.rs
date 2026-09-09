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
pub use ops::{RemoteEntry, RemoteMeta, SftpOps, SftpWriteStream};
pub use real::RealSftp;

use async_trait::async_trait;
use cairn_types::{Caps, ConnectionId, Entry, EntryKind, Scheme, UnixPerms, VfsPath};
use cairn_vfs::{
    ByteRange, CapabilityProvider, CommitMode, ListOpts, ListPage, ReadHandle, Recurse, Vfs,
    VfsError, WriteHandle, WriteOpts, WriteSink,
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
            let mut size = r.size;
            let mut modified = r.modified;
            let mut mode = r.mode;
            // Some SFTP servers omit the type/permission attrs in READDIR responses, so every entry
            // arrives looking like a plain file (`mode == None`). Left unresolved, a directory would
            // be misclassified — and a recursive delete driven by these kinds would never descend
            // into it, stranding the directory (and its parent) instead of removing it. Recover the
            // true kind with an `lstat` (not `stat`: it must NOT follow symlinks, or a symlink-to-dir
            // would be reclassified as a directory and a recursive delete would follow it and destroy
            // data outside the tree). Reuse the round-trip we're already paying to fill in the size,
            // perms, and mtime the listing lacked. OpenSSH sends the attrs, so this never fires on the
            // common path. NOTE: on a server that omits them, this is one `lstat` per entry — a real
            // cost for very large remote directories, tracked as a follow-up (pipeline/cap the stats).
            if kind == EntryKind::File && r.mode.is_none() {
                if let Ok(child) = dir.join(&r.name) {
                    if let Ok(m) = self.ops.lstat(&child.as_str()).await {
                        kind = m.kind;
                        size = m.size;
                        modified = m.modified;
                        mode = m.mode;
                    }
                }
            }
            let mut e = Entry::new(r.name, kind);
            if kind == EntryKind::File {
                e.size = size;
            }
            e.modified = modified;
            e.perms = mode.map(UnixPerms::from_mode);
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
        // The length hint comes from a `stat`, not from reading the file: the reader streams, so
        // nothing knows the size up front otherwise. Best-effort — the hint is advisory (a display
        // aid for the pager/sniff), so a failed stat must not fail a read that would succeed. Clamped
        // to the requested range so `len_hint` matches what the stream will yield.
        let remote = path.as_str();
        let len_hint = self
            .ops
            .stat(&remote)
            .await
            .ok()
            .and_then(|m| m.size)
            .map(|total| range.map_or(total, |r| r.clamped_len(total)));
        let reader = self.ops.open_read(&remote, range).await?;
        Ok(ReadHandle::new(reader, len_hint))
    }

    async fn open_write(&self, path: &VfsPath, opts: WriteOpts) -> Result<WriteHandle, VfsError> {
        // Honor `overwrite: false` up front (SFTP would happily truncate). A racing creator between
        // this check and the final rename is caught again there — the server rejects a rename onto
        // an existing destination, which `SftpWriteSink::finish` maps back to `AlreadyExists`.
        if !opts.overwrite && self.ops.lstat(&path.as_str()).await.is_ok() {
            return Err(VfsError::AlreadyExists(path.clone()));
        }
        // Stream into a hidden sibling temp and rename onto the target in `finish`. Opening the
        // target itself (CREATE|TRUNCATE) would destroy the user's existing file the moment the copy
        // *started* — so a cancel or a mid-file error, both of which `abort` the sink, would leave
        // them with nothing. With the temp, the original is untouched until the new content is fully
        // committed, and `abort`/a failed `finish` only ever remove the temp.
        let temp = temp_sibling(path)?;
        let stream = self.ops.open_write(&temp.as_str()).await?;
        Ok(WriteHandle::new(Box::new(SftpWriteSink {
            ops: self.ops.clone(),
            stream,
            target: path.clone(),
            temp,
            overwrite: opts.overwrite,
            written: 0,
        })))
    }

    async fn create_dir(&self, path: &VfsPath) -> Result<(), VfsError> {
        match self.ops.create_dir(&path.as_str()).await {
            Ok(()) => Ok(()),
            // OpenSSH's sftp-server reports `mkdir` on an existing path as a generic
            // `SSH_FX_FAILURE` ("Failure"); other servers phrase the same condition differently
            // (e.g. "not found"). The status code alone can't tell "already exists" apart from a real
            // failure, so disambiguate by probing the path: if it is already a directory, report
            // `AlreadyExists` (as the local backend does) — this is what lets a directory-merge copy
            // proceed (the transfer engine tolerates `AlreadyExists`, but aborts on any other error).
            //
            // `lstat`, not `stat`: a *symlink* at the destination must NOT be treated as the directory
            // it points to (that classifies as `Symlink`, so we fall through and abort), otherwise the
            // copy would descend and write the source's children through the link into its target —
            // possibly outside the destination tree. This mirrors the deliberate `lstat` caution in
            // `remove`. Any non-directory outcome (a file, a symlink, or the path having vanished in
            // the small window since `mkdir`) means the original failure stands; surface it unchanged.
            Err(e) => match self.ops.lstat(&path.as_str()).await {
                Ok(m) if m.kind == EntryKind::Dir => Err(VfsError::AlreadyExists(path.clone())),
                _ => Err(e),
            },
        }
    }

    async fn remove(&self, path: &VfsPath, recurse: Recurse) -> Result<(), VfsError> {
        // `lstat`, not `stat`: removing a *symlink* must unlink the link itself, never follow it. A
        // follow-`stat` on a symlink-to-directory would report `Dir` and route us into `remove_dir`
        // /`remove_recursive` against the link's target — deleting data outside the requested path.
        // With `lstat`, a symlink classifies as `Symlink` and falls to the `remove_file` arm (SFTP
        // `SSH_FXP_REMOVE` unlinks the link, leaving the target intact).
        let m = self.ops.lstat(&path.as_str()).await?;
        match (m.kind, recurse) {
            (EntryKind::Dir, Recurse::Yes) => self.remove_recursive(path).await,
            (EntryKind::Dir, Recurse::No) => self.ops.remove_dir(&path.as_str()).await,
            _ => self.ops.remove_file(&path.as_str()).await,
        }
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError> {
        rename_overwriting(&*self.ops, from, to).await
    }
}

/// Rename `from` onto `to`, overwriting an existing *file* at `to` the way POSIX (and every other
/// Cairn backend) does. Shared by [`Vfs::rename`] and the write sink's commit step.
async fn rename_overwriting<O: SftpOps>(
    ops: &O,
    from: &VfsPath,
    to: &VfsPath,
) -> Result<(), VfsError> {
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
    let backup: Option<VfsPath> = match ops.stat(&to_str).await {
        Ok(meta) if meta.kind != EntryKind::Dir => {
            let bname = format!(".cairn-rename-bak-{}", to.file_name().unwrap_or("file"));
            // A `join` failure (an invalid backup name) just skips the backup — the rename below
            // then fails on the still-present destination, leaving it intact.
            match to.parent().unwrap_or_else(VfsPath::root).join(&bname) {
                Ok(bpath) => {
                    // Clear a stale backup (from a prior interrupted rename) so moving the
                    // destination aside can't itself hit an existing-destination rejection.
                    let _ = ops.remove_file(&bpath.as_str()).await;
                    // If moving the destination aside fails, proceed with no backup.
                    if ops.rename(&to_str, &bpath.as_str()).await.is_ok() {
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
    match ops.rename(&from.as_str(), &to_str).await {
        Ok(()) => {
            if let Some(b) = backup {
                let _ = ops.remove_file(&b.as_str()).await; // best-effort cleanup post-success
            }
            Ok(())
        }
        Err(e) => {
            if let Some(b) = backup {
                // Best-effort restore of the original (the destination slot is free again).
                let _ = ops.rename(&b.as_str(), &to_str).await;
            }
            Err(e)
        }
    }
}

/// The hidden sibling a write streams into before being renamed onto `target`. Unique per process
/// and per open, so two concurrent writes to the same target (or a stale temp from a crashed run)
/// can't collide.
fn temp_sibling(target: &VfsPath) -> Result<VfsPath, VfsError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        ".{}.cairn-{}-{}.part",
        target.file_name().unwrap_or("file"),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    target
        .parent()
        .unwrap_or_else(VfsPath::root)
        .join(&name)
        .map_err(|e| VfsError::Backend {
            code: "sftp".to_owned(),
            msg: format!("cannot derive a temp name for {target}: {e}"),
            retryable: false,
        })
}

/// The [`WriteSink`] over an open [`SftpWriteStream`]. Every chunk goes straight to the transport;
/// nothing is buffered here, so the bytes the engine counts after `write_chunk` returns are bytes the
/// server has been handed (bounded by `russh-sftp`'s in-flight `WRITE` window), not a memcpy.
///
/// Writes land in `temp` and are renamed onto `target` by `finish`; `abort` and every failure path
/// in `finish` remove only `temp`, so the pre-existing target survives anything short of a
/// successful commit.
struct SftpWriteSink<O: SftpOps> {
    ops: Arc<O>,
    stream: Box<dyn SftpWriteStream>,
    target: VfsPath,
    temp: VfsPath,
    overwrite: bool,
    written: u64,
}

#[async_trait]
impl<O: SftpOps> WriteSink for SftpWriteSink<O> {
    fn commit_mode(&self) -> CommitMode {
        // Each chunk is on the transport's pipelined WRITE window before `write_all` returns.
        CommitMode::Streamed
    }

    async fn write_chunk(&mut self, chunk: bytes::Bytes) -> Result<(), VfsError> {
        self.stream.write_all(&chunk).await?;
        self.written += chunk.len() as u64;
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<Entry, VfsError> {
        let Self {
            ops,
            stream,
            target,
            temp,
            overwrite,
            written,
        } = *self;
        // A failed flush/CLOSE means the temp is not what we wrote; a failed rename means it never
        // reached the target. Either way remove the temp — once `finish` has returned an error the
        // handle is gone and nobody else can clean up.
        if let Err(e) = stream.finish().await {
            let _ = ops.remove_file(&temp.as_str()).await;
            return Err(e);
        }
        let committed = if overwrite {
            rename_overwriting(&*ops, &temp, &target).await
        } else {
            // A plain rename: the server rejects an existing destination, which here means a file
            // appeared under us since the `open_write` check — report it as such, not as a generic
            // transport failure.
            match ops.rename(&temp.as_str(), &target.as_str()).await {
                Err(_) if ops.lstat(&target.as_str()).await.is_ok() => {
                    Err(VfsError::AlreadyExists(target.clone()))
                }
                other => other,
            }
        };
        if let Err(e) = committed {
            let _ = ops.remove_file(&temp.as_str()).await;
            return Err(e);
        }
        let mut e = Entry::new(target.file_name().unwrap_or(""), EntryKind::File);
        e.size = Some(written);
        Ok(e)
    }

    async fn abort(self: Box<Self>) {
        // Removes the temp (created at open); the target was never touched.
        self.stream.abort().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ops::mock::{MockCall, MockSftp, MOCK_READ_CHUNK};
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
    async fn create_dir_on_existing_dir_reports_already_exists() {
        // Regression: OpenSSH's sftp-server returns a generic SSH_FX_FAILURE for `mkdir` on an
        // existing path, which the backend used to surface as an opaque `Backend` error. The transfer
        // engine only tolerates `AlreadyExists`, so copying a directory onto an existing remote
        // directory aborted the whole copy ("Copy failed: …"). `create_dir` must now map an existing
        // *directory* to `AlreadyExists` so a dir-merge copy proceeds.
        let vfs = backend();
        assert!(
            matches!(
                vfs.create_dir(&p("/d")).await,
                Err(VfsError::AlreadyExists(_))
            ),
            "mkdir on an existing dir must report AlreadyExists"
        );
        // A genuine failure on a path that is *not* an existing directory is surfaced unchanged: `/d`
        // is a dir, but `/top.txt` is a file — mkdir over it must not be masked as AlreadyExists.
        assert!(
            matches!(
                vfs.create_dir(&p("/top.txt")).await,
                Err(VfsError::Backend { .. })
            ),
            "mkdir colliding with a file must surface the real error, not AlreadyExists"
        );
    }

    #[tokio::test]
    async fn create_dir_on_a_symlink_to_a_dir_does_not_report_already_exists() {
        // A symlink at the destination must NOT be resolved to the directory it targets: the recovery
        // uses `lstat`, so a symlink classifies as `Symlink` (not `Dir`) and the original mkdir error
        // is surfaced — the copy then aborts rather than writing the source's children through the
        // link into its target (which could lie outside the destination tree).
        let mock = MockSftp::new()
            .with_dir("/target")
            .with_symlink("/link", "/target");
        let vfs = SftpVfs::new(ConnectionId(1), mock);
        assert!(
            matches!(
                vfs.create_dir(&p("/link")).await,
                Err(VfsError::Backend { .. })
            ),
            "mkdir on a symlink-to-dir must surface the real error, not AlreadyExists"
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

    /// A download must be served as it arrives, not fetched whole and then replayed: the read handle
    /// yields many transport-sized pieces even when the consumer offers a 1 MiB buffer (the transfer
    /// engine's real chunk). The mock caps each poll at `MOCK_READ_CHUNK`, so a whole-file buffer
    /// would show up here as a single oversized read.
    #[tokio::test]
    async fn open_read_streams_in_transport_sized_pieces() {
        let data: Vec<u8> = (0..(3 * MOCK_READ_CHUNK + 100))
            .map(|i| (i % 251) as u8)
            .collect();
        let vfs = SftpVfs::new(ConnectionId(1), MockSftp::new().with_file("/big", &data));
        let mut rh = vfs.open_read(&p("/big"), None).await.unwrap();
        assert_eq!(rh.len_hint(), Some(data.len() as u64));

        let mut buf = vec![0u8; 1 << 20];
        let mut out = Vec::new();
        let mut reads = 0;
        loop {
            let n = rh.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            assert!(n <= MOCK_READ_CHUNK, "a single read returned {n} bytes");
            out.extend_from_slice(&buf[..n]);
            reads += 1;
        }
        assert!(reads > 1, "the file was served in one read — not streaming");
        assert_eq!(out, data);
    }

    /// Every `write_chunk` reaches the transport *before* `finish` — that is what makes the bytes the
    /// engine counts after each chunk real. The mock logs each transport call in order. The stream
    /// targets a hidden `.part` sibling, which `finish` renames onto the real name.
    #[tokio::test]
    async fn open_write_streams_each_chunk_before_finish() {
        let vfs = SftpVfs::new(ConnectionId(1), MockSftp::new());
        let mut wh = vfs
            .open_write(&p("/out.bin"), WriteOpts::default())
            .await
            .unwrap();
        for chunk in [&b"aaaa"[..], &b"bb"[..], &b"cccccc"[..]] {
            wh.write_chunk(bytes::Bytes::copy_from_slice(chunk))
                .await
                .unwrap();
        }
        // Three transport writes, in order, all to the same temp, and no commit yet.
        let calls = vfs.ops.calls();
        let lens: Vec<usize> = calls
            .iter()
            .map(|c| match c {
                MockCall::Write { path, len } => {
                    assert!(
                        path.starts_with("/.out.bin.cairn-") && path.ends_with(".part"),
                        "{path}"
                    );
                    *len
                }
                other => panic!("unexpected {other:?} before finish"),
            })
            .collect();
        assert_eq!(lens, vec![4, 2, 6]);
        // Nothing at the target until commit.
        assert!(matches!(
            vfs.stat(&p("/out.bin")).await,
            Err(VfsError::NotFound(_))
        ));

        assert_eq!(wh.finish().await.unwrap().size, Some(12));
        assert!(matches!(
            vfs.ops.calls().last(),
            Some(MockCall::Finished(t)) if t.ends_with(".part")
        ));
        // Bytes landed in write order, at the target, and the temp is gone.
        let mut rh = vfs.open_read(&p("/out.bin"), None).await.unwrap();
        let mut out = Vec::new();
        rh.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"aaaabbcccccc");
        assert_eq!(vfs.ops.paths(), vec!["/".to_owned(), "/out.bin".to_owned()]);
    }

    /// The temp exists from the moment the write is opened (create/truncate), so an abort — even
    /// after some chunks went out — must leave nothing behind, and must never have touched the target.
    #[tokio::test]
    async fn abort_removes_the_partially_written_temp() {
        let vfs = SftpVfs::new(ConnectionId(1), MockSftp::new());
        let mut wh = vfs
            .open_write(&p("/partial"), WriteOpts::default())
            .await
            .unwrap();
        // The temp exists (empty) right after open — the case a lazy abort would miss.
        assert!(vfs.ops.paths().iter().any(|q| q.ends_with(".part")));
        wh.write_chunk(bytes::Bytes::from_static(b"half"))
            .await
            .unwrap();
        wh.abort().await;
        assert_eq!(vfs.ops.paths(), vec!["/".to_owned()]);
        assert!(matches!(
            vfs.ops.calls().last(),
            Some(MockCall::Aborted(t)) if t.ends_with(".part")
        ));
    }

    /// Regression (found in review): opening the *target* with CREATE|TRUNCATE destroyed the user's
    /// existing file the instant an overwrite copy started, so cancelling it (which aborts the sink)
    /// left them with nothing. The original must survive until the new content is fully committed.
    #[tokio::test]
    async fn cancelled_overwrite_preserves_the_existing_file() {
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new().with_file("/keep.txt", b"original"),
        );
        let mut wh = vfs
            .open_write(
                &p("/keep.txt"),
                WriteOpts {
                    overwrite: true, // what the engine passes for an overwrite copy
                    size_hint: None,
                },
            )
            .await
            .unwrap();
        wh.write_chunk(bytes::Bytes::from_static(b"new-"))
            .await
            .unwrap();
        wh.abort().await; // what a cancel or a mid-file read error does
        let mut rh = vfs.open_read(&p("/keep.txt"), None).await.unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "original");
        assert_eq!(
            vfs.ops.paths(),
            vec!["/".to_owned(), "/keep.txt".to_owned()]
        );
    }

    /// A failed flush/`CLOSE` (now surfaced instead of discarded) must not orphan the temp — the
    /// handle is consumed by `finish`, so nobody else could ever clean it up — and the original at
    /// the target is untouched.
    #[tokio::test]
    async fn failed_finish_removes_the_temp_and_keeps_the_original() {
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_file("/keep.txt", b"original")
                .with_failing_finish_for("keep.txt"),
        );
        let mut wh = vfs
            .open_write(
                &p("/keep.txt"),
                WriteOpts {
                    overwrite: true, // what the engine passes for an overwrite copy
                    size_hint: None,
                },
            )
            .await
            .unwrap();
        wh.write_chunk(bytes::Bytes::from_static(b"new"))
            .await
            .unwrap();
        assert!(matches!(
            wh.finish().await,
            Err(VfsError::Backend { code, .. }) if code == "sftp"
        ));
        assert_eq!(
            vfs.ops.paths(),
            vec!["/".to_owned(), "/keep.txt".to_owned()]
        );
        assert_eq!(vfs.stat(&p("/keep.txt")).await.unwrap().size, Some(8));
    }

    /// `WriteOpts::overwrite == false` is honored (SFTP itself would truncate): an existing target is
    /// `AlreadyExists` at open, with no temp created.
    #[tokio::test]
    async fn open_write_without_overwrite_rejects_an_existing_target() {
        let vfs = SftpVfs::new(ConnectionId(1), MockSftp::new().with_file("/exists", b"x"));
        let res = vfs
            .open_write(
                &p("/exists"),
                WriteOpts {
                    overwrite: false,
                    size_hint: None,
                },
            )
            .await;
        assert!(matches!(res, Err(VfsError::AlreadyExists(_))));
        assert_eq!(vfs.ops.paths(), vec!["/".to_owned(), "/exists".to_owned()]);
        // …and a fresh path works without overwrite.
        let wh = vfs
            .open_write(
                &p("/fresh"),
                WriteOpts {
                    overwrite: false,
                    size_hint: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(wh.finish().await.unwrap().size, Some(0));
        assert_eq!(vfs.stat(&p("/fresh")).await.unwrap().size, Some(0));
    }

    /// Zero chunks then `finish` is a legitimate empty file (CREATE|TRUNCATE semantics), not an error.
    #[tokio::test]
    async fn finish_with_no_chunks_commits_an_empty_file() {
        let vfs = SftpVfs::new(ConnectionId(1), MockSftp::new());
        let wh = vfs
            .open_write(&p("/empty"), WriteOpts::default())
            .await
            .unwrap();
        assert_eq!(wh.finish().await.unwrap().size, Some(0));
        assert_eq!(vfs.stat(&p("/empty")).await.unwrap().size, Some(0));
    }

    /// A ranged read is still streamed, and its length hint is the *clamped* range, not the file size.
    #[tokio::test]
    async fn ranged_read_streams_and_hints_the_clamped_length() {
        let data = vec![7u8; 2 * MOCK_READ_CHUNK];
        let vfs = SftpVfs::new(ConnectionId(1), MockSftp::new().with_file("/f", &data));
        let rh = vfs
            .open_read(
                &p("/f"),
                Some(ByteRange {
                    offset: MOCK_READ_CHUNK as u64,
                    // Overshoots the file: the hint must clamp to what will actually be yielded.
                    len: Some(10 * MOCK_READ_CHUNK as u64),
                }),
            )
            .await
            .unwrap();
        assert_eq!(rh.len_hint(), Some(MOCK_READ_CHUNK as u64));
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
    async fn list_reports_symlink_kind_even_when_readdir_omits_types() {
        // A symlink must never be recovered as its target's kind — otherwise the delete walk would
        // follow it. Under type-less mode the recovery uses `lstat`, so a symlink-to-dir stays a
        // symlink (and a recursive delete unlinks it instead of descending into the target).
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_dir("/d")
                .with_dir("/target")
                .with_file("/target/keep.txt", b"k")
                .with_symlink("/d/link", "/target")
                .hiding_readdir_types(),
        );
        let mut s = vfs.list(&p("/d"), ListOpts { all: true });
        let page = s.next().await.unwrap().unwrap();
        let link = page.entries.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(
            link.kind,
            EntryKind::Symlink,
            "a symlink must not be recovered as a directory"
        );
    }

    #[tokio::test]
    async fn recursive_remove_unlinks_symlink_to_dir_without_touching_target() {
        // Data-safety: deleting a directory that contains a symlink-to-directory must unlink the
        // symlink and leave the *target* (which lives outside the deleted tree) fully intact — even
        // on a type-less server where the recovery stat runs. A follow-`stat` here would recurse into
        // the target and destroy unrelated data.
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_dir("/d")
                .with_file("/d/own.txt", b"o")
                .with_symlink("/d/link", "/outside")
                .with_dir("/outside")
                .with_file("/outside/precious.txt", b"p")
                .hiding_readdir_types(),
        );
        vfs.remove(&p("/d"), Recurse::Yes)
            .await
            .expect("recursive remove");
        // The deleted tree is gone…
        assert!(matches!(
            vfs.stat(&p("/d")).await,
            Err(VfsError::NotFound(_))
        ));
        // …but the symlink target and its contents — outside the tree — survive untouched.
        assert!(vfs.stat(&p("/outside")).await.unwrap().is_dir());
        assert_eq!(
            vfs.stat(&p("/outside/precious.txt")).await.unwrap().size,
            Some(1)
        );
    }

    #[tokio::test]
    async fn recursive_remove_terminates_on_a_symlink_cycle() {
        // A symlink pointing back at an ancestor must not send the recursive delete into an infinite
        // loop: classified (via `lstat`) as a symlink, it is unlinked, never followed. The test
        // completing at all is the assertion that matters.
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_dir("/d")
                .with_dir("/d/sub")
                .with_file("/d/sub/f.txt", b"f")
                .with_symlink("/d/sub/loop", "/d")
                .hiding_readdir_types(),
        );
        vfs.remove(&p("/d"), Recurse::Yes)
            .await
            .expect("recursive remove terminates and succeeds");
        assert!(matches!(
            vfs.stat(&p("/d")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn removing_a_symlink_to_dir_unlinks_it_and_spares_the_target() {
        // Deleting a symlink-to-directory *directly* (the link is the delete root) must unlink the
        // link, never the target — `remove` classifies the root with `lstat`, not follow-`stat`.
        let vfs = SftpVfs::new(
            ConnectionId(1),
            MockSftp::new()
                .with_symlink("/link", "/target")
                .with_dir("/target")
                .with_file("/target/keep.txt", b"k"),
        );
        vfs.remove(&p("/link"), Recurse::Yes)
            .await
            .expect("unlink the symlink");
        assert!(matches!(
            vfs.stat(&p("/link")).await,
            Err(VfsError::NotFound(_))
        ));
        assert!(vfs.stat(&p("/target")).await.unwrap().is_dir());
        assert!(vfs.stat(&p("/target/keep.txt")).await.is_ok());
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
