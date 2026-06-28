//! The [`Caps`] capability model.

use bitflags::bitflags;

bitflags! {
    /// Backend capabilities — what operations a backend supports at a given location.
    ///
    /// The UI queries capabilities to decide which operations to *offer*; a backend asked to do
    /// something it does not advertise returns an `Unsupported` error. This is how Cairn expresses,
    /// for example, that object stores have no atomic rename and no `chmod`, or that a Kubernetes
    /// "log file" is a live stream rather than a seekable file.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Caps: u64 {
        /// Directory listing.
        const LIST = 1 << 0;
        /// Reading file contents.
        const READ = 1 << 1;
        /// Writing/creating files.
        const WRITE = 1 << 2;
        /// Creating directories (false for pure object stores where prefixes are implicit).
        const CREATE_DIR = 1 << 3;
        /// Deleting entries.
        const DELETE = 1 << 4;
        /// Renaming/moving entries.
        const RENAME = 1 << 5;
        /// Atomic rename (local filesystems only; object stores require copy+delete).
        const RENAME_ATOMIC = 1 << 6;
        /// Server-side copy within the same backend (e.g. S3 `CopyObject`).
        const COPY_SERVER = 1 << 7;
        /// Changing Unix permissions.
        const CHMOD = 1 << 8;
        /// Changing ownership.
        const CHOWN = 1 << 9;
        /// Creating symbolic links.
        const SYMLINK = 1 << 10;
        /// Appending to existing files.
        const APPEND = 1 << 11;
        /// Random-access / ranged reads (range GET, seek).
        const RANDOM_READ = 1 << 12;
        /// Resumable chunked/multipart upload.
        const MULTIPART = 1 << 13;
        /// Object/file versioning.
        const VERSIONS = 1 << 14;
        /// Change notification (e.g. inotify); absent for object stores.
        const WATCH = 1 << 15;
        /// Server-side content search (e.g. remote `grep` over SSH).
        const SEARCH_CONTENT = 1 << 16;
        /// Entries map to a real OS path that local processes can act on (local backend only). Gates
        /// features that shell out — see `Vfs::local_path` in `cairn-vfs`.
        const LOCAL_PATH = 1 << 17;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_compose() {
        let c = Caps::LIST | Caps::READ | Caps::WRITE;
        assert!(c.contains(Caps::READ));
        assert!(!c.contains(Caps::DELETE));
    }

    #[test]
    fn empty_contains_nothing() {
        assert!(!Caps::empty().contains(Caps::LIST));
    }
}
