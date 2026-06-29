//! The dedicated-thread bridge between the async `Vfs` world and a plugin's synchronous,
//! `!Send` wasmtime `Store`.
//!
//! A plugin instance is owned by one OS thread ([`plugin_thread`]); the async side
//! ([`PluginVfsBackend`](crate::PluginVfsBackend)) holds only a `Send + Sync` channel sender and
//! sends one [`PluginMsg`] per operation, awaiting a `oneshot` reply. This is what lets a `!Send`
//! `Store` back a `Send + Sync` `Vfs`. PR1 covered metadata + listing (stat + list); PR2 added
//! streaming reads (a `read-stream` resource, addressed across calls by a [`ResourceId`]); PR3 adds
//! streaming writes (a `write-sink` resource) and mutations (create-dir/remove/rename).
//!
//! The thread runs until every channel sender is dropped — the async backend *and* any live
//! `ReadHandle` each hold an `Arc<Sender>` clone, so the instance stays alive exactly as long as
//! something can still talk to it. On exit, any resources still open are freed.

use crate::component::{
    PluginComponent, ResourceAny, WitByteRange, WitEntry, WitListPageResult, WitVfsError,
};
use crate::epoch::EpochTicker;
use crate::{Limits, PluginError};
use bytes::Bytes;
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
    /// Open a write sink; reply carries a [`ResourceId`] for the live sink.
    OpenWrite {
        path: String,
        overwrite: bool,
        size_hint: Option<u64>,
        reply: Reply<ResourceId>,
    },
    /// Write the next chunk to an open sink. `Bytes` so the host→thread hand-off is zero-copy.
    WriteChunk {
        id: ResourceId,
        chunk: Bytes,
        reply: Reply<()>,
    },
    /// Commit a sink and free its guest resource, returning the resulting entry.
    FinishWrite {
        id: ResourceId,
        reply: Reply<WitEntry>,
    },
    /// Abort a sink and free its guest resource (fire-and-forget).
    AbortWrite { id: ResourceId },
    /// Create a directory.
    CreateDir { path: String, reply: Reply<()> },
    /// Remove an entry (optionally recursively).
    Remove {
        path: String,
        recursive: bool,
        reply: Reply<()>,
    },
    /// Rename/move an entry.
    Rename {
        src: String,
        dst: String,
        reply: Reply<()>,
    },
}

/// How often the [`EpochTicker`] advances the engine's epoch. Combined with `Limits::max_call_ticks`
/// this sets the per-call wall-clock ceiling (default 50 × 100 ms ≈ 5 s). Coarse on purpose — epoch
/// interruption is a backstop, not a precise deadline (fuel is the fine-grained, deterministic bound).
const EPOCH_TICK: Duration = Duration::from_millis(100);

