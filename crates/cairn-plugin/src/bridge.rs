//! The dedicated-thread bridge between the async `Vfs` world and a plugin's synchronous,
//! `!Send` wasmtime `Store`.
//!
//! A plugin instance is owned by one OS thread ([`plugin_thread`]); the async side
//! ([`PluginVfsBackend`](crate::PluginVfsBackend)) holds only a `Send + Sync` channel sender and
//! sends one [`PluginMsg`] per operation, awaiting a `oneshot` reply. This is what lets a `!Send`
//! `Store` back a `Send + Sync` `Vfs`. PR1 covered metadata + listing (stat + list); PR2 adds
//! streaming reads (a `read-stream` resource, addressed across calls by a [`ResourceId`]). Writes
//! and mutations are PR3.
//!
//! The thread runs until every channel sender is dropped — the async backend *and* any live
//! `ReadHandle` each hold an `Arc<Sender>` clone, so the instance stays alive exactly as long as
//! something can still talk to it. On exit, any resources still open are freed.

use crate::component::{
    PluginComponent, ResourceAny, WitByteRange, WitEntry, WitListPageResult, WitVfsError,
};
use crate::{Limits, PluginError};
use cairn_types::{Entry, EntryKind, VfsPath};
use cairn_vfs::VfsError;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::oneshot;

/// A host-side numeric handle for a live guest resource (e.g. an open read stream). `Send + Copy`,
/// unlike `ResourceAny` (`!Send`) which never leaves the plugin thread. `u64` so the monotonic
/// counter never realistically wraps and aliases a still-live handle.
pub(crate) type ResourceId = u64;

/// The reply payload shape: an outer [`PluginError`] (host/trap) wrapping the inner guest result.
type Reply<T> = oneshot::Sender<Result<Result<T, WitVfsError>, PluginError>>;

/// A request to the plugin thread. Each carries a `oneshot` reply channel. `scheme`/`caps` are
/// resolved once at construction (cached on the async side), so they are not messages.
pub(crate) enum PluginMsg {
    /// Fetch metadata for a path.
    Stat {
        path: String,
        reply: Reply<WitEntry>,
    },
    /// List one page of a directory.
    ListPage {
        dir: String,
        cursor: Option<String>,
        include_hidden: bool,
        reply: Reply<WitListPageResult>,
    },
    /// Open a read stream; reply carries a [`ResourceId`] for the live stream.
    OpenRead {
        path: String,
        range: Option<WitByteRange>,
        reply: Reply<ResourceId>,
    },
    /// Read the next chunk of an open stream (empty `Vec` = EOF).
    ReadChunk {
        id: ResourceId,
        max_bytes: u32,
        reply: Reply<Vec<u8>>,
    },
    /// Close a read stream and free its guest resource (fire-and-forget).
    CloseRead { id: ResourceId },
}

