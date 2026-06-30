//! Live Kubernetes adapter over `kube-rs 4.0` + `k8s-openapi 0.28`.
//!
//! [`KubeRsOps`] implements [`KubeOps`] against a real cluster. It reads the kubeconfig from the
//! standard location (`$KUBECONFIG` or `~/.kube/config`) and builds a per-call `kube::Client`
//! for the requested context. A per-call build is adequate for M6; a caching layer belongs in a
//! later milestone when connection-pool metrics are needed.
//!
//! # In-container filesystem access
//!
//! [`KubeOps::list_dir`], [`KubeOps::stat`], and [`KubeOps::read`] are implemented via
//! **tar-over-exec** (`kubectl cp` semantics, M6-5b):
//!
//! - `list_dir(path)` → `tar cf - -C <path> .` (tar the directory, parse immediate children)
//! - `stat(path)` → `tar cf - -C <parent> <basename>` (examine the first tar header)
//! - `read(path)` → same command as stat, extract file bytes
//!
//! The container must have `tar` on its `PATH`.  When `tar` is absent, all three methods return
//! [`VfsError::Backend`] with `code = "exec_unavailable"` — a clear, user-surfaceable error
//! rather than a misleading [`VfsError::NotFound`].
//!
//! # Log streaming (M6-6 first slice)
//!
//! [`KubeOps::logs`] is implemented via `Api::<Pod>::log_stream`, which returns an
//! `impl futures::io::AsyncBufRead + use<K>` over hyper's response body. Because the concrete
//! type may not implement `Unpin`, `Box::pin` is used to produce a pinned wrapper that does.
//! Frames are drained over a bounded `mpsc` channel from a spawned Tokio task, mirroring the
//! Docker adapter's log-streaming pattern. The task exits when the server closes the stream
//! (non-follow mode) or the receiver is dropped (caller cancelled).
//!
//! # Port-forwarding (M6-6, RFC-0009 §3)
//!
//! [`KubeOps::port_forward`] binds a local `TcpListener` on `127.0.0.1:<port>` and, for each
//! accepted TCP connection, opens a fresh `Portforwarder` via `Api::<Pod>::portforward` and
//! relays bytes bidirectionally with `tokio::io::copy_bidirectional`. One `Portforwarder` per
//! connection is the documented kube-rs pattern (each `Portforwarder` is a single WebSocket; a
//! single forwarder cannot multiplex concurrent connections). The accept loop runs in a spawned
//! Tokio task and exits when the `cancel` signal fires. A shared `CancellationToken` is distributed
//! to every relay task; cancelling it causes each relay to drop its `Portforwarder` stream and
//! local `TcpStream`, triggering TCP close on both sides. `done` resolves with `Ok(0)` after all
//! relay tasks have exited.

use crate::ops::{
    bind_loopback, ContainerInfo, ContextInfo, KubeOps, PodInfo, RemoteEntry, RemoteMeta,
};
use crate::tar_exec::{
    not_found, parse_list_dir, parse_read_tar, parse_stat_tar, tar_basename, tar_parent,
};
use async_trait::async_trait;
use bytes::Bytes;
use cairn_types::{PodPhase, VfsPath};
use cairn_vfs::{SessionHandle, VfsError};
use futures::stream::BoxStream;
use futures::StreamExt as _;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::{
    api::{AttachParams, ListParams, LogParams, TerminalSize},
    config::{KubeConfigOptions, Kubeconfig},
    Api, Client, ResourceExt,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

// ---------------------------------------------------------------------------
// Collected output from a single exec invocation
// ---------------------------------------------------------------------------

/// Raw output collected from a single `exec` call: stdout bytes, stderr bytes, and the
/// Kubernetes-level [`Status`] returned on the exec status channel.
struct ExecOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// `None` only if the WebSocket closed before the server sent a Status frame — treat as
    /// failure.
    status: Option<Status>,
}

impl ExecOutput {
    /// `true` when the exec terminated successfully (exit code 0).
    fn is_success(&self) -> bool {
        self.status
            .as_ref()
            .and_then(|s| s.status.as_deref())
            .map(|s| s == "Success")
            .unwrap_or(false)
    }

    /// Extract the numeric exit code from the Status causes, if present.
    fn exit_code(&self) -> Option<i32> {
        self.status
            .as_ref()?
            .details
            .as_ref()?
            .causes
            .as_ref()?
            .iter()
            .find(|c| c.reason.as_deref() == Some("ExitCode"))
            .and_then(|c| c.message.as_ref())
            .and_then(|m| m.parse::<i32>().ok())
    }

    /// `true` when the failure looks like `tar` (or another binary) is missing from the
    /// container's `PATH`.  Checks the exit code (126 = not executable, 127 = not found) and
    /// both the status message and stderr for common OCI/container-runtime error strings.
    fn is_exec_unavailable(&self) -> bool {
        let exit_code_missing = matches!(self.exit_code(), Some(126) | Some(127));
        let status_msg = self
            .status
            .as_ref()
            .and_then(|s| s.message.as_deref())
            .unwrap_or("");
        let stderr_str = String::from_utf8_lossy(&self.stderr);
        let in_msg = |haystack: &str| -> bool {
            haystack.contains("executable file not found")
                || haystack.contains("not found in $PATH")
                || haystack.contains("OCI runtime exec failed")
                || haystack.contains("no such file or directory")
        };
        exit_code_missing || in_msg(status_msg) || in_msg(&stderr_str)
    }

