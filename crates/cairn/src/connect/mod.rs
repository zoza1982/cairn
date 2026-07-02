//! The connection-management layer: the broker-backed opener and the RFC-0011 P1 abstractions.
//!
//! ## Structure
//!
//! - **[`ConnectionOpener`]** (this module) — the integration seam that turns a saved
//!   [`ConnectionProfile`] into a live [`Vfs`] backend. Resolves vault credentials through the
//!   [`Broker`] and dispatches to the matching backend connector.
//! - **[`descriptor`]** — runtime-side [`ConnectionDescriptor`](descriptor::ConnectionDescriptor)
//!   and related types. Lives binary-side only; never crosses into `cairn-core`.
//! - **[`provider`]** — the [`ConnectionProvider`](provider::ConnectionProvider) trait plus the
//!   P1 built-in providers ([`BuiltinLocalProvider`](provider::BuiltinLocalProvider),
//!   [`SavedProfileProvider`](provider::SavedProfileProvider)).
//! - **[`coordinator`]** — the [`ConnectionCoordinator`](coordinator::ConnectionCoordinator) that
//!   replaces the imperative body of `register_connections`.
//!
//! Each credential-bearing scheme is gated behind a cargo feature (`ssh`/`s3`/`gcs`/`azure`); a
//! profile whose scheme's feature is not compiled into this binary fails fast with
//! [`OpenError::BackendNotBuilt`] — never a network attempt. The opener module itself is always
//! compiled (it only needs the broker), so the lean build still has a real opener that cleanly
//! reports "not built into this binary".
//!
//! Docker/K8s are also dispatched here, but connect *without* a vault credential (local socket /
//! kubeconfig), so they skip the broker.
//!
//! **Deferred (follow-ups), noted so the gaps are visible:**
//! - The new-connection TUI (M4-5) that would let a user create/trigger a remote profile, and the
//!   `Overlay::VaultUnlock` flow (M3-7) that unlocks the broker. Until the latter lands the runtime
//!   wires a *locked* broker, so a real connect attempt returns [`BrokerError::Locked`] — the
//!   dispatch + parameter construction below are exercised regardless.

pub(crate) mod coordinator;
pub(crate) mod descriptor;
pub(crate) mod provider;

use std::sync::Arc;

use cairn_broker::{Actor, Broker, BrokerError};
use cairn_config::ConnectionProfile;
use cairn_types::ConnectionId;
use cairn_vfs::{Vfs, VfsError};

/// Why opening a connection from a profile failed.
///
/// Which variants are *constructed* depends on the compiled feature set: the lean build only reaches
/// [`BackendNotBuilt`](OpenError::BackendNotBuilt) / [`UnsupportedScheme`](OpenError::UnsupportedScheme),
/// whereas an all-backends build never constructs `BackendNotBuilt` (no scheme's feature is off).
/// Since *some* variant is unused under any given feature set, the `allow(dead_code)` is unconditional.
#[derive(thiserror::Error)]
#[allow(dead_code)]
pub(crate) enum OpenError {
    /// The profile's scheme is a known credential-bearing scheme, but its backend is not compiled
    /// into this binary (the corresponding cargo feature is off).
    #[error("no backend for scheme '{0}' is built into this binary")]
    BackendNotBuilt(String),

    /// The scheme is not one this opener handles (e.g. `local`, handled by the local backend, or an
    /// unrecognized scheme). Docker/K8s ARE handled — without a vault credential.
    #[error("scheme '{0}' is not opened by the connection broker")]
    UnsupportedScheme(String),

    /// A required non-secret endpoint field is missing from the profile.
    #[error("connection '{profile}' is missing required field '{field}'")]
    MissingField {
        /// The profile's display name.
        profile: String,
        /// The missing endpoint key.
        field: String,
    },

    /// An endpoint field is present but malformed (e.g. a non-numeric port).
    #[error("connection '{profile}': invalid '{field}': {reason}")]
    InvalidField {
        /// The profile's display name.
        profile: String,
        /// The offending endpoint key.
        field: String,
        /// A short, value-free reason.
        reason: String,
    },

    /// The profile carries no credential reference, but the scheme requires one.
    #[error("connection '{0}' has no credential reference (secret_ref)")]
    MissingCredential(String),

