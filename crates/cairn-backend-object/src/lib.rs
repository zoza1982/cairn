//! Object-store backend (S3 / GCS / Azure Blob).
//!
//! This crate provides the **provider-agnostic core**: the [`ObjectStore`] trait, the listing
//! synthesis that turns prefix+delimiter results into directory entries (the genuinely tricky,
//! SDK-independent part — object-vs-prefix semantics, common-prefix→directory, zero-byte markers),
//! and [`ObjectStoreVfs`] mapping it onto the [`Vfs`](cairn_vfs::Vfs) trait. A `MockObjectStore`
//! (behind the `test-utils` feature) exercises it hermetically.
//!
//! The concrete cloud providers live behind the `s3` / `gcs` / `azure` features and require the
//! official SDKs plus live or emulated services; they are not part of the default build. See
//! `docs/LLD.md` §8 and ADR-0003.

use async_trait::async_trait;
use cairn_types::{Caps, Entry, EntryExt, EntryKind};
use cairn_vfs::VfsError;
use smol_str::SmolStr;
use std::time::SystemTime;

mod vfs;
pub use vfs::ObjectStoreVfs;

// The live AWS S3 adapter (and S3-compatible stores like MinIO) lives behind the `s3` feature; it
// pulls the AWS SDK and the typed AWS credential. The provider-agnostic core above stays SDK-free.
#[cfg(feature = "s3")]
mod s3;
#[cfg(feature = "s3")]
pub use s3::{s3_connect, S3ConnectParams, S3ObjectStore};

// The live Google Cloud Storage adapter lives behind the `gcs` feature (pulls the GCS SDK + the
// typed GCP credential). The provider-agnostic core stays SDK-free.
#[cfg(feature = "gcs")]
mod gcs;
#[cfg(feature = "gcs")]
pub use gcs::{gcs_connect, GcsConnectParams, GcsObjectStore};

// The live Azure Blob Storage adapter lives behind the `azure` feature (pulls the Azure SDK + the
// typed Azure credential). The provider-agnostic core stays SDK-free.
#[cfg(feature = "azure")]
mod azure;
#[cfg(feature = "azure")]
pub use azure::{azure_connect, AzureConnectParams, AzureObjectStore};

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;
#[cfg(any(test, feature = "test-utils"))]
pub use mock::MockObjectStore;

/// Metadata for a single object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    /// The full object key.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// Content hash / version tag.
    pub etag: Option<SmolStr>,
    /// Last-modified time.
    pub modified: Option<SystemTime>,
    /// Storage class / tier.
    pub storage_class: Option<SmolStr>,
}

/// One page of a delimiter-based listing (mirrors S3 `ListObjectsV2`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListChunk {
    /// Common prefixes (directory-like groupings) under the queried prefix.
    pub common_prefixes: Vec<String>,
    /// Objects directly under the queried prefix.
    pub objects: Vec<ObjectMeta>,
    /// Continuation token for the next page, if any.
    pub next_token: Option<String>,
}

/// The provider-agnostic object-store interface. S3/GCS/Azure each implement this.
#[async_trait]
pub trait ObjectStore: Send + Sync + 'static {
    /// Capabilities for this store.
    fn capabilities(&self) -> Caps;

    /// List one page under `prefix`, grouping by `delimiter` (typically `/`).
    async fn list_page(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
        max: usize,
    ) -> Result<ListChunk, VfsError>;

    /// Fetch object metadata.
    async fn head(&self, key: &str) -> Result<ObjectMeta, VfsError>;

    /// Get an object's bytes, optionally a `(offset, len)` range.
    ///
    /// (Real providers stream; this returns bytes for the abstraction/mock. The trait will gain a
    /// streaming variant when the SDK providers land.)
    async fn get(&self, key: &str, range: Option<(u64, Option<u64>)>) -> Result<Vec<u8>, VfsError>;

    /// Put an object, returning its metadata.
    async fn put(&self, key: &str, data: Vec<u8>) -> Result<ObjectMeta, VfsError>;

    /// Delete an object.
    async fn delete(&self, key: &str) -> Result<(), VfsError>;

    /// Server-side copy.
    async fn copy(&self, from: &str, to: &str) -> Result<ObjectMeta, VfsError>;
}

