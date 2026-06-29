//! Streaming handles bridging guest `read-stream`/`write-sink` resources to Cairn's async I/O traits.
//!
//! Each resource is `!Send` and lives on the plugin thread (see [`crate::bridge`]); these handles are
//! the `Send` async faces the rest of Cairn sees, talking to the thread by [`ResourceId`].
//! [`PluginReadHandle`] is a [`tokio::io::AsyncRead`] (one [`PluginMsg::ReadChunk`] per poll that
//! needs bytes; empty chunk = EOF). [`PluginWriteHandle`] is a [`cairn_vfs::WriteSink`]. Both free
//! their guest resource on drop, so a handle abandoned mid-stream never leaks.

use crate::bridge::{
    map_entry_checked, plugin_dead_error, plugin_error_to_vfs, plugin_stream_limit_error,
    to_vfs_error, PluginMsg, ResourceId,
};
use crate::component::WitVfsError;
use crate::PluginError;
use bytes::Bytes;
use cairn_types::Entry;
use cairn_vfs::{VfsError, WriteSink};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::mpsc;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::oneshot;

/// The shape of a `ReadChunk` reply: an outer [`PluginError`] (host/trap) wrapping the guest result.
type ChunkReply = Result<Result<Vec<u8>, WitVfsError>, PluginError>;

/// Lower bound on bytes requested per chunk, so a caller using a small buffer doesn't force one
/// plugin-thread round-trip per few bytes. Anything beyond the caller's buffer is buffered locally.
const MIN_CHUNK: usize = 64 * 1024;

/// An [`AsyncRead`] backed by a guest read stream, identified by [`ResourceId`].
pub(crate) struct PluginReadHandle {
    id: ResourceId,
    tx: Arc<mpsc::Sender<PluginMsg>>,
    /// Bytes received from the guest but not yet handed to the caller.
    buf: Vec<u8>,
    /// Read cursor into `buf`.
    pos: usize,
    /// Total bytes received so far, bounded by `max_total`.
    received: u64,
    /// Hard cap on total bytes this stream may yield (cuts off a guest that never reports EOF).
    max_total: u64,
    /// Set once the guest reports EOF or an error; no further chunks are requested.
    eof: bool,
    /// An outstanding `ReadChunk` request whose reply we're awaiting.
    in_flight: Option<oneshot::Receiver<ChunkReply>>,
}

impl PluginReadHandle {
    pub(crate) fn new(id: ResourceId, tx: Arc<mpsc::Sender<PluginMsg>>, max_total: u64) -> Self {
        Self {
            id,
            tx,
            buf: Vec::new(),
            pos: 0,
            received: 0,
            max_total,
            eof: false,
            in_flight: None,
        }
    }
}

