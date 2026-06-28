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
    action_ids, apply_byte_range, join_abs_path, ActionCtx, ActionDescriptor, ActionId, ActionKind,
    ActionOutcome, ByteRange, CapabilityProvider, ListOpts, ListPage, ReadHandle, Recurse, Vfs,
    VfsError, WriteHandle, WriteOpts,
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
            ["images", tag] => {
                // The image must exist; its layer browse is deferred (RFC-0004).
                let exists = self
                    .ops
                    .list_images()
                    .await?
                    .iter()
                    .any(|img| img.tags.iter().any(|t| t == tag) || img.id == *tag);
                if exists {
                    Vec::new()
                } else {
                    return Err(VfsError::NotFound(dir));
                }
            }
            ["containers", name, rest @ ..] => {
                let in_path = join_abs_path(rest);
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

impl<O: ContainerOps> CapabilityProvider for DockerVfs<O> {
    fn caps(&self) -> Caps {
        // The Vfs mapping honors ranged reads (in-memory clamp). The real adapter's in-container
        // fs ops (list/read) are the deferred integration step; until then they return Unsupported.
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
            [] | ["containers"] | ["images"] => {
                Ok(Entry::new(path.file_name().unwrap_or(""), EntryKind::Dir))
            }
            ["images", tag] => {
                let exists = self
                    .ops
                    .list_images()
                    .await?
                    .iter()
                    .any(|img| img.tags.iter().any(|t| t == tag) || img.id == *tag);
                if exists {
                    Ok(Entry::new(*tag, EntryKind::Dir))
                } else {
                    Err(VfsError::NotFound(path.clone()))
                }
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
                let m = self.ops.stat(name, &join_abs_path(rest)).await?;
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
                self.ops.read(name, &join_abs_path(rest)).await?
            }
            _ => return Err(VfsError::Unsupported(Caps::READ)),
        };
        let sliced = match range {
            None => data,
            // Clamp in memory (no transport-level seek yet); saturating, so a pathological
            // caller-controlled range can never overflow or panic the slice.
            Some(r) => apply_byte_range(&data, r).to_vec(),
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

    /// Advertise the per-container actions (`logs`, `exec`) anywhere within a container's subtree.
    /// This reflects path *shape*, not existence (it does no I/O, mirroring how the UI calls it on an
    /// already-navigated node); existence is enforced by `stat`/`invoke`. The action surface is
    /// discoverable now; live invocation (streaming over the Docker API) is the integration step, so
    /// the inherited [`Vfs::invoke`] still returns [`VfsError::Unsupported`].
    fn actions_at(&self, path: &VfsPath) -> Vec<ActionDescriptor> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();
        match segs.as_slice() {
            ["containers", _name, ..] => vec![
                ActionDescriptor {
                    id: ActionId::new(action_ids::LOGS),
                    label: SmolStr::new("Stream logs"),
                    kind: ActionKind::Stream,
                    destructive: false,
                },
                ActionDescriptor {
                    id: ActionId::new(action_ids::EXEC),
                    label: SmolStr::new("Exec"),
                    kind: ActionKind::Interactive,
                    destructive: false,
                },
            ],
            _ => Vec::new(),
        }
    }

    async fn invoke(
        &self,
        _path: &VfsPath,
        action: ActionId,
        _ctx: ActionCtx,
    ) -> Result<ActionOutcome, VfsError> {
        // The actions are advertised by `actions_at`; live invocation over the Docker API (streaming
        // exec/logs) is the integration step — report that distinctly from a truly-unknown action.
        Err(VfsError::Backend {
            code: "not_implemented".to_owned(),
            msg: format!("action '{}' is not yet available", action.as_str()),
            retryable: false,
        })
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
    async fn ranged_read_clamps_and_never_panics() {
        let vfs = backend();
        let path = p("/containers/web/etc/hostname"); // "web-1\n", 6 bytes
                                                      // A pathological range must clamp to empty, not overflow/panic.
        let mut rh = vfs
            .open_read(
                &path,
                Some(ByteRange {
                    offset: u64::MAX,
                    len: Some(1),
                }),
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        rh.read_to_end(&mut out).await.unwrap();
        assert!(out.is_empty());
        // A normal sub-range still works.
        let mut rh = vfs
            .open_read(
                &path,
                Some(ByteRange {
                    offset: 0,
                    len: Some(3),
                }),
            )
            .await
            .unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "web");
    }

    #[tokio::test]
    async fn stat_rejects_unknown_image_and_routes() {
        let vfs = backend();
        assert!(vfs.stat(&p("/images/nginx:latest")).await.unwrap().is_dir());
        assert!(matches!(
            vfs.stat(&p("/images/nope")).await,
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            vfs.stat(&p("/bogus")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn listing_a_file_or_missing_dir_is_not_found() {
        let vfs = backend();
        assert!(matches!(
            vfs.list(&p("/containers/web/etc/hostname"), ListOpts::default())
                .next()
                .await
                .unwrap(),
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            vfs.list(&p("/containers/web/missing"), ListOpts::default())
                .next()
                .await
                .unwrap(),
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

    #[tokio::test]
    async fn containers_advertise_exec_and_logs_actions() {
        let vfs = backend();
        // Containers (and their subtree) expose exec + logs; the top-level tree does not.
        let ids: Vec<String> = vfs
            .actions_at(&p("/containers/web"))
            .iter()
            .map(|a| a.id.as_str().to_owned())
            .collect();
        assert_eq!(ids, vec!["logs", "exec"]);
        assert!(!vfs.actions_at(&p("/containers/web/etc")).is_empty());
        assert!(vfs.actions_at(&p("/containers")).is_empty());
        assert!(vfs.actions_at(&p("/images/nginx:latest")).is_empty());
        // Reflects path shape, not existence (a phantom container still lists actions).
        assert!(!vfs.actions_at(&p("/containers/ghost")).is_empty());
        // Invocation is the integration step — advertised but not yet implemented.
        assert!(matches!(
            vfs.invoke(&p("/containers/web"), ActionId::new(action_ids::LOGS), ActionCtx::None)
                .await,
            Err(VfsError::Backend { code, .. }) if code == "not_implemented"
        ));
    }
}
