//! Streaming read/write handles returned by [`Vfs::open_read`](crate::Vfs::open_read) and
//! [`Vfs::open_write`](crate::Vfs::open_write).

use crate::error::VfsError;
use bytes::Bytes;
use cairn_types::Entry;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// A streaming reader. Implements [`tokio::io::AsyncRead`] so it composes with the whole tokio I/O
/// ecosystem (`AsyncReadExt::read_to_end`, `io::copy`, hashers, …).
pub struct ReadHandle {
    inner: Box<dyn AsyncRead + Send + Unpin>,
    len_hint: Option<u64>,
}

impl ReadHandle {
    /// Wrap an async reader, optionally recording a known total length.
    #[must_use]
    pub fn new(inner: Box<dyn AsyncRead + Send + Unpin>, len_hint: Option<u64>) -> Self {
        Self { inner, len_hint }
    }

    /// The total length in bytes, if known up front.
    #[must_use]
    pub fn len_hint(&self) -> Option<u64> {
        self.len_hint
    }
}

impl AsyncRead for ReadHandle {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// When a [`WriteSink`]'s bytes actually reach the destination — the fact the transfer engine needs
/// to report honest progress.
///
/// A sink that buffers must say so. The engine counts a chunk as transferred when `write_chunk`
/// returns; for a `Buffered` sink that would be a memcpy, and the bar would race to 100% while the
/// real upload had not started (that was the object-store backends' behavior before this existed).
/// There is deliberately no default: a new backend has to make the call explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitMode {
    /// `write_chunk` returns only after the chunk has been handed to the destination under the
    /// transport's own backpressure (a socket, a pipelined protocol window, the OS page cache).
    /// Bytes it accepts may be reported as transferred.
    Streamed,
    /// `write_chunk` stages bytes in memory; the real transfer happens in `finish()`. Bytes it
    /// accepts must **not** be reported as transferred — the engine reports them as staged and
    /// shows the upload as in-flight with no byte-level progress.
    Buffered,
}

/// The backend side of a streaming write. A backend implements this; [`WriteHandle`] wraps it and
/// is what the transfer engine and viewer use. `finish` commits (e.g. completes a multipart upload)
/// and returns the resulting [`Entry`]; `abort` cancels and frees any server-side state.
#[async_trait::async_trait]
pub trait WriteSink: Send {
    /// Whether `write_chunk` streams to the destination or stages in memory (see [`CommitMode`]).
    /// The engine relies on this for honest progress; misreporting `Streamed` reintroduces the
    /// "bar at 100% while the upload has not started" lie.
    fn commit_mode(&self) -> CommitMode;
    /// Write the next chunk. Implementations apply backpressure by awaiting here — a `Streamed`
    /// sink returns once the chunk is with the destination; a `Buffered` one returns immediately.
    async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), VfsError>;
    /// Commit the write and return the final entry metadata.
    async fn finish(self: Box<Self>) -> Result<Entry, VfsError>;
    /// Abort the write, discarding partial data and any server-side state.
    async fn abort(self: Box<Self>);
}

/// A streaming writer. Hides whether the backend does a single-shot or multipart upload.
pub struct WriteHandle {
    sink: Box<dyn WriteSink>,
}

impl WriteHandle {
    /// Construct from a backend [`WriteSink`].
    #[must_use]
    pub fn new(sink: Box<dyn WriteSink>) -> Self {
        Self { sink }
    }

    /// Whether chunks reach the destination as they are written or are staged until `finish`
    /// (see [`CommitMode`]).
    #[must_use]
    pub fn commit_mode(&self) -> CommitMode {
        self.sink.commit_mode()
    }

    /// Write the next chunk, awaiting if the backend applies backpressure.
    ///
    /// # Errors
    /// Returns a [`VfsError`] if the underlying write fails.
    pub async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), VfsError> {
        self.sink.write_chunk(chunk).await
    }

    /// Commit the write and return the resulting entry.
    ///
    /// # Errors
    /// Returns a [`VfsError`] if committing fails.
    pub async fn finish(self) -> Result<Entry, VfsError> {
        self.sink.finish().await
    }

    /// Abort the write, discarding partial data.
    pub async fn abort(self) {
        self.sink.abort().await;
    }
}
