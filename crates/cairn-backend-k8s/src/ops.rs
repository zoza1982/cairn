//! The [`KubeOps`] transport seam for the Kubernetes backend, plus an in-memory mock.
//!
//! `KubeOps` is the entire cluster surface the [`crate::KubeVfs`] mapping depends on, so the
//! bug-prone depth-based path routing (context → namespace → pod → container → filesystem) is
//! unit-tested against an in-memory [`mock::MockKube`] with no cluster. The live `kube-rs` adapter
//! (and the TLS dependency stack it pulls in) is the integration step; see RFC-0005.

use async_trait::async_trait;
use bytes::Bytes;
use cairn_types::{EntryKind, PodPhase};
use cairn_vfs::{SessionHandle, VfsError};
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
/// integration-tested via `kind`) and by the in-memory mock used for the routing tests.
///
/// **Implemented (M6-6):** In-container filesystem access
/// ([`list_dir`](KubeOps::list_dir)/[`stat`](KubeOps::stat)/[`read`](KubeOps::read)) via
/// `kubectl cp` semantics (tar over `exec`); log streaming ([`logs`](KubeOps::logs)); interactive
/// exec ([`exec`](KubeOps::exec)); and port-forwarding ([`port_forward`](KubeOps::port_forward)).
///
/// **Deferred:** In-container writes (uploading files via `kubectl cp` semantics) — planned for a
/// follow-up PR after the M6-6 action surface is validated in integration testing.
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

    /// Open an interactive exec session in a running pod container.
    ///
    /// Returns a [`SessionHandle`] immediately; the remote process starts concurrently in a
    /// spawned task. The caller drives the session via the handle's channels:
    ///
    /// - Write to `stdin` to send bytes to the process's stdin.
    /// - Read from `stdout` to receive combined stdout (and, when `tty: false`, stderr) output.
    /// - Send `(rows, cols)` to `resize` (present only when `tty: true`) to propagate terminal
    ///   resize events.
    /// - Drop or send on `cancel` to request teardown; await `done` for the exit code.
    ///
    /// # Parameters
    ///
    /// - `ctx`: kubeconfig context name.
    /// - `ns`: namespace.
    /// - `pod`: pod name. The pod must be in the `Running` phase; a `Pending`/`Succeeded`/`Failed`
    ///   pod returns [`VfsError::NotFound`] or [`VfsError::Backend`].
    /// - `container`: container name within the pod.
    /// - `command`: argv passed directly to the container runtime (not a shell; use
    ///   `["sh", "-c", "…"]` for shell commands).
    /// - `tty`: allocate a pseudo-TTY. When `true`: stderr is merged into stdout (Docker/K8s
    ///   convention), the `resize` channel is populated, and the process sees a PTY. When `false`:
    ///   stderr is forwarded separately and interleaved into `stdout`; `resize` is `None`.
    ///
    /// # Error mapping
    ///
    /// 404 → [`VfsError::NotFound`]; 401 → [`VfsError::Auth`]; 403 → [`VfsError::Forbidden`];
    /// other API errors → [`VfsError::Backend`]. Credential material is never included in error
    /// messages.
    async fn exec(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: &str,
        command: Vec<String>,
        tty: bool,
    ) -> Result<SessionHandle, VfsError>;

    /// Forward a local TCP port to a remote port on a pod.
    ///
    /// Binds a `TcpListener` on `127.0.0.1:<local_port>` (or an ephemeral OS-assigned port when
    /// `local_port` is `None` or `Some(0)`). Returns a [`SessionHandle`] with `local_port` set to
    /// the actual bound port so the TUI can display the address before any connection arrives.
    ///
    /// Each accepted TCP connection opens a fresh `Portforwarder` WebSocket to the API server and
    /// relays bytes bidirectionally between the local stream and the pod's port stream (one
    /// `Portforwarder` per connection, as per the kube-rs documented pattern). Multiple simultaneous
    /// connections are fully multiplexed by the Kubernetes API server.
    ///
    /// # Cancellation
    ///
    /// Drop or send `()` on `cancel` to stop the accept loop and terminate all in-flight relay
    /// tasks. Each relay task is cancelled via a shared `CancellationToken`; both the local
    /// connection and the upstream `Portforwarder` stream are dropped, triggering TCP close on both
    /// sides. `done` resolves with `Ok(0)` after all relay tasks have exited.
    ///
    /// # Parameters
    ///
    /// - `ctx`: kubeconfig context name.
    /// - `ns`: namespace.
    /// - `pod`: pod name.
    /// - `remote_port`: port to forward on the pod side.
    /// - `local_port`: local TCP port to bind. `None` or `Some(0)` → OS assigns an ephemeral port.
    ///
    /// # Error mapping
    ///
    /// - `EADDRINUSE` on an explicit port → [`VfsError::Backend`] with `code = "port_in_use"`.
    /// - 404 (pod not found when the first connection opens) → relay task drops the connection
    ///   silently; the listener continues accepting (the pod may appear later).
    /// - Transport/API errors → [`VfsError::Backend`] or [`VfsError::Connection`] on a per-relay
    ///   basis; the accept loop is not terminated.
    /// - Bind failure (other than `EADDRINUSE`) → [`VfsError::Backend`] with `code = "bind_failed"`.
    ///
    /// Credential material is never included in error messages.
    async fn port_forward(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        remote_port: u16,
        local_port: Option<u16>,
    ) -> Result<SessionHandle, VfsError>;
}