    /// `true` when stderr indicates tar could not open the requested path (path does not exist
    /// inside the container, not a problem with tar itself), OR when the path is a file that was
    /// given as a directory argument to `tar -C` (which produces "Not a directory" / `ENOTDIR`).
    ///
    /// The trait contract says: `list_dir` on a file path returns [`VfsError::NotFound`], not a
    /// generic backend error.  Mapping `ENOTDIR` to `NotFound` keeps the live adapter consistent
    /// with the mock.
    ///
    /// Note on tar variants:
    /// - **GNU tar** exits 2 and says `Cannot open: No such file or directory`.
    /// - **BusyBox tar** (Alpine, the most common k8s base image) exits 1 and says
    ///   `tar: can't open '<path>': No such file or directory`.
    ///
    /// Both variants must be detected to return `NotFound` rather than a confusing `Backend` error.
    fn is_path_not_found(&self) -> bool {
        let s = String::from_utf8_lossy(&self.stderr);
        // GNU tar: "Cannot open: No such file or directory" (exit 2)
        s.contains("Cannot open: No such file or directory")
            || s.contains("cannot open: No such file or directory")
            // BusyBox tar: "can't open '<path>': No such file or directory" (exit 1)
            || (s.contains("can't open") && s.contains("No such file or directory"))
            // `tar -C <file>` fails with ENOTDIR: "Cannot change directory: Not a directory".
            // The trait says list_dir on a file path → NotFound, matching the mock behaviour.
            || s.contains("Cannot change directory")
            || s.contains("cannot change directory")
            // Broad fallback: "No such file or directory" anywhere in stderr on any non-zero exit,
            // gated on the exit code to reduce false positives.  The exec_unavailable check runs
            // first and filters out OCI-runtime "no such file" messages for the binary itself.
            || (matches!(self.exit_code(), Some(1) | Some(2))
                && s.contains("No such file or directory"))
    }

    /// Map a non-zero exit to a [`VfsError`], distinguishing exec-unavailable from path-not-found
    /// from a generic backend error.
    fn into_vfs_err(self, path: &str) -> VfsError {
        if self.is_exec_unavailable() {
            return VfsError::Backend {
                code: "exec_unavailable".to_owned(),
                msg: "container has no 'tar'; in-container filesystem browsing requires tar to be \
                      present on the container's PATH"
                    .to_owned(),
                retryable: false,
            };
        }
        if self.is_path_not_found() {
            return not_found(path);
        }
        // Prefer the status message; fall back to stderr; use a generic string if both are empty.
        let stderr_str = String::from_utf8_lossy(&self.stderr).into_owned();
        let msg = self
            .status
            .and_then(|s| s.message)
            .filter(|m| !m.is_empty())
            .or_else(|| Some(stderr_str).filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "exec: tar command failed with no output".to_owned());
        VfsError::Backend {
            code: "exec-failed".to_owned(),
            msg,
            retryable: false,
        }
    }
}

// ---------------------------------------------------------------------------
// KubeRsOps
// ---------------------------------------------------------------------------

/// A [`KubeOps`] implementation backed by a live Kubernetes cluster via `kube-rs`.
///
/// Uses the kubeconfig found at `$KUBECONFIG` or `~/.kube/config`. Builds a fresh
/// `kube::Client` per operation call, scoped to the requested context. Credentials are handled
/// entirely by `kube-rs` (exec plugins, OIDC refresh, service-account tokens, client certs); no
/// credential material is ever embedded in error messages.
///
/// In-container filesystem access uses tar-over-exec; see the module-level documentation for the
/// command strategy and error semantics.
pub struct KubeRsOps;

impl KubeRsOps {
    /// Create a new adapter. The kubeconfig is located at call-time via `$KUBECONFIG` or
    /// `~/.kube/config`; no I/O happens at construction. (A future milestone may add an optional
    /// explicit kubeconfig path or a per-context client cache.)
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Build a `kube::Client` scoped to the given context name.
    async fn client_for(&self, ctx: &str) -> Result<Client, VfsError> {
        build_client_for(ctx.to_owned()).await
    }

    /// Execute `command` in `container` inside `pod`/`ns`/`ctx` and collect stdout, stderr, and
    /// the process-level Status.
    ///
    /// Uses `AttachParams` with `stdin=false`, `stdout=true`, `stderr=true`, `tty=false` — the
    /// correct shape for a non-interactive data-extraction command like `tar`.  stdout and stderr
    /// are drained concurrently via `tokio::join!` to prevent the internal `DuplexStream` pipe
    /// from filling and deadlocking the background `AttachedProcess` task.
    async fn exec_tar(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: &str,
        command: &[&str],
    ) -> Result<ExecOutput, VfsError> {
        let client = self.client_for(ctx).await?;
        let api: Api<Pod> = Api::namespaced(client, ns);

        let ap = AttachParams::default()
            .container(container)
            .stdin(false)
            .stdout(true)
            .stderr(true);

        let mut proc = api
            .exec(pod, command.iter().copied(), &ap)
            .await
            .map_err(map_exec_error)?;

        // Take the readers and status future out of the process handle *before* the join.
        // `stdout()` and `stderr()` return `impl AsyncRead + Unpin` backed by a `DuplexStream`
        // whose writer side lives in the background task.  Reading to EOF unblocks the task.
        let stdout_r = proc.stdout().ok_or_else(|| VfsError::Backend {
            code: "exec-io".to_owned(),
            msg: "exec: stdout reader unavailable".to_owned(),
            retryable: false,
        })?;
        let stderr_r = proc.stderr().ok_or_else(|| VfsError::Backend {
            code: "exec-io".to_owned(),
            msg: "exec: stderr reader unavailable".to_owned(),
            retryable: false,
        })?;
        // `take_status()` returns `None` only when called a second time; safe to unwrap here.
        let status_fut = proc.take_status();

        // Drain stdout, stderr, and wait for the process status concurrently.  This is mandatory:
        // the DuplexStream pipe capacity is 1 KiB by default; without concurrent reading the
        // background task blocks on write, causing a deadlock.
        let (stdout_res, stderr_res, status_opt) = tokio::join!(
            async move {
                let mut r = stdout_r;
                let mut buf = Vec::new();
                r.read_to_end(&mut buf).await.map(|_| buf)
            },
            async move {
                let mut r = stderr_r;
                let mut buf = Vec::new();
                r.read_to_end(&mut buf).await.map(|_| buf)
            },
            async move {
                match status_fut {
                    Some(f) => f.await,
                    None => None,
                }
            },
        );

        // Clean up the background task now that all I/O is drained.
        let _ = proc.join().await;

        let stdout = stdout_res.map_err(|e| VfsError::Backend {
            code: "exec-io".to_owned(),
            msg: e.to_string(),
            retryable: false,
        })?;
        let stderr = stderr_res.map_err(|e| VfsError::Backend {
            code: "exec-io".to_owned(),
            msg: e.to_string(),
            retryable: false,
        })?;

        Ok(ExecOutput {
            stdout,
            stderr,
            status: status_opt,
        })
    }
}

