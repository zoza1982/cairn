//! The [`ConnectionProvider`] trait and the Phase P1 built-in providers (RFC-0011 §2).
//!
//! Providers enumerate connections from a single, homogeneous source and return a flat list of
//! [`ConnectionDescriptor`]s. They are **enumerate-only**: a `discover` call may do cheap,
//! offline-safe I/O (read env vars, check a config structure), but must NOT open backends over
//! the network, run credential plugins, or block startup for more than a millisecond or two.
//! Opening happens in the [`ConnectionCoordinator`](super::coordinator::ConnectionCoordinator).
//!
//! Phase P1 defines two providers:
//! - [`BuiltinLocalProvider`] — emits descriptors for `/` and `$HOME`.
//! - [`SavedProfileProvider`] — emits descriptors for every entry in `Config::connections`,
//!   local profiles first (in config order), then credential-bearing ones (also in config order).
//!   This ordering preserves the id-assignment contract of the original `register_connections`.

use std::path::PathBuf;

use async_trait::async_trait;
use cairn_config::Config;
use cairn_types::ConnectionId;

use super::descriptor::{
    ConnectionDescriptor, ConnectionKey, DescriptorProvenance, OpenTarget, Reachability,
};

/// Placeholder [`ConnectionId`] assigned to every freshly-discovered descriptor.
///
/// The coordinator always overwrites this before inserting the descriptor into the registry or
/// the side-map. Seeing this id in a live registry would indicate a coordinator bug. Named so
/// that construction sites are self-documenting rather than using the opaque literal `0`.
const UNASSIGNED: ConnectionId = ConnectionId(0);

/// Context passed to every [`ConnectionProvider::discover`] call.
pub(crate) struct DiscoveryCtx<'a> {
    /// The current user configuration (connection profiles, future discovery opt-outs).
    pub(crate) config: &'a Config,
    /// Whether the vault broker was locked at the time enumeration started.
    ///
    /// Used by [`SavedProfileProvider`] to classify credential-bearing profiles as
    /// [`Reachability::NeedsVault`] so the coordinator defers rather than eagerly opening them.
    pub(crate) vault_locked: bool,
}

/// A source of [`ConnectionDescriptor`]s.
///
/// Each provider is responsible for one homogeneous source of connections (built-in roots,
/// saved config profiles, or — in Phase P3 — the Docker socket or kubeconfig). The coordinator
/// runs all registered providers in order and merges their output.
///
/// # Contract
///
/// Implementations MUST be offline-safe and fast. They may read env vars, inspect a config
/// struct, or check local socket existence. They must NOT:
/// - Make network connections.
/// - Run credential plugins (`exec` in kubeconfig).
/// - Block the async runtime with long CPU or I/O work (use `spawn_blocking` if needed).
#[async_trait]
pub(crate) trait ConnectionProvider: Send + Sync {
    /// A short, stable, human-readable name for this provider.
    ///
    /// Used as a log tag and (in future phases) as the `ConnectionKey` source prefix.
    /// Examples: `"builtin"`, `"saved"`, `"docker"`, `"kubeconfig"`.
    // P2: the coordinator will call this to tag each descriptor's origin for re-enumeration diffing.
    #[allow(dead_code)]
    fn source_id(&self) -> &'static str;

    /// Enumerate connections from this source and return their descriptors.
    ///
    /// The returned descriptors have placeholder [`ConnectionId`]s ([`UNASSIGNED`]); the
    /// coordinator assigns real ids after merging all providers' output.
    async fn discover(&self, ctx: &DiscoveryCtx<'_>) -> Vec<ConnectionDescriptor>;
}

// ---------------------------------------------------------------------------
// BuiltinLocalProvider
// ---------------------------------------------------------------------------

/// Emits descriptors for the built-in local roots: `/` (always) and `$HOME` (when set).
///
/// These are the two roots that the original `register_connections` hard-coded before
/// processing saved profiles. The ordering (`/` first, then `$HOME`) is preserved so the
/// coordinator assigns the same [`ConnectionId`]s as the original function.
pub(crate) struct BuiltinLocalProvider;

#[async_trait]
impl ConnectionProvider for BuiltinLocalProvider {
    fn source_id(&self) -> &'static str {
        "builtin"
    }

    async fn discover(&self, _ctx: &DiscoveryCtx<'_>) -> Vec<ConnectionDescriptor> {
        let mut out = Vec::new();

        // The filesystem root is always present. On Unix this is `/`; on Windows it would be
        // the drive root derived from `current_dir()` — but `register_connections` hard-codes
        // `PathBuf::from("/")`, so we match that for behavioral equivalence in P1.
        let root = PathBuf::from("/");
        out.push(ConnectionDescriptor {
            id: UNASSIGNED, // placeholder; coordinator assigns real ids
            key: ConnectionKey::Builtin(root.clone()),
            provenance: DescriptorProvenance::Builtin,
            scheme: "local".to_owned(),
            display_name: "local: /".to_owned(),
            target: OpenTarget::LocalRoot(root),
            reachability: Reachability::Ready,
        });

        // `$HOME` is optional — only emit it when the variable is set and non-empty,
        // matching the original `register_connections` filter.
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            let home_path = PathBuf::from(&home);
            let label = format!("local: {}", home_path.to_string_lossy());
            out.push(ConnectionDescriptor {
                id: UNASSIGNED,
                key: ConnectionKey::Builtin(home_path.clone()),
                provenance: DescriptorProvenance::Builtin,
                scheme: "local".to_owned(),
                display_name: label,
                target: OpenTarget::LocalRoot(home_path),
                reachability: Reachability::Ready,
            });
        }