/// Bind a local `TcpListener` on `127.0.0.1:<port>` and return the listener together with the
/// actually-bound port (useful when `port == 0` asks the OS for an ephemeral port).
///
/// Maps bind errors to typed [`VfsError`]s:
/// - `EADDRINUSE` → `VfsError::Backend { code: "port_in_use" }`
/// - Any other failure → `VfsError::Backend { code: "bind_failed" }`
///
/// The port number is deliberately omitted from error messages — it is already surfaced via
/// `SessionHandle.local_port` so the UI can display it without doubling the information.
#[cfg(any(test, feature = "k8s"))]
pub(crate) async fn bind_loopback(port: u16) -> Result<(tokio::net::TcpListener, u16), VfsError> {
    let listener =
        tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .map_err(|e| VfsError::Backend {
                code: if e.kind() == std::io::ErrorKind::AddrInUse {
                    "port_in_use".to_owned()
                } else {
                    "bind_failed".to_owned()
                },
                // Omit the address: the port is surfaced via SessionHandle.local_port;
                // including it here would duplicate it and potentially leak intent.
                msg: "port-forward: failed to bind local port".to_owned(),
                retryable: false,
            })?;
    let bound_port = listener
        .local_addr()
        .map_err(|e| VfsError::Backend {
            code: "bind_failed".to_owned(),
            msg: e.to_string(),
            retryable: false,
        })?
        .port();
    Ok((listener, bound_port))
}

#[cfg(test)]
pub(crate) mod mock {
    use super::{ContainerInfo, ContextInfo, KubeOps, PodInfo, RemoteEntry, RemoteMeta};
    use async_trait::async_trait;
    use bytes::Bytes;
    use cairn_types::{EntryKind, PodPhase, VfsPath};
    use cairn_vfs::{SessionHandle, VfsError};
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

