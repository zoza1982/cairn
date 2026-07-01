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
/// - Docker / Kubeconfig: future, see RFC-0011 §3–§4.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ConnectionKey {
    /// A built-in local root, identified by its filesystem path.
    Builtin(PathBuf),
    /// A user-saved profile, identified by its stable UUID.
    Saved(Uuid),
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