    /// Resolving the credential reference through the broker failed (vault locked / unknown id).
    #[error("{0}")]
    Broker(#[from] BrokerError),

    /// The backend connector rejected the connection. The message is redacted — it never carries
    /// path, host, or credential material.
    #[error("{}", .0.redacted())]
    Vfs(#[from] VfsError),
}

// `Debug` mirrors `Display` so a `{:?}` log can never surface more than the redacted message. This
// matters for the `Vfs(VfsError)` variant: a *derived* `Debug` would expose the inner
// `VfsError::Connection(source)` / `VfsError::Backend { msg, .. }` detail that `redacted()` strips —
// and Cairn requires secrets/paths redacted in `Debug` as well as logs (CLAUDE.md §9).
impl std::fmt::Debug for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

/// Opens connections from saved profiles by resolving their credentials through the [`Broker`].
///
/// Cheap to clone (the broker is shared behind an `Arc`).
#[derive(Clone)]
pub(crate) struct ConnectionOpener {
    // Read only by the feature-gated scheme arms (to resolve credentials); unused in the lean build
    // where every arm short-circuits to `BackendNotBuilt` before touching the broker.
    #[cfg_attr(
        not(any(feature = "ssh", feature = "s3", feature = "gcs", feature = "azure")),
        allow(dead_code)
    )]
    broker: Arc<Broker>,
}

impl ConnectionOpener {
    /// Create an opener over the given (shared) broker.
    pub(crate) fn new(broker: Arc<Broker>) -> Self {
        Self { broker }
    }

    /// Whether the broker's vault is currently locked.
    ///
    /// Pass this as [`DiscoveryCtx::vault_locked`](provider::DiscoveryCtx::vault_locked) when
    /// calling the coordinator so that [`SavedProfileProvider`](provider::SavedProfileProvider)
    /// classifies credential-bearing profiles correctly. Using the live broker state (rather than
    /// a hardcoded literal) means a P2 re-enumeration after vault unlock will automatically
    /// classify those profiles as [`Reachability::Ready`](descriptor::Reachability::Ready).
    pub(crate) fn vault_locked(&self) -> bool {
        !self.broker.is_unlocked()
    }

    /// Open the backend described by `profile`, assigning it [`ConnectionId`] `conn`.
    ///
    /// `actor` is recorded in the broker's audit journal for the credential resolution. Dispatches
    /// on `profile.scheme`; an unknown or non-credential scheme returns
    /// [`OpenError::UnsupportedScheme`], a known scheme whose feature is off returns
    /// [`OpenError::BackendNotBuilt`].
    pub(crate) async fn open(
        &self,
        actor: Actor,
        conn: ConnectionId,
        profile: &ConnectionProfile,
    ) -> Result<Arc<dyn Vfs>, OpenError> {
        match profile.scheme.as_str() {
            "ssh" => self.open_ssh(actor, conn, profile).await,
            "s3" => self.open_s3(actor, conn, profile).await,
            "gcs" => self.open_gcs(actor, conn, profile).await,
            "azure" => self.open_azure(actor, conn, profile).await,
            // Docker/K8s connect without a vault credential (local socket / kubeconfig).
            "docker" => self.open_docker(conn).await,
            "kubernetes" | "k8s" => self.open_k8s(conn).await,
            other => Err(OpenError::UnsupportedScheme(other.to_owned())),
        }
    }

    // ---- SSH ---------------------------------------------------------------

    #[cfg(feature = "ssh")]
    async fn open_ssh(
        &self,
        actor: Actor,
        conn: ConnectionId,
        profile: &ConnectionProfile,
    ) -> Result<Arc<dyn Vfs>, OpenError> {
        let params = ssh_params(profile)?;
        let secret = self.broker.resolve(actor, credential_id(profile)?)?;
        let vfs = cairn_backend_ssh::ssh_connect(conn, &params, &secret).await?;
        Ok(Arc::new(vfs))
    }

    #[cfg(not(feature = "ssh"))]
    async fn open_ssh(
        &self,
        _actor: Actor,
        _conn: ConnectionId,
        _profile: &ConnectionProfile,
    ) -> Result<Arc<dyn Vfs>, OpenError> {
        Err(OpenError::BackendNotBuilt("ssh".to_owned()))
    }

    // ---- S3 ----------------------------------------------------------------

    #[cfg(feature = "s3")]
    async fn open_s3(
        &self,
        actor: Actor,
        conn: ConnectionId,
        profile: &ConnectionProfile,
    ) -> Result<Arc<dyn Vfs>, OpenError> {
        let params = s3_params(profile)?;
        let root = root_prefix(profile);
        let secret = self.broker.resolve(actor, credential_id(profile)?)?;
        let vfs = cairn_backend_object::s3_connect(conn, &params, &secret, &root).await?;
        Ok(Arc::new(vfs))
    }