impl Default for KubeRsOps {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Client builder (free function, callable from spawned tasks)
// ---------------------------------------------------------------------------

/// Build a `kube::Client` scoped to the given context name.
///
/// Takes an owned `String` so it can be called from inside a `tokio::spawn` closure without
/// borrowing `&self` — `KubeRsOps` is a unit struct, so there is no data to borrow anyway.
async fn build_client_for(ctx: String) -> Result<Client, VfsError> {
    // rustls 0.23 requires a process-wide `CryptoProvider`; install the ring provider once.
    ensure_crypto_provider();
    let opts = KubeConfigOptions {
        context: Some(ctx),
        ..Default::default()
    };
    let config = kube::Config::from_kubeconfig(&opts)
        .await
        .map_err(|e| VfsError::Connection(Box::new(e)))?;
    Client::try_from(config).map_err(|e| VfsError::Connection(Box::new(e)))
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

/// Install the process-wide rustls `CryptoProvider` exactly once.
///
/// rustls 0.23 requires a provider to be installed before a `ClientConfig` is built; kube's client
/// build otherwise panics. `install_default` returns `Err` if a provider is already installed (e.g.
/// another backend got there first) — that is fine, so the error is ignored.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Map a `kube::Error` from a plain API call (list, get) to a [`VfsError`].
///
/// HTTP status codes are examined for 401/403/404; all other API errors become
/// [`VfsError::Backend`] with a safe status message (API response text; no token or credential
/// material appears in `kube::Error::Api` messages). Transport/TLS errors become
/// [`VfsError::Connection`].
fn map_api_error(e: kube::Error) -> VfsError {
    match e {
        kube::Error::Api(ref status) => match status.code {
            401 => VfsError::Auth,
            403 => VfsError::Forbidden(VfsPath::root()),
            404 => VfsError::NotFound(VfsPath::root()),
            code => VfsError::Backend {
                code: format!("k8s-{code}"),
                // `Status.message` is server-provided API text; no credential material.
                msg: status.message.clone(),
                // 5xx (API-server rolling restart, etcd leader election, overloaded aggregated API
                // or ingress) and 429 are transient and safe to retry.
                retryable: matches!(code, 429 | 500 | 502 | 503 | 504),
            },
        },
        // Transport-level errors (TLS, connection refused, hyper, etc.) → Connection.
        other => VfsError::Connection(Box::new(other)),
    }
}

/// Map a `kube::Error` that arose while initiating an exec call.
///
/// This path handles errors from the WebSocket upgrade itself (not from the process running
/// inside the container).  HTTP 401/403/404 have the usual semantics; a message that looks like
/// an OCI exec-startup failure is surfaced as `exec_unavailable` so the UI can explain why
/// in-container browsing is not available.
fn map_exec_error(e: kube::Error) -> VfsError {
    if let kube::Error::Api(status) = &e {
        match status.code {
            401 => return VfsError::Auth,
            403 => return VfsError::Forbidden(VfsPath::root()),
            404 => return VfsError::NotFound(VfsPath::root()),
            _ => {
                let msg = &status.message;
                if msg.contains("executable file not found")
                    || msg.contains("not found in $PATH")
                    || msg.contains("OCI runtime exec failed")
                {
                    return VfsError::Backend {
                        code: "exec_unavailable".to_owned(),
                        msg: "container has no 'tar'; in-container filesystem browsing requires \
                              tar to be present on the container's PATH"
                            .to_owned(),
                        retryable: false,
                    };
                }
            }
        }
    }
    VfsError::Connection(Box::new(e))
}

/// Map a pod's `status.phase` string to [`PodPhase`].
fn map_phase(phase: Option<&str>) -> PodPhase {
    match phase {
        Some("Pending") => PodPhase::Pending,
        Some("Running") => PodPhase::Running,
        Some("Succeeded") => PodPhase::Succeeded,
        Some("Failed") => PodPhase::Failed,
        _ => PodPhase::Unknown,
    }
}

// ---------------------------------------------------------------------------
// KubeOps implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl KubeOps for KubeRsOps {
    async fn list_contexts(&self) -> Result<Vec<ContextInfo>, VfsError> {
        // Parse the kubeconfig from $KUBECONFIG / ~/.kube/config. No cluster call needed.
        // `Kubeconfig::read` is blocking file I/O, so run it off the async worker thread.
        let kubeconfig = tokio::task::spawn_blocking(Kubeconfig::read)
            .await
            .map_err(|e| VfsError::Backend {
                code: "k8s-join".to_owned(),
                msg: e.to_string(),
                retryable: false,
            })?
            .map_err(|e| VfsError::Connection(Box::new(e)))?;

        let contexts = kubeconfig
            .contexts
            .iter()
            .map(|named_ctx| {
                // Resolve the server URL by looking up the cluster this context points at.
                let server = named_ctx
                    .context
                    .as_ref()
                    .and_then(|ctx| {
                        kubeconfig
                            .clusters
                            .iter()
                            .find(|nc| nc.name == ctx.cluster)
                            .and_then(|nc| nc.cluster.as_ref())
                            .and_then(|c| c.server.clone())
                    })
                    .unwrap_or_default();
                ContextInfo {
                    name: named_ctx.name.clone(),
                    server,
                }
            })
            .collect();

        Ok(contexts)
    }

    async fn list_namespaces(&self, ctx: &str) -> Result<Vec<String>, VfsError> {
        let client = self.client_for(ctx).await?;
        let api: Api<Namespace> = Api::all(client);
        let list = api
            .list(&ListParams::default())
            .await
            .map_err(map_api_error)?;
        Ok(list
            .into_iter()
            .map(|ns| ns.name_any())
            .filter(|name| !name.is_empty())
            .collect())
    }

    async fn list_pods(&self, ctx: &str, ns: &str) -> Result<Vec<PodInfo>, VfsError> {
        let client = self.client_for(ctx).await?;
        let api: Api<Pod> = Api::namespaced(client, ns);
        let list = api
            .list(&ListParams::default())
            .await
            .map_err(map_api_error)?;

        Ok(list
            .into_iter()
            .map(|pod| {
                let name = pod.name_any();
                let status = pod.status.as_ref();
                let spec = pod.spec.as_ref();

                let phase = map_phase(status.and_then(|s| s.phase.as_deref()));

                // Ready count: containers where `container_statuses[].ready == true`.
                // Total count: regular containers in the spec (matches kubectl's n/n display).
                let ready_count = status
                    .and_then(|s| s.container_statuses.as_ref())
                    .map(|css| css.iter().filter(|cs| cs.ready).count() as u16)
                    .unwrap_or(0);
                // Total from the spec; but never report fewer than the ready count (a pod whose
                // `.spec` is absent in a partial response must not show an impossible `3/0`).
                let total_count = spec
                    .map(|s| s.containers.len() as u16)
                    .unwrap_or(0)
                    .max(ready_count);

                let node = spec.and_then(|s| s.node_name.clone());

                PodInfo {
                    name,
                    phase,
                    ready: (ready_count, total_count),
                    node,
                }
            })
            .collect())
    }

    async fn list_containers(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
    ) -> Result<Vec<ContainerInfo>, VfsError> {
        let client = self.client_for(ctx).await?;
        let api: Api<Pod> = Api::namespaced(client, ns);
        let pod_obj = api.get(pod).await.map_err(map_api_error)?;

        let spec = pod_obj.spec.as_ref();
        let mut containers: Vec<ContainerInfo> = Vec::new();

        // Regular containers.
        if let Some(s) = spec {
            for c in &s.containers {
                containers.push(ContainerInfo {
                    name: c.name.clone(),
                    is_init: false,
                    is_ephemeral: false,
                });
            }
            // Init containers.
            for c in s.init_containers.as_deref().unwrap_or(&[]) {
                containers.push(ContainerInfo {
                    name: c.name.clone(),
                    is_init: true,
                    is_ephemeral: false,
                });
            }
            // Ephemeral (debug) containers.
            for c in s.ephemeral_containers.as_deref().unwrap_or(&[]) {
                containers.push(ContainerInfo {
                    name: c.name.clone(),
                    is_init: false,
                    is_ephemeral: true,
                });
            }
        }

        Ok(containers)
    }

    /// List the immediate children of `path` inside a container's filesystem.
    ///
    /// Executes `tar cf - -C <path> .` in the container, collects the tar stream, and parses
    /// immediate children from it.  An existing but empty directory returns `Ok(vec![])` — never
    /// [`VfsError::NotFound`] — satisfying the trait contract that a container's root is always
    /// navigable.
    ///
    /// Returns [`VfsError::Backend`] with `code = "exec_unavailable"` when `tar` is absent from
    /// the container's `PATH`, giving the UI something actionable to display.
    async fn list_dir(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: &str,
        path: &str,
    ) -> Result<Vec<RemoteEntry>, VfsError> {
        let command = ["tar", "cf", "-", "-C", path, "."];
        let out = self.exec_tar(ctx, ns, pod, container, &command).await?;

        if out.is_success() {
            // parse_list_dir handles an empty-but-valid tar correctly (returns Ok(vec![])).
            return parse_list_dir(&out.stdout);
        }

        // Non-zero exit: distinguish path-not-found from exec-unavailable from other errors.
        // Special case: tar exits non-zero when it cannot open the directory itself.
        Err(out.into_vfs_err(path))
    }

    /// Stat `path` inside a container's filesystem.
    ///
    /// Executes `tar cf - -C <parent> <basename>` (or returns `Dir` immediately for `/`) and
    /// inspects the first tar header to determine whether the path is a file or directory and what
    /// its size is.
    ///
    /// Returns [`VfsError::NotFound`] when the path does not exist, and
    /// [`VfsError::Backend`] with `code = "exec_unavailable"` when `tar` is absent.
    async fn stat(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: &str,
        path: &str,
    ) -> Result<RemoteMeta, VfsError> {
        // The container root is always a directory — avoid a tar exec for the trivial case.
        if path == "/" {
            return Ok(RemoteMeta {
                kind: cairn_types::EntryKind::Dir,
                size: None,
            });
        }

        let parent = tar_parent(path);
        let basename = tar_basename(path);
        let command = ["tar", "cf", "-", "-C", parent, basename];
        let out = self.exec_tar(ctx, ns, pod, container, &command).await?;

        if out.is_success() {
            return parse_stat_tar(&out.stdout, path);
        }

        Err(out.into_vfs_err(path))
    }

    /// Read the full contents of a file at `path` inside a container's filesystem.
    ///
    /// Executes `tar cf - -C <parent> <basename>` and extracts the first entry's bytes.
    /// Returns [`VfsError::Unsupported`] when `path` is a directory,
    /// [`VfsError::NotFound`] when the path does not exist, and
    /// [`VfsError::Backend`] with `code = "exec_unavailable"` when `tar` is absent.
    ///
    /// **M6 memory note:** The entire tar archive (file bytes included) is buffered in memory
    /// before parsing.  A follow-up should stream-parse to reduce peak memory for large files.
    async fn read(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: &str,
        path: &str,
    ) -> Result<Vec<u8>, VfsError> {
        // Root is always a directory — reading it makes no sense and we can short-circuit before
        // constructing a command with an empty basename (which tar would reject unpredictably).
        if path == "/" {
            return Err(VfsError::Unsupported(cairn_types::Caps::READ));
        }
        let parent = tar_parent(path);
        let basename = tar_basename(path);
        let command = ["tar", "cf", "-", "-C", parent, basename];
        let out = self.exec_tar(ctx, ns, pod, container, &command).await?;

        if out.is_success() {
            return parse_read_tar(&out.stdout, path);
        }

        Err(out.into_vfs_err(path))
    }

    /// Stream log output from a pod's container via `Api::<Pod>::log_stream`.
    ///
    /// The kube 4.0 `log_stream` method returns `impl futures::io::AsyncBufRead + use<K>`,
    /// where `use<K>` (precise capturing) means the returned reader does not borrow `&self` —
    /// it is effectively `'static`. However, the concrete type (hyper body wrapped in
    /// `futures_util::io::IntoAsyncRead`) is typically not `Unpin`, so `Box::pin` is applied
    /// to obtain a `Pin<Box<T>>` which is always `Unpin` and thus usable with
    /// `futures::io::AsyncReadExt::read`.
    ///
    /// The client build and `log_stream` call happen inside a `tokio::spawn` task that owns
    /// all parameters, forwarding 8 KiB chunks over a bounded `mpsc` channel (capacity 64).
    /// The task exits when the server closes the stream (EOF in non-follow mode or daemon
    /// restart) or the receiver is dropped (caller cancelled the stream).
    ///
    /// Error mapping: connection/config failures and API errors (`map_api_error`) become `Err`
    /// items in the stream; the stream never panics.
    ///
    /// **Note:** The `since` field of [`crate::ActionCtx::Logs`] maps to `LogParams::since_time`
    /// or `since_seconds`; its conversion is deferred to a follow-up slice.
    fn logs(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: Option<&str>,
        follow: bool,
        tail: Option<i64>,
    ) -> BoxStream<'static, Result<Bytes, VfsError>> {
        let ctx = ctx.to_owned();
        let ns = ns.to_owned();
        let pod = pod.to_owned();
        let container = container.map(ToOwned::to_owned);

        // 64-frame buffer: ample back-pressure at typical log line sizes.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, VfsError>>(64);

        tokio::spawn(async move {
            // Build a client scoped to this context.
            let client = match build_client_for(ctx).await {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };

            let api: Api<Pod> = Api::namespaced(client, &ns);
            let lp = LogParams {
                container,
                follow,
                tail_lines: tail,
                ..Default::default()
            };

            // `log_stream` returns `impl futures::io::AsyncBufRead + use<K>`.
            // The return type is effectively 'static (use<K> captures only the type param Pod,
            // not &self or the name/lp references). The concrete type is not guaranteed to be
            // Unpin, so we Box::pin it: `Pin<Box<T>>` is always Unpin, enabling the
            // `futures::io::AsyncReadExt::read` convenience method.
            let reader = match api.log_stream(&pod, &lp).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(map_api_error(e))).await;
                    return;
                }
            };
            let mut reader = Box::pin(reader);

