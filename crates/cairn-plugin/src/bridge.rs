//! The dedicated-thread bridge between the async `Vfs` world and a plugin's synchronous,
//! `!Send` wasmtime `Store`.
//!
//! A plugin instance is owned by one OS thread ([`plugin_thread`]); the async side
//! ([`PluginVfsBackend`](crate::PluginVfsBackend)) holds only a `Send + Sync` channel sender and
//! sends one [`PluginMsg`] per operation, awaiting a `oneshot` reply. This is what lets a `!Send`
//! `Store` back a `Send + Sync` `Vfs`. M8-3b PR1 covers the read-only path (stat + list); streaming
//! resources, writes, and mutations are follow-ups.

use crate::component::{PluginComponent, WitEntry, WitListPageResult, WitVfsError};
use crate::{Limits, PluginError};
use cairn_types::{Entry, EntryKind, VfsPath};
use cairn_vfs::VfsError;
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::oneshot;

/// A request to the plugin thread. Each carries a `oneshot` reply channel. `scheme`/`caps` are
/// resolved once at construction (cached on the async side), so they are not messages.
pub(crate) enum PluginMsg {
    /// Fetch metadata for a path.
    Stat {
        path: String,
        reply: oneshot::Sender<Result<Result<WitEntry, WitVfsError>, PluginError>>,
    },
    /// List one page of a directory.
    ListPage {
        dir: String,
        cursor: Option<String>,
        include_hidden: bool,
        reply: oneshot::Sender<Result<Result<WitListPageResult, WitVfsError>, PluginError>>,
    },
    /// Stop the thread and tear the instance down.
    Shutdown,
}

/// The plugin thread: owns the `Store`/instance and serves messages until the channel closes or a
/// [`PluginMsg::Shutdown`] arrives. Refuels before each call so every op gets its own fuel budget.
pub(crate) fn plugin_thread(
    mut component: PluginComponent,
    limits: Limits,
    rx: mpsc::Receiver<PluginMsg>,
) {
    while let Ok(msg) = rx.recv() {
        match msg {
            PluginMsg::Shutdown => break,
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
        }
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
        WitVfsError::Connection(msg) => VfsError::Connection(Box::new(std::io::Error::other(msg))),
        WitVfsError::Auth => VfsError::Auth,
        WitVfsError::Conflict => VfsError::Conflict,
        WitVfsError::Backend(e) => VfsError::Backend {
            code: e.code,
            msg: e.msg,
            retryable: e.retryable,
        },
        WitVfsError::Cancelled => VfsError::Cancelled,
        WitVfsError::Io(msg) => VfsError::Io(std::io::Error::other(msg)),
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
