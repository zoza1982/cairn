//! The Cairn transfer engine.
//!
//! Moves bytes within and across backends by composing two `Arc<dyn Vfs>` (source and destination).
//! This is the only place cross-backend logic lives, so "pod → S3" is the same code as
//! "local → local". A same-connection server-side copy is used as a fast path; otherwise data is
//! streamed through a bounded buffer with cooperative cancellation. See `docs/LLD.md` §7 and
//! RFC-0002.

use bytes::Bytes;
use cairn_types::{Caps, Entry, EntryKind, VfsPath};
use cairn_vfs::{CommitMode, ListOpts, Recurse, Vfs, VfsError, WriteOpts};
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

/// A progress signal from the engine to its caller. `Bytes` drives the percentage bar and is only
/// ever emitted for bytes that have actually reached the destination (a [`CommitMode::Streamed`]
/// sink's chunks, or a buffered file *after* its upload completed). `Finalizing` marks that a
/// streamed file's bytes are all written and the engine is now flushing/closing (and, under
/// size-verify, re-stat'ing) it — opaque backend work that moves no bytes, so the caller can show an
/// honest 100% + "Finalizing…" instead of a bar that appears stuck just short of done. The next
/// file's first `Bytes` (or the transfer completing) implicitly clears the finalizing state.
///
/// A [`CommitMode::Buffered`] sink (the object stores today) gets a different vocabulary: its chunks
/// are `Staged`, its `finish()` — the real transfer — is announced with `Uploading`, and one `Bytes`
/// catches the totals up once it returns. `Heartbeat` ticks while *any* `finish()` is awaited so an
/// animation clock can keep moving through a long fsync/CLOSE/upload that emits nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressEvent {
    /// `n` more bytes reached the destination (a streamed chunk, a completed buffered file, or a
    /// whole server-side copy).
    Bytes(u64),
    /// The current (streamed) file's bytes are all written; `finish()`/verify is running next.
    Finalizing,
    /// `n` more bytes were handed to a buffering sink's memory — a memcpy. Never fold this into the
    /// percentage bar or the throughput rate; it is only useful as "N buffered so far".
    Staged(u64),
    /// The current buffered file is fully staged (`n` = its size) and its `finish()` — the actual
    /// transfer — is in flight. No byte-level progress exists until it returns, then `Bytes(n)`
    /// follows (or an error and the file contributes nothing). Mutually exclusive with `Finalizing`
    /// for a given file.
    Uploading(u64),
    /// A ~120 ms timer tick while the engine awaits an opaque `finish()` (or server-side copy) that
    /// emits no bytes of its own. Carries no state: the caller re-signals whatever tail phase it is
    /// already in so its marquee advances.
    Heartbeat,
}

/// How often [`ProgressEvent::Heartbeat`] fires while a `finish()` is awaited. Matches the UI's
/// minimum progress interval so the marquee moves at the same cadence as byte ticks.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

