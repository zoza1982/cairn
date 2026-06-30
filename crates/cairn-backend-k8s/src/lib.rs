//! Kubernetes backend.
//!
//! Presents a cluster as a navigable tree: `/<context>/<namespace>/<pod>/<container>/<path…>`.
//! The depth-based path-routing and entry-mapping logic lives in [`KubeVfs`] over a [`KubeOps`]
//! seam and is fully unit-tested against an in-memory mock. Capabilities vary by depth
//! ([`CapabilityProvider::caps_at`]): listing everywhere, file read only inside a container.
//!
//! **Implemented actions (M6-6):**
//!
//! - `logs` — `KubeOps::logs` over `Api::<Pod>::log_stream`; `KubeVfs::invoke("logs")` returns
//!   `ActionOutcome::Stream`. See `real.rs` for the spawn+mpsc pattern.
//! - `exec` — `KubeOps::exec` over `Api::<Pod>::exec` with `AttachParams`; `KubeVfs::invoke("exec")`
//!   with `ActionCtx::Exec { argv, tty }` returns `ActionOutcome::Session(SessionHandle)`. Relay
//!   tasks wire `AttachedProcess` stdin/stdout/stderr to the handle channels; a TTY resize relay
//!   forwards `(rows, cols)` to `AttachedProcess::terminal_size`. See RFC-0009 §3.
//!
//! **Implemented actions (M6-6, continued):**
//!
//! - `port-forward` — `KubeOps::port_forward` over `Api::<Pod>::portforward` + a local
//!   `TcpListener`; `KubeVfs::invoke("port-forward")` with `ActionCtx::PortForward { local, remote }`
//!   returns `ActionOutcome::Session(SessionHandle)`. See RFC-0009 §3.
//!
//! **Deferred (follow-up PR):** The TUI exec pane (RFC-0009 §4) is PR-4.

mod ops;
#[cfg(feature = "k8s")]
mod real;
#[cfg(feature = "k8s")]
mod tar_exec;

