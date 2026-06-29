//! [`PluginVfsBackend`] — a [`Vfs`] backed by a WASM plugin component, indistinguishable from a
//! built-in backend to the rest of Cairn.
//!
//! The plugin's `Store` is `!Send`, so it lives on a dedicated thread (see [`crate::bridge`]); this
//! type is the `Send + Sync` async face that messages it. M8-3b PR1 implements the read-only path
//! (`scheme`/`connection`/`caps`/`list`/`stat`); `open_read`/`open_write`/mutations are follow-ups.

use crate::bridge::{
    map_entry, plugin_dead_error, plugin_error_to_vfs, plugin_thread, to_vfs_error,
    valid_leaf_name, PluginMsg,
};
use crate::{Limits, PluginComponent, PluginError};
use cairn_types::{Caps, ConnectionId, Entry, Scheme, VfsPath};
use cairn_vfs::{
    ByteRange, CapabilityProvider, ListOpts, ListPage, PageCursor, ReadHandle, Recurse, Vfs,
    VfsError, WriteHandle, WriteOpts,
};
use futures::stream::{self, BoxStream, StreamExt};
use std::sync::mpsc;
use std::sync::Arc;
use tokio::sync::oneshot;

/// A `Vfs` whose operations are served by a sandboxed plugin component on a dedicated thread.
pub struct PluginVfsBackend {
    /// Sender to the plugin thread. `Arc` so cloning into the `list` stream is cheap and the thread
    /// stays alive as long as any handle or in-flight stream holds a clone.
    tx: Arc<mpsc::Sender<PluginMsg>>,
    scheme: Scheme,
    connection: ConnectionId,
    caps: Caps,
}

impl PluginVfsBackend {
    /// Spawn the plugin thread and build the backend. `scheme` and `caps` are read from the component
    /// synchronously up front (so the trait's sync `scheme()`/`caps()` need no round-trip), then the
    /// component is moved onto its own thread.
    ///
    /// # Errors
    /// [`PluginError`] if the initial `scheme`/`caps` calls trap.
    pub fn new(
        mut component: PluginComponent,
        limits: Limits,
        connection: ConnectionId,
    ) -> Result<Self, PluginError> {
        let scheme = Scheme::Plugin(component.scheme()?.into());
        let caps = component.caps()?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("cairn-plugin-{}", connection.0))
            .spawn(move || plugin_thread(component, limits, rx))
            .map_err(|e| PluginError::Instantiate(format!("failed to spawn plugin thread: {e}")))?;
        Ok(Self {
            tx: Arc::new(tx),
            scheme,
            connection,
            caps,
        })
    }
}

impl Drop for PluginVfsBackend {
    fn drop(&mut self) {
        // Best-effort: the thread may already be gone.
        let _ = self.tx.send(PluginMsg::Shutdown);
    }
}

impl CapabilityProvider for PluginVfsBackend {
    fn caps(&self) -> Caps {
        self.caps
    }
    // `caps_at` uses the backend-wide baseline (the trait default). Per-path refinement from the
    // guest is deferred: `CapabilityProvider::caps_at` is sync and can't do a channel round-trip to
    // the plugin thread without blocking the runtime. Most plugins report uniform caps.
}

#[async_trait::async_trait]
impl Vfs for PluginVfsBackend {
    fn scheme(&self) -> Scheme {
        self.scheme.clone()
    }

    fn connection(&self) -> ConnectionId {
        self.connection
    }

    fn list<'a>(
        &'a self,
        dir: &VfsPath,
        opts: ListOpts,
    ) -> BoxStream<'a, Result<ListPage, VfsError>> {
        // Capture a 'static clone of the sender so the stream owns its state (no borrow of `&self`
        // survives); a 'static stream coerces to BoxStream<'a>. Pages are pulled by messaging the
        // thread; iteration stops when the guest reports `done`. A page-count cap bounds a malicious
        // guest that never reports `done` (fuel bounds each call, but not the host-side page loop).
        const MAX_LIST_PAGES: u32 = 100_000;
        let tx = Arc::clone(&self.tx);
        let dir = dir.as_str();
        let include_hidden = opts.all;
        stream::unfold(
            (tx, dir, None::<String>, false, 0u32),
            move |(tx, dir, cursor, done, pages)| async move {
                if done {
                    return None;
                }
                if pages >= MAX_LIST_PAGES {
                    let e = VfsError::Backend {
                        code: "plugin_pagination_limit".into(),
                        msg: format!("plugin exceeded {MAX_LIST_PAGES} list pages"),
                        retryable: false,
                    };
                    return Some((Err(e), (tx, dir, None, true, pages)));
                }
                let (reply_tx, reply_rx) = oneshot::channel();
                if tx
                    .send(PluginMsg::ListPage {
                        dir: dir.clone(),
                        cursor,
                        include_hidden,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    return Some((Err(plugin_dead_error()), (tx, dir, None, true, pages)));
                }
                match reply_rx.await {
                    Err(_) => Some((Err(plugin_dead_error()), (tx, dir, None, true, pages))),
                    Ok(Err(pe)) => {
                        Some((Err(plugin_error_to_vfs(pe)), (tx, dir, None, true, pages)))
                    }
                    Ok(Ok(Err(we))) => Some((Err(to_vfs_error(we)), (tx, dir, None, true, pages))),
                    Ok(Ok(Ok(page))) => {
                        let done = page.done;
                        let next = page.cursor;
                        let mapped = ListPage {
                            // Drop entries with an invalid leaf name (traversal/control-char defense)
                            // rather than letting a malicious name reach path joins or the renderer.
                            entries: page
                                .entries
                                .into_iter()
                                .filter(|e| valid_leaf_name(&e.name))
                                .map(map_entry)
                                .collect(),
                            cursor: next.as_deref().map(|c| PageCursor(c.into())),
                            done,
                        };
                        Some((Ok(mapped), (tx, dir, next, done, pages + 1)))
                    }
                }
            },
        )
        .boxed()
    }

    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PluginMsg::Stat {
                path: path.as_str(),
                reply: reply_tx,
            })
            .map_err(|_| plugin_dead_error())?;
        reply_rx
            .await
            .map_err(|_| plugin_dead_error())?
            .map_err(plugin_error_to_vfs)?
            .map_err(to_vfs_error)
            .map(map_entry)
    }

    async fn open_read(
        &self,
        _path: &VfsPath,
        _range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError> {
        // M8-3b PR2: the streaming `read-stream` resource → `ReadHandle` bridge.
        Err(VfsError::Unsupported(Caps::READ))
    }

    async fn open_write(&self, _path: &VfsPath, _opts: WriteOpts) -> Result<WriteHandle, VfsError> {
        // M8-3b PR2: the `write-sink` resource → `WriteHandle` bridge.
        Err(VfsError::Unsupported(Caps::WRITE))
    }

    async fn remove(&self, _path: &VfsPath, _recurse: Recurse) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::DELETE))
    }
}