    #[cfg(not(feature = "s3"))]
    async fn open_s3(
        &self,
        _actor: Actor,
        _conn: ConnectionId,
        _profile: &ConnectionProfile,
    ) -> Result<Arc<dyn Vfs>, OpenError> {
        Err(OpenError::BackendNotBuilt("s3".to_owned()))
    }

    // ---- GCS ---------------------------------------------------------------

    #[cfg(feature = "gcs")]
    async fn open_gcs(
        &self,
        actor: Actor,
        conn: ConnectionId,
        profile: &ConnectionProfile,
    ) -> Result<Arc<dyn Vfs>, OpenError> {
        let params = gcs_params(profile)?;
        let root = root_prefix(profile);
        let secret = self.broker.resolve(actor, credential_id(profile)?)?;
        let vfs = cairn_backend_object::gcs_connect(conn, &params, &secret, &root).await?;
        Ok(Arc::new(vfs))
    }

    #[cfg(not(feature = "gcs"))]
    async fn open_gcs(
        &self,
        _actor: Actor,
        _conn: ConnectionId,
        _profile: &ConnectionProfile,
    ) -> Result<Arc<dyn Vfs>, OpenError> {
        Err(OpenError::BackendNotBuilt("gcs".to_owned()))
    }

    // ---- Azure -------------------------------------------------------------

    #[cfg(feature = "azure")]
    async fn open_azure(
        &self,
        actor: Actor,
        conn: ConnectionId,
        profile: &ConnectionProfile,
    ) -> Result<Arc<dyn Vfs>, OpenError> {
        let params = azure_params(profile)?;
        let root = root_prefix(profile);
        let secret = self.broker.resolve(actor, credential_id(profile)?)?;
        let vfs = cairn_backend_object::azure_connect(conn, &params, &secret, &root).await?;
        Ok(Arc::new(vfs))
    }

    #[cfg(not(feature = "azure"))]
    async fn open_azure(
        &self,
        _actor: Actor,
        _conn: ConnectionId,
        _profile: &ConnectionProfile,
    ) -> Result<Arc<dyn Vfs>, OpenError> {
        Err(OpenError::BackendNotBuilt("azure".to_owned()))
    }

    // ---- Docker ------------------------------------------------------------

    /// Docker connects to the local engine (socket / named pipe / env) — no vault credential.
    #[cfg(feature = "docker")]
    async fn open_docker(&self, conn: ConnectionId) -> Result<Arc<dyn Vfs>, OpenError> {
        let docker = cairn_backend_docker::BollardDocker::connect_local()?;
        // See the matching call in `cairn::app::open_docker_socket`: start the ADR-0010
        // ephemeral-container reapers at real connection-open time, not deferred to first image
        // browse, so the crash-safety sweep begins reaping orphans as soon as this daemon is
        // talked to.
        docker.ensure_background_tasks().await;
        Ok(Arc::new(cairn_backend_docker::DockerVfs::new(conn, docker)))
    }

    #[cfg(not(feature = "docker"))]
    async fn open_docker(&self, _conn: ConnectionId) -> Result<Arc<dyn Vfs>, OpenError> {
        Err(OpenError::BackendNotBuilt("docker".to_owned()))
    }

    // ---- Kubernetes --------------------------------------------------------

    /// Kubernetes reads the kubeconfig (context resolved per call) — no vault credential.
    #[cfg(feature = "k8s")]
    async fn open_k8s(&self, conn: ConnectionId) -> Result<Arc<dyn Vfs>, OpenError> {
        Ok(Arc::new(cairn_backend_k8s::KubeVfs::new(
            conn,
            cairn_backend_k8s::KubeRsOps::new(),
        )))
    }

    #[cfg(not(feature = "k8s"))]
    async fn open_k8s(&self, _conn: ConnectionId) -> Result<Arc<dyn Vfs>, OpenError> {
        Err(OpenError::BackendNotBuilt("k8s".to_owned()))
    }
}

/// The credential id a credential-bearing profile must carry (its `secret_ref`).
///
/// [`CredentialId`](cairn_types::CredentialId) is just an opaque handle (a UUID); the secret itself
/// is resolved by the broker. Compiled only when a credential-bearing backend is enabled.
#[cfg(any(feature = "ssh", feature = "s3", feature = "gcs", feature = "azure"))]
fn credential_id(profile: &ConnectionProfile) -> Result<cairn_types::CredentialId, OpenError> {
    profile
        .secret_ref
        .ok_or_else(|| OpenError::MissingCredential(profile.display_name.clone()))
}