/// Await `fut` while emitting [`ProgressEvent::Heartbeat`] every [`HEARTBEAT_INTERVAL`].
///
/// Cancel-safe by construction: `fut` is pinned once and polled through `&mut` across `select!`
/// iterations, so a `finish(self: Box<Self>)` future is created exactly once and never dropped
/// early; `Interval::tick` losing a race loses nothing. The first tick is scheduled one interval
/// out (not immediately), so an instant `finish` produces no heartbeat at all.
async fn await_with_heartbeat<F: std::future::Future>(
    fut: F,
    progress: &mut (dyn FnMut(ProgressEvent) + Send),
) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
        HEARTBEAT_INTERVAL,
    );
    // A heartbeat is a liveness pulse, not a schedule to catch up on: if the task went unpolled for
    // several intervals, one tick now — not a burst that teleports the marquee several cells.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            out = &mut fut => return out,
            _ = tick.tick() => progress(ProgressEvent::Heartbeat),
        }
    }
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
/// `progress` receives a [`ProgressEvent`] per chunk — [`ProgressEvent::Bytes`] for a
/// [`CommitMode::Streamed`] sink, [`ProgressEvent::Staged`] for a [`CommitMode::Buffered`] one — plus
/// the per-file tail signals ([`ProgressEvent::Finalizing`], or [`ProgressEvent::Uploading`] then a
/// catch-up `Bytes` for a buffered sink) and [`ProgressEvent::Heartbeat`]s while a `finish()` is
/// awaited; see the variant docs. Cancellation is cooperative: the token is checked between chunks,
/// once more between the last chunk and `finish()`, and between items; an in-flight write is aborted.
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
    // Same-connection move with rename support: a single atomic rename. The conflict policy still
    // decides the destination — `rename(2)` replaces an existing file, so taking this path without
    // resolving first silently overwrote what `Skip`/`Prompt`/`Rename` were asked to protect.
    if spec.op == TransferOp::Move
        && src.connection() == dst.connection()
        && src.caps_at(from).contains(Caps::RENAME)
    {
        let to = match resolve_conflict(src, dst, from, to, spec.conflict).await? {
            Resolution::Write { path, .. } => path,
            Resolution::Skip => {
                outcome.skipped += 1;
                return Ok(());
            }
        };
        src.rename(from, &to).await?;
        outcome.files += 1;
        return Ok(());
    }

    // A skip is not a copy: deleting the source afterwards would destroy the only remaining copy of
    // data the destination never received. One skipped leaf spares the whole source tree — losing a
    // move the user asked for is recoverable (repeat it), losing their data is not.
    let skipped_before = outcome.skipped;
    copy_tree(src, dst, from, to, spec, cancel, paused, progress, outcome).await?;

    if spec.op == TransferOp::Move && outcome.skipped == skipped_before {
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
        match meta.kind {
            EntryKind::Dir => {
                // Object stores have no directories: creating one is `Unsupported`, which used to
                // abort the whole transfer before a single byte moved. Ask the backend first.
                if dst.caps_at(&t).contains(Caps::CREATE_DIR) {
                    match dst.create_dir(&t).await {
                        Ok(()) => outcome.dirs += 1,
                        Err(VfsError::AlreadyExists(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                let mut stream = src.list(&f, ListOpts { all: true });
                while let Some(page) = stream.next().await {
                    for entry in page?.entries {
                        stack.push_back((f.join(&entry.name)?, t.join(&entry.name)?));
                    }
                }
            }
            EntryKind::File => {
                copy_file(src, dst, &f, &t, spec, cancel, paused, progress, outcome).await?;
            }
            // Anything that is not a file or a directory is skipped, never opened. Reading a FIFO
            // blocks in the OS until a writer appears — with no cancellation point, so the transfer
            // hung and `Esc` did nothing; a symlink is reported by `stat` (not followed), and
            // opening one that points at a directory failed the whole transfer half-way through.
            // Recreating links needs a `Vfs::symlink` the trait does not have yet, so for now the
            // honest outcome is "skipped", which the summary already reports.
            EntryKind::Symlink | EntryKind::Special | EntryKind::Stream => {
                outcome.skipped += 1;
            }
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
        await_with_heartbeat(src.copy_within(from, &to), progress).await?;
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
    let commit_mode = writer.commit_mode();

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
        // On a mid-file error the destination must be aborted, not just dropped: a streaming sink
        // (SFTP, or anything that opens the remote file at `open_write`) has already created — and
        // partly written — the target, and only `abort` removes it. Without this the failed copy left
        // an orphaned partial file that later looked like a complete one.
        let n = match reader.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                writer.abort().await;
                return Err(VfsError::Io(e).into());
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = writer.write_chunk(Bytes::copy_from_slice(&buf[..n])).await {
            writer.abort().await;
            return Err(e.into());
        }
        written += n as u64;
        // Only a streamed sink's chunk has actually gone anywhere. A buffered sink's chunk is a
        // memcpy; reporting it as `Bytes` is exactly how the bar used to race to 100% before the
        // upload had started.
        progress(match commit_mode {
            CommitMode::Streamed => ProgressEvent::Bytes(n as u64),
            CommitMode::Buffered => ProgressEvent::Staged(n as u64),
        });
    }
    // A cancel that arrived after the last chunk but before `finish()` used to be invisible until
    // `finish()` — of arbitrary duration — returned. For a buffered sink this is also the *only*
    // cheap cancellation point once staging is done (its `finish` is the whole upload).
    if cancel.is_cancelled() {
        writer.abort().await;
        return Err(TransferError::Cancelled(TransferOutcome::default()));
    }
    let entry: Entry = match commit_mode {
        CommitMode::Streamed => {
            // Bytes are all read/written; the flush/close (and the size-verify stat below) is opaque
            // backend work that moves no bytes — signal it so the caller shows a real 100% +
            // "Finalizing…" rather than a bar pinned just short of done while a slow fsync completes.
            progress(ProgressEvent::Finalizing);
            await_with_heartbeat(writer.finish(), progress).await?
        }
        CommitMode::Buffered => {
            // The real transfer starts now. Announce it and keep the caller's clock ticking through
            // it; the cumulative bytes catch up in one jump *after* the verify below, so a file that
            // fails verification is never shown as transferred.
            progress(ProgressEvent::Uploading(written));
            await_with_heartbeat(writer.finish(), progress).await?
        }
    };

    if spec.verify == VerifyPolicy::Size {
        // Ask the *destination* what landed. The `Entry` a sink returns from `finish` typically
        // carries its own byte counter (local, SFTP), which is the same number we just summed — a
        // verify that compares it to `written` can never fail. Fall back to the sink's word only when
        // the stat is unavailable. (The old `entry.size.or(dst.stat(..).await…)` also ran the stat
        // eagerly on every file and then ignored it whenever `entry.size` was `Some`.)
        let dst_size = match dst.stat(&to).await.ok().and_then(|e| e.size) {
            Some(n) => Some(n),
            None => entry.size,
        };
        if let Some(ds) = dst_size {
            if ds != written {
                return Err(TransferError::VerifyFailed(to.clone()));
            }
        }
    }
    if commit_mode == CommitMode::Buffered {
        progress(ProgressEvent::Bytes(written));
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

    /// Collect the whole event stream, in order.
    fn record(
        log: &std::sync::Arc<std::sync::Mutex<Vec<ProgressEvent>>>,
    ) -> impl FnMut(ProgressEvent) + Send {
        let log = log.clone();
        move |e| log.lock().unwrap().push(e)
    }

    /// A buffering sink's chunks are `Staged`, never `Bytes`; the file's real transfer is announced
    /// with `Uploading(size)`; and only after `finish()` returns does one `Bytes(size)` catch the
    /// totals up. `Finalizing` is never emitted for it — the two vocabularies are exclusive.
    #[tokio::test]
    async fn buffered_sink_stages_then_uploads_then_catches_up() {
        // 2.5 chunks so the loop runs three times.
        let data = vec![7u8; (CHUNK * 5) / 2];
        let src: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(1)).with_file("/f", &data));
        let dst: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(2)).with_buffered_writes());
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut record(&log),
        )
        .await
        .unwrap();
        let size = data.len() as u64;
        let events = log.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                ProgressEvent::Staged(CHUNK as u64),
                ProgressEvent::Staged(CHUNK as u64),
                ProgressEvent::Staged(CHUNK as u64 / 2),
                ProgressEvent::Uploading(size),
                ProgressEvent::Bytes(size),
            ]
        );
        assert_eq!(out.bytes, size);
        assert_eq!(read_file(&dst, "/f").await.len(), data.len());
    }

    /// A streamed sink is untouched by the buffered vocabulary: `Bytes` per chunk, one `Finalizing`.
    #[tokio::test]
    async fn streamed_sink_keeps_bytes_then_finalizing() {
        let src: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(1)).with_file("/f", b"abc"));
        let dst: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(2)));
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut record(&log),
        )
        .await
        .unwrap();
        assert_eq!(
            log.lock().unwrap().clone(),
            vec![ProgressEvent::Bytes(3), ProgressEvent::Finalizing]
        );
    }

    /// While a slow `finish()` is awaited the engine emits heartbeats at the UI cadence, so the
    /// caller's marquee keeps moving through an upload/fsync that produces no bytes of its own.
    /// Paused tokio time makes the 500 ms finish instantaneous and the tick count exact.
    #[tokio::test(start_paused = true)]
    async fn heartbeats_tick_while_finish_is_slow() {
        let src: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(1)).with_file("/f", b"abc"));
        let dst: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(2))
                .with_buffered_writes()
                .with_finish_delay(std::time::Duration::from_millis(500)),
        );
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut record(&log),
        )
        .await
        .unwrap();
        let events = log.lock().unwrap().clone();
        let beats = events
            .iter()
            .filter(|e| **e == ProgressEvent::Heartbeat)
            .count();
        // 500 ms / 120 ms → ticks at 120, 240, 360, 480.
        assert_eq!(beats, 4, "{events:?}");
        // Heartbeats sit strictly between `Uploading` and the catch-up `Bytes`.
        let up = events
            .iter()
            .position(|e| matches!(e, ProgressEvent::Uploading(_)))
            .unwrap();
        let done = events
            .iter()
            .rposition(|e| matches!(e, ProgressEvent::Bytes(_)))
            .unwrap();
        assert!(events[up + 1..done]
            .iter()
            .all(|e| *e == ProgressEvent::Heartbeat));
    }

    /// An instant `finish()` produces no heartbeat (the first tick is scheduled one interval out).
    #[tokio::test(start_paused = true)]
    async fn no_heartbeat_for_an_instant_finish() {
        let src: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(1)).with_file("/f", b"abc"));
        let dst: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(2)));
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut record(&log),
        )
        .await
        .unwrap();
        assert!(!log.lock().unwrap().contains(&ProgressEvent::Heartbeat));
    }

    /// A cancel that lands *during the final read* — the `read()` that returns EOF — is invisible to
    /// the between-chunks check (`wait_while_paused` already returned for that iteration), so
    /// without the dedicated check between the last chunk and `finish()` a buffered sink would run
    /// its entire upload after the user pressed Esc. The mock fires the cancel from inside that read.
    /// (Cancelling from a `Staged` callback instead would be caught by the next iteration's
    /// `wait_while_paused` and would not exercise this path.)
    #[tokio::test]
    async fn cancel_during_the_final_read_aborts_instead_of_finishing() {
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_file("/f", b"abc")
                .with_on_eof("/f", move || c2.cancel()),
        );
        let dst_mock = Arc::new(MockVfs::new(ConnectionId(2)).with_buffered_writes());
        let dst: Arc<dyn Vfs> = dst_mock.clone();
        let res = run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            TransferSpec::default(),
            &cancel,
            &never_paused(),
            &mut noop,
        )
        .await;
        assert!(matches!(res, Err(TransferError::Cancelled(_))));
        assert_eq!(dst_mock.aborted_writes(), vec!["/f".to_owned()]);
        assert!(matches!(
            dst.stat(&p("/f")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    /// Regression: `Move` + a policy that skips must not delete the source. `copy_file` returns
    /// `Ok` for a skipped file, so the unconditional `remove` afterwards destroyed the only copy of
    /// data the destination never received.
    #[tokio::test]
    async fn move_never_deletes_a_source_it_skipped() {
        let src: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(1)).with_file("/f", b"source"));
        let dst: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(2)).with_file("/f", b"dest"));
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            TransferSpec {
                op: TransferOp::Move,
                conflict: ConflictPolicy::Skip,
                ..TransferSpec::default()
            },
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(out.skipped, 1);
        assert_eq!(
            read_file(&src, "/f").await,
            "source",
            "the source was destroyed"
        );
        assert_eq!(read_file(&dst, "/f").await, "dest");
    }

    /// One skipped leaf spares the whole source tree: a recursive delete would take the files that
    /// *did* copy along with the one that did not.
    #[tokio::test]
    async fn move_of_a_tree_spares_the_source_when_any_leaf_is_skipped() {
        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_dir("/d")
                .with_file("/d/a.txt", b"aaa")
                .with_file("/d/b.txt", b"bbb"),
        );
        let dst: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(2))
                .with_dir("/d")
                .with_file("/d/b.txt", b"existing"),
        );
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/d"), p("/d"))],
            TransferSpec {
                op: TransferOp::Move,
                conflict: ConflictPolicy::Skip,
                ..TransferSpec::default()
            },
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(out.skipped, 1);
        assert_eq!(read_file(&src, "/d/a.txt").await, "aaa");
        assert_eq!(read_file(&src, "/d/b.txt").await, "bbb");
    }

    /// The same-connection rename fast path must resolve the conflict first: `rename(2)` replaces an
    /// existing destination, so it silently overwrote what every non-`Overwrite` policy protects.
    #[tokio::test]
    async fn same_connection_move_honours_the_conflict_policy() {
        // Skip: the destination keeps its content and the source stays put.
        let vfs: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(7))
                .with_file("/a.txt", b"new")
                .with_file("/b.txt", b"old"),
        );
        let out = run_transfer(
            &vfs,
            &vfs,
            &[(p("/a.txt"), p("/b.txt"))],
            TransferSpec {
                op: TransferOp::Move,
                conflict: ConflictPolicy::Skip,
                ..TransferSpec::default()
            },
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(out.skipped, 1);
        assert_eq!(
            read_file(&vfs, "/b.txt").await,
            "old",
            "the destination was clobbered"
        );
        assert_eq!(read_file(&vfs, "/a.txt").await, "new");

        // Prompt: the caller is asked, not overruled.
        let res = run_transfer(
            &vfs,
            &vfs,
            &[(p("/a.txt"), p("/b.txt"))],
            TransferSpec {
                op: TransferOp::Move,
                conflict: ConflictPolicy::Prompt,
                ..TransferSpec::default()
            },
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await;
        assert!(matches!(res, Err(TransferError::Conflict(_))));

        // Rename: the move lands beside the existing file instead of on top of it.
        let out = run_transfer(
            &vfs,
            &vfs,
            &[(p("/a.txt"), p("/b.txt"))],
            TransferSpec {
                op: TransferOp::Move,
                conflict: ConflictPolicy::Rename,
                ..TransferSpec::default()
            },
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(out.files, 1);
        assert_eq!(read_file(&vfs, "/b.txt").await, "old");
        assert_eq!(read_file(&vfs, "/b.txt (1)").await, "new");
    }

    /// A FIFO/socket/device in the tree must be skipped, never opened: `open_read` on one blocks in
    /// the OS until a writer appears, and the engine has no cancellation point there — the transfer
    /// hung with `Esc` dead. The mock errors on such a read, so reaching it fails this test.
    #[tokio::test]
    async fn a_special_node_is_skipped_not_opened() {
        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_dir("/d")
                .with_file("/d/real.txt", b"data")
                .with_special("/d/pipe"),
        );
        let dst: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(2)));
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/d"), p("/d"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!((out.files, out.skipped), (1, 1));
        assert_eq!(read_file(&dst, "/d/real.txt").await, "data");
        assert!(matches!(
            dst.stat(&p("/d/pipe")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    /// A symlink is reported by `stat`, not followed — so one pointing at a directory used to reach
    /// `copy_file`, whose first read failed and aborted the whole transfer, leaving the destination
    /// half-written. It is skipped, and the rest of the tree still lands.
    #[tokio::test]
    async fn a_symlink_is_skipped_and_the_rest_of_the_tree_still_copies() {
        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_dir("/d")
                .with_symlink("/d/link", "/d/sub")
                .with_dir("/d/sub")
                .with_file("/d/sub/inner.txt", b"inner")
                .with_file("/d/z.txt", b"zzz"),
        );
        let dst: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(2)));
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/d"), p("/d"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!((out.files, out.skipped), (2, 1));
        assert_eq!(read_file(&dst, "/d/z.txt").await, "zzz");
        assert_eq!(read_file(&dst, "/d/sub/inner.txt").await, "inner");
    }

    /// A destination without `CREATE_DIR` (every object store — they have no directories) must not
    /// abort the transfer: the tree is walked and the files land under their prefixes.
    #[tokio::test]
    async fn a_tree_copies_into_a_backend_that_cannot_create_directories() {
        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_dir("/d")
                .with_dir("/d/sub")
                .with_file("/d/sub/f.txt", b"payload"),
        );
        let dst: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(2)).without_create_dir());
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/d"), p("/d"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!((out.files, out.dirs), (1, 0), "no directories are created");
        assert_eq!(read_file(&dst, "/d/sub/f.txt").await, "payload");
    }

    /// The server-side copy fast path (`Caps::COPY_SERVER`) was unreachable from the tests, so its
    /// byte accounting and its `Finalizing`-before-the-copy signal were never checked.
    #[tokio::test]
    async fn same_connection_copy_uses_the_server_side_fast_path() {
        let vfs: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(3))
                .with_copy_server()
                .with_file("/a.txt", b"payload"),
        );
        let mut events = Vec::new();
        let out = run_transfer(
            &vfs,
            &vfs,
            &[(p("/a.txt"), p("/b.txt"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!((out.files, out.bytes), (1, 7));
        assert_eq!(read_file(&vfs, "/b.txt").await, "payload");
        // Announced before the copy runs, so a slow same-bucket copy is not invisible.
        assert_eq!(events.first(), Some(&ProgressEvent::Finalizing));
        assert!(events.contains(&ProgressEvent::Bytes(7)));
    }

    /// `VerifyPolicy::Size` compares what the destination reports against what was written; the
    /// mismatch arm was unreachable while the mock always agreed with itself.
    #[tokio::test]
    async fn size_verify_fails_when_the_destination_disagrees() {
        let src: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(1)).with_file("/f", b"12345"));
        let dst: Arc<dyn Vfs> =
            Arc::new(MockVfs::new(ConnectionId(2)).with_reported_size("/f", 99));
        let res = run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            TransferSpec {
                verify: VerifyPolicy::Size,
                ..TransferSpec::default()
            },
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await;
        assert!(matches!(res, Err(TransferError::VerifyFailed(_))));
    }

    /// `NewerWins` needs timestamps on both sides; without them every comparison fell through to
    /// "skip" and the policy's whole point went untested.
    #[tokio::test]
    async fn newer_wins_overwrites_only_when_the_source_is_newer() {
        let older = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let newer = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000);
        let spec = TransferSpec {
            conflict: ConflictPolicy::NewerWins,
            ..TransferSpec::default()
        };

        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_file("/f", b"fresh")
                .with_modified("/f", newer),
        );
        let dst: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(2))
                .with_file("/f", b"stale")
                .with_modified("/f", older),
        );
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            spec,
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!((out.files, out.skipped), (1, 0));
        assert_eq!(read_file(&dst, "/f").await, "fresh");

        // The other way round: the destination is newer, so it is kept.
        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_file("/f", b"stale")
                .with_modified("/f", older),
        );
        let dst: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(2))
                .with_file("/f", b"fresh")
                .with_modified("/f", newer),
        );
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            spec,
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!((out.files, out.skipped), (0, 1));
        assert_eq!(read_file(&dst, "/f").await, "fresh");
    }

    /// Merging into an existing destination directory: the backend reports `AlreadyExists` and the
    /// walk continues. The mock used to succeed silently, leaving this branch unexercised.
    #[tokio::test]
    async fn copying_a_tree_onto_an_existing_directory_merges_into_it() {
        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_dir("/d")
                .with_file("/d/new.txt", b"new"),
        );
        let dst: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(2))
                .with_dir("/d")
                .with_file("/d/kept.txt", b"kept"),
        );
        let out = run_transfer(
            &src,
            &dst,
            &[(p("/d"), p("/d"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(
            (out.files, out.dirs),
            (1, 0),
            "the existing directory is not recounted"
        );
        assert_eq!(read_file(&dst, "/d/new.txt").await, "new");
        assert_eq!(read_file(&dst, "/d/kept.txt").await, "kept");
    }

    /// Regression: a source read that fails mid-file must abort the destination sink, not merely
    /// drop it. A streaming sink has already created (and partly written) the remote file at that
    /// point, and only `abort` removes it — the old `?` left an orphaned partial file behind.
    #[tokio::test]
    async fn read_error_mid_file_aborts_the_destination() {
        let src: Arc<dyn Vfs> = Arc::new(
            MockVfs::new(ConnectionId(1))
                .with_file("/big", &[1u8; 4096])
                .with_read_fault("/big", 100),
        );
        let dst_mock = Arc::new(MockVfs::new(ConnectionId(2)));
        let dst: Arc<dyn Vfs> = dst_mock.clone();
        let res = run_transfer(
            &src,
            &dst,
            &[(p("/big"), p("/big"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await;
        assert!(matches!(res, Err(TransferError::Vfs(VfsError::Io(_)))));
        assert_eq!(dst_mock.aborted_writes(), vec!["/big".to_owned()]);
    }

    /// Same for a destination that rejects a chunk: the sink is aborted before the error propagates.
    #[tokio::test]
    async fn write_error_mid_file_aborts_the_destination() {
        let src: Arc<dyn Vfs> = Arc::new(MockVfs::new(ConnectionId(1)).with_file("/f", b"payload"));
        let dst_mock = Arc::new(MockVfs::new(ConnectionId(2)).with_write_fault("/f"));
        let dst: Arc<dyn Vfs> = dst_mock.clone();
        let res = run_transfer(
            &src,
            &dst,
            &[(p("/f"), p("/f"))],
            TransferSpec::default(),
            &CancellationToken::new(),
            &never_paused(),
            &mut noop,
        )
        .await;
        assert!(matches!(res, Err(TransferError::Vfs(VfsError::Io(_)))));
        assert_eq!(dst_mock.aborted_writes(), vec!["/f".to_owned()]);
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
                // A streamed sink never stages or uploads; a heartbeat only fires on a slow finish.
                ProgressEvent::Staged(_) | ProgressEvent::Uploading(_) => {
                    panic!("streamed sink emitted a buffered-sink event: {e:?}")
                }
                ProgressEvent::Heartbeat => {}
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
