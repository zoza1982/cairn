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

/// The backend side of a streaming write. A backend implements this; [`WriteHandle`] wraps it and
/// is what the transfer engine and viewer use. `finish` commits (e.g. completes a multipart upload)
/// and returns the resulting [`Entry`]; `abort` cancels and frees any server-side state.
#[async_trait::async_trait]
pub trait WriteSink: Send {
    /// Write the next chunk. Implementations apply backpressure by awaiting here.
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