// ---------------------------------------------------------------------------
// Profile → parameters (one builder per scheme, gated by its feature)
// ---------------------------------------------------------------------------

/// Read a required endpoint field, or report which one is missing.
#[cfg(any(feature = "ssh", feature = "s3", feature = "gcs", feature = "azure"))]
fn required<'a>(profile: &'a ConnectionProfile, field: &str) -> Result<&'a str, OpenError> {
    profile
        .endpoint
        .get(field)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| OpenError::MissingField {
            profile: profile.display_name.clone(),
            field: field.to_owned(),
        })
}

/// Read an optional endpoint field (empty string is treated as absent).
#[cfg(any(feature = "ssh", feature = "s3", feature = "gcs", feature = "azure"))]
fn optional<'a>(profile: &'a ConnectionProfile, field: &str) -> Option<&'a str> {
    profile
        .endpoint
        .get(field)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

/// The object-store key prefix the backend is rooted at — `endpoint.root` (or `prefix`), default
/// `""` (the bucket/container root).
#[cfg(any(feature = "s3", feature = "gcs", feature = "azure"))]
fn root_prefix(profile: &ConnectionProfile) -> String {
    optional(profile, "root")
        .or_else(|| optional(profile, "prefix"))
        .unwrap_or("")
        .to_owned()
}

/// Parse a boolean endpoint field (`true`/`false`), or `default` when absent.
#[cfg(feature = "s3")]
fn optional_bool(
    profile: &ConnectionProfile,
    field: &str,
    default: bool,
) -> Result<bool, OpenError> {
    match optional(profile, field) {
        None => Ok(default),
        Some(v) => v.parse::<bool>().map_err(|_| OpenError::InvalidField {
            profile: profile.display_name.clone(),
            field: field.to_owned(),
            reason: "expected 'true' or 'false'".to_owned(),
        }),
    }
}

/// Build [`SshConnectParams`](cairn_backend_ssh::SshConnectParams) from a profile.
///
/// Reads `host` (required), `user` (required), `port` (default `22`), `known_hosts` (default
/// `~/.ssh/known_hosts`), and `host_key` policy — `accept-new` (TOFU, the default) or `strict`.
/// The credential is resolved separately by the broker. The connect/auth timeouts always use the
/// connector defaults (10 s / 30 s); per-profile timeout configuration is a follow-up.
#[cfg(feature = "ssh")]
fn ssh_params(
    profile: &ConnectionProfile,
) -> Result<cairn_backend_ssh::SshConnectParams, OpenError> {
    use cairn_backend_ssh::{HostKeyPolicy, SshConnectParams};

    let host = required(profile, "host")?;
    let user = required(profile, "user")?;
    let port = match optional(profile, "port") {
        None => 22,
        Some(p) => p.parse::<u16>().map_err(|_| OpenError::InvalidField {
            profile: profile.display_name.clone(),
            field: "port".to_owned(),
            reason: "expected a TCP port number".to_owned(),
        })?,
    };
    let known_hosts = optional(profile, "known_hosts")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_known_hosts);
    // TOFU by default so a first connection to a new host succeeds and records the key; `strict`
    // requires the key to already be present. (No "accept anything" mode exists — a changed key is
    // always rejected under both policies.)
    let host_key = match optional(profile, "host_key") {
        Some("strict") => HostKeyPolicy::Strict { known_hosts },
        None | Some("accept-new") => HostKeyPolicy::AcceptNew { known_hosts },
        Some(other) => {
            return Err(OpenError::InvalidField {
                profile: profile.display_name.clone(),
                field: "host_key".to_owned(),
                reason: format!("unknown policy '{other}' (expected 'strict' or 'accept-new')"),
            })
        }
    };
    Ok(SshConnectParams::new(host, port, user, host_key))
}

/// The default `known_hosts` path (`~/.ssh/known_hosts`), falling back to a bare `known_hosts` in
/// the working directory when no home directory is known.
#[cfg(feature = "ssh")]
fn default_known_hosts() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    match home {
        Some(h) => h.join(".ssh").join("known_hosts"),
        None => std::path::PathBuf::from("known_hosts"),
    }
}

