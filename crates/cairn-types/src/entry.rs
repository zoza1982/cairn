//! [`Entry`]: uniform directory-entry metadata across every backend.

use crate::path::VfsPath;
use smol_str::SmolStr;
use std::time::SystemTime;

/// Uniform metadata for one entry in a listing, for any backend.
///
/// Backend-specific facts live in the typed [`EntryExt`] extension rather than a stringly-typed map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The leaf name (no path separators).
    pub name: SmolStr,
    /// What kind of entry this is.
    pub kind: EntryKind,
    /// Size in bytes, or `None` when unknown / streaming / not applicable.
    pub size: Option<u64>,
    /// Last-modified time, if the backend exposes one.
    pub modified: Option<SystemTime>,
    /// Unix permissions, where the backend has a permission model.
    pub perms: Option<UnixPerms>,
    /// Symlink target, if `kind` is [`EntryKind::Symlink`].
    pub symlink_target: Option<VfsPath>,
    /// Content hash / version tag (object stores) used for integrity and conflict detection.
    pub etag: Option<SmolStr>,
    /// Typed backend-specific extension.
    pub ext: EntryExt,
}

impl Entry {
    /// Construct a minimal entry of the given name and kind, with all optional fields unset.
    #[must_use]
    pub fn new(name: impl Into<SmolStr>, kind: EntryKind) -> Self {
        Self {
            name: name.into(),
            kind,
            size: None,
            modified: None,
            perms: None,
            symlink_target: None,
            etag: None,
            ext: EntryExt::None,
        }
    }

    /// Whether this entry is directory-like (a real directory, an object-store prefix, or a
    /// container/pod presented as a directory).
    #[must_use]
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir)
    }

    /// Whether this entry is the synthetic `..` parent-navigation sentinel.
    ///
    /// The `..` entry is injected by the UI layer as a navigation affordance (MC convention). It is
    /// never a real VFS path and must be excluded from all bulk operations (copy/move/delete/mark)
    /// and from operations that resolve the name via [`VfsPath::join`]. Every call site that
    /// special-cases `..` by name should call this method so the sentinel check remains auditable
    /// in one place.
    #[must_use]
    pub fn is_dotdot_sentinel(&self) -> bool {
        self.name.as_str() == ".."
    }
}

/// The kind of a directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory, an object-store prefix, or a container/pod presented as a directory.
    Dir,
    /// A symbolic link.
    Symlink,
    /// A live byte stream (e.g. a container/pod log); `size` is `None` and reads use a stream path.
    Stream,
    /// A special node (socket, device, fifo) on local filesystems.
    Special,
}

/// Unix-style permission bits plus optional owner/group ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnixPerms {
    /// The permission bits (e.g. `0o644`).
    pub mode: u32,
    /// Owner user id, if known.
    pub uid: Option<u32>,
    /// Owner group id, if known.
    pub gid: Option<u32>,
}

impl UnixPerms {
    /// Construct from a raw mode with unknown owner/group.
    #[must_use]
    pub fn from_mode(mode: u32) -> Self {
        Self {
            mode,
            uid: None,
            gid: None,
        }
    }
}

/// Typed backend-specific extension data attached to an [`Entry`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryExt {
    /// No backend-specific data.
    None,
    /// Object-store object metadata.
    Object {
        /// Storage class / tier (e.g. `STANDARD`, `GLACIER`).
        storage_class: Option<SmolStr>,
        /// Object version id, where versioning is enabled.
        version_id: Option<SmolStr>,
    },
    /// A Docker/OCI container.
    Container {
        /// Container id.
        id: SmolStr,
        /// Runtime state.
        state: ContainerState,
        /// Image reference.
        image: SmolStr,
    },
    /// A Docker/OCI image.
    Image {
        /// Image id.
        id: SmolStr,
        /// Number of layers.
        layers: u32,
        /// Tags pointing at this image.
        tags: Vec<SmolStr>,
    },
    /// A Kubernetes pod.
    Pod {
        /// Pod phase.
        phase: PodPhase,
        /// Ready containers out of total (`ready`, `total`).
        ready: (u16, u16),
        /// Node the pod is scheduled on, if any.
        node: Option<SmolStr>,
    },
    /// A container within a Kubernetes pod.
    KubeContainer {
        /// Whether this is an init container.
        is_init: bool,
        /// Whether this is an ephemeral (debug) container.
        is_ephemeral: bool,
    },
    /// A generic Kubernetes resource.
    K8sResource {
        /// Resource kind (e.g. `Service`).
        kind: SmolStr,
        /// Namespace, if namespaced.
        namespace: Option<SmolStr>,
    },
}

/// Docker container runtime state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContainerState {
    /// Created but not started.
    Created,
    /// Running.
    Running,
    /// Paused.
    Paused,
    /// Restarting.
    Restarting,
    /// Exited.
    Exited,
    /// Dead.
    Dead,
    /// State could not be determined.
    Unknown,
}

/// Kubernetes pod phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PodPhase {
    /// Accepted but not yet running.
    Pending,
    /// Running.
    Running,
    /// All containers succeeded.
    Succeeded,
    /// At least one container failed.
    Failed,
    /// Phase could not be determined.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_entry_defaults() {
        let e = Entry::new("file.txt", EntryKind::File);
        assert_eq!(e.name, "file.txt");
        assert!(!e.is_dir());
        assert_eq!(e.size, None);
        assert_eq!(e.ext, EntryExt::None);
    }

    #[test]
    fn dir_detection() {
        assert!(Entry::new("d", EntryKind::Dir).is_dir());
        assert!(!Entry::new("s", EntryKind::Stream).is_dir());
    }

    #[test]
    fn perms_from_mode() {
        let p = UnixPerms::from_mode(0o644);
        assert_eq!(p.mode, 0o644);
        assert_eq!(p.uid, None);
    }
}
