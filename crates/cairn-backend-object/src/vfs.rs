//! [`ObjectStoreVfs`]: maps an [`ObjectStore`](crate::ObjectStore) onto the [`Vfs`] trait.

use crate::{merge_listing, ObjectStore};
use async_trait::async_trait;
use bytes::Bytes;
use cairn_types::{Caps, ConnectionId, Entry, EntryKind, Scheme, VfsPath};
use cairn_vfs::{
    ByteRange, CapabilityProvider, CommitMode, ListOpts, ListPage, PageCursor, ReadHandle, Recurse,
    Vfs, VfsError, WriteHandle, WriteOpts, WriteSink,
};
use futures::stream::{self, BoxStream, StreamExt};
use smol_str::SmolStr;
use std::sync::Arc;

/// A [`Vfs`] over an [`ObjectStore`], rooted at an optional key prefix.
pub struct ObjectStoreVfs {
    conn: ConnectionId,
    scheme: Scheme,
    store: Arc<dyn ObjectStore>,
    /// Root prefix without a trailing slash (`""` for the bucket root).
    root: String,
}

impl ObjectStoreVfs {
    /// Create a backend over `store`, rooted at `root` (a key prefix; `""` = bucket root).
    #[must_use]
    pub fn new(
        conn: ConnectionId,
        scheme: Scheme,
        store: Arc<dyn ObjectStore>,
        root: &str,
    ) -> Self {
        Self {
            conn,
            scheme,
            store,
            root: root.trim_end_matches('/').to_owned(),
        }
    }

    /// The object key for a path.
    fn key_for(&self, path: &VfsPath) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !self.root.is_empty() {
            parts.push(&self.root);
        }
        for s in path.segments() {
            parts.push(s.as_str());
        }
        parts.join("/")
    }

    /// The listing prefix for a directory (`""` or `"…/"`).
    fn dir_prefix(&self, path: &VfsPath) -> String {
        let key = self.key_for(path);
        if key.is_empty() {
            String::new()
        } else {
            format!("{key}/")
        }
    }
}

impl CapabilityProvider for ObjectStoreVfs {
    fn caps(&self) -> Caps {
        self.store.capabilities()
    }
}

#[async_trait]
impl Vfs for ObjectStoreVfs {
    fn scheme(&self) -> Scheme {
        self.scheme.clone()
    }

    fn connection(&self) -> ConnectionId {
        self.conn
    }

