//! The Cairn transfer engine.
//!
//! Moves bytes within and across backends by composing two `Arc<dyn Vfs>` (source and destination).
//! This is the only place cross-backend logic lives, so "pod → S3" is the same code as
//! "local → local". A same-connection server-side copy is used as a fast path; otherwise data is
//! streamed through a bounded buffer with cooperative cancellation. See `docs/LLD.md` §7 and
//! RFC-0002.

use bytes::Bytes;
use cairn_types::{Caps, Entry, VfsPath};
use cairn_vfs::{ListOpts, Recurse, Vfs, VfsError, WriteOpts};
use futures::StreamExt;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

mod error;
pub use error::TransferError;

/// The chunk size used by the stream-through copy path.
const CHUNK: usize = 1 << 20; // 1 MiB

/// Whether a transfer copies or moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOp {
    /// Copy the source, leaving it in place.
    Copy,
    /// Move the source (copy, verify, then delete the original).
    Move,
}

/// What to do when a destination entry already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Skip the item.
    Skip,
    /// Overwrite the destination.
    Overwrite,
    /// Write to a non-colliding renamed path (`name (1)`).
    Rename,
    /// Overwrite only if the source is newer.
    NewerWins,
    /// Defer to the caller (the engine returns [`TransferError::Conflict`]).
    Prompt,
}

/// Post-transfer verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyPolicy {
    /// No verification.
    None,
    /// Verify the destination size matches the bytes written.
    Size,
}

/// Parameters for a transfer.
#[derive(Debug, Clone, Copy)]
pub struct TransferSpec {
    /// Copy or move.
    pub op: TransferOp,
    /// How to handle conflicts.
    pub conflict: ConflictPolicy,
    /// Verification policy.
    pub verify: VerifyPolicy,
}

impl Default for TransferSpec {
    fn default() -> Self {
        Self {
            op: TransferOp::Copy,
            conflict: ConflictPolicy::Overwrite,
            verify: VerifyPolicy::Size,
        }
    }
}

/// A summary of a completed transfer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransferOutcome {
    /// Number of files copied.
    pub files: u64,
    /// Number of directories created.
    pub dirs: u64,
    /// Number of items skipped due to conflict policy.
    pub skipped: u64,
    /// Total bytes transferred.
    pub bytes: u64,
}