impl AsyncRead for PluginReadHandle {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        dst: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // A zero-capacity buffer is not an EOF request; nothing to do (and don't fire an IPC call).
        if dst.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            // 1. Serve from the buffer first.
            if this.pos < this.buf.len() {
                let n = (this.buf.len() - this.pos).min(dst.remaining());
                dst.put_slice(&this.buf[this.pos..this.pos + n]);
                this.pos += n;
                return Poll::Ready(Ok(()));
            }
            // 2. Buffer drained — at EOF we're done (leave `dst` unfilled).
            if this.eof {
                return Poll::Ready(Ok(()));
            }
            // 3. Take the in-flight request, or start one. Owning it locally (rather than borrowing
            //    `this.in_flight`) lets us mutate `this` freely in the match arms below.
            let mut rx = match this.in_flight.take() {
                Some(rx) => rx,
                None => {
                    // Request at least `MIN_CHUNK` (extra is buffered) to bound round-trips.
                    let max_bytes =
                        u32::try_from(dst.remaining().max(MIN_CHUNK)).unwrap_or(u32::MAX);
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if this
                        .tx
                        .send(PluginMsg::ReadChunk {
                            id: this.id,
                            max_bytes,
                            reply: reply_tx,
                        })
                        .is_err()
                    {
                        this.eof = true;
                        return Poll::Ready(Err(io::Error::other(plugin_dead_error())));
                    }
                    reply_rx
                }
            };
            // 4. Poll the reply; re-stash it if still pending.
            match Pin::new(&mut rx).poll(cx) {
                Poll::Pending => {
                    this.in_flight = Some(rx);
                    return Poll::Pending;
                }
                Poll::Ready(Err(_)) => {
                    this.eof = true;
                    return Poll::Ready(Err(io::Error::other(plugin_dead_error())));
                }
                Poll::Ready(Ok(Err(pe))) => {
                    this.eof = true;
                    return Poll::Ready(Err(io::Error::other(plugin_error_to_vfs(pe))));
                }
                Poll::Ready(Ok(Ok(Err(we)))) => {
                    this.eof = true;
                    return Poll::Ready(Err(io::Error::other(to_vfs_error(we))));
                }
                Poll::Ready(Ok(Ok(Ok(chunk)))) => {
                    if chunk.is_empty() {
                        this.eof = true;
                        return Poll::Ready(Ok(()));
                    }
                    this.received = this.received.saturating_add(chunk.len() as u64);
                    if this.received > this.max_total {
                        this.eof = true;
                        return Poll::Ready(Err(io::Error::other(plugin_stream_limit_error(
                            this.max_total,
                        ))));
                    }
                    this.buf = chunk;
                    this.pos = 0;
                    // Loop back to serve from the freshly filled buffer.
                }
            }
        }
    }
}

impl Drop for PluginReadHandle {
    fn drop(&mut self) {
        // Best-effort: free the guest resource. The thread may already be gone.
        let _ = self.tx.send(PluginMsg::CloseRead { id: self.id });
    }
}

/// A [`WriteSink`] backed by a guest write sink, identified by [`ResourceId`].
pub(crate) struct PluginWriteHandle {
    id: ResourceId,
    tx: Arc<mpsc::Sender<PluginMsg>>,
    /// Set once `finish`/`abort` has consumed the sink, so `Drop` doesn't send a spurious abort.
    done: bool,
}

impl PluginWriteHandle {
    pub(crate) fn new(id: ResourceId, tx: Arc<mpsc::Sender<PluginMsg>>) -> Self {
        Self {
            id,
            tx,
            done: false,
        }
    }
}

#[async_trait::async_trait]
impl WriteSink for PluginWriteHandle {
    async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), VfsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PluginMsg::WriteChunk {
                id: self.id,
                chunk, // `Bytes` moves through the channel — no copy
                reply: reply_tx,
            })
            .map_err(|_| plugin_dead_error())?;
        reply_rx
            .await
            .map_err(|_| plugin_dead_error())?
            .map_err(plugin_error_to_vfs)?
            .map_err(to_vfs_error)
    }

    async fn finish(mut self: Box<Self>) -> Result<Entry, VfsError> {
        self.done = true; // committing — suppress the Drop-time abort
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PluginMsg::FinishWrite {
                id: self.id,
                reply: reply_tx,
            })
            .map_err(|_| plugin_dead_error())?;
        let entry = reply_rx
            .await
            .map_err(|_| plugin_dead_error())?
            .map_err(plugin_error_to_vfs)?
            .map_err(to_vfs_error)?;
        // The guest's returned entry name is untrusted (traversal/injection); reject a bad one.
        map_entry_checked(entry)
    }

    async fn abort(mut self: Box<Self>) {
        self.done = true; // explicit abort — Drop must not send a second one
        let _ = self.tx.send(PluginMsg::AbortWrite { id: self.id });
    }
}

impl Drop for PluginWriteHandle {
    fn drop(&mut self) {
        // A handle dropped without `finish`/`abort` must not silently commit a partial write — abort
        // and free the guest resource. No-op if `finish`/`abort` already ran.
        if !self.done {
            let _ = self.tx.send(PluginMsg::AbortWrite { id: self.id });
        }
    }
}
