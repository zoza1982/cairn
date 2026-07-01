//! Runtime-side connection descriptors (RFC-0011 §1).
//!
//! These types are the authoritative binary-side representation of every connection the
//! [`ConnectionCoordinator`](super::coordinator::ConnectionCoordinator) enumerates. They live
//! binary-side only — they may carry [`ConnectionProfile`] references and other non-secret
//! open instructions, but they never cross the `cairn-core` boundary. The coordinator stores
//! them in a side-map keyed by [`ConnectionId`] that lives in the runtime alongside the other
//! control maps.
//!
//! The pure-core projection ([`ConnectionChoice`](cairn_core::ConnectionChoice)) is derived from
//! the descriptor; the reducer only ever sees the projection.

use std::path::PathBuf;

use cairn_config::ConnectionProfile;
use cairn_core::DiscoverySource;
use cairn_types::ConnectionId;
use uuid::Uuid;

/// A stable, content-derived identity for a connection across enumeration rounds.
///
/// Used by the coordinator to detect whether a connection from a previous enumeration is the
/// same logical connection (e.g. after a config reload), preserving live [`ConnectionId`]s and
/// avoiding a pane repoint on re-enumeration. The string forms are:
/// - Built-in: `"builtin:<absolute-path>"` (e.g. `"builtin:/"`)
/// - Saved: `"saved:<uuid>"` (the profile's stable UUID)
/// - Docker socket: `"docker:socket:<path>"` (e.g. `"docker:socket:default"` for the platform
///   default, or `"docker:socket:/run/user/1000/docker.sock"` for an explicit rootless socket)
/// - Kubeconfig: `"kube:kubeconfig"` (the merged kubeconfig from `$KUBECONFIG` / `~/.kube/config`)
/// - In-cluster: `"kube:in-cluster"` (the pod's service-account credentials)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ConnectionKey {
    /// A built-in local root, identified by its filesystem path.
    Builtin(PathBuf),
    /// A user-saved profile, identified by its stable UUID.
    Saved(Uuid),
    /// An auto-discovered Docker daemon socket.
    ///
    /// `socket_path` is `"default"` when the platform-default socket was probed via
    /// [`BollardDocker::connect_local`], or the raw path string for explicit rootless /
    /// Podman sockets.
    ///
    /// Constructed by `DockerProvider` — only available with the `docker` feature. The variant
    /// must exist in all build configurations for match exhaustiveness in `as_key_str`.
    #[cfg_attr(not(feature = "docker"), allow(dead_code))]
    Docker {
        /// The socket path key: `"default"` for the platform default, or the explicit path.
        socket_path: String,
    },
    /// The Kubernetes cluster reachable via the merged kubeconfig
    /// (`$KUBECONFIG` / `~/.kube/config`).
    ///
    /// Constructed by `KubeconfigProvider` — only available with the `k8s` feature.
    #[cfg_attr(not(feature = "k8s"), allow(dead_code))]
    Kubeconfig,
    /// The Kubernetes cluster reached via the pod's in-cluster service-account credentials.
    ///
    /// Constructed by `KubeconfigProvider` — only available with the `k8s` feature.
    #[cfg_attr(not(feature = "k8s"), allow(dead_code))]
    InCluster,
}

impl ConnectionKey {
    /// Stable string representation used for hidden/pinned config matching.
    ///
    /// Forms:
    /// - `"builtin:<path>"`, `"saved:<uuid>"`
    /// - `"docker:socket:<socket_path>"` (e.g. `"docker:socket:default"`)
    /// - `"kube:kubeconfig"`, `"kube:in-cluster"`
    pub(crate) fn as_key_str(&self) -> String {
        match self {
            Self::Builtin(p) => format!("builtin:{}", p.display()),
            Self::Saved(u) => format!("saved:{u}"),
            Self::Docker { socket_path } => format!("docker:socket:{socket_path}"),
            Self::Kubeconfig => "kube:kubeconfig".to_owned(),
            Self::InCluster => "kube:in-cluster".to_owned(),
        }
    }
}