/// Build [`S3ConnectParams`](cairn_backend_object::S3ConnectParams) from a profile.
///
/// Reads `bucket` (required), `region`, `endpoint` (for S3-compatible stores), and
/// `force_path_style` (default: on when a custom `endpoint` is set, matching MinIO and most
/// self-hosted stores).
#[cfg(feature = "s3")]
fn s3_params(
    profile: &ConnectionProfile,
) -> Result<cairn_backend_object::S3ConnectParams, OpenError> {
    use cairn_backend_object::S3ConnectParams;

    let bucket = required(profile, "bucket")?;
    let endpoint = optional(profile, "endpoint").map(ToOwned::to_owned);
    let force_path_style = optional_bool(profile, "force_path_style", endpoint.is_some())?;
    Ok(S3ConnectParams {
        bucket: bucket.to_owned(),
        region: optional(profile, "region").map(ToOwned::to_owned),
        endpoint,
        force_path_style,
    })
}

/// Build [`GcsConnectParams`](cairn_backend_object::GcsConnectParams) from a profile.
///
/// Reads `bucket` (required) and `endpoint` (for the `fake-gcs-server` emulator).
#[cfg(feature = "gcs")]
fn gcs_params(
    profile: &ConnectionProfile,
) -> Result<cairn_backend_object::GcsConnectParams, OpenError> {
    use cairn_backend_object::GcsConnectParams;

    let bucket = required(profile, "bucket")?;
    Ok(GcsConnectParams {
        bucket: bucket.to_owned(),
        endpoint: optional(profile, "endpoint").map(ToOwned::to_owned),
    })
}

