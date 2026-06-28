//! Kubernetes backend.
//!
//! Presents a cluster as a navigable tree: `/<context>/<namespace>/<pod>/<container>/<path…>`.
//! The depth-based path-routing and entry-mapping logic lives in [`KubeVfs`] over a [`KubeOps`]
//! seam and is fully unit-tested against an in-memory mock. Capabilities vary by depth
//! ([`CapabilityProvider::caps_at`]): listing everywhere, file read only inside a container.
//!
//! This crate ships the read-only mapping core. The live `kube-rs` adapter (kubeconfig/exec-plugin
//! auth via the broker, tar-over-`exec` filesystem access) and the action surface (log streaming,
//! interactive `exec`, port-forward) are the integration step; see `docs/LLD.md` and RFC-0005.

mod ops;

pub use ops::{ContainerInfo, ContextInfo, KubeOps, PodInfo, RemoteEntry, RemoteMeta};

use async_trait::async_trait;
use cairn_types::{Caps, ConnectionId, Entry, EntryExt, EntryKind, Scheme, VfsPath};
use cairn_vfs::{
    apply_byte_range, ByteRange, CapabilityProvider, ListOpts, ListPage, ReadHandle, Recurse, Vfs,
    VfsError, WriteHandle, WriteOpts,
};
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use smol_str::SmolStr;
use std::sync::Arc;

/// The path depth at which a container's filesystem begins: `[ctx, ns, pod, container]`.
const CONTAINER_DEPTH: usize = 4;

/// A [`Vfs`] over a Kubernetes cluster. Read-only browse of contexts, namespaces, pods, containers,
/// and (via the real adapter) container filesystems.
pub struct KubeVfs<O: KubeOps> {
    conn: ConnectionId,
    ops: Arc<O>,
}

impl<O: KubeOps> KubeVfs<O> {
    /// Create a backend over the given cluster surface.
    pub fn new(conn: ConnectionId, ops: O) -> Self {
        Self {
            conn,
            ops: Arc::new(ops),
        }
    }

    async fn list_dir(&self, dir: VfsPath) -> Result<ListPage, VfsError> {
        let segs: Vec<&str> = dir.segments().iter().map(SmolStr::as_str).collect();
        let entries = match segs.as_slice() {
            [] => self
                .ops
                .list_contexts()
                .await?
                .into_iter()
                .map(|c| Entry::new(c.name, EntryKind::Dir))
                .collect(),
            [ctx] => self
                .ops
                .list_namespaces(ctx)
                .await?
                .into_iter()
                .map(|ns| Entry::new(ns, EntryKind::Dir))
                .collect(),
            [ctx, ns] => self
                .ops
                .list_pods(ctx, ns)
                .await?
                .into_iter()
                .map(|p| {
                    let mut e = Entry::new(p.name, EntryKind::Dir);
                    e.ext = EntryExt::Pod {
                        phase: p.phase,
                        ready: p.ready,
                        node: p.node.map(SmolStr::new),
                    };
                    e
                })
                .collect(),
            [ctx, ns, pod] => self
                .ops
                .list_containers(ctx, ns, pod)
                .await?
                .into_iter()
                .map(|c| Entry::new(c.name, EntryKind::Dir))
                .collect(),
            [ctx, ns, pod, container, rest @ ..] => self
                .ops
                .list_dir(ctx, ns, pod, container, &join_in_container(rest))
                .await?
                .into_iter()
                .map(|r| {
                    let mut e = Entry::new(r.name, r.kind);
                    if r.kind == EntryKind::File {
                        e.size = r.size;
                    }
                    e
                })
                .collect(),
        };
        Ok(ListPage {
            entries,
            cursor: None,
            done: true,
        })
    }
}

/// Build the in-container absolute path from the trailing segments below the container root.
fn join_in_container(rest: &[&str]) -> String {
    if rest.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", rest.join("/"))
    }
}

impl<O: KubeOps> CapabilityProvider for KubeVfs<O> {
    fn caps(&self) -> Caps {
        // Baseline: listing the cluster tree. File read is refined in per-path `caps_at`.
        Caps::LIST
    }

    fn caps_at(&self, path: &VfsPath) -> Caps {
        // A container's filesystem (depth >= 4) supports reads; the navigation tree above it
        // (contexts/namespaces/pods/containers) is list-only. Writes/exec/logs are M6-6 actions.
        if path.segments().len() >= CONTAINER_DEPTH {
            Caps::LIST | Caps::READ | Caps::RANDOM_READ
        } else {
            Caps::LIST
        }
    }
}

