//! The Cairn virtual filesystem abstraction.
//!
//! Every backend (local, SSH, S3/GCS/Azure, Docker, Kubernetes, and plugins) implements the [`Vfs`]
//! trait, held behind `Arc<dyn Vfs>`. The [capability model](cairn_types::Caps) expresses what each
//! backend can and cannot do so the UI offers only valid operations; backend-specific actions
//! (exec/logs/port-forward) surface through a uniform action interface ([`ActionDescriptor`],
//! [`ActionOutcome`]). See `docs/LLD.md` §3.

mod action;
mod error;
mod handle;
mod registry;
mod retry;
mod vfs;

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;

pub use action::{
    action_ids, ActionCtx, ActionDescriptor, ActionId, ActionKind, ActionOutcome, SessionHandle,
};
pub use error::{BoxError, RedactedError, VfsError};
pub use handle::{ReadHandle, WriteHandle, WriteSink};
pub use registry::VfsRegistry;
pub use retry::{backoff_delay, retry, RetryPolicy};
pub use vfs::{
    apply_byte_range, join_abs_path, ByteRange, CapabilityProvider, ListOpts, ListPage, PageCursor,
    Recurse, Vfs, WriteOpts,
};