        out
    }
}

// ---------------------------------------------------------------------------
// SavedProfileProvider
// ---------------------------------------------------------------------------

/// Emits descriptors for every entry in [`Config::connections`].
///
/// Returns local profiles (`scheme = "local"`) first (in config order), then
/// credential-bearing profiles (also in config order). This two-pass ordering is critical:
/// it replicates the id-assignment contract of the original `register_connections`, where
/// local profiles were processed in the "targets" group (together with builtin roots) and
/// credential-bearing profiles in a separate trailing loop. The coordinator relies on this
/// ordering to assign the same [`ConnectionId`]s as the original function.
///
/// Local profiles with a missing or empty `endpoint.path` are silently skipped (with a warning
/// log), matching the original behavior.
pub(crate) struct SavedProfileProvider;

#[async_trait]
impl ConnectionProvider for SavedProfileProvider {
    fn source_id(&self) -> &'static str {
        "saved"
    }

    async fn discover(&self, ctx: &DiscoveryCtx<'_>) -> Vec<ConnectionDescriptor> {
        let mut out = Vec::new();

        // ── Pass 1: local profiles ────────────────────────────────────────────────────────────
        // Processed first so they receive ids in the same group as builtin roots (matching the
        // original "targets" array that was built before the credential-bearing loop).
        for prof in &ctx.config.connections {
            if prof.scheme != "local" {
                continue;
            }
            match prof.endpoint.get("path") {
                Some(path) if !path.is_empty() => {
                    let fs_path = PathBuf::from(path);
                    out.push(ConnectionDescriptor {
                        id: UNASSIGNED,
                        key: ConnectionKey::Saved(prof.id),
                        provenance: DescriptorProvenance::Saved {
                            profile_id: prof.id,
                        },
                        scheme: "local".to_owned(),
                        display_name: format!("local: {}", prof.display_name),
                        target: OpenTarget::LocalRoot(fs_path),
                        reachability: Reachability::Ready,
                    });
                }
                // Missing or empty path: skip with a warning, matching the original behavior.
                _ => {
                    tracing::warn!(
                        name = %prof.display_name,
                        "skipping local connection profile: missing endpoint.path"
                    );
                }
            }
        }

        // ── Pass 2: credential-bearing (non-local) profiles ───────────────────────────────────
        // These are processed in the trailing loop of the original function.  Docker and K8s
        // profiles connect without a vault credential (`secret_ref = None`) so they are always
        // `Reachability::Ready`; SSH/S3/GCS/Azure require a vault credential and are classified
        // `NeedsVault` when the vault is locked.
        for prof in &ctx.config.connections {
            if prof.scheme == "local" {
                continue;
            }
            // A credential-bearing profile that has no `secret_ref` (misconfigured) is still
            // classified Ready — the coordinator will call `opener.open`, which will fail with
            // `MissingCredential` (not `BrokerError::Locked`), so it is logged and skipped.
            // This matches the original: the id was consumed and the profile was dropped with a
            // warning.
            let reachability = if ctx.vault_locked && prof.secret_ref.is_some() {
                Reachability::NeedsVault
            } else {
                Reachability::Ready
            };
            out.push(ConnectionDescriptor {
                id: UNASSIGNED,
                key: ConnectionKey::Saved(prof.id),
                provenance: DescriptorProvenance::Saved {
                    profile_id: prof.id,
                },
                scheme: prof.scheme.clone(),
                display_name: format!("{}: {}", prof.scheme, prof.display_name),
                target: OpenTarget::Profile(prof.clone()),
                reachability,
            });
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::{Config, ConnectionProfile};
    use uuid::Uuid;

    fn ctx(config: &Config, vault_locked: bool) -> DiscoveryCtx<'_> {
        DiscoveryCtx {
            config,
            vault_locked,
        }
    }

    // ── BuiltinLocalProvider ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn builtin_provider_always_includes_filesystem_root() {
        let config = Config::default();
        let descs = BuiltinLocalProvider.discover(&ctx(&config, true)).await;

        let has_root = descs.iter().any(|d| match &d.target {
            OpenTarget::LocalRoot(p) => p == std::path::Path::new("/"),
            OpenTarget::Profile(_) => false,
        });
        assert!(has_root, "/ must always be in builtin descriptors");
    }

    #[tokio::test]
    async fn builtin_provider_emits_one_or_two_roots() {
        let config = Config::default();
        let descs = BuiltinLocalProvider.discover(&ctx(&config, true)).await;

        // 1 root when HOME is unset, 2 roots when HOME is set. Both are valid in CI.
        assert!(
            descs.len() == 1 || descs.len() == 2,
            "builtin provider emits 1 (no HOME) or 2 (with HOME) roots, got {}",
            descs.len()
        );
        for d in &descs {
            assert_eq!(d.provenance, DescriptorProvenance::Builtin);
            assert_eq!(d.reachability, Reachability::Ready);
            assert_eq!(d.scheme, "local");
        }
    }

    // ── SavedProfileProvider ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn saved_provider_maps_profiles_one_to_one() {
        let mut config = Config::default();
        // A local profile with a valid path.
        let mut local = ConnectionProfile::new("local", "my-dir");
        local.endpoint.insert("path".into(), "/some/path".into());
        config.connections.push(local);
        // A credential-bearing profile (no secret_ref; vault state doesn't matter here).
        config
            .connections
            .push(ConnectionProfile::new("ssh", "bastion"));

        let descs = SavedProfileProvider.discover(&ctx(&config, false)).await;
        assert_eq!(descs.len(), 2, "one descriptor per profile");
        // Local profiles come first (Pass 1), then credential-bearing (Pass 2).
        assert_eq!(descs[0].scheme, "local", "local profile first");
        assert_eq!(descs[1].scheme, "ssh", "ssh profile second");
    }

    #[tokio::test]
    async fn saved_provider_local_profile_missing_path_is_skipped() {
        let mut config = Config::default();
        // A local profile with no endpoint.path — should be skipped silently.
        config
            .connections
            .push(ConnectionProfile::new("local", "broken"));

        let descs = SavedProfileProvider.discover(&ctx(&config, true)).await;
        assert!(
            descs.is_empty(),
            "local profile with no path must produce no descriptor"
        );
    }

    #[tokio::test]
    async fn saved_provider_credential_bearing_with_vault_locked_is_needs_vault() {
        let mut config = Config::default();
        let mut prof = ConnectionProfile::new("ssh", "bastion");
        prof.secret_ref = Some(Uuid::new_v4()); // has a credential reference
        config.connections.push(prof);

        let descs = SavedProfileProvider.discover(&ctx(&config, true)).await;
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].reachability, Reachability::NeedsVault);
    }

    #[tokio::test]
    async fn saved_provider_credential_bearing_without_vault_lock_is_ready() {
        let mut config = Config::default();
        let mut prof = ConnectionProfile::new("ssh", "bastion");
        prof.secret_ref = Some(Uuid::new_v4());
        config.connections.push(prof);

        // Vault is unlocked → opener will attempt to open; classify as Ready.
        let descs = SavedProfileProvider.discover(&ctx(&config, false)).await;
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].reachability, Reachability::Ready);
    }

    #[tokio::test]
    async fn saved_provider_docker_no_secret_ref_is_always_ready() {
        // Docker connects without a vault credential, so it must be Ready even when vault locked.
        let mut config = Config::default();
        config
            .connections
            .push(ConnectionProfile::new("docker", "local-docker"));

        let descs = SavedProfileProvider.discover(&ctx(&config, true)).await;
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].reachability, Reachability::Ready);
    }

    #[tokio::test]
    async fn saved_provider_preserves_profile_kind() {
        let mut config = Config::default();
        let mut local = ConnectionProfile::new("local", "x");
        local.endpoint.insert("path".into(), "/x".into());
        let local_id = local.id;
        config.connections.push(local);

        let descs = SavedProfileProvider.discover(&ctx(&config, true)).await;
        assert_eq!(descs.len(), 1);
        assert_eq!(
            descs[0].provenance,
            DescriptorProvenance::Saved {
                profile_id: local_id
            }
        );
        assert!(
            matches!(&descs[0].key, ConnectionKey::Saved(id) if *id == local_id),
            "key must be Saved with the profile's UUID"
        );
    }

    #[tokio::test]
    async fn saved_provider_local_profile_empty_path_is_skipped() {
        // Deliberate behavior fix over the original `register_connections`: an empty-string
        // `endpoint.path` is now treated identically to a missing key — the entry is skipped
        // with a warning rather than mounting a broken `LocalVfs` rooted at an empty path and
        // consuming a `ConnectionId`. This ensures that a later credential-bearing profile in
        // the same config is NOT id-shifted by the empty-path entry.
        let mut config = Config::default();
        // A local profile with an explicitly empty path.
        let mut empty_path = ConnectionProfile::new("local", "broken-empty");
        empty_path.endpoint.insert("path".into(), "".into());
        config.connections.push(empty_path);
        // A credential-bearing profile that follows — its position in the output must not shift.
        let mut remote = ConnectionProfile::new("ssh", "bastion");
        remote.secret_ref = Some(Uuid::new_v4());
        config.connections.push(remote);

        let descs = SavedProfileProvider.discover(&ctx(&config, true)).await;

        // The empty-path local entry must not appear.
        assert!(
            descs.iter().all(|d| d.scheme != "local"),
            "local profile with empty endpoint.path must produce no descriptor"
        );
        // The following ssh profile still emits exactly one descriptor at index 0.
        assert_eq!(descs.len(), 1, "only the ssh profile should appear");
        assert_eq!(descs[0].scheme, "ssh");
    }
}