#[async_trait]
impl<O: KubeOps> Vfs for KubeVfs<O> {
    fn scheme(&self) -> Scheme {
        Scheme::Kubernetes
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
            [] => Ok(Entry::new("", EntryKind::Dir)),
            [ctx] => {
                if self
                    .ops
                    .list_contexts()
                    .await?
                    .iter()
                    .any(|c| c.name == *ctx)
                {
                    Ok(Entry::new(*ctx, EntryKind::Dir))
                } else {
                    Err(VfsError::NotFound(path.clone()))
                }
            }
            [ctx, ns] => {
                if self.ops.list_namespaces(ctx).await?.iter().any(|n| n == ns) {
                    Ok(Entry::new(*ns, EntryKind::Dir))
                } else {
                    Err(VfsError::NotFound(path.clone()))
                }
            }
            [ctx, ns, pod] => {
                let info = self
                    .ops
                    .list_pods(ctx, ns)
                    .await?
                    .into_iter()
                    .find(|p| p.name == *pod)
                    .ok_or_else(|| VfsError::NotFound(path.clone()))?;
                let mut e = Entry::new(*pod, EntryKind::Dir);
                e.ext = EntryExt::Pod {
                    phase: info.phase,
                    ready: info.ready,
                    node: info.node.map(SmolStr::new),
                };
                Ok(e)
            }
            [ctx, ns, pod, container] => {
                if self
                    .ops
                    .list_containers(ctx, ns, pod)
                    .await?
                    .iter()
                    .any(|c| c.name == *container)
                {
                    Ok(Entry::new(*container, EntryKind::Dir))
                } else {
                    Err(VfsError::NotFound(path.clone()))
                }
            }
            [ctx, ns, pod, container, rest @ ..] => {
                let m = self
                    .ops
                    .stat(ctx, ns, pod, container, &join_in_container(rest))
                    .await?;
                let mut e = Entry::new(path.file_name().unwrap_or(""), m.kind);
                if m.kind == EntryKind::File {
                    e.size = m.size;
                }
                Ok(e)
            }
        }
    }

    async fn open_read(
        &self,
        path: &VfsPath,
        range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();
        let data = match segs.as_slice() {
            [ctx, ns, pod, container, rest @ ..] if !rest.is_empty() => {
                self.ops
                    .read(ctx, ns, pod, container, &join_in_container(rest))
                    .await?
            }
            _ => return Err(VfsError::Unsupported(Caps::READ)),
        };
        let sliced = match range {
            None => data,
            // Clamp in memory (no transport-level seek yet); saturating, so a pathological
            // caller-controlled range can never overflow or panic.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use ops::mock::MockKube;
    use tokio::io::AsyncReadExt;

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    fn backend() -> KubeVfs<MockKube> {
        KubeVfs::new(ConnectionId(1), MockKube::new())
    }

    async fn names(vfs: &KubeVfs<MockKube>, path: &str) -> Vec<String> {
        let mut s = vfs.list(&p(path), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let mut n: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        n.sort();
        n
    }

    #[tokio::test]
    async fn navigates_the_cluster_tree() {
        let vfs = backend();
        assert_eq!(names(&vfs, "/").await, vec!["prod"]);
        assert_eq!(names(&vfs, "/prod").await, vec!["default"]);
        assert_eq!(names(&vfs, "/prod/default").await, vec!["web-0"]);
        assert_eq!(
            names(&vfs, "/prod/default/web-0").await,
            vec!["app", "sidecar"]
        );
    }

    #[tokio::test]
    async fn navigates_container_filesystem() {
        let vfs = backend();
        assert_eq!(
            names(&vfs, "/prod/default/web-0/app").await,
            vec!["app", "etc"]
        );
        assert_eq!(
            names(&vfs, "/prod/default/web-0/app/etc").await,
            vec!["hostname", "hosts"]
        );
    }

    #[tokio::test]
    async fn reads_a_container_file() {
        let vfs = backend();
        let mut rh = vfs
            .open_read(&p("/prod/default/web-0/app/etc/hostname"), None)
            .await
            .unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "web-0\n");
    }

    #[tokio::test]
    async fn ranged_read_clamps_and_never_panics() {
        let vfs = backend();
        let path = p("/prod/default/web-0/app/etc/hostname"); // "web-0\n", 6 bytes
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
    }

    #[tokio::test]
    async fn stat_reports_pod_phase_and_rejects_unknown() {
        let vfs = backend();
        let pod = vfs.stat(&p("/prod/default/web-0")).await.unwrap();
        assert!(matches!(
            pod.ext,
            EntryExt::Pod {
                phase: cairn_types::PodPhase::Running,
                ready: (2, 2),
                ..
            }
        ));
        assert!(matches!(
            vfs.stat(&p("/prod/default/nope")).await,
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            vfs.stat(&p("/nope")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn caps_vary_by_depth() {
        let vfs = backend();
        assert_eq!(vfs.caps_at(&p("/prod/default/web-0")), Caps::LIST);
        assert_eq!(
            vfs.caps_at(&p("/prod/default/web-0/app/etc/hostname")),
            Caps::LIST | Caps::READ | Caps::RANDOM_READ
        );
    }

    #[tokio::test]
    async fn writes_are_unsupported() {
        let vfs = backend();
        assert!(matches!(
            vfs.open_write(&p("/prod/default/web-0/app/x"), WriteOpts::default())
                .await,
            Err(VfsError::Unsupported(_))
        ));
    }
}