        /// Echo-style port-forward mock: binds a local `TcpListener` on `127.0.0.1:port` (or
        /// an ephemeral port when `local_port` is `None`/`Some(0)`) and, for each accepted TCP
        /// connection, echoes bytes back to the sender. Returns a [`SessionHandle`] with
        /// `local_port` set to the actually-bound port.
        ///
        /// Honors the `cancel` signal: the accept loop exits, all in-flight echo tasks are
        /// cancelled via a shared `CancellationToken`, and `done` resolves with `Ok(0)` after
        /// all relay tasks have exited.
        ///
        /// Unknown pods return [`VfsError::NotFound`].
        async fn port_forward(
            &self,
            _ctx: &str,
            _ns: &str,
            pod: &str,
            _remote_port: u16,
            local_port: Option<u16>,
        ) -> Result<SessionHandle, VfsError> {
            if !self.pods.values().flatten().any(|p| p.name == pod) {
                return Err(Self::nf(pod));
            }

            // FIX 5: shared bind helper removes duplicate bind/error-mapping code.
            let (listener, bound_port) = crate::ops::bind_loopback(local_port.unwrap_or(0)).await?;

            let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
            let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Result<i32, VfsError>>();

            // FIX 1: shared token — cancel() on teardown fires cancelled() in every relay task.
            let token = tokio_util::sync::CancellationToken::new();

            tokio::spawn(async move {
                let mut join_set = tokio::task::JoinSet::<()>::new();

                loop {
                    tokio::select! {
                        biased;
                        _ = &mut cancel_rx => break,
                        accept = listener.accept() => {
                            match accept {
                                Ok((stream, _peer)) => {
                                    // Echo server: copy bytes from the read half back to the
                                    // write half. `tokio::io::split` takes ownership of `stream`
                                    // so both halves can be used concurrently without conflict.
                                    // FIX 1: each relay selects against the cancellation token.
                                    let relay_token = token.clone();
                                    join_set.spawn(async move {
                                        let (mut reader, mut writer) =
                                            tokio::io::split(stream);
                                        tokio::select! {
                                            // Cancel path: drop both halves → closes TCP conn.
                                            _ = relay_token.cancelled() => {}
                                            _ = tokio::io::copy(&mut reader, &mut writer) => {}
                                        }
                                        // reader + writer dropped here — TcpStream is closed.
                                    });
                                }
                                Err(_) => break,
                            }
                        }
                    }
                }

                // FIX 1: cancel all in-flight echo tasks and wait for them to exit before
                // resolving `done`, so callers can trust that the port is fully released.
                token.cancel();
                join_set.shutdown().await;

                // If the done receiver was dropped (session pane torn down), discard the error.
                let _ = done_tx.send(Ok(0));
            });

            Ok(SessionHandle::new(
                cancel_tx,
                done_rx,
                Some(bound_port),
                None, // port-forward has no stdin/stdout — the relay is at the TCP level
                None,
                None, // no TTY resize for port-forward
            ))
        }

        /// Echo-style exec mock: relays each stdin chunk back to stdout, then exits with code 0.
        ///
        /// The cancel signal (drop or explicit send on [`SessionHandle::cancel`]) is honoured
        /// cooperatively: the relay loop selects between `cancel` and `stdin.recv()`. Resize events
        /// are accepted and discarded (the mock has no TTY state). Unknown pods return
        /// [`VfsError::NotFound`].
        async fn exec(
            &self,
            _ctx: &str,
            _ns: &str,
            pod: &str,
            _container: &str,
            _command: Vec<String>,
            tty: bool,
        ) -> Result<SessionHandle, VfsError> {
            if !self.pods.values().flatten().any(|p| p.name == pod) {
                return Err(Self::nf(pod));
            }

            let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Bytes>(16);
            let (stdout_tx, stdout_rx) = tokio::sync::mpsc::channel::<Bytes>(16);
            let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
            let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Result<i32, VfsError>>();

            // TTY-only resize channel — accepted and drained; the mock ignores resize geometry.
            let (resize_tx, resize_rx) = if tty {
                let (t, r) = tokio::sync::mpsc::channel::<(u16, u16)>(4);
                (Some(t), Some(r))
            } else {
                (None, None)
            };

            tokio::spawn(async move {
                // Drain the resize channel in a background task so backpressure never stalls the
                // main echo loop (the TTY resize sender would block otherwise).
                if let Some(mut rr) = resize_rx {
                    tokio::spawn(async move { while rr.recv().await.is_some() {} });
                }

                // Echo loop: relay stdin → stdout until stdin closes or cancel fires.
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut cancel_rx => break,
                        chunk = stdin_rx.recv() => {
                            match chunk {
                                Some(c) => {
                                    // Best-effort echo; stop if the stdout receiver was dropped.
                                    if stdout_tx.send(c).await.is_err() {
                                        break;
                                    }
                                }
                                None => break, // stdin sender dropped → EOF
                            }
                        }
                    }
                }

                // Exit code 0 regardless of how the loop ended.
                // If done_rx has already been dropped (session pane torn down), discard the error.
                let _ = done_tx.send(Ok(0));
            });

            Ok(SessionHandle::new(
                cancel_tx,
                done_rx,
                None, // local_port: exec sessions never bind a port
                Some(stdin_tx),
                Some(stdout_rx),
                resize_tx,
            ))
        }
    }
}