/// The plugin thread: owns the `Store`/instance (and the live-resource table) and serves messages
/// until every sender is dropped (`recv` then errors). Refuels before each guest call so every op
/// gets its own fuel budget.
pub(crate) fn plugin_thread(
    mut component: PluginComponent,
    limits: Limits,
    rx: mpsc::Receiver<PluginMsg>,
) {
    // Owned guest resource handles live here, never crossing the thread boundary (`ResourceAny` is
    // `!Send`). The async side refers to them only by `ResourceId`.
    let mut resources: HashMap<ResourceId, ResourceAny> = HashMap::new();
    let mut next_id: ResourceId = 0;

    while let Ok(msg) = rx.recv() {
        match msg {
            PluginMsg::Stat { path, reply } => {
                component.refuel(limits.fuel);
                let _ = reply.send(component.stat(&path));
            }
            PluginMsg::ListPage {
                dir,
                cursor,
                include_hidden,
                reply,
            } => {
                component.refuel(limits.fuel);
                let _ = reply.send(component.list_page(&dir, cursor.as_deref(), include_hidden));
            }
            PluginMsg::OpenRead { path, range, reply } => {
                component.refuel(limits.fuel);
                match component.open_read(&path, range) {
                    Ok(Ok(res)) => {
                        let id = next_id;
                        next_id = next_id.wrapping_add(1);
                        resources.insert(id, res);
                        // If the caller's future was dropped between sending `OpenRead` and awaiting
                        // the reply, the resource we just created would otherwise leak (no
                        // `ReadHandle` ever learns its id) — free it on send failure.
                        if reply.send(Ok(Ok(id))).is_err() {
                            if let Some(res) = resources.remove(&id) {
                                component.close_read(res);
                            }
                        }
                    }
                    Ok(Err(we)) => {
                        let _ = reply.send(Ok(Err(we)));
                    }
                    Err(pe) => {
                        let _ = reply.send(Err(pe));
                    }
                }
            }
            PluginMsg::ReadChunk {
                id,
                max_bytes,
                reply,
            } => {
                component.refuel(limits.fuel);
                // `ResourceAny` is `Copy`, so the handle stays in the table across chunks.
                let r = match resources.get(&id).copied() {
                    Some(res) => match component.read_chunk(res, max_bytes) {
                        // Enforce the `<= max_bytes` contract: a malicious guest must not be able to
                        // force a host allocation larger than what we requested.
                        Ok(Ok(chunk)) if chunk.len() > max_bytes as usize => {
                            Err(PluginError::Trap(format!(
                                "guest returned {} bytes for a {max_bytes}-byte read request",
                                chunk.len()
                            )))
                        }
                        other => other,
                    },
                    None => Err(PluginError::Trap("unknown read-stream handle".to_owned())),
                };
                let _ = reply.send(r);
            }
            PluginMsg::CloseRead { id } => {
                if let Some(res) = resources.remove(&id) {
                    component.refuel(limits.fuel);
                    component.close_read(res);
                }
            }
        }
    }

    // Free any resources still open when the instance shuts down (guest destructors run before the
    // Store drops).
    for (_, res) in resources.drain() {
        component.refuel(limits.fuel);
        component.close_read(res);
    }
}

/// The error returned when the plugin thread is gone (panicked or shut down) — its channel send or
/// reply failed. Never carries secret material.
pub(crate) fn plugin_dead_error() -> VfsError {
    VfsError::Backend {
        code: "plugin_dead".into(),
        msg: "plugin instance is no longer running".into(),
        retryable: false,
    }
}

/// The error returned when a read stream exceeds [`Limits::max_stream_bytes`](crate::Limits) — a
/// guest that never reports EOF is cut off rather than driving the consumer to exhaust memory/disk.
pub(crate) fn plugin_stream_limit_error(limit: u64) -> VfsError {
    VfsError::Backend {
        code: "plugin_stream_limit".into(),
        msg: format!("plugin read stream exceeded {limit} bytes"),
        retryable: false,
    }
}

/// Strip control characters from and length-cap a guest-supplied diagnostic string. Guest error text
/// reaches logs and the TUI renderer untrusted: control/escape sequences are a terminal-injection
/// vector and an unbounded string is a memory vector.
fn sanitize_msg(s: String) -> String {
    const MAX: usize = 1024;
    s.chars().filter(|c| !c.is_control()).take(MAX).collect()
}

/// Map a host-side [`PluginError`] (trap, fuel exhaustion) to a [`VfsError`].
pub(crate) fn plugin_error_to_vfs(e: PluginError) -> VfsError {
    let code = match &e {
        PluginError::OutOfFuel => "plugin_fuel_exhausted",
        PluginError::Trap(_) => "plugin_trap",
        PluginError::Compile(_) => "plugin_compile",
        PluginError::Instantiate(_) => "plugin_instantiate",
        PluginError::Export(_) => "plugin_export",
    };
    VfsError::Backend {
        code: code.into(),
        msg: e.to_string(),
        retryable: false,
    }
}