/// The plugin thread: owns the `Store`/instance (and the live-resource table) and serves messages
/// until every sender is dropped (`recv` then errors). Arms fuel + the epoch deadline before each
/// guest call so every op gets its own budget.
pub(crate) fn plugin_thread(
    mut component: PluginComponent,
    limits: Limits,
    rx: mpsc::Receiver<PluginMsg>,
) {
    // Drive the engine's epoch for this instance's whole lifetime so the per-call epoch deadline
    // actually fires. Tied to the thread (not the backend), so it also covers a `ReadHandle` that
    // outlives the backend (such a handle keeps this thread — and thus the ticker — alive). Dropped
    // when the thread exits.
    //
    // INVARIANT: one `Engine` per instance (each `PluginVfsBackend` is built from its own engine), so
    // exactly one ticker drives each engine's epoch. If a future caller shares one engine across
    // instances it must share a single ticker too — N tickers advance one epoch N× and shrink the
    // effective deadline to 1/N (see `crate::epoch`).
    let _ticker = EpochTicker::spawn(component.engine(), EPOCH_TICK);

    // Owned guest resource handles live here, never crossing the thread boundary (`ResourceAny` is
    // `!Send`). The async side refers to them only by `ResourceId`. Read streams and write sinks are
    // kept in separate tables (distinct guest resource types), sharing one id counter so ids are
    // globally unique.
    let mut reads: HashMap<ResourceId, ResourceAny> = HashMap::new();
    let mut sinks: HashMap<ResourceId, ResourceAny> = HashMap::new();
    let mut next_id: ResourceId = 0;

    while let Ok(msg) = rx.recv() {
        match msg {
            PluginMsg::Stat { path, reply } => {
                component.arm(limits);
                let _ = reply.send(component.stat(&path));
            }
            PluginMsg::ListPage {
                dir,
                cursor,
                include_hidden,
                reply,
            } => {
                component.arm(limits);
                let _ = reply.send(component.list_page(&dir, cursor.as_deref(), include_hidden));
            }
            PluginMsg::OpenRead { path, range, reply } => {
                component.arm(limits);
                match component.open_read(&path, range) {
                    Ok(Ok(res)) => {
                        let id = next_id;
                        next_id = next_id.wrapping_add(1);
                        reads.insert(id, res);
                        // If the caller's future was dropped between sending `OpenRead` and awaiting
                        // the reply, the resource we just created would otherwise leak (no
                        // `ReadHandle` ever learns its id) — free it on send failure.
                        if reply.send(Ok(Ok(id))).is_err() {
                            if let Some(res) = reads.remove(&id) {
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
                component.arm(limits);
                // `ResourceAny` is `Copy`, so the handle stays in the table across chunks.
                let r = match reads.get(&id).copied() {
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
                if let Some(res) = reads.remove(&id) {
                    component.arm(limits);
                    component.close_read(res);
                }
            }
            PluginMsg::OpenWrite {
                path,
                overwrite,
                size_hint,
                reply,
            } => {
                component.arm(limits);
                match component.open_write(&path, overwrite, size_hint) {
                    Ok(Ok(res)) => {
                        let id = next_id;
                        next_id = next_id.wrapping_add(1);
                        sinks.insert(id, res);
                        // Free the sink if the caller's future was dropped before learning the id
                        // (cancellation leak, mirroring `OpenRead`).
                        if reply.send(Ok(Ok(id))).is_err() {
                            if let Some(res) = sinks.remove(&id) {
                                component.abort_write(res);
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
            PluginMsg::WriteChunk { id, chunk, reply } => {
                component.arm(limits);
                let r = match sinks.get(&id).copied() {
                    Some(res) => component.write_chunk(res, &chunk),
                    None => Err(PluginError::Trap("unknown write-sink handle".to_owned())),
                };
                let _ = reply.send(r);
            }
            PluginMsg::FinishWrite { id, reply } => {
                component.arm(limits);
                // `finish` consumes the sink, so remove it from the table first.
                let r = match sinks.remove(&id) {
                    Some(res) => component.finish_write(res),
                    None => Err(PluginError::Trap("unknown write-sink handle".to_owned())),
                };
                let _ = reply.send(r);
            }
            PluginMsg::AbortWrite { id } => {
                if let Some(res) = sinks.remove(&id) {
                    component.arm(limits);
                    component.abort_write(res);
                }
            }
            PluginMsg::CreateDir { path, reply } => {
                component.arm(limits);
                let _ = reply.send(component.create_dir(&path));
            }
            PluginMsg::Remove {
                path,
                recursive,
                reply,
            } => {
                component.arm(limits);
                let _ = reply.send(component.remove(&path, recursive));
            }
            PluginMsg::Rename { src, dst, reply } => {
                component.arm(limits);
                let _ = reply.send(component.rename(&src, &dst));
            }
        }
    }

    // Free any resources still open when the instance shuts down (guest destructors run before the
    // Store drops). Sinks are aborted (no implicit commit on teardown).
    for (_, res) in reads.drain() {
        component.arm(limits);
        component.close_read(res);
    }
    for (_, res) in sinks.drain() {
        component.arm(limits);
        component.abort_write(res);
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
        PluginError::Timeout => "plugin_timeout",
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

/// An upper bound on a guest-supplied leaf name. Generous (far above any real filesystem's limit)
/// but finite, so a guest can't force a multi-megabyte name allocation per entry.
const MAX_LEAF_NAME: usize = 4096;

/// Whether `name` is a valid leaf name for an [`Entry`] (no path separators, no `.`/`..`, no control
/// chars, bounded length). Guest-supplied names are untrusted: a name with `/` would be a traversal
/// vector if a consumer joined it naively, control chars are a terminal-injection risk in the
/// renderer, and an unbounded name is a memory vector.
pub(crate) fn valid_leaf_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_LEAF_NAME
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.chars().any(|c| c.is_control())
}

/// Map a single guest `entry`, rejecting an invalid leaf name. `list` *filters* bad entries out of a
/// page, but single-entry callers (`stat`, write `finish`) have nothing to filter — they must surface
/// an error rather than hand an unvalidated, possibly traversal/injection-bearing name to a consumer.
pub(crate) fn map_entry_checked(e: WitEntry) -> Result<Entry, VfsError> {
    if !valid_leaf_name(&e.name) {
        return Err(VfsError::Backend {
            code: "plugin_invalid_entry".into(),
            msg: "plugin returned an entry with an invalid name".into(),
            retryable: false,
        });
    }
    Ok(map_entry(e))
}

/// Map a guest `entry` to a [`cairn_types::Entry`]. Callers handling a single entry should prefer
/// [`map_entry_checked`]; this is used directly only by `list`, which validates names via its filter.
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
        assert!(valid_leaf_name(&"a".repeat(MAX_LEAF_NAME)));
        assert!(!valid_leaf_name(&"a".repeat(MAX_LEAF_NAME + 1)));
    }

    #[test]
    fn plugin_error_to_vfs_maps_timeout_and_fuel() {
        for (err, code) in [
            (PluginError::Timeout, "plugin_timeout"),
            (PluginError::OutOfFuel, "plugin_fuel_exhausted"),
        ] {
            match plugin_error_to_vfs(err) {
                VfsError::Backend {
                    code: c, retryable, ..
                } => {
                    assert_eq!(c, code);
                    assert!(!retryable);
                }
                other => panic!("expected Backend, got {other:?}"),
            }
        }
    }

    #[test]
    fn map_entry_checked_rejects_an_invalid_name() {
        let bad = WitEntry {
            name: "../evil".to_owned(),
            kind: crate::component::WitEntryKind::File,
            size: Some(1),
            modified_secs: None,
            etag: None,
            symlink_target: None,
        };
        assert!(matches!(
            map_entry_checked(bad),
            Err(VfsError::Backend { .. })
        ));
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