/// Build [`AzureConnectParams`](cairn_backend_object::AzureConnectParams) from a profile.
///
/// Reads `account` (required), `container` (required), and `endpoint` (for Azurite or a custom
/// blob-service URL).
#[cfg(feature = "azure")]
fn azure_params(
    profile: &ConnectionProfile,
) -> Result<cairn_backend_object::AzureConnectParams, OpenError> {
    use cairn_backend_object::AzureConnectParams;

    let account = required(profile, "account")?;
    let container = required(profile, "container")?;
    Ok(AzureConnectParams {
        account: account.to_owned(),
        container: container.to_owned(),
        endpoint: optional(profile, "endpoint").map(ToOwned::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::ConnectionProfile;

    fn profile(scheme: &str, fields: &[(&str, &str)]) -> ConnectionProfile {
        let mut p = ConnectionProfile::new(scheme, "test-conn");
        for (k, v) in fields {
            p.endpoint.insert((*k).to_owned(), (*v).to_owned());
        }
        p
    }

    /// Open a profile expecting failure. (`Result::unwrap_err` is unusable here because the `Ok`
    /// type `Arc<dyn Vfs>` is not `Debug`.)
    async fn open_err(
        opener: &ConnectionOpener,
        conn: ConnectionId,
        profile: &ConnectionProfile,
    ) -> OpenError {
        match opener.open(Actor::User, conn, profile).await {
            Err(e) => e,
            Ok(_) => panic!("expected open() to fail"),
        }
    }

    // Dispatch is always testable, with or without backend features.

    #[test]
    fn open_error_debug_is_redacted_like_display() {
        // The `Vfs` variant must redact in `Debug` too (CLAUDE.md §9): a `{:?}` log of an
        // `OpenError` wrapping a path-bearing `VfsError` must not surface that path.
        let err = OpenError::Vfs(VfsError::NotFound(
            cairn_types::VfsPath::parse("/secret/dir").expect("test path"),
        ));
        let dbg = format!("{err:?}");
        assert_eq!(
            dbg,
            format!("{err}"),
            "Debug must mirror the redacted Display"
        );
        assert!(!dbg.contains("/secret/dir"), "Debug leaked the path: {dbg}");
    }

    #[tokio::test]
    async fn unknown_and_local_schemes_are_unsupported() {
        let opener = ConnectionOpener::new(Arc::new(Broker::locked()));
        // `local` is handled by the local backend, not the opener; an unknown scheme is rejected.
        // (Docker/K8s are known schemes — see the feature-gated test below.)
        for scheme in ["local", "ftp"] {
            let err = open_err(&opener, ConnectionId(9), &profile(scheme, &[])).await;
            assert!(
                matches!(err, OpenError::UnsupportedScheme(ref s) if s == scheme),
                "{scheme}: {err}"
            );
        }
    }

    /// Docker/K8s are recognized schemes; without their feature they report "not built in" (lean).
    #[cfg(not(feature = "docker"))]
    #[tokio::test]
    async fn docker_without_feature_is_not_built() {
        let opener = ConnectionOpener::new(Arc::new(Broker::locked()));
        let err = open_err(&opener, ConnectionId(9), &profile("docker", &[])).await;
        assert!(
            matches!(err, OpenError::BackendNotBuilt(ref s) if s == "docker"),
            "{err}"
        );
    }

    #[cfg(not(feature = "k8s"))]
    #[tokio::test]
    async fn k8s_without_feature_is_not_built() {
        let opener = ConnectionOpener::new(Arc::new(Broker::locked()));
        let err = open_err(&opener, ConnectionId(9), &profile("kubernetes", &[])).await;
        assert!(
            matches!(err, OpenError::BackendNotBuilt(ref s) if s == "k8s"),
            "{err}"
        );
    }

    // ---- lean build: every credential-bearing scheme reports "not built in" -------------------

    #[cfg(not(feature = "ssh"))]
    #[tokio::test]
    async fn ssh_without_feature_is_not_built() {
        let opener = ConnectionOpener::new(Arc::new(Broker::locked()));
        let err = open_err(&opener, ConnectionId(1), &profile("ssh", &[])).await;
        assert!(
            matches!(err, OpenError::BackendNotBuilt(ref s) if s == "ssh"),
            "{err}"
        );
    }

    #[cfg(not(feature = "s3"))]
    #[tokio::test]
    async fn s3_without_feature_is_not_built() {
        let opener = ConnectionOpener::new(Arc::new(Broker::locked()));
        let err = open_err(&opener, ConnectionId(1), &profile("s3", &[])).await;
        assert!(
            matches!(err, OpenError::BackendNotBuilt(ref s) if s == "s3"),
            "{err}"
        );
    }

    #[cfg(not(feature = "gcs"))]
    #[tokio::test]
    async fn gcs_without_feature_is_not_built() {
        let opener = ConnectionOpener::new(Arc::new(Broker::locked()));
        let err = open_err(&opener, ConnectionId(1), &profile("gcs", &[])).await;
        assert!(
            matches!(err, OpenError::BackendNotBuilt(ref s) if s == "gcs"),
            "{err}"
        );
    }

    #[cfg(not(feature = "azure"))]
    #[tokio::test]
    async fn azure_without_feature_is_not_built() {
        let opener = ConnectionOpener::new(Arc::new(Broker::locked()));
        let err = open_err(&opener, ConnectionId(1), &profile("azure", &[])).await;
        assert!(
            matches!(err, OpenError::BackendNotBuilt(ref s) if s == "azure"),
            "{err}"
        );
    }

    // ---- with features on: parameter construction + the resolve→dispatch credential path -------

    #[cfg(feature = "ssh")]
    #[test]
    fn ssh_params_reads_endpoint_fields() {
        use cairn_backend_ssh::HostKeyPolicy;
        let p = profile(
            "ssh",
            &[
                ("host", "bastion.example"),
                ("user", "deploy"),
                ("port", "2222"),
                ("known_hosts", "/etc/cairn/known_hosts"),
                ("host_key", "strict"),
            ],
        );
        let params = ssh_params(&p).unwrap();
        assert_eq!(params.host, "bastion.example");
        assert_eq!(params.user, "deploy");
        assert_eq!(params.port, 2222);
        match params.host_key {
            HostKeyPolicy::Strict { known_hosts } => {
                assert_eq!(known_hosts, std::path::Path::new("/etc/cairn/known_hosts"));
            }
            HostKeyPolicy::AcceptNew { .. } => panic!("expected the strict policy"),
        }
    }

    #[cfg(feature = "ssh")]
    #[test]
    fn ssh_params_defaults_port_and_policy() {
        use cairn_backend_ssh::HostKeyPolicy;
        let p = profile("ssh", &[("host", "h"), ("user", "u")]);
        let params = ssh_params(&p).unwrap();
        assert_eq!(params.port, 22);
        assert!(matches!(params.host_key, HostKeyPolicy::AcceptNew { .. }));
    }

    #[cfg(feature = "ssh")]
    #[test]
    fn ssh_params_requires_host_and_user() {
        assert!(matches!(
            ssh_params(&profile("ssh", &[("user", "u")])),
            Err(OpenError::MissingField { field, .. }) if field == "host"
        ));
        assert!(matches!(
            ssh_params(&profile("ssh", &[("host", "h")])),
            Err(OpenError::MissingField { field, .. }) if field == "user"
        ));
        assert!(matches!(
            ssh_params(&profile("ssh", &[("host", "h"), ("user", "u"), ("port", "nope")])),
            Err(OpenError::InvalidField { field, .. }) if field == "port"
        ));
        // An unrecognised host-key policy is a config error, not a silent default.
        assert!(matches!(
            ssh_params(&profile("ssh", &[("host", "h"), ("user", "u"), ("host_key", "yolo")])),
            Err(OpenError::InvalidField { field, .. }) if field == "host_key"
        ));
    }

    #[cfg(feature = "s3")]
    #[test]
    fn s3_params_reads_fields_and_path_style_defaults_with_endpoint() {
        // A genuine AWS bucket: no endpoint, path-style off.
        let aws = s3_params(&profile(
            "s3",
            &[("bucket", "prod"), ("region", "eu-west-1")],
        ))
        .unwrap();
        assert_eq!(aws.bucket, "prod");
        assert_eq!(aws.region.as_deref(), Some("eu-west-1"));
        assert!(aws.endpoint.is_none());
        assert!(!aws.force_path_style);
        // A MinIO-style endpoint flips force_path_style on by default.
        let minio = s3_params(&profile(
            "s3",
            &[("bucket", "b"), ("endpoint", "http://localhost:9000")],
        ))
        .unwrap();
        assert_eq!(minio.endpoint.as_deref(), Some("http://localhost:9000"));
        assert!(minio.force_path_style);
        // And requires a bucket.
        assert!(matches!(
            s3_params(&profile("s3", &[])),
            Err(OpenError::MissingField { field, .. }) if field == "bucket"
        ));
        // A non-boolean force_path_style is rejected rather than silently treated as false.
        assert!(matches!(
            s3_params(&profile("s3", &[("bucket", "b"), ("force_path_style", "yes")])),
            Err(OpenError::InvalidField { field, .. }) if field == "force_path_style"
        ));
    }

    #[cfg(feature = "s3")]
    #[test]
    fn root_prefix_defaults_empty_and_reads_root_or_prefix() {
        assert_eq!(root_prefix(&profile("s3", &[("bucket", "b")])), "");
        assert_eq!(
            root_prefix(&profile("s3", &[("bucket", "b"), ("root", "logs/")])),
            "logs/"
        );
        assert_eq!(
            root_prefix(&profile("s3", &[("bucket", "b"), ("prefix", "data/")])),
            "data/"
        );
        // When both are present, `root` wins (documents the precedence contract).
        assert_eq!(
            root_prefix(&profile(
                "s3",
                &[("bucket", "b"), ("root", "r/"), ("prefix", "p/")]
            )),
            "r/"
        );
    }

    #[cfg(feature = "gcs")]
    #[test]
    fn gcs_params_require_bucket() {
        let ok = gcs_params(&profile("gcs", &[("bucket", "ml-data")])).unwrap();
        assert_eq!(ok.bucket, "ml-data");
        assert!(ok.endpoint.is_none());
        assert!(matches!(
            gcs_params(&profile("gcs", &[])),
            Err(OpenError::MissingField { field, .. }) if field == "bucket"
        ));
    }

    #[cfg(feature = "azure")]
    #[test]
    fn azure_params_require_account_and_container() {
        let ok = azure_params(&profile(
            "azure",
            &[("account", "acct"), ("container", "backups")],
        ))
        .unwrap();
        assert_eq!(ok.account, "acct");
        assert_eq!(ok.container, "backups");
        assert!(matches!(
            azure_params(&profile("azure", &[("account", "acct")])),
            Err(OpenError::MissingField { field, .. }) if field == "container"
        ));
    }

    // The credential resolve → dispatch path is unit-testable without a network because every
    // connector rejects a mismatched credential kind with `VfsError::Auth` *before* any I/O.
    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn s3_profile_with_an_ssh_credential_is_rejected_as_auth() {
        use cairn_secrets::SecretString;
        use cairn_vault::{CredentialSecret, KdfParams, SshCredential, Vault};

        let dir = tempfile::tempdir().unwrap();
        let mut vault = Vault::create_with_params(
            dir.path().join("v"),
            &SecretString::from("pw".to_owned()),
            KdfParams::fast_for_tests(),
        )
        .unwrap();
        // A wrong-family credential (SSH) referenced by an S3 profile.
        let id = vault.add(
            "ssh-cred",
            CredentialSecret::Ssh(SshCredential::Password(SecretString::from(
                "hunter2".to_owned(),
            ))),
        );
        let opener = ConnectionOpener::new(Arc::new(Broker::new(vault)));

        let mut p = profile("s3", &[("bucket", "prod")]);
        p.secret_ref = Some(id);
        let err = open_err(&opener, ConnectionId(5), &p).await;
        // Proves the full chain: scheme dispatch → broker.resolve → connector pre-network check.
        assert!(matches!(err, OpenError::Vfs(VfsError::Auth)), "{err}");
    }

    // A credential-bearing scheme with no secret_ref is reported before any broker work.
    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn s3_profile_without_a_credential_reference_is_rejected() {
        let opener = ConnectionOpener::new(Arc::new(Broker::locked()));
        let err = open_err(&opener, ConnectionId(5), &profile("s3", &[("bucket", "b")])).await;
        assert!(matches!(err, OpenError::MissingCredential(_)), "{err}");
    }

    /// Every `OpenError` variant, when formatted via `"{scheme}: {e}"` as
    /// `run_open_connection_effect` does, must not expose a raw host address or credential value
    /// in the string pushed to the UI status line.
    ///
    /// For `Vfs` variants the critical mechanism is [`VfsError::redacted`] — it strips
    /// path/host/source detail and emits only the error category.  For `Broker` variants the
    /// `Display` strings are static category labels ("vault is locked", "credential not found")
    /// with no dynamic data.  For the remaining variants the embedded fields are metadata (scheme
    /// name, profile display-name, endpoint key name), not endpoint values or secrets; the
    /// assertions here document that no stray endpoint data is interpolated.
    #[test]
    fn open_error_display_does_not_leak_host_or_credential() {
        // Sentinel values embedded in error internals; neither must appear in the surfaced string.
        const HOST: &str = "bastion.corp.example";
        const CRED: &str = "sEcrEt-p@ssw0rd-42";
        const SCHEME: &str = "ssh";

        macro_rules! check {
            ($label:expr, $e:expr) => {{
                let surfaced = format!("{SCHEME}: {}", $e);
                assert!(
                    !surfaced.contains(HOST),
                    "{}: surfaced string exposed HOST in: {:?}",
                    $label,
                    surfaced
                );
                assert!(
                    !surfaced.contains(CRED),
                    "{}: surfaced string exposed CRED in: {:?}",
                    $label,
                    surfaced
                );
            }};
        }

        // ── Vfs: the critical cases — VfsError::redacted() must strip all inner detail ─────────

        // NotFound path encodes the sentinel host — must redact to "not found".
        check!(
            "Vfs::NotFound",
            OpenError::Vfs(VfsError::NotFound(
                cairn_types::VfsPath::parse(&format!("/{HOST}/data")).expect("test path"),
            ))
        );

        // Forbidden path encodes the sentinel credential — must redact to "permission denied".
        check!(
            "Vfs::Forbidden",
            OpenError::Vfs(VfsError::Forbidden(
                cairn_types::VfsPath::parse(&format!("/{CRED}")).expect("test path"),
            ))
        );

        // Connection source error embeds the hostname — must redact to "connection failed".
        check!(
            "Vfs::Connection",
            OpenError::Vfs(VfsError::Connection(Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                HOST,
            ))))
        );

        // Backend msg embeds a credential-like string — only the stable code must surface.
        check!(
            "Vfs::Backend",
            OpenError::Vfs(VfsError::Backend {
                code: "AccessDenied".to_owned(),
                msg: format!("access denied for key '{CRED}' on host {HOST}"),
                retryable: false,
            })
        );

        // ── Broker: static category strings only — no dynamic host or credential data ──────────
        check!("Broker::Locked", OpenError::Broker(BrokerError::Locked));
        check!("Broker::NotFound", OpenError::Broker(BrokerError::NotFound));

        // ── Metadata variants: scheme name / display-name / endpoint key name — not values ──────
        // BackendNotBuilt and UnsupportedScheme carry only the scheme identifier (not a host).
        check!(
            "BackendNotBuilt",
            OpenError::BackendNotBuilt("ftp".to_owned())
        );
        check!(
            "UnsupportedScheme",
            OpenError::UnsupportedScheme("unknown".to_owned())
        );
        // MissingField/InvalidField carry the profile display-name and endpoint key name ("host",
        // "port") — neither is the actual host value or a credential.
        check!(
            "MissingField",
            OpenError::MissingField {
                profile: "dev-bastion".to_owned(),
                field: "host".to_owned(),
            }
        );
        check!(
            "InvalidField",
            OpenError::InvalidField {
                profile: "prod-s3".to_owned(),
                field: "port".to_owned(),
                reason: "not a number".to_owned(),
            }
        );
        // MissingCredential carries the display-name, not the credential value itself.
        check!(
            "MissingCredential",
            OpenError::MissingCredential("dev-bastion".to_owned())
        );
    }
}