            let mut buf = [0u8; 8192];
            loop {
                use futures::io::AsyncReadExt as _;
                match reader.read(&mut buf).await {
                    Ok(0) => break, // EOF — stream ended (non-follow) or server closed it
                    Ok(n) => {
                        // If the receiver was dropped (caller cancelled), stop producing.
                        if tx
                            .send(Ok(Bytes::copy_from_slice(&buf[..n])))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(VfsError::Backend {
                                code: "k8s-logs".to_owned(),
                                msg: e.to_string(),
                                retryable: false,
                            }))
                            .await;
                        break;
                    }
                }
            }
            // Task exits here; dropping `tx` closes the channel, signalling EOF to the consumer.
        });

        futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed()
    }

    /// Open an interactive exec session in a running pod container via the Kubernetes API.
    ///
    /// Uses `Api::<Pod>::exec` with [`AttachParams`] configured for stdin/stdout/stderr (and TTY
    /// when requested). The [`kube`] client negotiates the WebSocket/SPDY upgrade transparently;
    /// this method sees only `AsyncRead`/`AsyncWrite` handles.
    ///
    /// # Task model
    ///
    /// A single Tokio task is spawned that owns the `AttachedProcess`. Inside it:
    ///
    /// - **stdin relay**: drains the `stdin` mpsc channel and writes each chunk to the process
    ///   stdin `AsyncWrite`. When the channel is empty and the sender is dropped, the write-half
    ///   is dropped too, closing stdin on the remote side.
    /// - **stdout relay**: reads from the process `AsyncRead` in 4 KiB increments and forwards
    ///   chunks to the `stdout` mpsc sender. When `tty: false`, a parallel stderr relay does the
    ///   same and the two streams are interleaved (arrival order).
    /// - **resize relay** (TTY only): reads `(rows, cols)` from the resize mpsc channel and
    ///   writes them to the `TerminalSize` watch sender returned by `AttachedProcess::terminal_size`.
    ///   Uses the watch directly — no polling, the latest value is always applied.
    /// - **main select**: waits for either `cancel` (drop/send on the oneshot) or the process
    ///   status future (`AttachedProcess::take_status`) to resolve. On cancel, relay tasks are
    ///   aborted and `done` resolves with `Ok(-1)`. On natural exit, the exit code is extracted
    ///   from the Kubernetes `Status` causes (`ExitCode` reason); a missing cause means `Ok(0)`.
    ///
    /// # Exit code extraction
    ///
    /// The Kubernetes API encodes the process exit code in `Status.details.causes[].message`
    /// where `reason == "ExitCode"`. A `Status.status == "Success"` (or a missing cause) maps
    /// to exit code 0. All non-zero exits are `Ok(n)`, not an error — the error path is reserved
    /// for transport/API failures.
    ///
    /// # Credential safety
    ///
    /// No credential material appears in `VfsError` messages; `kube::Error::Api` messages contain
    /// only API-server-provided text, and transport errors are wrapped in `VfsError::Connection`.
    async fn exec(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        container: &str,
        command: Vec<String>,
        tty: bool,
    ) -> Result<SessionHandle, VfsError> {
        let client = build_client_for(ctx.to_owned()).await?;
        let api: Api<Pod> = Api::namespaced(client, ns);

        let ap = AttachParams::default()
            .container(container)
            .stdin(true)
            .stdout(true)
            .stderr(!tty) // K8s merges stderr into stdout when tty=true
            .tty(tty);

        let mut proc = api
            .exec(pod, command.iter().map(String::as_str), &ap)
            .await
            .map_err(map_exec_error)?;

        // Extract all I/O handles from the process before spawning any tasks.
        // `AttachedProcess` drives the WebSocket in a background task; taking the readers/writers
        // here does NOT block it — the internal DuplexStream pipes remain open until we drop them.
        let proc_stdin = proc.stdin().ok_or_else(|| VfsError::Backend {
            code: "exec-io".to_owned(),
            msg: "exec: stdin writer unavailable from AttachedProcess".to_owned(),
            retryable: false,
        })?;
        let proc_stdout = proc.stdout().ok_or_else(|| VfsError::Backend {
            code: "exec-io".to_owned(),
            msg: "exec: stdout reader unavailable from AttachedProcess".to_owned(),
            retryable: false,
        })?;
        // stderr is None when tty=true (Docker/K8s convention: TTY merges streams).
        let proc_stderr = proc.stderr();
        // terminal_size is Some only when tty=true.
        let terminal_size_watch = proc.terminal_size();
        // take_status() returns the future for the Kubernetes-level process Status.
        // Calling it a second time would return None; safe to unwrap once.
        let status_fut = proc.take_status();

        // Session channels.
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<Bytes>(32);
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::channel::<Bytes>(64);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Result<i32, VfsError>>();

        // Resize channel: present iff tty=true.
        let (resize_tx, resize_rx) = if tty {
            let (t, r) = tokio::sync::mpsc::channel::<(u16, u16)>(8);
            (Some(t), Some(r))
        } else {
            (None, None)
        };

        // Spawn the owning task. It holds `proc` for its lifetime and drives all relay sub-tasks.
        tokio::spawn(async move {
            // --- stdin relay ---
            // Moves `stdin_rx` and `proc_stdin` into a sub-task. When `stdin_rx` is exhausted
            // (sender dropped), the write-half is dropped, signalling EOF to the remote process.
            let stdin_task = {
                let mut writer = proc_stdin;
                let mut rx = stdin_rx;
                tokio::spawn(async move {
                    while let Some(chunk) = rx.recv().await {
                        if writer.write_all(&chunk).await.is_err() {
                            break;
                        }
                    }
                    // Drop `writer` here → closes the stdin pipe to the remote process.
                })
            };

            // --- stdout relay ---
            // Reads from proc stdout in 4 KiB chunks and forwards to `stdout_tx`.
            // When the consumer drops `stdout_rx`, `send` returns Err and we stop reading.
            let stdout_task = {
                let tx = stdout_tx.clone();
                let mut reader = proc_stdout;
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        match reader.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if tx.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
            };

            // --- stderr relay (non-TTY only) ---
            // Interleaves stderr into the same `stdout_tx` channel.  When tty=true proc_stderr
            // is None and this task is not spawned.
            let stderr_task = if let Some(mut reader) = proc_stderr {
                Some(tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        match reader.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if stdout_tx
                                    .send(Bytes::copy_from_slice(&buf[..n]))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                }))
            } else {
                drop(stdout_tx); // last clone; stdout_task holds the other — drop this one
                None
            };

            // --- resize relay (TTY only) ---
            // Converts (rows, cols) → TerminalSize and forwards to the kube-rs
            // `futures::channel::mpsc::Sender<TerminalSize>` returned by
            // `AttachedProcess::terminal_size()`. `try_send` is used (non-blocking / best-effort):
            // if the internal channel buffer is full we simply discard the resize — the TUI will
            // send another event on the next window change, so no data is permanently lost.
            if let (Some(mut rr), Some(mut ts)) = (resize_rx, terminal_size_watch) {
                tokio::spawn(async move {
                    while let Some((rows, cols)) = rr.recv().await {
                        // `try_send` is synchronous; errors (full buffer or closed) are silently
                        // ignored — the session may have already ended.
                        let _ = ts.try_send(TerminalSize {
                            width: cols,
                            height: rows,
                        });
                    }
                });
            }

            // --- main select: cancel vs. process exit ---
            tokio::select! {
                biased;

                // Cancel path: abort relay tasks and send the sentinel exit code -1.
                _ = cancel_rx => {
                    stdin_task.abort();
                    stdout_task.abort();
                    if let Some(t) = stderr_task { t.abort(); }
                    // Best-effort join — may have already exited.
                    let _ = proc.join().await;
                    let _ = done_tx.send(Ok(-1));
                }

                // Normal exit path: the Kubernetes Status future resolves when the remote process
                // exits. Extract the numeric exit code from the Status causes.
                status_opt = async {
                    match status_fut {
                        Some(f) => f.await,
                        None => None,
                    }
                } => {
                    // Wait for stdout relay to drain so the consumer receives all output before
                    // `done` resolves. Ignore the join error (task may already be done).
                    let _ = stdout_task.await;
                    stdin_task.abort();
                    if let Some(t) = stderr_task { t.abort(); }
                    let _ = proc.join().await;

                    // Extract the numeric exit code. A `Status.status == "Success"` (or an absent
                    // ExitCode cause) maps to 0. Any parse failure also falls back to 0 (defensive).
                    let code = status_opt
                        .as_ref()
                        .and_then(|s| s.details.as_ref())
                        .and_then(|d| d.causes.as_ref())
                        .and_then(|cs| {
                            cs.iter()
                                .find(|c| c.reason.as_deref() == Some("ExitCode"))
                        })
                        .and_then(|c| c.message.as_ref())
                        .and_then(|m| m.parse::<i32>().ok())
                        .unwrap_or(0);

                    // If done_rx was dropped (session pane torn down), discard the send error.
                    let _ = done_tx.send(Ok(code));
                }
            }
        });

        Ok(SessionHandle::new(
            cancel_tx,
            done_rx,
            None, // local_port: exec never binds a port
            Some(stdin_tx),
            Some(stdout_rx),
            resize_tx,
        ))
    }

    /// Forward a local TCP port to `remote_port` on a running pod.
    ///
    /// Binds a `TcpListener` on `127.0.0.1:<local_port>` (or an ephemeral port when
    /// `local_port` is `None` or `Some(0)`). Builds the `kube::Client` eagerly before starting
    /// the accept loop — auth/config failures are surfaced immediately as [`VfsError`] rather
    /// than being silently swallowed per-connection. Returns a [`SessionHandle`] with `local_port`
    /// set to the actually-bound port immediately, before any connection arrives, so the TUI can
    /// display the address at once.
    ///
    /// # Accept loop and relay model
    ///
    /// A single Tokio task owns the listener and loops, accepting connections with
    /// `tokio::select!` between `cancel` and `listener.accept()`. For each accepted TCP
    /// connection, an independent relay sub-task is added to a `JoinSet` and:
    ///
    /// 1. Clones the pre-built `kube::Client` (cheap — `Client` is Arc-backed).
    /// 2. Calls `Api::<Pod>::portforward(pod, &[remote_port])` to open a WebSocket to the
    ///    API server (one WebSocket per connection — the documented kube-rs pattern).
    /// 3. Takes the port stream via `Portforwarder::take_stream(remote_port)`.
    /// 4. `tokio::select!`s `copy_bidirectional` against the shared `CancellationToken`.
    /// 5. Drops the upstream stream, then joins the `Portforwarder` task.
    ///
    /// On `cancel`: the accept loop breaks, `token.cancel()` fires, all relay tasks exit their
    /// `select!`, and `join_set.shutdown()` awaits their completion before `done` resolves.
    ///
    /// Accept errors are classified: `ConnectionAborted`/`Interrupted` are transient and retried;
    /// fd-exhaustion (`EMFILE`/`ENFILE`) backs off 100 ms then retries; other OS errors are fatal
    /// and resolve `done` with `Err(VfsError::Backend { code: "accept_failed", retryable: true })`.
    ///
    /// # Credential safety
    ///
    /// No credential material appears in `VfsError` messages. Bind-failure messages omit
    /// the port number (it is already surfaced in `SessionHandle.local_port`).
    async fn port_forward(
        &self,
        ctx: &str,
        ns: &str,
        pod: &str,
        remote_port: u16,
        local_port: Option<u16>,
    ) -> Result<SessionHandle, VfsError> {
        // FIX 5: shared bind helper removes duplicated bind/error-mapping block.
        let (listener, bound_port) = bind_loopback(local_port.unwrap_or(0)).await?;

        // FIX 2: build the client once, eagerly, before the accept loop. Auth/config failures
        // are surfaced here as VfsError rather than being silently dropped per-connection.
        // kube::Client is Arc-backed; clone() into each relay task is cheap.
        let client = build_client_for(ctx.to_owned()).await?;

        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Result<i32, VfsError>>();

        // FIX 1: CancellationToken shared to all relay tasks so they can be torn down on cancel.
        let token = tokio_util::sync::CancellationToken::new();

        let ns = ns.to_owned();
        let pod = pod.to_owned();

        tokio::spawn(async move {
            let mut join_set = tokio::task::JoinSet::<()>::new();
            // FIX 3: track a fatal accept error to forward to `done`.
            let mut fatal_err: Option<VfsError> = None;

            'accept: loop {
                tokio::select! {
                    biased;
                    // Cancel path: stop accepting; relay tasks will be torn down below.
                    _ = &mut cancel_rx => break 'accept,
                    accept = listener.accept() => {
                        match accept {
                            Ok((conn, _peer)) => {
                                // Spawn an independent relay task per connection.
                                // Clone cheaply — Client is Arc-backed; Strings are small names.
                                let relay_client = client.clone();
                                let ns = ns.clone();
                                let pod = pod.clone();
                                let relay_token = token.clone();
                                join_set.spawn(async move {
                                    let api: Api<Pod> = Api::namespaced(relay_client, &ns);

                                    // One Portforwarder per connection (each is a WebSocket).
                                    let mut forwarder =
                                        match api.portforward(&pod, &[remote_port]).await {
                                            Ok(f) => f,
                                            // TODO(tracing): warn! here with redacted pod/ns
                                            Err(_) => return,
                                        };

                                    // take_stream() returns None only when the port was not
                                    // included in the portforward() call (impossible here) or
                                    // when called a second time — safe to drop silently.
                                    let mut upstream =
                                        match forwarder.take_stream(remote_port) {
                                            Some(s) => s,
                                            None => return,
                                        };

                                    let mut client_conn = conn;

                                    // FIX 1: relay bytes in both directions, but abort when the
                                    // cancellation token fires (session cancelled by the user).
                                    // On cancel, dropping upstream + client_conn closes both TCP
                                    // sides, which also unblocks the Portforwarder WebSocket.
                                    tokio::select! {
                                        _ = relay_token.cancelled() => {
                                            // TODO(tracing): debug! "relay cancelled"
                                        }
                                        result = tokio::io::copy_bidirectional(
                                            &mut client_conn,
                                            &mut upstream,
                                        ) => {
                                            // Ignore copy error — the peer may have closed first.
                                            let _ = result;
                                        }
                                    }

                                    // Explicit drop signals EOF on the upstream write half
                                    // before joining the Portforwarder WebSocket task.
                                    drop(upstream);
                                    let _ = forwarder.join().await;
                                });
                            }
                            // FIX 3: classify accept errors rather than treating all as fatal.
                            Err(e) => {
                                use std::io::ErrorKind as K;
                                match e.kind() {
                                    // Transient: client disconnected before the OS delivered the
                                    // fd, or the accept() call was interrupted by a signal.
                                    K::ConnectionAborted | K::Interrupted => continue 'accept,
                                    // fd exhaustion: sleep briefly so other fds can be released,
                                    // then retry. The OS error numbers are EMFILE (24) and ENFILE
                                    // (23) on Linux; reported as `Other` in older Rust, or via
                                    // raw_os_error() on all versions.
                                    K::Other => {
                                        let raw = e.raw_os_error().unwrap_or(0);
                                        if raw == 24 /* EMFILE */ || raw == 23 /* ENFILE */ {
                                            // TODO(tracing): warn! about fd exhaustion
                                            tokio::time::sleep(
                                                std::time::Duration::from_millis(100),
                                            )
                                            .await;
                                            continue 'accept;
                                        }
                                        // Non-fd-exhaustion Other: treat as fatal.
                                        fatal_err = Some(VfsError::Backend {
                                            code: "accept_failed".to_owned(),
                                            msg: "port-forward: accept loop failed".to_owned(),
                                            retryable: true,
                                        });
                                        break 'accept;
                                    }
                                    // Any other OS error is fatal (e.g. listener was forcibly
                                    // closed by the OS, or the socket became invalid).
                                    _ => {
                                        // TODO(tracing): warn! with e.kind() (no port/address)
                                        fatal_err = Some(VfsError::Backend {
                                            code: "accept_failed".to_owned(),
                                            msg: "port-forward: accept loop failed".to_owned(),
                                            retryable: true,
                                        });
                                        break 'accept;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // FIX 1: cancel all in-flight relay tasks and wait for them to exit so that both
            // TCP connections and Portforwarder WebSockets are cleanly closed before `done`
            // resolves. `shutdown()` aborts remaining tasks and awaits their drop.
            token.cancel();
            join_set.shutdown().await;

            // Resolve done with the accept-loop outcome. On clean cancel or natural exit: Ok(0).
            // On a fatal accept error: Err so the UI can surface the failure.
            let result = fatal_err.map_or(Ok(0), Err);
            // If the receiver was dropped (session pane torn down), discard the send error.
            let _ = done_tx.send(result);
        });

        Ok(SessionHandle::new(
            cancel_tx,
            done_rx,
            Some(bound_port),
            None, // port-forward has no stdin/stdout — relay is transparent TCP
            None,
            None, // no TTY resize for port-forward
        ))
    }
}

// ---------------------------------------------------------------------------
// Unit tests for exec-output classification heuristics
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::ExecOutput;
    use cairn_vfs::VfsError;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Status, StatusCause, StatusDetails};

    fn make_status(code: i32) -> Status {
        Status {
            status: Some("Failure".to_owned()),
            reason: Some("NonZeroExitCode".to_owned()),
            details: Some(StatusDetails {
                causes: Some(vec![StatusCause {
                    reason: Some("ExitCode".to_owned()),
                    message: Some(code.to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn output(stderr: &[u8], exit_code: i32) -> ExecOutput {
        ExecOutput {
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
            status: Some(make_status(exit_code)),
        }
    }

    // -- GNU tar not-found detection -----------------------------------------

    #[test]
    fn gnu_tar_cannot_open_maps_to_not_found() {
        let out = output(
            b"tar: /etc/missing: Cannot open: No such file or directory\n\
              tar: Error is not recoverable: exiting now\n",
            2,
        );
        assert!(!out.is_exec_unavailable());
        assert!(out.is_path_not_found());
        assert!(matches!(
            out.into_vfs_err("/etc/missing"),
            VfsError::NotFound(_)
        ));
    }

    // -- BusyBox tar not-found detection (Alpine containers) -----------------

    #[test]
    fn busybox_tar_cant_open_exit1_maps_to_not_found() {
        // BusyBox tar uses "can't open" and exits 1 instead of GNU tar's "Cannot open" + exit 2.
        let out = output(
            b"tar: can't open '/etc/missing': No such file or directory\n",
            1,
        );
        assert!(!out.is_exec_unavailable());
        assert!(out.is_path_not_found());
        assert!(matches!(
            out.into_vfs_err("/etc/missing"),
            VfsError::NotFound(_)
        ));
    }

    // -- exec_unavailable detection -------------------------------------------

    #[test]
    fn exit_127_maps_to_exec_unavailable() {
        // Exit code 127 = "command not found" (shell) / binary not in PATH.
        let out = output(b"", 127);
        assert!(out.is_exec_unavailable());
        let err = out.into_vfs_err("/etc");
        assert!(matches!(&err, VfsError::Backend { code, .. } if code == "exec_unavailable"));
    }

    #[test]
    fn exit_126_maps_to_exec_unavailable() {
        let out = output(b"", 126);
        assert!(out.is_exec_unavailable());
    }

    #[test]
    fn oci_message_in_status_maps_to_exec_unavailable() {
        let mut status = make_status(1);
        status.message = Some(
            "OCI runtime exec failed: exec: \"tar\": executable file not found in $PATH".to_owned(),
        );
        let out = ExecOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: Some(status),
        };
        assert!(out.is_exec_unavailable());
    }

    // -- ENOTDIR (list_dir on a file path) ------------------------------------

    #[test]
    fn cannot_chdir_maps_to_not_found() {
        // `tar cf - -C /etc/hostname .` fails because /etc/hostname is a file, not a dir.
        let out = output(
            b"tar: /etc/hostname: Cannot change directory: Not a directory\n",
            2,
        );
        assert!(!out.is_exec_unavailable());
        assert!(out.is_path_not_found());
    }

    // -- fallback empty message -----------------------------------------------

    #[test]
    fn empty_stderr_and_no_status_message_gives_generic_msg() {
        let out = ExecOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: None,
        };
        let err = out.into_vfs_err("/somewhere");
        assert!(matches!(&err, VfsError::Backend { code, msg, .. }
            if code == "exec-failed" && !msg.is_empty()));
    }
}