/// Run a transfer of `items` (source path → destination path) from `src` to `dst`.
///
/// `progress` is called with the number of bytes written by each chunk. Cancellation is cooperative:
/// the token is checked between chunks and between items; an in-flight write is aborted.
///
/// # Errors
/// Returns [`TransferError`] on the first failing item (I/O, conflict under `Prompt`, or cancellation).
pub async fn run_transfer(
    src: &Arc<dyn Vfs>,
    dst: &Arc<dyn Vfs>,
    items: &[(VfsPath, VfsPath)],
    spec: TransferSpec,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<TransferOutcome, TransferError> {
    let mut outcome = TransferOutcome::default();
    for (from, to) in items {
        if cancel.is_cancelled() {
            return Err(TransferError::Cancelled(outcome));
        }
        // On cancellation, report the work actually completed so far (the nested marker carries a
        // placeholder outcome; the accumulated `outcome` here is the real one).
        if let Err(e) = transfer_one(src, dst, from, to, spec, cancel, progress, &mut outcome).await
        {
            return match e {
                TransferError::Cancelled(_) => Err(TransferError::Cancelled(outcome)),
                other => Err(other),
            };
        }
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
async fn transfer_one(
    src: &Arc<dyn Vfs>,
    dst: &Arc<dyn Vfs>,
    from: &VfsPath,
    to: &VfsPath,
    spec: TransferSpec,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64) + Send),
    outcome: &mut TransferOutcome,
) -> Result<(), TransferError> {
    // Same-connection move with rename support: a single atomic rename.
    if spec.op == TransferOp::Move
        && src.connection() == dst.connection()
        && src.caps_at(from).contains(Caps::RENAME)
    {
        src.rename(from, to).await?;
        outcome.files += 1;
        return Ok(());
    }

    copy_tree(src, dst, from, to, spec, cancel, progress, outcome).await?;

    if spec.op == TransferOp::Move {
        src.remove(from, Recurse::Yes).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn copy_tree(
    src: &Arc<dyn Vfs>,
    dst: &Arc<dyn Vfs>,
    from: &VfsPath,
    to: &VfsPath,
    spec: TransferSpec,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64) + Send),
    outcome: &mut TransferOutcome,
) -> Result<(), TransferError> {
    let mut stack: VecDeque<(VfsPath, VfsPath)> = VecDeque::new();
    stack.push_back((from.clone(), to.clone()));

    while let Some((f, t)) = stack.pop_back() {
        if cancel.is_cancelled() {
            // INVARIANT: this placeholder outcome is always replaced by `run_transfer` with the
            // real accumulated outcome; these private helpers never surface `Cancelled` to callers.
            return Err(TransferError::Cancelled(TransferOutcome::default()));
        }
        let meta = src.stat(&f).await?;
        if meta.is_dir() {
            match dst.create_dir(&t).await {
                Ok(()) => outcome.dirs += 1,
                Err(VfsError::AlreadyExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
            let mut stream = src.list(&f, ListOpts { all: true });
            while let Some(page) = stream.next().await {
                for entry in page?.entries {
                    stack.push_back((f.join(&entry.name)?, t.join(&entry.name)?));
                }
            }
        } else {
            copy_file(src, dst, &f, &t, spec, cancel, progress, outcome).await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn copy_file(
    src: &Arc<dyn Vfs>,
    dst: &Arc<dyn Vfs>,
    from: &VfsPath,
    to: &VfsPath,
    spec: TransferSpec,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(u64) + Send),
    outcome: &mut TransferOutcome,
) -> Result<(), TransferError> {
    // Resolve conflicts against the destination before writing.
    let target = match resolve_conflict(src, dst, from, to, spec.conflict).await? {
        Resolution::Write { path, overwrite } => (path, overwrite),
        Resolution::Skip => {
            outcome.skipped += 1;
            return Ok(());
        }
    };
    let (to, overwrite) = target;

    // Same-connection server-side copy fast path.
    if src.connection() == dst.connection() && src.caps_at(from).contains(Caps::COPY_SERVER) {
        src.copy_within(from, &to).await?;
        let written = dst.stat(&to).await?.size.unwrap_or(0);
        progress(written);
        outcome.files += 1;
        outcome.bytes += written;
        return Ok(());
    }

    let src_size = src.stat(from).await?.size;
    let mut reader = src.open_read(from, None).await?;
    let mut writer = dst
        .open_write(
            &to,
            WriteOpts {
                overwrite,
                size_hint: src_size,
            },
        )
        .await?;

    let mut buf = vec![0u8; CHUNK];
    let mut written: u64 = 0;
    loop {
        if cancel.is_cancelled() {
            writer.abort().await;
            // INVARIANT: this placeholder outcome is always replaced by `run_transfer` with the
            // real accumulated outcome; these private helpers never surface `Cancelled` to callers.
            return Err(TransferError::Cancelled(TransferOutcome::default()));
        }
        let n = reader.read(&mut buf).await.map_err(VfsError::Io)?;
        if n == 0 {
            break;
        }
        writer
            .write_chunk(Bytes::copy_from_slice(&buf[..n]))
            .await?;
        written += n as u64;
        progress(n as u64);
    }
    let entry: Entry = writer.finish().await?;

    if spec.verify == VerifyPolicy::Size {
        let dst_size = entry.size.or(dst.stat(&to).await.ok().and_then(|e| e.size));
        if let Some(ds) = dst_size {
            if ds != written {
                return Err(TransferError::VerifyFailed(to.clone()));
            }
        }
    }

    outcome.files += 1;
    outcome.bytes += written;
    Ok(())
}

enum Resolution {
    Write { path: VfsPath, overwrite: bool },
    Skip,
}

async fn resolve_conflict(
    src: &Arc<dyn Vfs>,
    dst: &Arc<dyn Vfs>,
    from: &VfsPath,
    to: &VfsPath,
    policy: ConflictPolicy,
) -> Result<Resolution, TransferError> {
    let existing = match dst.stat(to).await {
        Ok(e) => Some(e),
        Err(VfsError::NotFound(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let Some(existing) = existing else {
        return Ok(Resolution::Write {
            path: to.clone(),
            overwrite: false,
        });
    };
    match policy {
        ConflictPolicy::Overwrite => Ok(Resolution::Write {
            path: to.clone(),
            overwrite: true,
        }),
        ConflictPolicy::Skip => Ok(Resolution::Skip),
        ConflictPolicy::Prompt => Err(TransferError::Conflict(to.clone())),
        ConflictPolicy::NewerWins => {
            let src_m = src.stat(from).await?.modified;
            match (src_m, existing.modified) {
                (Some(s), Some(d)) if s > d => Ok(Resolution::Write {
                    path: to.clone(),
                    overwrite: true,
                }),
                _ => Ok(Resolution::Skip),
            }
        }
        ConflictPolicy::Rename => Ok(Resolution::Write {
            path: unique_name(dst, to).await?,
            overwrite: false,
        }),
    }
}

/// Find a non-colliding destination path by appending ` (n)` before the extension-less name.
async fn unique_name(dst: &Arc<dyn Vfs>, to: &VfsPath) -> Result<VfsPath, TransferError> {
    let parent = to.parent().unwrap_or_else(VfsPath::root);
    let base = to.file_name().unwrap_or("file").to_owned();
    for n in 1..=9999 {
        let candidate = parent.join(&format!("{base} ({n})"))?;
        match dst.stat(&candidate).await {
            Err(VfsError::NotFound(_)) => return Ok(candidate),
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err(TransferError::Conflict(to.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_types::ConnectionId;
    use cairn_vfs::mock::MockVfs;

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    fn noop(_b: u64) {}

    async fn read_file(vfs: &Arc<dyn Vfs>, path: &str) -> String {
        use tokio::io::AsyncReadExt;
        let mut rh = vfs.open_read(&p(path), None).await.unwrap();
        let mut s = String::new();
        rh.read_to_string(&mut s).await.unwrap();
        s
    }

    fn cross() -> (Arc<dyn Vfs>, Arc<dyn Vfs>) {
        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_dir("/d")
                .with_file("/d/a.txt", b"hello")
                .with_file("/d/b.txt", b"world")
                .with_file("/top.txt", b"top"),
        );
        let dst: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(2)));
        (src, dst)
    }

    #[tokio::test]
    async fn copy_single_file_cross_backend() {
        let (src, dst) = cross();
        let cancel = CancellationToken::new();
        let mut bytes = 0u64;
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/top.txt"), p("/top.txt"))],
            TransferSpec::default(),
            &cancel,
            &mut |b| bytes += b,
        )
        .await
        .unwrap();
        assert_eq!(out.files, 1);
        assert_eq!(out.bytes, 3);
        assert_eq!(bytes, 3);
        assert_eq!(read_file(&dst, "/top.txt").await, "top");
    }

    #[tokio::test]
    async fn copy_directory_tree() {
        let (src, dst) = cross();
        let cancel = CancellationToken::new();
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/d"), p("/d"))],
            TransferSpec::default(),
            &cancel,
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(out.files, 2);
        assert_eq!(read_file(&dst, "/d/a.txt").await, "hello");
        assert_eq!(read_file(&dst, "/d/b.txt").await, "world");
    }

    #[tokio::test]
    async fn move_deletes_source() {
        let (src, dst) = cross();
        let cancel = CancellationToken::new();
        let spec = TransferSpec {
            op: TransferOp::Move,
            ..TransferSpec::default()
        };
        run_transfer(
            &src,
            &dst,
            &[(p("/top.txt"), p("/top.txt"))],
            spec,
            &cancel,
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(read_file(&dst, "/top.txt").await, "top");
        assert!(matches!(
            src.stat(&p("/top.txt")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn conflict_skip_and_prompt() {
        let (src, dst) = cross();
        // pre-existing destination
        let dst = dst;
        run_transfer(
            &src,
            &dst,
            &[(p("/top.txt"), p("/x.txt"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &mut noop,
        )
        .await
        .unwrap();

        // Skip: existing target, nothing copied.
        let spec = TransferSpec {
            conflict: ConflictPolicy::Skip,
            ..TransferSpec::default()
        };
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/d/a.txt"), p("/x.txt"))],
            spec,
            &CancellationToken::new(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(out.skipped, 1);
        assert_eq!(read_file(&dst, "/x.txt").await, "top"); // unchanged

        // Prompt: returns Conflict.
        let spec = TransferSpec {
            conflict: ConflictPolicy::Prompt,
            ..TransferSpec::default()
        };
        let res = run_transfer(
            &src,
            &dst,
            &[(p("/d/a.txt"), p("/x.txt"))],
            spec,
            &CancellationToken::new(),
            &mut noop,
        )
        .await;
        assert!(matches!(res, Err(TransferError::Conflict(_))));
    }

    #[tokio::test]
    async fn conflict_rename_writes_new_path() {
        let (src, dst) = cross();
        run_transfer(
            &src,
            &dst,
            &[(p("/top.txt"), p("/x.txt"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &mut noop,
        )
        .await
        .unwrap();
        let spec = TransferSpec {
            conflict: ConflictPolicy::Rename,
            ..TransferSpec::default()
        };
        run_transfer(
            &src,
            &dst,
            &[(p("/d/a.txt"), p("/x.txt"))],
            spec,
            &CancellationToken::new(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(read_file(&dst, "/x.txt").await, "top"); // original kept
        assert_eq!(read_file(&dst, "/x.txt (1)").await, "hello"); // renamed copy
    }

    #[tokio::test]
    async fn cancelled_before_start() {
        let (src, dst) = cross();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let res = run_transfer(
            &src,
            &dst,
            &[(p("/top.txt"), p("/top.txt"))],
            TransferSpec::default(),
            &cancel,
            &mut noop,
        )
        .await;
        // Cancelled before any item ran → outcome reports nothing done.
        assert!(matches!(
            res,
            Err(TransferError::Cancelled(o)) if o == TransferOutcome::default()
        ));
    }

    #[tokio::test]
    async fn cancel_after_first_item_reports_partial_outcome() {
        let (src, dst) = cross();
        // Each small file is one chunk → one progress call. Cancel on the *second* call: the first
        // item is fully copied, the second is aborted mid-chunk. The reported outcome should reflect
        // exactly the completed first item.
        let cancel = CancellationToken::new();
        let mut calls = 0u32;
        let mut on_progress = |_n: u64| {
            calls += 1;
            if calls == 2 {
                cancel.cancel();
            }
        };
        let res = run_transfer(
            &src,
            &dst,
            &[(p("/top.txt"), p("/a.txt")), (p("/top.txt"), p("/b.txt"))],
            TransferSpec::default(),
            &cancel,
            &mut on_progress,
        )
        .await;
        match res {
            Err(TransferError::Cancelled(o)) => {
                assert_eq!(o.files, 1, "first item completed before cancel");
                assert!(o.bytes > 0);
            }
            _ => panic!("expected Cancelled with a partial outcome"),
        }
        // The first destination was written; the second was aborted (MockVfs::abort removes the
        // partial entry, so its stat must fail).
        assert!(dst.stat(&p("/a.txt")).await.is_ok());
        assert!(dst.stat(&p("/b.txt")).await.is_err());
    }

    #[tokio::test]
    async fn cancelled_move_reports_only_fully_moved_items() {
        let (src, dst) = cross(); // cross-backend → move = copy then remove source
        let cancel = CancellationToken::new();
        let mut calls = 0u32;
        let mut on_progress = |_n: u64| {
            calls += 1;
            if calls == 2 {
                cancel.cancel();
            }
        };
        let spec = TransferSpec {
            op: TransferOp::Move,
            ..TransferSpec::default()
        };
        let res = run_transfer(
            &src,
            &dst,
            &[(p("/d/a.txt"), p("/a.txt")), (p("/d/b.txt"), p("/b.txt"))],
            spec,
            &cancel,
            &mut on_progress,
        )
        .await;
        match res {
            Err(TransferError::Cancelled(o)) => assert_eq!(o.files, 1),
            _ => panic!("expected Cancelled with one fully-moved file"),
        }
        // Item 1 was fully moved: dest exists, source gone.
        assert!(dst.stat(&p("/a.txt")).await.is_ok());
        assert!(src.stat(&p("/d/a.txt")).await.is_err());
        // Item 2 was cancelled mid-copy: source still intact (no data lost), dest aborted.
        assert!(src.stat(&p("/d/b.txt")).await.is_ok());
        assert!(dst.stat(&p("/b.txt")).await.is_err());
    }
}
