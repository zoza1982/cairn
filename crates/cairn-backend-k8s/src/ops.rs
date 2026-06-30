//! The [`KubeOps`] transport seam for the Kubernetes backend, plus an in-memory mock.
//!
//! `KubeOps` is the entire cluster surface the [`crate::KubeVfs`] mapping depends on, so the
//! bug-prone depth-based path routing (context → namespace → pod → container → filesystem) is
//! unit-tested against an in-memory [`mock::MockKube`] with no cluster. The live `kube-rs` adapter
//! (and the TLS dependency stack it pulls in) is the integration step; see RFC-0005.

use async_trait::async_trait;
use bytes::Bytes;
use cairn_types::{EntryKind, PodPhase};
use cairn_vfs::VfsError;
use futures::stream::BoxStream;

/// A kubeconfig context (no cluster call needed to enumerate these).
#[derive(Debug, Clone)]
pub struct ContextInfo {
    /// Context name.
    pub name: String,
    /// Display-only cluster endpoint; never used for auth decisions.
    pub server: String,
}

/// Summary of a pod.
#[derive(Debug, Clone)]
pub struct PodInfo {
    /// Pod name.
    pub name: String,
    /// Lifecycle phase.
    pub phase: PodPhase,
    /// Ready containers out of total (`ready`, `total`).
    pub ready: (u16, u16),
    /// Node the pod is scheduled on, if any.
    pub node: Option<String>,
}

/// Summary of a container within a pod.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    /// Container name.
    pub name: String,
    /// Whether this is an init container.
    pub is_init: bool,
    /// Whether this is an ephemeral (debug) container.
    pub is_ephemeral: bool,
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

/// The Kubernetes cluster surface the backend needs. Implemented by the `kube-rs` adapter (live,
/// integration-deferred) and by the in-memory mock used for the routing tests.
///
/// In-container filesystem access ([`list_dir`](KubeOps::list_dir)/[`stat`](KubeOps::stat)/
/// [`read`](KubeOps::read)) uses `kubectl cp` semantics (tar over `exec`) in the real adapter;
/// container writes, log streaming, interactive `exec`, and port-forward are the action surface
/// added in a later milestone (M6-6) and are intentionally not part of this read-only seam yet.
#[async_trait]
pub trait KubeOps: Send + Sync + 'static {
    /// List kubeconfig contexts.
    async fn list_contexts(&self) -> Result<Vec<ContextInfo>, VfsError>;
    /// List namespace names in a context. May return [`VfsError::Forbidden`] under RBAC.
    async fn list_namespaces(&self, ctx: &str) -> Result<Vec<String>, VfsError>;
    /// List pods in a namespace.
    async fn list_pods(&self, ctx: &str, ns: &str) -> Result<Vec<PodInfo>, VfsError>;
    /// List containers (regular + init + ephemeral) in a pod.
    async fn list_containers(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
    ) -> Result<Vec<ContainerInfo>, VfsError>;
    /// List a directory inside a container's filesystem.
    ///
    /// An existing container's root (`"/"`) must list as `Ok(vec![])` when empty — never
    /// [`VfsError::NotFound`]; `NotFound` is reserved for a missing directory or a path that is a
    /// file. (The live adapter must preserve this so an empty/just-started container still browses.)
    async fn list_dir(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: &str,
        path: &str,
    ) -> Result<Vec<RemoteEntry>, VfsError>;
    /// Stat a path inside a container.
    async fn stat(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: &str,
        path: &str,
    ) -> Result<RemoteMeta, VfsError>;
    /// Read a file inside a container.
    async fn read(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: &str,
        path: &str,
    ) -> Result<Vec<u8>, VfsError>;

    /// Stream log output from a pod's container.
    ///
    /// Returns a `'static` stream of raw log chunks as [`Bytes`]. Each chunk carries one or
    /// more UTF-8 log lines as delivered by the API server; line boundaries are not guaranteed
    /// to align with chunk boundaries.
    ///
    /// - `container`: target container within the pod. `None` lets the API server select the
    ///   sole container (single-container pods) or return an error (multi-container pods).
    /// - `follow`: `true` for live tail; `false` for a bounded history snapshot.
    /// - `tail`: limit the history to the last `n` lines. `None` means all available history.
    ///
    /// Error mapping: 404 → [`VfsError::NotFound`]; 401 → [`VfsError::Auth`];
    /// 403 → [`VfsError::Forbidden`]; other API errors → [`VfsError::Backend`].
    /// Errors appear as `Err` items in the stream, not panics.
    fn logs(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: Option<&str>,
        follow: bool,
        tail: Option<i64>,
    ) -> BoxStream<'static, Result<Bytes, VfsError>>;
}

