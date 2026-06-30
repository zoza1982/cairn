//! Live Kubernetes adapter over `kube-rs 4.0` + `k8s-openapi 0.28`.
//!
//! [`KubeRsOps`] implements [`KubeOps`] against a real cluster. It reads the kubeconfig from the
//! standard location (`$KUBECONFIG` or `~/.kube/config`) and builds a per-call [`kube::Client`]
//! for the requested context. A per-call build is adequate for M6; a caching layer belongs in a
//! later milestone when connection-pool metrics are needed.
//!
//! In-container filesystem access — [`KubeOps::list_dir`], [`KubeOps::stat`], and
//! [`KubeOps::read`] — is **deferred to M6-5b** (tar-over-exec / `kubectl cp` semantics) and
//! returns [`VfsError::Unsupported`] with the appropriate [`Caps`] flag.

use crate::ops::{ContainerInfo, ContextInfo, KubeOps, PodInfo, RemoteEntry, RemoteMeta};
use async_trait::async_trait;
use cairn_types::{Caps, PodPhase, VfsPath};
use cairn_vfs::VfsError;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::{
    api::ListParams,
    config::{KubeConfigOptions, Kubeconfig},
    Api, Client, ResourceExt,
};

/// A [`KubeOps`] implementation backed by a live Kubernetes cluster via `kube-rs`.
///
/// Uses the kubeconfig found at `$KUBECONFIG` or `~/.kube/config`. Builds a fresh
/// [`kube::Client`] per operation call, scoped to the requested context. Credentials are handled
/// entirely by `kube-rs` (exec plugins, OIDC refresh, service-account tokens, client certs); no
/// credential material is ever embedded in error messages.
pub struct KubeRsOps;

impl KubeRsOps {
    /// Create a new adapter. The kubeconfig is located at call-time via `$KUBECONFIG` or
    /// `~/.kube/config`; no I/O happens at construction. (A future milestone may add an optional
    /// explicit kubeconfig path or a per-context client cache.)
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Build a [`kube::Client`] scoped to the given context name.
    async fn client_for(&self, ctx: &str) -> Result<Client, VfsError> {
        let opts = KubeConfigOptions {
            context: Some(ctx.to_owned()),
            ..Default::default()
        };
        let config = kube::Config::from_kubeconfig(&opts)
            .await
            .map_err(|e| VfsError::Connection(Box::new(e)))?;
        Client::try_from(config).map_err(|e| VfsError::Connection(Box::new(e)))
    }
}

impl Default for KubeRsOps {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a `kube::Error` from an API call to a [`VfsError`].
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

#[async_trait]
impl KubeOps for KubeRsOps {
    async fn list_contexts(&self) -> Result<Vec<ContextInfo>, VfsError> {
        // Parse the kubeconfig from $KUBECONFIG / ~/.kube/config. No cluster call needed.
        // `Kubeconfig::read` is blocking file I/O, so run it off the async worker thread (§9).
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

    /// List a directory inside a container's filesystem.
    ///
    /// **Deferred — M6-5b.** In-container filesystem browsing via tar-over-`exec` (`kubectl cp`
    /// semantics) is the next integration step. Until then this returns an **empty** listing rather
    /// than an error: `KubeVfs::caps_at` advertises `LIST` at this depth and the trait contract
    /// requires a container's root to list as `Ok(vec![])` (never an error), so a container is
    /// navigable (shows an empty filesystem) instead of erroring on first descent. Because the
    /// listing is empty, no `stat`/`read` of an in-container path is reachable through normal
    /// navigation while the feature is deferred.
    async fn list_dir(
        &self,
        _ctx: &str,
        _ns: &str,
        _pod: &str,
        _container: &str,
        _path: &str,
    ) -> Result<Vec<RemoteEntry>, VfsError> {
        Ok(Vec::new())
    }

    /// Stat a path inside a container's filesystem.
    ///
    /// **Deferred — M6-5b.** See [`list_dir`](Self::list_dir).
    async fn stat(
        &self,
        _ctx: &str,
        _ns: &str,
        _pod: &str,
        _container: &str,
        _path: &str,
    ) -> Result<RemoteMeta, VfsError> {
        Err(VfsError::Unsupported(Caps::READ))
    }

    /// Read a file inside a container's filesystem.
    ///
    /// **Deferred — M6-5b.** See [`list_dir`](Self::list_dir).
    async fn read(
        &self,
        _ctx: &str,
        _ns: &str,
        _pod: &str,
        _container: &str,
        _path: &str,
    ) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::Unsupported(Caps::READ))
    }
}
