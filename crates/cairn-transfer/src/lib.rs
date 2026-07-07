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
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

mod error;
pub use error::TransferError;

/// The chunk size used by the stream-through copy path.
const CHUNK: usize = 1 << 20; // 1 MiB

/// A progress signal from the engine to its caller. Bytes drive the percentage bar; `Finalizing`
/// marks that a file's bytes are all written and the engine is now flushing/closing (and, under
/// size-verify, re-stat'ing) it — opaque backend work that moves no bytes, so the caller can show an
/// honest 100% + "Finalizing…" instead of a bar that appears stuck just short of done. The next
/// file's first `Bytes` (or the transfer completing) implicitly clears the finalizing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressEvent {
    /// `n` more bytes were written by a chunk (or a whole server-side copy).
    Bytes(u64),
    /// The current file's bytes are all written; `finish()`/verify is running next.
    Finalizing,
}

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
/// `progress` receives a [`ProgressEvent`] per chunk ([`ProgressEvent::Bytes`]) and once per file
/// right before its flush/verify tail ([`ProgressEvent::Finalizing`]). Cancellation is cooperative:
/// the token is checked between chunks and between items; an in-flight write is aborted.
///
/// While `paused` holds `true` the transfer blocks at the next check-point (between items, tree
/// nodes, and chunks) until it flips back to `false` or the token is cancelled. If the `paused`
/// sender is dropped, the transfer treats it as resumed and proceeds.
///
/// # Errors
/// Returns [`TransferError`] on the first failing item (I/O, conflict under `Prompt`, or cancellation).
#[allow(clippy::too_many_arguments)]
pub async fn run_transfer(
    src: &Arc<dyn Vfs>,
    dst: &Arc<dyn Vfs>,
    items: &[(VfsPath, VfsPath)],
    spec: TransferSpec,
    cancel: &CancellationToken,
    paused: &watch::Receiver<bool>,
    progress: &mut (dyn FnMut(ProgressEvent) + Send),
) -> Result<TransferOutcome, TransferError> {
    let mut outcome = TransferOutcome::default();
    for (from, to) in items {
        if !wait_while_paused(paused, cancel).await {
            return Err(TransferError::Cancelled(outcome));
        }
        // On cancellation, report the work actually completed so far (the nested marker carries a
        // placeholder outcome; the accumulated `outcome` here is the real one).
        if let Err(e) = transfer_one(
            src,
            dst,
            from,
            to,
            spec,
            cancel,
            paused,
            progress,
            &mut outcome,
        )
        .await
        {
            return match e {
                TransferError::Cancelled(_) => Err(TransferError::Cancelled(outcome)),
                other => Err(other),
            };
        }
    }
    Ok(outcome)
}