#[cfg(test)]
pub(crate) mod mock {
    use super::{ContainerInfo, ContextInfo, KubeOps, PodInfo, RemoteEntry, RemoteMeta};
    use async_trait::async_trait;
    use bytes::Bytes;
    use cairn_types::{EntryKind, PodPhase, VfsPath};
    use cairn_vfs::VfsError;
    use futures::stream::{self, BoxStream};
    use futures::StreamExt as _;
    use std::collections::BTreeMap;

    /// An in-container path → bytes tree (directories are implied by path prefixes).
    type FileTree = BTreeMap<String, Vec<u8>>;
    /// (ctx, ns, pod, container) → that container's file tree.
    type ContainerFiles = BTreeMap<(String, String, String, String), FileTree>;

    /// In-memory cluster for tests: one context, one namespace, one pod with two containers, and an
    /// in-container file tree for the `app` container.
    pub(crate) struct MockKube {
        contexts: Vec<ContextInfo>,
        /// ctx -> namespaces.
        namespaces: BTreeMap<String, Vec<String>>,
        /// (ctx, ns) -> pods.
        pods: BTreeMap<(String, String), Vec<PodInfo>>,
        /// (ctx, ns, pod) -> containers.
        containers: BTreeMap<(String, String, String), Vec<ContainerInfo>>,
        /// (ctx, ns, pod, container) -> the container's file tree.
        files: ContainerFiles,
    }

    impl MockKube {
        pub(crate) fn new() -> Self {
            let key = (
                "prod".to_owned(),
                "default".to_owned(),
                "web-0".to_owned(),
                "app".to_owned(),
            );
            let mut tree: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            tree.insert("/etc/hostname".to_owned(), b"web-0\n".to_vec());
            tree.insert("/etc/hosts".to_owned(), b"127.0.0.1 localhost\n".to_vec());
            tree.insert("/app/main.rs".to_owned(), b"fn main() {}\n".to_vec());
            let mut files = BTreeMap::new();
            files.insert(key, tree);
            // `sidecar` exists but has an empty filesystem: its root must list empty, not NotFound.
            files.insert(
                (
                    "prod".to_owned(),
                    "default".to_owned(),
                    "web-0".to_owned(),
                    "sidecar".to_owned(),
                ),
                BTreeMap::new(),
            );

            let mut namespaces = BTreeMap::new();
            namespaces.insert("prod".to_owned(), vec!["default".to_owned()]);

            let mut pods = BTreeMap::new();
            pods.insert(
                ("prod".to_owned(), "default".to_owned()),
                vec![PodInfo {
                    name: "web-0".to_owned(),
                    phase: PodPhase::Running,
                    ready: (2, 2),
                    node: Some("node-1".to_owned()),
                }],
            );

            let mut containers = BTreeMap::new();
            containers.insert(
                ("prod".to_owned(), "default".to_owned(), "web-0".to_owned()),
                vec![
                    ContainerInfo {
                        name: "app".to_owned(),
                        is_init: false,
                        is_ephemeral: false,
                    },
                    ContainerInfo {
                        name: "sidecar".to_owned(),
                        is_init: false,
                        is_ephemeral: false,
                    },
                ],
            );

            Self {
                contexts: vec![ContextInfo {
                    name: "prod".to_owned(),
                    server: "https://prod.example:6443".to_owned(),
                }],
                namespaces,
                pods,
                containers,
                files,
            }
        }

        fn nf(name: &str) -> VfsError {
            // `name` may be a bare segment (e.g. a container name); root it so the error path is
            // meaningful in test output rather than collapsing to `/`.
            let rooted = if name.starts_with('/') {
                name.to_owned()
            } else {
                format!("/{name}")
            };
            VfsError::NotFound(VfsPath::parse(&rooted).unwrap_or_else(|_| VfsPath::root()))
        }
    }

