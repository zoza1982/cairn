//! Docker/OCI container backend.
//!
//! Presents containers and images as a navigable tree: `/containers/<name>/…` browses a container's
//! filesystem and `/images/<tag>` lists images. The path-routing and entry-mapping logic lives in
//! [`DockerVfs`] over a [`ContainerOps`] seam and is fully unit-tested against an in-memory mock; the
//! real engine access is the [`BollardDocker`] adapter. See `docs/LLD.md` §3.6 and RFC-0004.

mod ops;
mod real;

pub use ops::{ContainerInfo, ContainerOps, ImageInfo, RemoteEntry, RemoteMeta};
pub use real::BollardDocker;

use async_trait::async_trait;
use cairn_types::{Caps, ConnectionId, Entry, EntryExt, EntryKind, Scheme, VfsPath};
use cairn_vfs::{
    ByteRange, CapabilityProvider, ListOpts, ListPage, ReadHandle, Recurse, Vfs, VfsError,
    WriteHandle, WriteOpts,
};
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use smol_str::SmolStr;
use std::sync::Arc;

/// A [`Vfs`] over a Docker engine. Read-only browse of containers, images, and container filesystems.
pub struct DockerVfs<O: ContainerOps> {
    conn: ConnectionId,
    ops: Arc<O>,
}

impl<O: ContainerOps> DockerVfs<O> {
    /// Create a backend over the given engine.
    pub fn new(conn: ConnectionId, ops: O) -> Self {
        Self {
            conn,
            ops: Arc::new(ops),
        }
    }

    async fn list_dir(&self, dir: VfsPath) -> Result<ListPage, VfsError> {
        let segs: Vec<&str> = dir.segments().iter().map(SmolStr::as_str).collect();
        let entries = match segs.as_slice() {
            [] => vec![
                Entry::new("containers", EntryKind::Dir),
                Entry::new("images", EntryKind::Dir),
            ],
            ["containers"] => self
                .ops
                .list_containers()
                .await?
                .into_iter()
                .map(|c| {
                    let mut e = Entry::new(c.name, EntryKind::Dir);
                    e.ext = EntryExt::Container {
                        id: SmolStr::new(c.id),
                        state: c.state,
                        image: SmolStr::new(c.image),
                    };
                    e
                })
                .collect(),
            ["images"] => self
                .ops
                .list_images()
                .await?
                .into_iter()
                .map(|img| {
                    let name = img.tags.first().cloned().unwrap_or_else(|| img.id.clone());
                    let mut e = Entry::new(name, EntryKind::Dir);
                    e.ext = EntryExt::Image {
                        id: SmolStr::new(img.id),
                        layers: 0,
                        tags: img.tags.into_iter().map(SmolStr::new).collect(),
                    };
                    e
                })
                .collect(),
            ["images", _one] => Vec::new(), // image layer browse deferred
            ["containers", name, rest @ ..] => {
                let in_path = join_in_container(rest);
                self.ops
                    .list_dir(name, &in_path)
                    .await?
                    .into_iter()
                    .map(|r| {
                        let mut e = Entry::new(r.name, r.kind);
                        if r.kind == EntryKind::File {
                            e.size = r.size;
                        }
                        e
                    })
                    .collect()
            }
            _ => return Err(VfsError::NotFound(dir)),
        };
        Ok(ListPage {
            entries,
            cursor: None,
            done: true,
        })
    }
}

/// Build the in-container absolute path from the trailing segments.
fn join_in_container(rest: &[&str]) -> String {
    if rest.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", rest.join("/"))
    }
}

impl<O: ContainerOps> CapabilityProvider for DockerVfs<O> {
    fn caps(&self) -> Caps {
        Caps::LIST | Caps::READ | Caps::RANDOM_READ
    }
}

#[async_trait]
impl<O: ContainerOps> Vfs for DockerVfs<O> {
    fn scheme(&self) -> Scheme {
        Scheme::Docker
    }

    fn connection(&self) -> ConnectionId {
        self.conn
    }

