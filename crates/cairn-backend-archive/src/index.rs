//! The in-memory directory index shared by [`crate::tar_backend::TarOps`] and
//! [`crate::zip_backend::ZipOps`].
//!
//! Both tar and zip give us a flat list of members with full relative paths; neither guarantees an
//! explicit entry for every intermediate directory (tar usually does, but a `Directory` header is
//! not required; zip directory entries are a convention, not a requirement). [`IndexBuilder`]
//! synthesizes any missing ancestor directories as it goes, so [`ArchiveIndex::list_children`] never
//! needs to re-derive the tree at listing time (unlike the object-store backend's common-prefix
//! folding, which *does* re-derive it per listing — here we already have the whole member list up
//! front, so building the tree once at open time is both simpler and cheaper).
//!
//! Generic over `L`, the backend-specific "locator" needed to physically read a member's bytes: a
//! byte offset into the tar file, or an index into the zip central directory.

use cairn_types::{Entry, EntryKind, VfsPath};
use cairn_vfs::VfsError;
use std::collections::HashMap;

/// One indexed archive member (or a synthesized directory).
#[derive(Debug, Clone)]
pub(crate) enum StoredEntry<L> {
    /// A directory — either an explicit archive entry or synthesized as an ancestor of some
    /// deeper member. Never has readable content.
    Dir,
    /// A regular file with a known (validated, capped-at-read-time) size and a locator telling the
    /// backend where to physically read it from.
    File { size: u64, locator: L },
    /// A symlink or hardlink member, presented inert: its target is shown for information only and
    /// is never followed or resolved against any real filesystem (see RFC-0013 §Security).
    Symlink { target: Option<VfsPath> },
}

impl<L> StoredEntry<L> {
    fn kind(&self) -> EntryKind {
        match self {
            Self::Dir => EntryKind::Dir,
            Self::File { .. } => EntryKind::File,
            Self::Symlink { .. } => EntryKind::Symlink,
        }
    }

    fn to_entry(&self, name: &str) -> Entry {
        let mut e = Entry::new(name, self.kind());
        match self {
            Self::File { size, .. } => e.size = Some(*size),
            Self::Symlink { target } => e.symlink_target = target.clone(),
            Self::Dir => {}
        }
        e
    }
}

/// The built, queryable index for one mounted archive.
pub(crate) struct ArchiveIndex<L> {
    entries: HashMap<VfsPath, StoredEntry<L>>,
    children: HashMap<VfsPath, Vec<VfsPath>>,
}

impl<L> ArchiveIndex<L> {
    /// List the immediate children of `dir`. The root always exists (even in an empty archive);
    /// any other directory must have been indexed (explicitly or synthesized) or this returns
    /// [`VfsError::NotFound`].
    pub(crate) fn list_children(&self, dir: &VfsPath) -> Result<Vec<Entry>, VfsError> {
        if !dir.is_root() {
            match self.entries.get(dir) {
                Some(StoredEntry::Dir) => {}
                Some(_) => return Err(VfsError::NotFound(dir.clone())),
                None => return Err(VfsError::NotFound(dir.clone())),
            }
        }
        let children = self.children.get(dir).cloned().unwrap_or_default();
        Ok(children
            .into_iter()
            .map(|p| {
                let name = p.file_name().unwrap_or_default().to_owned();
                self.entries
                    .get(&p)
                    .map(|se| se.to_entry(&name))
                    .unwrap_or_else(|| Entry::new(name, EntryKind::File))
            })
            .collect())
    }

    /// Metadata for one path (not the root — callers handle the root specially, matching every
    /// other backend's `stat` convention).
    pub(crate) fn entry_meta(&self, path: &VfsPath) -> Result<Entry, VfsError> {
        let name = path.file_name().unwrap_or_default();
        self.entries
            .get(path)
            .map(|se| se.to_entry(name))
            .ok_or_else(|| VfsError::NotFound(path.clone()))
    }

    /// Look up the stored entry (including its locator) for a read.
    pub(crate) fn get(&self, path: &VfsPath) -> Option<&StoredEntry<L>> {
        self.entries.get(path)
    }
}

/// Incrementally builds an [`ArchiveIndex`], synthesizing ancestor directories as members are
/// inserted. Entry-count enforcement against [`crate::security::ARCHIVE_MAX_ENTRIES`] is the
/// caller's responsibility (it must be checked against every *raw* header seen, including ones
/// this builder never receives because they were rejected before `insert` was called).
pub(crate) struct IndexBuilder<L> {
    entries: HashMap<VfsPath, StoredEntry<L>>,
    children: HashMap<VfsPath, Vec<VfsPath>>,
}