/// Map the guest's `vfs-error` to a [`VfsError`].
pub(crate) fn to_vfs_error(e: WitVfsError) -> VfsError {
    match e {
        WitVfsError::NotFound(p) => VfsError::NotFound(parse_path(&p)),
        WitVfsError::Forbidden(p) => VfsError::Forbidden(parse_path(&p)),
        WitVfsError::AlreadyExists(p) => VfsError::AlreadyExists(parse_path(&p)),
        WitVfsError::Unsupported(caps) => VfsError::Unsupported(crate::component::map_caps(caps)),
        WitVfsError::TimeoutMs(ms) => VfsError::Timeout(Duration::from_millis(ms)),
        WitVfsError::Connection(msg) => {
            VfsError::Connection(Box::new(std::io::Error::other(sanitize_msg(msg))))
        }
        WitVfsError::Auth => VfsError::Auth,
        WitVfsError::Conflict => VfsError::Conflict,
        WitVfsError::Backend(e) => VfsError::Backend {
            code: sanitize_msg(e.code),
            msg: sanitize_msg(e.msg),
            retryable: e.retryable,
        },
        WitVfsError::Cancelled => VfsError::Cancelled,
        WitVfsError::Io(msg) => VfsError::Io(std::io::Error::other(sanitize_msg(msg))),
    }
}

/// Best-effort parse of a guest-supplied path string; an unparseable one (traversal, control chars)
/// degrades to root rather than panicking (the guest is untrusted), with a debug trace so a
/// misbehaving plugin is diagnosable.
fn parse_path(p: &str) -> VfsPath {
    VfsPath::parse(p).unwrap_or_else(|_| {
        tracing::debug!(target: "plugin", "guest returned an invalid path; using root");
        VfsPath::root()
    })
}

/// Whether `name` is a valid leaf name for an [`Entry`] (no path separators, no `.`/`..`, no control
/// chars). Guest-supplied names are untrusted: a name with `/` would be a traversal vector if a
/// consumer joined it naively, and control chars are a terminal-injection risk in the renderer.
pub(crate) fn valid_leaf_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.chars().any(|c| c.is_control())
}

/// Map a guest `entry` to a [`cairn_types::Entry`].
pub(crate) fn map_entry(e: WitEntry) -> Entry {
    use crate::component::WitEntryKind;
    Entry {
        name: e.name.into(),
        kind: match e.kind {
            WitEntryKind::File => EntryKind::File,
            WitEntryKind::Dir => EntryKind::Dir,
            WitEntryKind::Symlink => EntryKind::Symlink,
            WitEntryKind::StreamEntry => EntryKind::Stream,
            WitEntryKind::Special => EntryKind::Special,
        },
        size: e.size,
        // `checked_add`: an untrusted guest `modified_secs` (e.g. u64::MAX) must not panic the host
        // (`UNIX_EPOCH + Duration` overflows) — drop a bogus timestamp instead.
        modified: e
            .modified_secs
            .and_then(|s| UNIX_EPOCH.checked_add(Duration::from_secs(s))),
        perms: None,
        symlink_target: e
            .symlink_target
            .as_deref()
            .and_then(|p| VfsPath::parse(p).ok()),
        etag: e.etag.map(Into::into),
        ext: cairn_types::EntryExt::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_leaf_name_rejects_traversal_and_control_chars() {
        assert!(valid_leaf_name("a.txt"));
        assert!(valid_leaf_name("résumé.md"));
        assert!(!valid_leaf_name(""));
        assert!(!valid_leaf_name("."));
        assert!(!valid_leaf_name(".."));
        assert!(!valid_leaf_name("a/b"));
        assert!(!valid_leaf_name("a\\b"));
        assert!(!valid_leaf_name("a\0b"));
        assert!(!valid_leaf_name("a\nb"));
        assert!(!valid_leaf_name("\x1b[31mred"));
    }

    #[test]
    fn sanitize_msg_strips_control_chars_and_caps_length() {
        // Terminal-injection defense: escape/control sequences are removed, plain text kept.
        assert_eq!(
            sanitize_msg("ok \x1b[31mred\x1b[0m\n".to_owned()),
            "ok [31mred[0m"
        );
        // Memory defense: a huge guest string is capped.
        let big = "a".repeat(10_000);
        assert_eq!(sanitize_msg(big).len(), 1024);
    }

    #[test]
    fn map_entry_does_not_panic_on_overflow_timestamp() {
        let e = WitEntry {
            name: "x".to_owned(),
            kind: crate::component::WitEntryKind::File,
            size: Some(1),
            modified_secs: Some(u64::MAX), // would overflow UNIX_EPOCH + Duration
            etag: None,
            symlink_target: None,
        };
        let mapped = map_entry(e);
        assert_eq!(mapped.name, "x");
        assert!(
            mapped.modified.is_none(),
            "bogus timestamp dropped, not panicked"
        );
    }
}