    #[async_trait]
    impl KubeOps for MockKube {
        async fn list_contexts(&self) -> Result<Vec<ContextInfo>, VfsError> {
            Ok(self.contexts.clone())
        }

        async fn list_namespaces(&self, ctx: &str) -> Result<Vec<String>, VfsError> {
            self.namespaces
                .get(ctx)
                .cloned()
                .ok_or_else(|| Self::nf(ctx))
        }

        async fn list_pods(&self, ctx: &str, ns: &str) -> Result<Vec<PodInfo>, VfsError> {
            self.pods
                .get(&(ctx.to_owned(), ns.to_owned()))
                .cloned()
                .ok_or_else(|| Self::nf(ns))
        }

        async fn list_containers(
            &self,
            ctx: &str,
            ns: &str,
            pod: &str,
        ) -> Result<Vec<ContainerInfo>, VfsError> {
            self.containers
                .get(&(ctx.to_owned(), ns.to_owned(), pod.to_owned()))
                .cloned()
                .ok_or_else(|| Self::nf(pod))
        }

        async fn list_dir(
            &self,
            ctx: &str,
            ns: &str,
            pod: &str,
            container: &str,
            path: &str,
        ) -> Result<Vec<RemoteEntry>, VfsError> {
            let tree = self
                .files
                .get(&(
                    ctx.to_owned(),
                    ns.to_owned(),
                    pod.to_owned(),
                    container.to_owned(),
                ))
                .ok_or_else(|| Self::nf(container))?;
            if tree.contains_key(path) {
                return Err(Self::nf(path)); // a file is not a directory
            }
            let prefix = if path == "/" {
                "/".to_owned()
            } else {
                format!("{path}/")
            };
            if path != "/" && !tree.keys().any(|k| k.starts_with(&prefix)) {
                return Err(Self::nf(path));
            }
            let mut dirs = std::collections::BTreeSet::new();
            let mut out_files = Vec::new();
            for (key, data) in tree {
                let Some(rest) = key.strip_prefix(&prefix) else {
                    continue;
                };
                match rest.split_once('/') {
                    Some((dir, _)) => {
                        dirs.insert(dir.to_owned());
                    }
                    None => out_files.push(RemoteEntry {
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
            out.extend(out_files);
            Ok(out)
        }

        async fn stat(
            &self,
            ctx: &str,
            ns: &str,
            pod: &str,
            container: &str,
            path: &str,
        ) -> Result<RemoteMeta, VfsError> {
            let tree = self
                .files
                .get(&(
                    ctx.to_owned(),
                    ns.to_owned(),
                    pod.to_owned(),
                    container.to_owned(),
                ))
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

        async fn read(
            &self,
            ctx: &str,
            ns: &str,
            pod: &str,
            container: &str,
            path: &str,
        ) -> Result<Vec<u8>, VfsError> {
            let tree = self
                .files
                .get(&(
                    ctx.to_owned(),
                    ns.to_owned(),
                    pod.to_owned(),
                    container.to_owned(),
                ))
                .ok_or_else(|| Self::nf(container))?;
            tree.get(path).cloned().ok_or_else(|| Self::nf(path))
        }

        fn logs(
            &self,
            _ctx: &str,
            _ns: &str,
            pod: &str,
            _container: Option<&str>,
            _follow: bool,
            _tail: Option<i64>,
        ) -> BoxStream<'static, Result<Bytes, VfsError>> {
            // Return a canned two-line log for the known pod; surface NotFound for unknown ones.
            if self.pods.values().flatten().any(|p| p.name == pod) {
                stream::iter(vec![
                    Ok(Bytes::from_static(b"[mock] k8s log line 1\n")),
                    Ok(Bytes::from_static(b"[mock] k8s log line 2\n")),
                ])
                .boxed()
            } else {
                let err = VfsError::NotFound(
                    VfsPath::parse(&format!("/{pod}")).unwrap_or_else(|_| VfsPath::root()),
                );
                stream::iter(vec![Err(err)]).boxed()
            }
        }
    }
}