    fn list<'a>(
        &'a self,
        dir: &VfsPath,
        _opts: ListOpts,
    ) -> BoxStream<'a, Result<ListPage, VfsError>> {
        let dir = dir.clone();
        stream::once(async move { self.list_dir(dir).await }).boxed()
    }

    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();
        match segs.as_slice() {
            [] | ["containers"] | ["images"] | ["images", _] => {
                Ok(Entry::new(path.file_name().unwrap_or(""), EntryKind::Dir))
            }
            ["containers", name] => {
                let exists = self
                    .ops
                    .list_containers()
                    .await?
                    .iter()
                    .any(|c| c.name == *name);
                if exists {
                    Ok(Entry::new(*name, EntryKind::Dir))
                } else {
                    Err(VfsError::NotFound(path.clone()))
                }
            }
            ["containers", name, rest @ ..] => {
                let m = self.ops.stat(name, &join_in_container(rest)).await?;
                let mut e = Entry::new(path.file_name().unwrap_or(""), m.kind);
                if m.kind == EntryKind::File {
                    e.size = m.size;
                }
                Ok(e)
            }
            _ => Err(VfsError::NotFound(path.clone())),
        }
    }

    async fn open_read(
        &self,
        path: &VfsPath,
        range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();
        let data = match segs.as_slice() {
            ["containers", name, rest @ ..] if !rest.is_empty() => {
                self.ops.read(name, &join_in_container(rest)).await?
            }
            _ => return Err(VfsError::Unsupported(Caps::READ)),
        };
        let sliced = match range {
            None => data,
            Some(r) => {
                let total = data.len() as u64;
                let start = r.offset.min(total) as usize;
                let end = match r.len {
                    Some(l) => ((r.offset + l).min(total)) as usize,
                    None => data.len(),
                };
                data[start..end].to_vec()
            }
        };
        let len = sliced.len() as u64;
        Ok(ReadHandle::new(
            Box::new(std::io::Cursor::new(sliced)),
            Some(len),
        ))
    }

    async fn open_write(&self, _path: &VfsPath, _opts: WriteOpts) -> Result<WriteHandle, VfsError> {
        Err(VfsError::Unsupported(Caps::WRITE))
    }

    async fn remove(&self, _path: &VfsPath, _recurse: Recurse) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::DELETE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ops::mock::MockDocker;
    use tokio::io::AsyncReadExt;

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    fn backend() -> DockerVfs<MockDocker> {
        DockerVfs::new(ConnectionId(1), MockDocker::new())
    }

    async fn names(vfs: &DockerVfs<MockDocker>, path: &str) -> Vec<String> {
        let mut s = vfs.list(&p(path), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let mut n: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        n.sort();
        n
    }

    #[tokio::test]
    async fn root_lists_containers_and_images() {
        assert_eq!(names(&backend(), "/").await, vec!["containers", "images"]);
    }

    #[tokio::test]
    async fn lists_containers_and_images() {
        assert_eq!(names(&backend(), "/containers").await, vec!["web"]);
        assert_eq!(names(&backend(), "/images").await, vec!["nginx:latest"]);
    }

    #[tokio::test]
    async fn navigates_container_filesystem() {
        let vfs = backend();
        assert_eq!(names(&vfs, "/containers/web").await, vec!["app", "etc"]);
        assert_eq!(
            names(&vfs, "/containers/web/etc").await,
            vec!["hostname", "hosts"]
        );
    }

    #[tokio::test]
    async fn reads_a_container_file() {
        let vfs = backend();
        let mut rh = vfs
            .open_read(&p("/containers/web/etc/hostname"), None)
            .await
            .unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "web-1\n");
    }

    #[tokio::test]
    async fn stat_distinguishes_dirs_and_files() {
        let vfs = backend();
        assert!(vfs.stat(&p("/containers")).await.unwrap().is_dir());
        assert!(vfs.stat(&p("/containers/web")).await.unwrap().is_dir());
        assert!(vfs.stat(&p("/containers/web/etc")).await.unwrap().is_dir());
        assert_eq!(
            vfs.stat(&p("/containers/web/etc/hostname"))
                .await
                .unwrap()
                .size,
            Some(6)
        );
        assert!(matches!(
            vfs.stat(&p("/containers/nope")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn writes_are_unsupported() {
        let vfs = backend();
        assert!(matches!(
            vfs.open_write(&p("/containers/web/x"), WriteOpts::default())
                .await,
            Err(VfsError::Unsupported(_))
        ));
    }
}