/// Block while the transfer is paused, returning when it resumes. Returns `false` if cancelled (so
/// the caller aborts), `true` to proceed. Deadlock-safe: a cloned `watch` receiver tracks versions,
/// so a resume that races the wait is never lost.
async fn wait_while_paused(paused: &watch::Receiver<bool>, cancel: &CancellationToken) -> bool {
    let mut rx = paused.clone();
    loop {
        // Cancellation always wins, even on the not-paused fast path: this helper replaces the bare
        // `cancel.is_cancelled()` guards at the loop tops, so it must honour the token whether or not
        // a pause is active (otherwise an Esc during a same-connection rename/server-copy — which have
        // no inner chunk loop to re-check — would be ignored).
        if cancel.is_cancelled() {
            return false;
        }
        // `borrow_and_update` reads the latest value and marks it seen, so the next `changed()`
        // waits for the *next* toggle — closing the lost-wakeup window.
        if !*rx.borrow_and_update() {
            return true;
        }
        tokio::select! {
            res = rx.changed() => {
                if res.is_err() {
                    return true; // sender dropped → treat as unpaused
                }
            }
            () = cancel.cancelled() => return false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn transfer_one(
    src: &Arc<dyn Vfs>,
    dst: &Arc<dyn Vfs>,
    from: &VfsPath,
    to: &VfsPath,
    spec: TransferSpec,
    cancel: &CancellationToken,
    paused: &watch::Receiver<bool>,
    progress: &mut (dyn FnMut(ProgressEvent) + Send),
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

    copy_tree(src, dst, from, to, spec, cancel, paused, progress, outcome).await?;

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
    paused: &watch::Receiver<bool>,
    progress: &mut (dyn FnMut(ProgressEvent) + Send),
    outcome: &mut TransferOutcome,
) -> Result<(), TransferError> {
    let mut stack: VecDeque<(VfsPath, VfsPath)> = VecDeque::new();
    stack.push_back((from.clone(), to.clone()));

    while let Some((f, t)) = stack.pop_back() {
        if !wait_while_paused(paused, cancel).await {
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
            copy_file(src, dst, &f, &t, spec, cancel, paused, progress, outcome).await?;
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
    paused: &watch::Receiver<bool>,
    progress: &mut (dyn FnMut(ProgressEvent) + Send),
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

    // Same-connection server-side copy fast path. It writes no bytes through us and the server-side
    // copy can itself be slow (a large same-bucket object), so signal `Finalizing` first — otherwise
    // the whole operation would be invisible until it's already done.
    if src.connection() == dst.connection() && src.caps_at(from).contains(Caps::COPY_SERVER) {
        progress(ProgressEvent::Finalizing);
        src.copy_within(from, &to).await?;
        let written = dst.stat(&to).await?.size.unwrap_or(0);
        progress(ProgressEvent::Bytes(written));
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
        // Pause is checked between chunks (mid-file): block here until resumed or cancelled.
        // `wait_while_paused` checks the token itself (cancel wins even when not paused), so a
        // separate `is_cancelled()` here would be redundant.
        if !wait_while_paused(paused, cancel).await {
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
        progress(ProgressEvent::Bytes(n as u64));
    }
    // Bytes are all read/written; the flush/close (and the size-verify stat below) is opaque backend
    // work that moves no bytes — signal it so the caller shows a real 100% + "Finalizing…" rather
    // than a bar pinned just short of done while a slow remote fsync completes.
    progress(ProgressEvent::Finalizing);
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

    fn noop(_e: ProgressEvent) {}

    /// Sum only the byte events from a progress stream (ignoring `Finalizing`), for tests that assert
    /// on total bytes reported.
    fn add_bytes(acc: &mut u64, e: ProgressEvent) {
        if let ProgressEvent::Bytes(n) = e {
            *acc += n;
        }
    }

    /// A receiver that is never paused. The sender is dropped immediately, which is fine: while the
    /// value is `false`, `wait_while_paused` returns before it ever awaits `changed()`.
    fn never_paused() -> watch::Receiver<bool> {
        watch::channel(false).1
    }

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

    /// A single backend used as both source and destination, so a `Move` takes the same-connection
    /// rename fast-path (no chunk loop) — the path where a missed cancellation check would matter.
    fn same_conn() -> Arc<dyn Vfs> {
        Arc::new(
            MockVfs::new(ConnectionId(7))
                .with_file("/a.txt", b"aaa")
                .with_file("/b.txt", b"bbb"),
        )
    }

    #[tokio::test]
    async fn cancel_before_same_connection_move_does_nothing() {
        // Regression: the per-item cancel check lives in `wait_while_paused`, which must honour a
        // pre-cancelled token even on the not-paused fast path. The rename fast-path has no inner
        // chunk loop, so without that check a cancelled bulk move would still rename every item.
        let vfs = same_conn();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let spec = TransferSpec {
            op: TransferOp::Move,
            ..TransferSpec::default()
        };
        let res = run_transfer(
            &vfs,
            &vfs,
            &[
                (p("/a.txt"), p("/moved-a.txt")),
                (p("/b.txt"), p("/moved-b.txt")),
            ],
            spec,
            &cancel,
            &never_paused(),
            &mut noop,
        )
        .await;
        assert!(matches!(
            res,
            Err(TransferError::Cancelled(o)) if o.files == 0
        ));
        // Neither source was renamed away.
        assert!(vfs.stat(&p("/a.txt")).await.is_ok());
        assert!(vfs.stat(&p("/b.txt")).await.is_ok());
        assert!(vfs.stat(&p("/moved-a.txt")).await.is_err());
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
            &never_paused(),
            &mut |e| add_bytes(&mut bytes, e),
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
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(out.files, 2);
        assert_eq!(read_file(&dst, "/d/a.txt").await, "hello");
        assert_eq!(read_file(&dst, "/d/b.txt").await, "world");
    }

    #[tokio::test]
    async fn emits_one_finalizing_signal_per_file() {
        // The UI relies on exactly one `Finalizing` per file to render the flush/verify tail as an
        // honest 100% instead of a stall. Copying a two-file tree must emit two.
        let (src, dst) = cross();
        let cancel = CancellationToken::new();
        let mut finalizing = 0u32;
        let mut byte_calls = 0u32;
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/d"), p("/d"))],
            TransferSpec::default(),
            &cancel,
            &never_paused(),
            &mut |e| match e {
                ProgressEvent::Finalizing => finalizing += 1,
                ProgressEvent::Bytes(_) => byte_calls += 1,
            },
        )
        .await
        .unwrap();
        assert_eq!(out.files, 2);
        assert_eq!(finalizing, 2, "one Finalizing per file");
        assert!(byte_calls >= 2, "each file's bytes were reported");
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
            &never_paused(),
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
            &never_paused(),
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
            &never_paused(),
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
            &never_paused(),
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
            &never_paused(),
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
            &never_paused(),
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
            &never_paused(),
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
        // Each small file is one chunk → one byte-progress call. Cancel on the *second* byte call
        // (ignoring the per-file `Finalizing` signal): the first item is fully copied, the second is
        // aborted mid-chunk. The reported outcome should reflect exactly the completed first item.
        let cancel = CancellationToken::new();
        let mut calls = 0u32;
        let mut on_progress = |e: ProgressEvent| {
            if matches!(e, ProgressEvent::Bytes(_)) {
                calls += 1;
                if calls == 2 {
                    cancel.cancel();
                }
            }
        };
        let res = run_transfer(
            &src,
            &dst,
            &[(p("/top.txt"), p("/a.txt")), (p("/top.txt"), p("/b.txt"))],
            TransferSpec::default(),
            &cancel,
            &never_paused(),
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
        let mut on_progress = |e: ProgressEvent| {
            if matches!(e, ProgressEvent::Bytes(_)) {
                calls += 1;
                if calls == 2 {
                    cancel.cancel();
                }
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
            &never_paused(),
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

    // Current-thread flavor is required: the `yield_now` below deterministically hands control to the
    // spawned transfer, which runs until it parks on the paused watch and no further.
    #[tokio::test(flavor = "current_thread")]
    async fn pause_holds_then_resume_completes() {
        // Start paused: the engine must block before writing anything, then complete once resumed.
        // The default current-thread runtime makes this deterministic — `yield_now` lets the spawned
        // transfer run up to the point where it parks on the paused watch, and no further.
        let (src, dst) = cross();
        let cancel = CancellationToken::new();
        let (pause_tx, paused) = watch::channel(true);
        let task = {
            let (src, dst) = (src.clone(), dst.clone());
            tokio::spawn(async move {
                run_transfer(
                    &src,
                    &dst,
                    &[(p("/top.txt"), p("/top.txt"))],
                    TransferSpec::default(),
                    &cancel,
                    &paused,
                    &mut noop,
                )
                .await
            })
        };
        // Give the transfer a chance to run; while paused it must not touch the destination.
        tokio::task::yield_now().await;
        assert!(
            dst.stat(&p("/top.txt")).await.is_err(),
            "destination must not be written while paused"
        );
        // Resume → the transfer proceeds to completion.
        pause_tx.send(false).unwrap();
        let out = task.await.unwrap().unwrap();
        assert_eq!(out.files, 1);
        assert_eq!(read_file(&dst, "/top.txt").await, "top");
    }

    // Current-thread flavor: see `pause_holds_then_resume_completes` — `yield_now` parks the spawned
    // transfer on the paused watch before we cancel it.
    #[tokio::test(flavor = "current_thread")]
    async fn pause_then_cancel_aborts() {
        // A paused transfer that is then cancelled must abort with a partial (here, empty) outcome
        // rather than hang — `wait_while_paused` selects on cancellation too.
        let (src, dst) = cross();
        let cancel = CancellationToken::new();
        let (_pause_tx, paused) = watch::channel(true);
        let res = {
            let c = cancel.clone();
            let task = tokio::spawn(async move {
                run_transfer(
                    &src,
                    &dst,
                    &[(p("/top.txt"), p("/top.txt"))],
                    TransferSpec::default(),
                    &c,
                    &paused,
                    &mut noop,
                )
                .await
            });
            tokio::task::yield_now().await;
            cancel.cancel();
            task.await.unwrap()
        };
        assert!(matches!(
            res,
            Err(TransferError::Cancelled(o)) if o == TransferOutcome::default()
        ));
    }
}