    fn list<'a>(
        &'a self,
        dir: &VfsPath,
        _opts: ListOpts,
    ) -> BoxStream<'a, Result<ListPage, VfsError>> {
        let store = self.store.clone();
        let prefix = self.dir_prefix(dir);
        stream::unfold((false, None::<String>), move |(done, token)| {
            let store = store.clone();
            let prefix = prefix.clone();
            async move {
                if done {
                    return None;
                }
                match store
                    .list_page(&prefix, Some("/"), token.as_deref(), 1000)
                    .await
                {
                    Ok(chunk) => {
                        let next = chunk.next_token.clone();
                        let entries = merge_listing(&chunk, &prefix);
                        let page = ListPage {
                            entries,
                            cursor: next.as_deref().map(|t| PageCursor(SmolStr::new(t))),
                            done: next.is_none(),
                        };
                        Some((Ok(page), (next.is_none(), next)))
                    }
                    Err(e) => Some((Err(e), (true, None))),
                }
            }
        })
        .boxed()
    }

    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError> {
        if path.is_root() {
            return Ok(Entry::new("", EntryKind::Dir));
        }
        let key = self.key_for(path);
        match self.store.head(&key).await {
            Ok(meta) => {
                let mut e = Entry::new(path.file_name().unwrap_or(""), EntryKind::File);
                e.size = Some(meta.size);
                e.modified = meta.modified;
                e.etag = meta.etag;
                Ok(e)
            }
            Err(VfsError::NotFound(_)) => {
                // Maybe it's a prefix (directory) with children.
                let prefix = format!("{key}/");
                let chunk = self.store.list_page(&prefix, Some("/"), None, 1).await?;
                if chunk.objects.is_empty() && chunk.common_prefixes.is_empty() {
                    Err(VfsError::NotFound(path.clone()))
                } else {
                    Ok(Entry::new(path.file_name().unwrap_or(""), EntryKind::Dir))
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn open_read(
        &self,
        path: &VfsPath,
        range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError> {
        let key = self.key_for(path);
        let range = range.map(|r| (r.offset, r.len));
        let data = self.store.get(&key, range).await?;
        let len = data.len() as u64;
        Ok(ReadHandle::new(
            Box::new(std::io::Cursor::new(data)),
            Some(len),
        ))
    }

    async fn open_write(&self, path: &VfsPath, _opts: WriteOpts) -> Result<WriteHandle, VfsError> {
        Ok(WriteHandle::new(Box::new(ObjectWriteSink {
            store: self.store.clone(),
            key: self.key_for(path),
            buf: Vec::new(),
        })))
    }

    async fn remove(&self, path: &VfsPath, recurse: Recurse) -> Result<(), VfsError> {
        // Resolve what the path actually is rather than trusting delete-by-key: every provider
        // reports deleting a missing key as success, so a "directory" (a prefix, which is not an
        // object at all) used to be a silent no-op that reported `Ok`. The transfer engine takes
        // that `Ok` as proof a Move's source is gone, so moving a folder out of a bucket claimed
        // success and left every object in place.
        let key = self.key_for(path);
        if self.store.head(&key).await.is_ok() {
            return self.store.delete(&key).await;
        }

        // Not an object: it is a prefix, or it does not exist. List it flat (no delimiter, so the
        // whole subtree comes back) and delete what is there.
        let prefix = self.dir_prefix(path);
        let mut token: Option<String> = None;
        let mut deleted = 0usize;
        loop {
            let chunk = self
                .store
                .list_page(&prefix, None, token.as_deref(), 1000)
                .await?;
            if deleted == 0 && chunk.objects.is_empty() && chunk.next_token.is_none() {
                return Err(VfsError::NotFound(path.clone()));
            }
            if recurse == Recurse::No && (deleted > 0 || !chunk.objects.is_empty()) {
                // A non-recursive remove must refuse a non-empty prefix, as every other backend's
                // `rmdir` does, rather than deleting a subtree the caller did not ask for.
                return Err(VfsError::Backend {
                    code: "object".to_owned(),
                    msg: format!("{path} is not empty"),
                    retryable: false,
                });
            }
            for obj in &chunk.objects {
                self.store.delete(&obj.key).await?;
                deleted += 1;
            }
            match chunk.next_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        Ok(())
    }

    async fn copy_within(&self, from: &VfsPath, to: &VfsPath) -> Result<(), VfsError> {
        self.store
            .copy(&self.key_for(from), &self.key_for(to))
            .await
            .map(|_| ())
    }
}

struct ObjectWriteSink {
    store: Arc<dyn ObjectStore>,
    key: String,
    buf: Vec<u8>,
}

#[async_trait]
impl WriteSink for ObjectWriteSink {
    fn commit_mode(&self) -> CommitMode {
        // Honest declaration: every chunk is staged in `buf` and the single `put` runs in `finish`.
        // Streaming/multipart uploads are the M5-4 milestone (see issue #189); until then the engine
        // must not count these bytes as transferred.
        CommitMode::Buffered
    }

    async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), VfsError> {
        self.buf.extend_from_slice(&chunk);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<Entry, VfsError> {
        let meta = self.store.put(&self.key, self.buf).await?;
        let name = self.key.rsplit('/').next().unwrap_or(&self.key);
        let mut e = Entry::new(name, EntryKind::File);
        e.size = Some(meta.size);
        e.etag = meta.etag;
        Ok(e)
    }

    async fn abort(self: Box<Self>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockObjectStore;
    use tokio::io::AsyncReadExt;

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    fn backend() -> ObjectStoreVfs {
        let store = Arc::new(
            MockObjectStore::new()
                .with_object("top.txt", b"top")
                .with_object("logs/a.log", b"aaa")
                .with_object("logs/2026/b.log", b"bbbb"),
        );
        ObjectStoreVfs::new(ConnectionId(1), Scheme::S3, store, "")
    }

    /// Regression: `remove` deleted a single key and ignored `Recurse`. A "directory" in an object
    /// store is a prefix, not an object, so deleting its key was a no-op — and every provider
    /// reports deleting a missing key as success. The engine takes that `Ok` as proof a Move's
    /// source is gone, so moving a folder out of a bucket claimed success and left everything in it.
    #[tokio::test]
    async fn recursive_remove_deletes_the_whole_prefix() {
        let vfs = backend();
        vfs.remove(&p("/logs"), Recurse::Yes).await.unwrap();
        for gone in ["/logs/a.log", "/logs/2026/b.log", "/logs"] {
            assert!(
                matches!(vfs.stat(&p(gone)).await, Err(VfsError::NotFound(_))),
                "{gone} survived the remove"
            );
        }
        // Only the requested prefix: a sibling object is untouched.
        assert_eq!(vfs.stat(&p("/top.txt")).await.unwrap().size, Some(3));
    }

    /// A non-recursive remove refuses a non-empty prefix, like every other backend's `rmdir` —
    /// rather than silently succeeding or deleting a subtree nobody asked it to.
    #[tokio::test]
    async fn non_recursive_remove_refuses_a_non_empty_prefix() {
        let vfs = backend();
        assert!(matches!(
            vfs.remove(&p("/logs"), Recurse::No).await,
            Err(VfsError::Backend { .. })
        ));
        assert_eq!(vfs.stat(&p("/logs/a.log")).await.unwrap().size, Some(3));
    }

    /// A plain object still deletes by key, and a path that is neither an object nor a prefix is
    /// `NotFound` rather than a success that deleted nothing.
    #[tokio::test]
    async fn remove_deletes_an_object_and_reports_a_missing_path() {
        let vfs = backend();
        vfs.remove(&p("/top.txt"), Recurse::No).await.unwrap();
        assert!(matches!(
            vfs.stat(&p("/top.txt")).await,
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            vfs.remove(&p("/never-existed"), Recurse::Yes).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn lists_root_with_prefix_dirs() {
        let vfs = backend();
        let mut s = vfs.list(&p("/"), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let mut names: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["logs", "top.txt"]);
        assert!(page
            .entries
            .iter()
            .find(|e| e.name == "logs")
            .unwrap()
            .is_dir());
    }

    #[tokio::test]
    async fn navigates_into_prefix() {
        let vfs = backend();
        let mut s = vfs.list(&p("/logs"), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let mut names: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["2026", "a.log"]);
    }

    #[tokio::test]
    async fn stat_file_and_prefix_and_missing() {
        let vfs = backend();
        assert_eq!(vfs.stat(&p("/top.txt")).await.unwrap().size, Some(3));
        assert!(vfs.stat(&p("/logs")).await.unwrap().is_dir());
        assert!(matches!(
            vfs.stat(&p("/nope")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn read_write_copy_delete() {
        let vfs = backend();
        let mut rh = vfs.open_read(&p("/top.txt"), None).await.unwrap();
        let mut s = String::new();
        rh.read_to_string(&mut s).await.unwrap();
        assert_eq!(s, "top");

        let mut wh = vfs
            .open_write(&p("/new.txt"), WriteOpts::default())
            .await
            .unwrap();
        wh.write_chunk(Bytes::from_static(b"hello")).await.unwrap();
        let e = wh.finish().await.unwrap();
        assert_eq!(e.size, Some(5));

        vfs.copy_within(&p("/new.txt"), &p("/copy.txt"))
            .await
            .unwrap();
        assert_eq!(vfs.stat(&p("/copy.txt")).await.unwrap().size, Some(5));

        vfs.remove(&p("/new.txt"), Recurse::No).await.unwrap();
        assert!(matches!(
            vfs.stat(&p("/new.txt")).await,
            Err(VfsError::NotFound(_))
        ));
    }
}
