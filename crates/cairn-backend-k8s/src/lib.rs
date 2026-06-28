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
    apply_byte_range, join_abs_path, ActionDescriptor, ActionId, ActionKind, ByteRange,
    CapabilityProvider, ListOpts, ListPage, ReadHandle, Recurse, Vfs, VfsError, WriteHandle,
    WriteOpts,
};
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use smol_str::SmolStr;
use std::sync::Arc;

/// Depth of the container node `[ctx, ns, pod, container]`. Paths strictly deeper than this are
/// inside the container's filesystem.
const CONTAINER_LEVEL: usize = 4;

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
                .map(|c| {
                    let mut e = Entry::new(c.name, EntryKind::Dir);
                    e.ext = EntryExt::KubeContainer {
                        is_init: c.is_init,
                        is_ephemeral: c.is_ephemeral,
                    };
                    e
                })
                .collect(),
            [ctx, ns, pod, container, rest @ ..] => self
                .ops
                .list_dir(ctx, ns, pod, container, &join_abs_path(rest))
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

impl<O: KubeOps> CapabilityProvider for KubeVfs<O> {
    fn caps(&self) -> Caps {
        // Baseline: listing the cluster tree. File read is refined in per-path `caps_at`.
        Caps::LIST
    }

    fn caps_at(&self, path: &VfsPath) -> Caps {
        // Reads are served only strictly inside a container's filesystem (depth > 4). The container
        // node itself (depth 4) and the navigation tree above it (contexts/namespaces/pods/
        // containers) are list-only — matching `open_read`, which serves a read only when there is
        // an in-container path below the container. Writes/exec/logs are M6-6 actions.
        if path.segments().len() > CONTAINER_LEVEL {
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
                let info = self
                    .ops
                    .list_containers(ctx, ns, pod)
                    .await?
                    .into_iter()
                    .find(|c| c.name == *container)
                    .ok_or_else(|| VfsError::NotFound(path.clone()))?;
                let mut e = Entry::new(*container, EntryKind::Dir);
                e.ext = EntryExt::KubeContainer {
                    is_init: info.is_init,
                    is_ephemeral: info.is_ephemeral,
                };
                Ok(e)
            }
            [ctx, ns, pod, container, rest @ ..] => {
                let m = self
                    .ops
                    .stat(ctx, ns, pod, container, &join_abs_path(rest))
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
                    .read(ctx, ns, pod, container, &join_abs_path(rest))
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

    /// Advertise the action surface by depth: a pod offers `logs` and `port-forward`; a container
    /// offers `logs` and `exec`. Actions are discoverable now; live invocation (log/exec streams and
    /// port-forward sessions over the cluster API) is the integration step, so the inherited
    /// [`Vfs::invoke`] still returns [`VfsError::Unsupported`].
    fn actions_at(&self, path: &VfsPath) -> Vec<ActionDescriptor> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();
        let logs = || ActionDescriptor {
            id: ActionId::new("logs"),
            label: SmolStr::new("Stream logs"),
            kind: ActionKind::Stream,
            destructive: false,
        };
        match segs.as_slice() {
            [_ctx, _ns, _pod] => vec![
                logs(),
                ActionDescriptor {
                    id: ActionId::new("port-forward"),
                    label: SmolStr::new("Port-forward"),
                    kind: ActionKind::Session,
                    destructive: false,
                },
            ],
            [_ctx, _ns, _pod, _container, ..] => vec![
                logs(),
                ActionDescriptor {
                    id: ActionId::new("exec"),
                    label: SmolStr::new("Exec"),
                    kind: ActionKind::Interactive,
                    destructive: false,
                },
            ],
            _ => Vec::new(),
        }
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
    async fn empty_container_root_lists_empty_not_not_found() {
        // `sidecar` exists but has no files: its root must list empty, never NotFound.
        let vfs = backend();
        assert_eq!(
            names(&vfs, "/prod/default/web-0/sidecar").await,
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn container_root_is_list_only_and_not_readable() {
        // The container node (depth 4) is a directory: caps_at and open_read agree it is not
        // readable; reads begin strictly inside the container's filesystem (depth > 4).
        let vfs = backend();
        let path = p("/prod/default/web-0/app");
        assert_eq!(vfs.caps_at(&path), Caps::LIST);
        assert!(matches!(
            vfs.open_read(&path, None).await,
            Err(VfsError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn stat_and_list_in_container_edge_cases() {
        let vfs = backend();
        // A subdirectory deep in the container is a Dir.
        assert!(vfs
            .stat(&p("/prod/default/web-0/app/etc"))
            .await
            .unwrap()
            .is_dir());
        // A file's size is reported.
        assert_eq!(
            vfs.stat(&p("/prod/default/web-0/app/etc/hostname"))
                .await
                .unwrap()
                .size,
            Some(6)
        );
        // Listing a file path, or a missing directory, is NotFound.
        assert!(matches!(
            vfs.list(
                &p("/prod/default/web-0/app/etc/hostname"),
                ListOpts::default()
            )
            .next()
            .await
            .unwrap(),
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            vfs.list(&p("/prod/default/web-0/app/missing"), ListOpts::default())
                .next()
                .await
                .unwrap(),
            Err(VfsError::NotFound(_))
        ));
        // An unknown context lists as NotFound.
        assert!(matches!(
            vfs.list(&p("/nope"), ListOpts::default())
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
            vfs.open_write(&p("/prod/default/web-0/app/x"), WriteOpts::default())
                .await,
            Err(VfsError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn action_surface_varies_by_depth() {
        let vfs = backend();
        let ids = |path: &str| -> Vec<String> {
            vfs.actions_at(&p(path))
                .iter()
                .map(|a| a.id.0.to_string())
                .collect()
        };
        // Pod: logs + port-forward. Container: logs + exec.
        assert_eq!(ids("/prod/default/web-0"), vec!["logs", "port-forward"]);
        assert_eq!(ids("/prod/default/web-0/app"), vec!["logs", "exec"]);
        // The navigation tree above a pod has no actions.
        assert!(vfs.actions_at(&p("/prod/default")).is_empty());
        assert!(vfs.actions_at(&p("/")).is_empty());
        // Invocation is the integration step — still unsupported.
        assert!(matches!(
            vfs.invoke(ActionId::new("logs"), cairn_vfs::ActionCtx::None)
                .await,
            Err(VfsError::Unsupported(_))
        ));
    }
}