/// What the coordinator will open when a connection is selected or eagerly mounted at startup.
///
/// Carries only **non-secret** open instructions; secrets are resolved through the broker at
/// open time, exactly as [`ConnectionOpener::open`](super::ConnectionOpener::open) does today.
#[derive(Debug, Clone)]
pub(crate) enum OpenTarget {
    /// Open a `LocalVfs` at the given filesystem root.
    LocalRoot(PathBuf),
    /// Open a backend via [`ConnectionOpener::open`](super::ConnectionOpener::open) using this
    /// profile. The profile carries no secret material; the broker resolves the credential at
    /// open time.
    Profile(ConnectionProfile),
    /// Open a Docker backend at the given Unix socket path.
    ///
    /// `path = None` means use the platform-default socket (equivalent to
    /// [`BollardDocker::connect_local`]). `path = Some(p)` connects to the explicit socket at `p`
    /// (rootless Docker or Podman socket discovered by [`DockerProvider`]).
    ///
    /// Constructed by `DockerProvider` — only available with the `docker` feature. The variant
    /// must exist in all build configurations for match exhaustiveness in `run_open_connection_effect`.
    #[cfg_attr(not(feature = "docker"), allow(dead_code))]
    DockerSocket {
        /// The explicit socket path, or `None` for the platform default.
        path: Option<PathBuf>,
    },
    /// Open a Kubernetes backend using the kubeconfig file resolved at open time
    /// (`$KUBECONFIG` / `~/.kube/config`).
    ///
    /// Constructed by `KubeconfigProvider` — only available with the `k8s` feature.
    #[cfg_attr(not(feature = "k8s"), allow(dead_code))]
    KubeconfigDefault,
    /// Open a Kubernetes backend using the pod's in-cluster service-account credentials.
    ///
    /// Requires `KUBERNETES_SERVICE_HOST` to be set and a readable SA token at the standard path.
    ///
    /// Constructed by `KubeconfigProvider` — only available with the `k8s` feature.
    #[cfg_attr(not(feature = "k8s"), allow(dead_code))]
    InCluster,
}

/// Reachability as assessed at enumeration time (without actually opening the connection).
///
/// The coordinator sets this during the `discover` phase, before any open attempt. It drives
/// the [`ChoiceStatus`](cairn_core::ChoiceStatus) of the resulting
/// [`ConnectionChoice`](cairn_core::ConnectionChoice) and the coordinator's open/defer decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reachability {
    /// The connection can be opened immediately (local root or credential-free profile).
    Ready,
    /// Opening requires vault unlock; the vault was locked at enumeration time.
    NeedsVault,
}

/// The full runtime descriptor for one connection.
///
/// Held binary-side in the coordinator's side-map (`HashMap<ConnectionId, ConnectionDescriptor>`
/// in [`event_loop`](crate::app)). The pure-core
/// [`ConnectionChoice`](cairn_core::ConnectionChoice) is derived from this by
/// `coordinator::core_projection` (module-private; called inside
/// [`ConnectionCoordinator::run`](super::coordinator::ConnectionCoordinator::run)). The
/// descriptor is the single source of truth for the P2 open effect and future re-enumeration
/// diffing.
#[derive(Debug, Clone)]
pub(crate) struct ConnectionDescriptor {
    /// The stable [`ConnectionId`] assigned by the coordinator for this enumeration round.
    pub(crate) id: ConnectionId,
    /// A content-derived key that identifies this connection across re-enumeration rounds.
    // Used by the coordinator's diff-and-preserve logic on re-enumeration.
    pub(crate) key: ConnectionKey,
    /// How this connection entered the list.
    pub(crate) provenance: DescriptorProvenance,
    /// The URI scheme string (e.g. `"local"`, `"ssh"`, `"s3"`).
    // Read by the coordinator's log calls and future P3/P4 re-enumeration diffing.
    #[allow(dead_code)]
    pub(crate) scheme: String,
    /// Human-readable display name shown in the switcher.
    pub(crate) display_name: String,
    /// What to open when this connection is selected or eagerly mounted.
    pub(crate) target: OpenTarget,
    /// Reachability assessed at enumeration time (without opening the connection).
    pub(crate) reachability: Reachability,
}

/// How a [`ConnectionDescriptor`] entered the runtime list.
///
/// Carries richer data than the pure-core [`ChoiceProvenance`](cairn_core::ChoiceProvenance):
/// the `Saved` variant includes the profile UUID so the P2 open effect and P4 edit flow can
/// look up the profile without scanning the config list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DescriptorProvenance {
    /// A built-in local root (`/`, `$HOME`).
    Builtin,
    /// A user-saved profile with the given stable UUID.
    Saved {
        /// The profile's stable UUID (`ConnectionProfile::id` from `cairn-config`).
        profile_id: Uuid,
    },
    /// Auto-discovered from the environment (Docker socket, kubeconfig, …).
    // P2+: constructed by the Docker / Kubeconfig providers when those are added.
    #[allow(dead_code)]
    Discovered {
        /// The discovery mechanism that surfaced this connection.
        source: DiscoverySource,
    },
}