impl<L> IndexBuilder<L> {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            children: HashMap::new(),
        }
    }

    /// Ensure every proper ancestor of `path` exists as a directory, synthesizing any that are
    /// missing. Returns `false` (without synthesizing further) if an ancestor already exists as a
    /// non-directory — a malformed archive where one member's path is used both as a file and as a
    /// directory prefix of another; the caller should skip the conflicting member.
    fn ensure_ancestors(&mut self, path: &VfsPath) -> bool {
        // Collect proper ancestors, root-first, by repeatedly taking `parent()`.
        let mut ancestors = Vec::new();
        let mut cur = path.clone();
        while let Some(parent) = cur.parent() {
            ancestors.push(parent.clone());
            cur = parent;
        }
        ancestors.reverse(); // root-first
        for anc in ancestors {
            if anc.is_root() {
                // The root always exists implicitly; never stored as an entry or as its own child.
                continue;
            }
            match self.entries.get(&anc) {
                Some(StoredEntry::Dir) => continue,
                Some(_) => return false,
                None => {
                    self.entries.insert(anc.clone(), StoredEntry::Dir);
                    let parent = anc.parent().unwrap_or_else(VfsPath::root);
                    self.children.entry(parent).or_default().push(anc);
                }
            }
        }
        true
    }

    /// Insert a validated member. Ancestors are synthesized first; if that finds a conflicting
    /// non-directory ancestor, or `path` itself already exists as a directory while this member is
    /// not one, the member is dropped (the caller is expected to log a warning) rather than
    /// corrupting the tree. A member that legitimately re-declares a path already seen (tar/zip
    /// both permit duplicate entries; the last one wins on real extraction) overwrites cleanly.
    pub(crate) fn insert(&mut self, path: VfsPath, entry: StoredEntry<L>) {
        if path.is_root() {
            return; // the root always exists implicitly; nothing to store.
        }
        if !self.ensure_ancestors(&path) {
            tracing::warn!("skipping archive member: path/directory conflict with an ancestor");
            return;
        }
        let is_new = !self.entries.contains_key(&path);
        if let Some(StoredEntry::Dir) = self.entries.get(&path) {
            if !matches!(entry, StoredEntry::Dir) {
                tracing::warn!(
                    "skipping archive member: conflicts with a directory of the same name"
                );
                return;
            }
        }
        self.entries.insert(path.clone(), entry);
        if is_new {
            let parent = path.parent().unwrap_or_else(VfsPath::root);
            self.children.entry(parent).or_default().push(path);
        }
    }

    pub(crate) fn finish(self) -> ArchiveIndex<L> {
        ArchiveIndex {
            entries: self.entries,
            children: self.children,
        }
    }
}

impl<L> Default for IndexBuilder<L> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    #[test]
    fn synthesizes_missing_ancestor_directories() {
        let mut b: IndexBuilder<u64> = IndexBuilder::new();
        b.insert(
            p("a/b/c.txt"),
            StoredEntry::File {
                size: 3,
                locator: 0,
            },
        );
        let idx = b.finish();
        let root_children = idx.list_children(&VfsPath::root()).unwrap();
        assert_eq!(root_children.len(), 1);
        assert!(root_children[0].is_dir());
        assert_eq!(root_children[0].name, "a");

        let ab = idx.list_children(&p("a/b")).unwrap();
        assert_eq!(ab.len(), 1);
        assert_eq!(ab[0].name, "c.txt");
        assert_eq!(ab[0].size, Some(3));
    }

    #[test]
    fn explicit_directory_entry_is_not_duplicated() {
        let mut b: IndexBuilder<u64> = IndexBuilder::new();
        b.insert(p("dir"), StoredEntry::Dir);
        b.insert(
            p("dir/file.txt"),
            StoredEntry::File {
                size: 1,
                locator: 0,
            },
        );
        let idx = b.finish();
        let root_children = idx.list_children(&VfsPath::root()).unwrap();
        assert_eq!(root_children.len(), 1); // "dir" appears exactly once
    }

    #[test]
    fn conflicting_file_then_dir_child_is_dropped() {
        let mut b: IndexBuilder<u64> = IndexBuilder::new();
        b.insert(
            p("a"),
            StoredEntry::File {
                size: 1,
                locator: 0,
            },
        );
        // "a/b" implies "a" is a directory, but "a" is already a file — dropped, not panicking.
        b.insert(
            p("a/b"),
            StoredEntry::File {
                size: 1,
                locator: 1,
            },
        );
        let idx = b.finish();
        assert!(idx.list_children(&p("a")).is_err());
        assert_eq!(idx.entry_meta(&p("a")).unwrap().kind, EntryKind::File);
    }

    #[test]
    fn missing_directory_is_not_found() {
        let b: IndexBuilder<u64> = IndexBuilder::new();
        let idx = b.finish();
        assert!(matches!(
            idx.list_children(&p("nope")),
            Err(VfsError::NotFound(_))
        ));
    }
}
