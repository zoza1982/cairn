//! The [`ContainerOps`] transport seam for the Docker backend, plus an in-memory mock.

use async_trait::async_trait;
use bytes::Bytes;
use cairn_types::{ContainerState, EntryKind};
use cairn_vfs::VfsError;
use futures::stream::BoxStream;

/// Summary of a container.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    /// Container id.
    pub id: String,
    /// Primary name.
    pub name: String,
    /// Image reference.
    pub image: String,
    /// Runtime state.
    pub state: ContainerState,
}

/// Summary of an image.
#[derive(Debug, Clone)]
pub struct ImageInfo {
    /// Image id.
    pub id: String,
    /// Tags pointing at the image.
    pub tags: Vec<String>,
}

/// One entry inside a container's filesystem.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    /// Leaf name.
    pub name: String,
    /// Kind.
    pub kind: EntryKind,
    /// Size (files).
    pub size: Option<u64>,
}

/// Metadata for a path inside a container.
#[derive(Debug, Clone)]
pub struct RemoteMeta {
    /// Kind.
    pub kind: EntryKind,
    /// Size (files).
    pub size: Option<u64>,
}

/// The Docker engine surface the backend needs. Implemented by the bollard adapter and the mock.
#[async_trait]
pub trait ContainerOps: Send + Sync + 'static {
    /// List containers.
    async fn list_containers(&self) -> Result<Vec<ContainerInfo>, VfsError>;
    /// List images.
    async fn list_images(&self) -> Result<Vec<ImageInfo>, VfsError>;
    /// List a directory inside a container's filesystem.
    async fn list_dir(&self, container: &str, path: &str) -> Result<Vec<RemoteEntry>, VfsError>;
    /// Stat a path inside a container.
    async fn stat(&self, container: &str, path: &str) -> Result<RemoteMeta, VfsError>;
    /// Read a file inside a container.
    async fn read(&self, container: &str, path: &str) -> Result<Vec<u8>, VfsError>;
    /// Stream log output from a container.
    ///
    /// Each item in the returned stream carries one log frame as raw [`Bytes`]. Bollard's
    /// [`LogOutput`] type already demultiplexes Docker's 8-byte multiplexed stream header (used
    /// when no TTY is allocated), so callers receive plain payload bytes — stdout and stderr
    /// interleaved in arrival order — not raw wire frames.
    ///
    /// `follow` controls whether the stream tails live output (`true`) or returns only buffered
    /// history and then ends (`false`). `tail` is passed verbatim as the Docker `tail` query
    /// parameter (`"all"` for all history, a decimal number for the last N lines).
    ///
    /// Error mapping: a 404 from the daemon (container not found) surfaces as
    /// [`VfsError::NotFound`] in the stream; any other engine error becomes [`VfsError::Backend`].
    /// The stream never panics; it may be empty for containers with no log output.
    fn logs(
        &self,
        container: &str,
        follow: bool,
        tail: &str,
    ) -> BoxStream<'static, Result<Bytes, VfsError>>;
}

#[cfg(test)]
pub(crate) mod mock {
    use super::{ContainerInfo, ContainerOps, ImageInfo, RemoteEntry, RemoteMeta};
    use async_trait::async_trait;
    use bytes::Bytes;
    use cairn_types::{ContainerState, EntryKind, VfsPath};
    use cairn_vfs::VfsError;
    use futures::stream::{self, BoxStream};
    use futures::StreamExt as _;
    use std::collections::BTreeMap;

    /// In-memory Docker engine for tests: a few containers, each with a file tree, and images.
    pub(crate) struct MockDocker {
        containers: Vec<ContainerInfo>,
        images: Vec<ImageInfo>,
        /// container name -> (path -> file bytes; directories are implied by path prefixes).
        files: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    }

    impl MockDocker {
        pub(crate) fn new() -> Self {
            let mut files = BTreeMap::new();
            let mut web: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            web.insert("/etc/hostname".to_owned(), b"web-1\n".to_vec());
            web.insert("/etc/hosts".to_owned(), b"127.0.0.1 localhost\n".to_vec());
            web.insert("/app/main.rs".to_owned(), b"fn main() {}\n".to_vec());
            files.insert("web".to_owned(), web);
            Self {
                containers: vec![ContainerInfo {
                    id: "abc123".to_owned(),
                    name: "web".to_owned(),
                    image: "nginx:latest".to_owned(),
                    state: ContainerState::Running,
                }],
                images: vec![ImageInfo {
                    id: "img1".to_owned(),
                    tags: vec!["nginx:latest".to_owned()],
                }],
                files,
            }
        }

