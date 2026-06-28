//! [`MockObjectStore`]: an in-memory [`ObjectStore`] emulating S3-style prefix/delimiter listing.

use crate::{ListChunk, ObjectMeta, ObjectStore};
use async_trait::async_trait;
use cairn_types::Caps;
use cairn_vfs::VfsError;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// In-memory object store for tests.
pub struct MockObjectStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl Default for MockObjectStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MockObjectStore {
    /// Empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(BTreeMap::new()),
        }
    }

    /// Builder: insert an object.
    #[must_use]
    pub fn with_object(self, key: &str, data: &[u8]) -> Self {
        self.objects
            .lock()
            .expect("mock store mutex")
            .insert(key.to_owned(), data.to_vec());
        self
    }

    fn meta(key: &str, data: &[u8]) -> ObjectMeta {
        ObjectMeta {
            key: key.to_owned(),
            size: data.len() as u64,
            etag: None,
            modified: None,
            storage_class: None,
        }
    }
}

#[async_trait]
impl ObjectStore for MockObjectStore {
    fn capabilities(&self) -> Caps {
        Caps::LIST | Caps::READ | Caps::WRITE | Caps::DELETE | Caps::COPY_SERVER | Caps::RANDOM_READ
    }

    async fn list_page(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        _token: Option<&str>,
        _max: usize,
    ) -> Result<ListChunk, VfsError> {
        let objects = self.objects.lock().expect("mock store mutex");
        let mut common: BTreeSet<String> = BTreeSet::new();
        let mut direct: Vec<ObjectMeta> = Vec::new();
        for (key, data) in objects.iter() {
            let Some(rest) = key.strip_prefix(prefix) else {
                continue;
            };
            match delimiter.and_then(|d| rest.find(d)) {
                Some(idx) => {
                    // Group under the common prefix up to and including the delimiter.
                    let cp = &rest[..=idx];
                    common.insert(format!("{prefix}{cp}"));
                }
                None => direct.push(Self::meta(key, data)),
            }
        }
        Ok(ListChunk {
            common_prefixes: common.into_iter().collect(),
            objects: direct,
            next_token: None,
        })
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, VfsError> {
        let objects = self.objects.lock().expect("mock store mutex");
        objects
            .get(key)
            .map(|d| Self::meta(key, d))
            .ok_or_else(|| VfsError::NotFound(cairn_types::VfsPath::root()))
    }

    async fn get(&self, key: &str, range: Option<(u64, Option<u64>)>) -> Result<Vec<u8>, VfsError> {
        let objects = self.objects.lock().expect("mock store mutex");
        let data = objects
            .get(key)
            .ok_or_else(|| VfsError::NotFound(cairn_types::VfsPath::root()))?;
        Ok(match range {
            None => data.clone(),
            Some((off, len)) => {
                let total = data.len() as u64;
                let start = off.min(total) as usize;
                let end = match len {
                    Some(l) => ((off + l).min(total)) as usize,
                    None => data.len(),
                };
                data[start..end].to_vec()
            }
        })
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> Result<ObjectMeta, VfsError> {
        let meta = Self::meta(key, &data);
        self.objects
            .lock()
            .expect("mock store mutex")
            .insert(key.to_owned(), data);
        Ok(meta)
    }

    async fn delete(&self, key: &str) -> Result<(), VfsError> {
        self.objects.lock().expect("mock store mutex").remove(key);
        Ok(())
    }

    async fn copy(&self, from: &str, to: &str) -> Result<ObjectMeta, VfsError> {
        let mut objects = self.objects.lock().expect("mock store mutex");
        let data = objects
            .get(from)
            .cloned()
            .ok_or_else(|| VfsError::NotFound(cairn_types::VfsPath::root()))?;
        let meta = Self::meta(to, &data);
        objects.insert(to.to_owned(), data);
        Ok(meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_page_groups_by_delimiter() {
        let store = MockObjectStore::new()
            .with_object("a.txt", b"1")
            .with_object("dir/b.txt", b"2")
            .with_object("dir/sub/c.txt", b"3");
        let chunk = store.list_page("", Some("/"), None, 100).await.unwrap();
        assert_eq!(chunk.common_prefixes, vec!["dir/".to_owned()]);
        assert_eq!(chunk.objects.len(), 1);
        assert_eq!(chunk.objects[0].key, "a.txt");

        let sub = store.list_page("dir/", Some("/"), None, 100).await.unwrap();
        assert_eq!(sub.common_prefixes, vec!["dir/sub/".to_owned()]);
        assert_eq!(sub.objects.len(), 1);
        assert_eq!(sub.objects[0].key, "dir/b.txt");
    }
}