pub use ops::{ContainerInfo, ContextInfo, KubeOps, PodInfo, RemoteEntry, RemoteMeta};
#[cfg(feature = "k8s")]
pub use real::KubeRsOps;

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
    /// offers `logs` and `exec`. This reflects path *shape*, not existence (it does no I/O); existence
    /// is enforced by `stat`/`invoke`. Actions are discoverable now; live invocation (log/exec streams
    /// and port-forward sessions over the cluster API) is the integration step, so the overridden
    /// [`Vfs::invoke`] returns `VfsError::Backend { code: "not_implemented" }`.
    fn actions_at(&self, path: &VfsPath) -> Vec<ActionDescriptor> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();
        let logs = || ActionDescriptor {
            id: ActionId::new(action_ids::LOGS),
            label: SmolStr::new("Stream logs"),
            kind: ActionKind::Stream,
            destructive: false,
        };
        match segs.as_slice() {
            // depth == CONTAINER_LEVEL - 1: a pod.
            [_ctx, _ns, _pod] => vec![
                logs(),
                ActionDescriptor {
                    id: ActionId::new(action_ids::PORT_FORWARD),
                    label: SmolStr::new("Port-forward"),
                    kind: ActionKind::Session,
                    destructive: false,
                },
            ],
            // depth >= CONTAINER_LEVEL: a container, or a path inside its filesystem. When `invoke`
            // is wired live it must route to the pod+container (first four segments); the in-container
            // path tail, if any, is not used by logs/exec.
            [_ctx, _ns, _pod, _container, ..] => vec![
                logs(),
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

    /// Invoke a per-node Kubernetes action.
    ///
    /// **Implemented:**
    ///
    /// - `logs` (M6-6 first slice) — streams pod/container log output as
    ///   `ActionOutcome::Stream(BoxStream<'static, Result<Bytes, VfsError>>)`. For a pod path
    ///   (`/<ctx>/<ns>/<pod>`) the container is taken from `ActionCtx::Logs { container }` (or
    ///   the API server's default when `None`). For a container path (`/<ctx>/<ns>/<pod>/<container>`
    ///   or deeper) the container is taken directly from the path. `follow` defaults to `false`
    ///   (bounded history); `tail` defaults to `Some(100)` in non-follow mode so the stream
    ///   terminates predictably for history reads and integration tests.
    ///
    /// - `exec` (M6-6, RFC-0009 §3) — opens an interactive exec session in a running container,
    ///   returning `ActionOutcome::Session(SessionHandle)`. Requires `ActionCtx::Exec { argv, tty }`;
    ///   any other `ActionCtx` variant returns `VfsError::Backend { code: "invalid_ctx" }`. The path
    ///   must reach depth ≥ 4 (`/<ctx>/<ns>/<pod>/<container>`); shallower paths return
    ///   `VfsError::Backend { code: "not_available" }`. The pod must be in the `Running` phase —
    ///   a `Pending`/`Succeeded`/`Failed` pod returns `VfsError::NotFound` or `VfsError::Backend`
    ///   from the API server.
    ///
    /// - `port-forward` (M6-6, RFC-0009 §3) — binds a local `TcpListener` and forwards TCP
    ///   connections to a pod port, returning `ActionOutcome::Session(SessionHandle)`. Requires
    ///   `ActionCtx::PortForward { local, remote }` where `remote` must be non-zero. `local == 0`
    ///   requests an OS-assigned ephemeral port. `done` resolves `Ok(0)` on clean cancel, or
    ///   `Err(VfsError::Backend { code: "accept_failed" })` on a fatal listener error.
    ///
    /// **Deferred (follow-up PRs):**
    ///
    /// - TUI exec/port-forward pane integration (RFC-0009 §4).
    async fn invoke(
        &self,
        path: &VfsPath,
        action: ActionId,
        ctx: ActionCtx,
    ) -> Result<ActionOutcome, VfsError> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();

        match action.as_str() {
            action_ids::LOGS => {
                // Parse the kube context, namespace, pod, and (optionally) container from the
                // path segments. The container may also come from `ActionCtx::Logs`.
                let (kube_ctx, ns, pod, path_container) = match segs.as_slice() {
                    // Pod-level logs: container comes from ActionCtx or API-server default.
                    [kube_ctx, ns, pod] => (*kube_ctx, *ns, *pod, None::<&str>),
                    // Container or deeper: container is the fourth segment.
                    [kube_ctx, ns, pod, container, ..] => (*kube_ctx, *ns, *pod, Some(*container)),
                    _ => {
                        return Err(VfsError::Backend {
                            code: "not_implemented".to_owned(),
                            msg: format!(
                                "action '{}' is not available at this path",
                                action.as_str()
                            ),
                            retryable: false,
                        })
                    }
                };

                // `ActionCtx::Logs` carries `follow` and an optional per-call container
                // override. The path container takes priority; the ctx container is the
                // fallback for pod-level paths.
                let (follow, log_container) = match &ctx {
                    ActionCtx::Logs {
                        follow,
                        container: ctx_container,
                        ..
                    } => {
                        let effective = path_container.or(ctx_container.as_deref());
                        (*follow, effective)
                    }
                    _ => (false, path_container),
                };

                // Bound non-follow mode so history reads terminate without a timeout; follow
                // mode streams until the caller drops the stream.
                let tail = if follow { None } else { Some(100) };

                let stream = self
                    .ops
                    .logs(kube_ctx, ns, pod, log_container, follow, tail);
                Ok(ActionOutcome::Stream(stream))
            }

            action_ids::EXEC => {
                // Exec requires depth ≥ 4: [ctx, ns, pod, container, ...].
                let (kube_ctx, ns, pod, container) = match segs.as_slice() {
                    [kube_ctx, ns, pod, container, ..] => (*kube_ctx, *ns, *pod, *container),
                    _ => {
                        return Err(VfsError::Backend {
                            code: "not_available".to_owned(),
                            msg: "exec is only available at container paths \
                                  (/<ctx>/<ns>/<pod>/<container> or deeper)"
                                .to_owned(),
                            retryable: false,
                        });
                    }
                };

                // Extract argv and tty from the action context.
                let (argv, tty) = match ctx {
                    ActionCtx::Exec { argv, tty } => (argv, tty),
                    _ => {
                        return Err(VfsError::Backend {
                            code: "invalid_ctx".to_owned(),
                            msg: "exec requires ActionCtx::Exec { argv, tty }".to_owned(),
                            retryable: false,
                        });
                    }
                };

                let handle = self
                    .ops
                    .exec(kube_ctx, ns, pod, container, argv, tty)
                    .await?;
                Ok(ActionOutcome::Session(handle))
            }

            action_ids::PORT_FORWARD => {
                // Port-forward requires depth >= 3: [ctx, ns, pod, ...].
                // Deeper paths (container level) are also valid — the forward targets the pod.
                let (kube_ctx, ns, pod) = match segs.as_slice() {
                    [kube_ctx, ns, pod, ..] => (*kube_ctx, *ns, *pod),
                    _ => {
                        return Err(VfsError::Backend {
                            code: "not_available".to_owned(),
                            msg: "port-forward is only available at pod paths \
                                  (/<ctx>/<ns>/<pod> or deeper)"
                                .to_owned(),
                            retryable: false,
                        });
                    }
                };

                // Extract local and remote ports from the action context.
                let (local, remote) = match ctx {
                    ActionCtx::PortForward { local, remote } => (local, remote),
                    _ => {
                        return Err(VfsError::Backend {
                            code: "invalid_ctx".to_owned(),
                            msg: "port-forward requires \
                                  ActionCtx::PortForward { local, remote }"
                                .to_owned(),
                            retryable: false,
                        });
                    }
                };

                // FIX 6: validate the remote port eagerly — port 0 is not a valid Kubernetes
                // port number and would produce a confusing API error from the server.
                if remote == 0 {
                    return Err(VfsError::Backend {
                        code: "invalid_ctx".to_owned(),
                        msg: "port-forward remote port must be non-zero".to_owned(),
                        retryable: false,
                    });
                }

                // local == 0 → let the OS choose an ephemeral port (documented convention).
                let local_port = if local == 0 { None } else { Some(local) };

                let handle = self
                    .ops
                    .port_forward(kube_ctx, ns, pod, remote, local_port)
                    .await?;
                Ok(ActionOutcome::Session(handle))
            }

            other => Err(VfsError::Backend {
                code: "not_implemented".to_owned(),
                msg: format!("action '{other}' is not available"),
                retryable: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
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
                .map(|a| a.id.as_str().to_owned())
                .collect()
        };
        // Pod: logs + port-forward. Container (and deeper in its fs): logs + exec.
        assert_eq!(ids("/prod/default/web-0"), vec!["logs", "port-forward"]);
        assert_eq!(ids("/prod/default/web-0/app"), vec!["logs", "exec"]);
        assert_eq!(
            ids("/prod/default/web-0/app/etc/hostname"),
            vec!["logs", "exec"]
        );
        // The navigation tree above a pod has no actions.
        assert!(vfs.actions_at(&p("/prod/default")).is_empty());
        assert!(vfs.actions_at(&p("/")).is_empty());
    }

    #[tokio::test]
    async fn invoke_logs_pod_path_returns_stream_with_canned_output() {
        let vfs = backend();
        // Pod-level path: container unspecified → mock returns canned lines.
        let outcome = vfs
            .invoke(
                &p("/prod/default/web-0"),
                ActionId::new(action_ids::LOGS),
                ActionCtx::None,
            )
            .await
            .expect("invoke logs on a known pod must succeed");

        let stream = match outcome {
            ActionOutcome::Stream(s) => s,
            _ => panic!("expected ActionOutcome::Stream"),
        };

        let chunks: Vec<Vec<u8>> = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.expect("stream item must be Ok").to_vec())
            .collect();
        let text = String::from_utf8(chunks.concat()).expect("mock log output must be valid UTF-8");
        assert!(text.contains("[mock] k8s log line 1\n"), "got: {text:?}");
        assert!(text.contains("[mock] k8s log line 2\n"), "got: {text:?}");
    }

    #[tokio::test]
    async fn invoke_logs_container_path_returns_stream() {
        let vfs = backend();
        // Container-level path: container taken from the path segment.
        let outcome = vfs
            .invoke(
                &p("/prod/default/web-0/app"),
                ActionId::new(action_ids::LOGS),
                ActionCtx::None,
            )
            .await
            .expect("invoke logs on a container path must succeed");
        assert!(matches!(outcome, ActionOutcome::Stream(_)));
    }

    #[tokio::test]
    async fn invoke_logs_ctx_follow_is_forwarded() {
        let vfs = backend();
        // ActionCtx::Logs { follow: true } must still return a stream (the mock ignores follow).
        let outcome = vfs
            .invoke(
                &p("/prod/default/web-0"),
                ActionId::new(action_ids::LOGS),
                ActionCtx::Logs {
                    follow: true,
                    since: None,
                    container: None,
                },
            )
            .await
            .expect("invoke logs with follow=true must succeed");
        assert!(matches!(outcome, ActionOutcome::Stream(_)));
    }

    #[tokio::test]
    async fn invoke_logs_on_navigation_path_errors() {
        let vfs = backend();
        // A namespace path has no pod to target → not_implemented.
        assert!(matches!(
            vfs.invoke(
                &p("/prod/default"),
                ActionId::new(action_ids::LOGS),
                ActionCtx::None
            )
            .await,
            Err(VfsError::Backend { code, .. }) if code == "not_implemented"
        ));
    }

    // -----------------------------------------------------------------------
    // Port-forward session tests (mock, hermetic)
    // -----------------------------------------------------------------------

    /// `invoke("port-forward")` on a pod path with `ActionCtx::None` must return
    /// `invalid_ctx` — the action requires `ActionCtx::PortForward { local, remote }`.
    #[tokio::test]
    async fn invoke_port_forward_with_wrong_ctx_returns_invalid_ctx() {
        let vfs = backend();
        assert!(matches!(
            vfs.invoke(
                &p("/prod/default/web-0"),
                ActionId::new(action_ids::PORT_FORWARD),
                ActionCtx::None
            )
            .await,
            Err(VfsError::Backend { code, .. }) if code == "invalid_ctx"
        ));
    }

    /// `invoke("port-forward")` on a shallow path (namespace depth) must return `not_available`.
    #[tokio::test]
    async fn invoke_port_forward_on_shallow_path_returns_not_available() {
        let vfs = backend();
        assert!(matches!(
            vfs.invoke(
                &p("/prod/default"),
                ActionId::new(action_ids::PORT_FORWARD),
                ActionCtx::PortForward { local: 0, remote: 8080 }
            )
            .await,
            Err(VfsError::Backend { code, .. }) if code == "not_available"
        ));
    }

    /// `invoke("port-forward")` with `local: 0` binds an ephemeral port, returns a
    /// `Session` with `local_port` set, and the port is reachable via TCP.
    ///
    /// This test also verifies the echo semantics of `MockKube::port_forward`: bytes written
    /// to the local port are echoed back, and `cancel` resolves `done` with `Ok(0)`.
    #[tokio::test]
    async fn invoke_port_forward_ephemeral_port_and_echo_round_trip() {
        let vfs = backend();
        let outcome = vfs
            .invoke(
                &p("/prod/default/web-0"),
                ActionId::new(action_ids::PORT_FORWARD),
                ActionCtx::PortForward {
                    local: 0,
                    remote: 8080,
                },
            )
            .await
            .expect("invoke port-forward on a known pod must succeed");

        let handle = match outcome {
            ActionOutcome::Session(h) => h,
            _ => panic!("expected ActionOutcome::Session from invoke port-forward"),
        };

        // local_port must be set (the mock bound a real listener).
        let bound_port = handle
            .local_port
            .expect("port-forward must report the bound local_port");
        assert!(bound_port > 0, "bound_port must be a valid port number");

        // Port-forward sessions have no stdin/stdout/resize (relay is at TCP level).
        assert!(handle.stdin.is_none(), "port-forward must have no stdin");
        assert!(handle.stdout.is_none(), "port-forward must have no stdout");
        assert!(handle.resize.is_none(), "port-forward must have no resize");

        // Connect a TcpStream to the bound port and verify the echo.
        let mut stream = tokio::net::TcpStream::connect(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            bound_port,
        )))
        .await
        .expect("TcpStream::connect to mock port-forward must succeed");

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        stream
            .write_all(b"hello port-forward\n")
            .await
            .expect("write to mock port-forward must succeed");

        let mut echo = [0u8; 19];
        stream
            .read_exact(&mut echo)
            .await
            .expect("read from mock port-forward (echo) must succeed");
        assert_eq!(&echo, b"hello port-forward\n");

        // Cancel the session: accept loop exits, done resolves with Ok(0).
        handle
            .cancel
            .send(())
            .expect("cancel send must succeed while session is running");

        let exit = handle.done.await.expect("done channel must not be dropped");
        assert!(
            matches!(exit, Ok(0)),
            "port-forward clean teardown must resolve done with Ok(0), got: {exit:?}"
        );
    }

    /// `invoke("port-forward")` on an unknown pod returns `VfsError::NotFound`.
    #[tokio::test]
    async fn invoke_port_forward_unknown_pod_returns_not_found() {
        let vfs = backend();
        assert!(matches!(
            vfs.invoke(
                &p("/prod/default/no-such-pod"),
                ActionId::new(action_ids::PORT_FORWARD),
                ActionCtx::PortForward {
                    local: 0,
                    remote: 8080
                }
            )
            .await,
            Err(VfsError::NotFound(_))
        ));
    }

    /// `invoke("port-forward")` with `remote: 0` must return `invalid_ctx`.
    ///
    /// Port 0 is not a valid Kubernetes port number; it would produce a confusing API-server
    /// error if forwarded. We catch it early in `KubeVfs::invoke` (FIX 6).
    #[tokio::test]
    async fn invoke_port_forward_with_zero_remote_port_returns_invalid_ctx() {
        let vfs = backend();
        assert!(matches!(
            vfs.invoke(
                &p("/prod/default/web-0"),
                ActionId::new(action_ids::PORT_FORWARD),
                ActionCtx::PortForward { local: 0, remote: 0 }
            )
            .await,
            Err(VfsError::Backend { code, .. }) if code == "invalid_ctx"
        ));
    }

    /// After `cancel` fires, in-flight relay tasks must be torn down: a still-open TCP
    /// connection is closed from the relay side so reading from it returns EOF.
    ///
    /// This verifies FIX 1 (CancellationToken + JoinSet in MockKube::port_forward): without the
    /// token, the echo task would keep running indefinitely even after `done` resolves, leaving a
    /// lingering half-open connection.
    #[tokio::test]
    async fn invoke_port_forward_cancel_tears_down_inflight_connection() {
        let vfs = backend();
        let outcome = vfs
            .invoke(
                &p("/prod/default/web-0"),
                ActionId::new(action_ids::PORT_FORWARD),
                ActionCtx::PortForward {
                    local: 0,
                    remote: 8080,
                },
            )
            .await
            .expect("port-forward on a known pod must succeed");

        let handle = match outcome {
            ActionOutcome::Session(h) => h,
            _ => panic!("expected ActionOutcome::Session from invoke port-forward"),
        };

        let bound_port = handle
            .local_port
            .expect("port-forward must report the bound local_port");

        // Open a connection and hold it open without writing or reading.
        let stream = tokio::net::TcpStream::connect(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            bound_port,
        )))
        .await
        .expect("TCP connect to mock port-forward must succeed");

        // Cancel the session.
        handle
            .cancel
            .send(())
            .expect("cancel send must succeed while session is running");

        // `done` must resolve with Ok(0). This also proves that join_set.shutdown() completed,
        // meaning every relay task (including the echo task for our open connection) has exited.
        let exit = handle.done.await.expect("done channel must not be dropped");
        assert!(
            matches!(exit, Ok(0)),
            "port-forward cancel must resolve done with Ok(0), got: {exit:?}"
        );

        // The relay task dropped its half of the TcpStream, so reading from our end must see
        // EOF (0 bytes). This fails if the relay is still alive (it would keep reading/echoing).
        use tokio::io::AsyncReadExt as _;
        let (mut reader, _writer) = tokio::io::split(stream);
        let mut buf = [0u8; 16];
        // A connection reset counts as teardown too; unwrap_or(0) accepts both cases.
        let n = reader.read(&mut buf).await.unwrap_or(0);
        assert_eq!(
            n, 0,
            "relay teardown must have closed the connection (expected EOF)"
        );
    }

    // -----------------------------------------------------------------------
    // Interactive exec session tests (mock, hermetic)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn invoke_exec_with_wrong_ctx_returns_invalid_ctx() {
        let vfs = backend();
        // exec with ActionCtx::None (not ActionCtx::Exec) must return invalid_ctx.
        assert!(matches!(
            vfs.invoke(
                &p("/prod/default/web-0/app"),
                ActionId::new(action_ids::EXEC),
                ActionCtx::None
            )
            .await,
            Err(VfsError::Backend { code, .. }) if code == "invalid_ctx"
        ));
    }

    #[tokio::test]
    async fn invoke_exec_on_shallow_path_returns_not_available() {
        let vfs = backend();
        // exec on a pod path (depth 3) is not available — only container paths (depth ≥ 4).
        assert!(matches!(
            vfs.invoke(
                &p("/prod/default/web-0"),
                ActionId::new(action_ids::EXEC),
                ActionCtx::Exec {
                    argv: vec!["sh".to_owned()],
                    tty: false,
                }
            )
            .await,
            Err(VfsError::Backend { code, .. }) if code == "not_available"
        ));
    }

    #[tokio::test]
    async fn invoke_exec_non_tty_returns_session_with_echo() {
        let vfs = backend();
        let outcome = vfs
            .invoke(
                &p("/prod/default/web-0/app"),
                ActionId::new(action_ids::EXEC),
                ActionCtx::Exec {
                    argv: vec!["sh".to_owned(), "-c".to_owned(), "cat".to_owned()],
                    tty: false,
                },
            )
            .await
            .expect("invoke exec on a known container must succeed");

        let handle = match outcome {
            ActionOutcome::Session(h) => h,
            _ => panic!("expected ActionOutcome::Session from invoke exec"),
        };

        // Non-TTY: stdin/stdout present; resize absent; local_port absent.
        let stdin = handle.stdin.expect("non-tty exec must have a stdin sender");
        let mut stdout = handle
            .stdout
            .expect("non-tty exec must have a stdout receiver");
        assert!(
            handle.resize.is_none(),
            "non-tty exec must NOT have a resize channel"
        );
        assert!(
            handle.local_port.is_none(),
            "exec must never set local_port"
        );

        // Write a chunk to stdin; the mock echoes it back to stdout.
        stdin
            .send(Bytes::from_static(b"hello world\n"))
            .await
            .expect("stdin send must succeed");

        let echo = stdout
            .recv()
            .await
            .expect("stdout must yield the echoed chunk");
        assert_eq!(echo, Bytes::from_static(b"hello world\n"));

        // Drop stdin → signals EOF; the mock relay task exits and resolves `done` with Ok(0).
        drop(stdin);
        let exit = handle.done.await.expect("done channel must not be dropped");
        // VfsError doesn't implement PartialEq, so unwrap Ok and check the code directly.
        assert!(
            matches!(exit, Ok(0)),
            "mock exec must exit with code 0, got: {exit:?}"
        );
    }

    #[tokio::test]
    async fn invoke_exec_tty_has_resize_channel() {
        let vfs = backend();
        let outcome = vfs
            .invoke(
                &p("/prod/default/web-0/app"),
                ActionId::new(action_ids::EXEC),
                ActionCtx::Exec {
                    argv: vec!["sh".to_owned()],
                    tty: true,
                },
            )
            .await
            .expect("invoke exec (tty=true) on a known container must succeed");

        let handle = match outcome {
            ActionOutcome::Session(h) => h,
            _ => panic!("expected ActionOutcome::Session from invoke exec tty"),
        };

        // TTY: resize channel must be present.
        let resize = handle.resize.expect("tty exec must have a resize channel");

        // Send a resize event — the mock accepts and discards it.
        resize
            .send((24, 80))
            .await
            .expect("resize send must succeed while session is alive");

        // Cancel via the cancel sender; done must resolve (with any Ok value).
        handle
            .cancel
            .send(())
            .expect("cancel send must succeed while session is running");

        let exit = handle.done.await.expect("done channel must not be dropped");
        assert!(
            exit.is_ok(),
            "cancelled exec must resolve done with Ok(_), got: {exit:?}"
        );
    }

    #[tokio::test]
    async fn invoke_exec_cancel_by_dropping_sender_resolves_done() {
        let vfs = backend();
        let outcome = vfs
            .invoke(
                &p("/prod/default/web-0/app"),
                ActionId::new(action_ids::EXEC),
                ActionCtx::Exec {
                    argv: vec!["sh".to_owned()],
                    tty: false,
                },
            )
            .await
            .unwrap();

        let handle = match outcome {
            ActionOutcome::Session(h) => h,
            _ => panic!("expected ActionOutcome::Session"),
        };

        // Drop the cancel sender (equivalent to sending ()) — done must still resolve.
        drop(handle.cancel);
        let exit = handle
            .done
            .await
            .expect("done channel must resolve after cancel drop");
        assert!(
            exit.is_ok(),
            "cancel-by-drop must yield Ok(_), got: {exit:?}"
        );
    }
}