        fn nf(path: &str) -> VfsError {
            VfsError::NotFound(VfsPath::parse(path).unwrap_or_else(|_| VfsPath::root()))
        }
    }

    #[async_trait]
    impl ContainerOps for MockDocker {
        async fn list_containers(&self) -> Result<Vec<ContainerInfo>, VfsError> {
            Ok(self.containers.clone())
        }

        async fn list_images(&self) -> Result<Vec<ImageInfo>, VfsError> {
            Ok(self.images.clone())
        }

        async fn list_dir(
            &self,
            container: &str,
            path: &str,
        ) -> Result<Vec<RemoteEntry>, VfsError> {
            let tree = self
                .files
                .get(container)
                .ok_or_else(|| Self::nf(container))?;
            // A path that is itself a file is not a directory.
            if tree.contains_key(path) {
                return Err(Self::nf(path));
            }
            let prefix = if path == "/" {
                "/".to_owned()
            } else {
                format!("{path}/")
            };
            // A non-root directory must have at least one child to exist.
            if path != "/" && !tree.keys().any(|k| k.starts_with(&prefix)) {
                return Err(Self::nf(path));
            }
            let mut dirs = std::collections::BTreeSet::new();
            let mut files = Vec::new();
            for (key, data) in tree {
                let Some(rest) = key.strip_prefix(&prefix) else {
                    continue;
                };
                match rest.split_once('/') {
                    Some((dir, _)) => {
                        dirs.insert(dir.to_owned());
                    }
                    None => files.push(RemoteEntry {
                        name: rest.to_owned(),
                        kind: EntryKind::File,
                        size: Some(data.len() as u64),
                    }),
                }
            }
            let mut out: Vec<RemoteEntry> = dirs
                .into_iter()
                .map(|name| RemoteEntry {
                    name,
                    kind: EntryKind::Dir,
                    size: None,
                })
                .collect();
            out.extend(files);
            Ok(out)
        }

        async fn stat(&self, container: &str, path: &str) -> Result<RemoteMeta, VfsError> {
            let tree = self
                .files
                .get(container)
                .ok_or_else(|| Self::nf(container))?;
            // The container root is always a directory (even when empty).
            if path == "/" {
                return Ok(RemoteMeta {
                    kind: EntryKind::Dir,
                    size: None,
                });
            }
            if let Some(data) = tree.get(path) {
                return Ok(RemoteMeta {
                    kind: EntryKind::File,
                    size: Some(data.len() as u64),
                });
            }
            // A directory if any key is under it.
            let prefix = format!("{path}/");
            if tree.keys().any(|k| k.starts_with(&prefix)) {
                Ok(RemoteMeta {
                    kind: EntryKind::Dir,
                    size: None,
                })
            } else {
                Err(Self::nf(path))
            }
        }

        async fn read(&self, container: &str, path: &str) -> Result<Vec<u8>, VfsError> {
            let tree = self
                .files
                .get(container)
                .ok_or_else(|| Self::nf(container))?;
            tree.get(path).cloned().ok_or_else(|| Self::nf(path))
        }

        fn logs(
            &self,
            container: &str,
            _follow: bool,
            _tail: &str,
        ) -> BoxStream<'static, Result<Bytes, VfsError>> {
            // Return a canned two-line log for the known "web" container; error for unknown ones.
            if self.containers.iter().any(|c| c.name == container) {
                let lines: Vec<Result<Bytes, VfsError>> = vec![
                    Ok(Bytes::from_static(b"[mock] log line 1\n")),
                    Ok(Bytes::from_static(b"[mock] log line 2\n")),
                ];
                stream::iter(lines).boxed()
            } else {
                let err = VfsError::NotFound(
                    VfsPath::parse(container).unwrap_or_else(|_| VfsPath::root()),
                );
                stream::iter(vec![Err(err)]).boxed()
            }
        }
    }
}