/// Turn a delimiter-listing result into directory [`Entry`]s relative to `prefix`.
///
/// Handles object-store quirks: common prefixes become directories; a zero-byte object whose key is
/// exactly a common prefix (a "directory marker") is folded into that directory rather than shown as
/// a file; trailing-slash zero-byte markers become directories.
#[must_use]
pub fn merge_listing(chunk: &ListChunk, prefix: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let dir_keys: std::collections::HashSet<&str> =
        chunk.common_prefixes.iter().map(String::as_str).collect();

    for cp in &chunk.common_prefixes {
        let name = cp.strip_prefix(prefix).unwrap_or(cp).trim_end_matches('/');
        if !name.is_empty() {
            entries.push(Entry::new(name, EntryKind::Dir));
        }
    }

    for obj in &chunk.objects {
        // Suppress an object that is exactly a directory marker for a common prefix.
        if dir_keys.contains(format!("{}/", obj.key).as_str())
            || dir_keys.contains(obj.key.as_str())
        {
            continue;
        }
        let rel = obj.key.strip_prefix(prefix).unwrap_or(&obj.key);
        if rel.is_empty() {
            continue; // the prefix's own marker object
        }
        if rel.ends_with('/') && obj.size == 0 {
            let name = rel.trim_end_matches('/');
            if !name.is_empty() {
                entries.push(Entry::new(name, EntryKind::Dir));
            }
            continue;
        }
        if rel.contains('/') {
            continue; // not a direct child (shouldn't happen with a delimiter)
        }
        let mut e = Entry::new(rel, EntryKind::File);
        e.size = Some(obj.size);
        e.modified = obj.modified;
        e.etag = obj.etag.clone();
        e.ext = EntryExt::Object {
            storage_class: obj.storage_class.clone(),
            version_id: None,
        };
        entries.push(e);
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(key: &str, size: u64) -> ObjectMeta {
        ObjectMeta {
            key: key.to_owned(),
            size,
            etag: None,
            modified: None,
            storage_class: None,
        }
    }

    #[test]
    fn merges_prefixes_and_objects() {
        let chunk = ListChunk {
            common_prefixes: vec!["logs/2026/".to_owned()],
            objects: vec![obj("logs/readme.txt", 12), obj("logs/data.bin", 99)],
            next_token: None,
        };
        let entries = merge_listing(&chunk, "logs/");
        let mut names: Vec<_> = entries.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["2026", "data.bin", "readme.txt"]);
        let dir = entries.iter().find(|e| e.name == "2026").unwrap();
        assert!(dir.is_dir());
        let file = entries.iter().find(|e| e.name == "readme.txt").unwrap();
        assert_eq!(file.size, Some(12));
    }

    #[test]
    fn directory_marker_object_is_folded_not_shown_as_file() {
        // "logs/2026/" appears both as a common prefix and as a zero-byte marker object.
        let chunk = ListChunk {
            common_prefixes: vec!["logs/2026/".to_owned()],
            objects: vec![obj("logs/2026/", 0)],
            next_token: None,
        };
        let entries = merge_listing(&chunk, "logs/");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "2026");
        assert!(entries[0].is_dir());
    }

    #[test]
    fn zero_byte_trailing_slash_object_becomes_dir() {
        let chunk = ListChunk {
            common_prefixes: vec![],
            objects: vec![obj("logs/emptydir/", 0)],
            next_token: None,
        };
        let entries = merge_listing(&chunk, "logs/");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_dir());
        assert_eq!(entries[0].name, "emptydir");
    }
}
