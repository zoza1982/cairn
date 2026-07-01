//! The [`ConnectionCoordinator`] — the RFC-0011 P1 replacement for `register_connections`.
//!
//! The coordinator runs the registered [`ConnectionProvider`]s in order, assigns stable
//! [`ConnectionId`]s (preserving the contract of the original `register_connections`), mounts
//! local/credential-free connections immediately, defers vault-locked ones for retry after
//! vault unlock, and returns:
//!
//! 1. `Vec<ConnectionChoice>` — the switcher entries for `AppState::connections`.
//! 2. `Vec<DeferredConnection>` — the vault-locked profiles held in `VaultContext` (in `app`).
//! 3. `HashMap<ConnectionId, ConnectionDescriptor>` — the runtime side-map, established here for
//!    P2 use (unused by reducer logic in P1).
//!
//! ## Id-assignment contract (P1 → P2 migration note)
//!
//! **P1 behavior:** ids are assigned by sequential counter (`base_id + i`) in provider-output
//! order. Since [`BuiltinLocalProvider`] emits `/` then `$HOME`, and [`SavedProfileProvider`]
//! emits local profiles first then credential-bearing profiles, the assignment matches the
//! original `register_connections`:
//!
//! ```text
//! id = base+0  →  builtin "/"
//! id = base+1  →  builtin "$HOME"  (when HOME is set)
//! id = base+N  →  first local config profile  (N = num builtin roots)
//! id = base+M  →  first credential-bearing config profile
//! ```
//!
//! **P2 MUST change this to key-based stable-id reuse.** On config reload or re-enumeration,
//! P2 should look up the existing [`ConnectionId`] for each `ConnectionKey` already live in
//! a pane and reuse it — minting a fresh id only for genuinely new keys. Without this, a config
//! reload will silently repoint open panes to new ids, breaking any in-flight operation.
//!
//! **Descriptor-map contract:** `descriptors.insert(id, desc)` runs for every descriptor,
//! including those whose open failed with a non-deferrable error (e.g. `BackendNotBuilt` in the
//! lean build). The map therefore contains ids with no corresponding choice or registry entry.
//! P2 must account for this when deciding whether an id is "live".

use std::collections::HashMap;
use std::sync::Arc;

use cairn_broker::{Actor, BrokerError};
use cairn_core::{ChoiceProvenance, ChoiceStatus, ConnectionChoice, ConnectionKind};
use cairn_types::ConnectionId;
use cairn_vfs::VfsRegistry;

use super::descriptor::{ConnectionDescriptor, DescriptorProvenance, OpenTarget, Reachability};
use super::provider::{
    BuiltinLocalProvider, ConnectionProvider, DiscoveryCtx, SavedProfileProvider,
};
use super::OpenError;

/// A credential-bearing connection profile that could not be opened at startup because the vault
/// was locked. Its [`ConnectionId`] is reserved up front so the connection keeps a positionally
/// stable slot; the vault-unlock flow retries exactly these once the broker is unlocked.
pub(crate) struct DeferredConnection {
    /// The pre-assigned, stable connection id.
    pub(crate) id: ConnectionId,
    /// The profile to retry via `ConnectionOpener::open` after vault unlock.
    pub(crate) profile: cairn_config::ConnectionProfile,
}

/// Coordinates connection enumeration and eager mounting at startup (RFC-0011 P1).
///
/// Replaces the imperative body of `register_connections`. Runs the built-in providers, assigns
/// stable [`ConnectionId`]s in the same order as the original function, eager-mounts
/// local/credential-free connections, defers vault-locked ones, and returns the switcher choices,
/// the deferred list, and the runtime descriptor side-map.
pub(crate) struct ConnectionCoordinator {
    opener: super::ConnectionOpener,
    /// The first [`ConnectionId`] to assign (exclusive of the startup pane ids, which are 1/2).
    base_id: u64,
}

impl ConnectionCoordinator {
    /// Create a coordinator that assigns [`ConnectionId`]s starting at `base_id`.
    ///
    /// `base_id` must be greater than the ids of both startup panes (in `app.rs`,
    /// `LEFT = ConnectionId(1)`, `RIGHT = ConnectionId(2)`, so `base_id = 3`).
    pub(crate) fn new(opener: super::ConnectionOpener, base_id: u64) -> Self {
        Self { opener, base_id }
    }

    /// Run enumeration, assign ids, mount ready connections, and return the triple.
    ///
    /// The observable output — switcher choices, id assignment, eager-mount set, deferred list —
    /// is identical to the original `register_connections` for the same config and environment.
    /// The only additions are: the returned descriptor side-map, and the new fields
    /// (`provenance`, `status`, `kind`) on each [`ConnectionChoice`].
    pub(crate) async fn run(
        &self,
        registry: &VfsRegistry,
        ctx: &DiscoveryCtx<'_>,
    ) -> (
        Vec<ConnectionChoice>,
        Vec<DeferredConnection>,
        HashMap<ConnectionId, ConnectionDescriptor>,
    ) {
        // Collect descriptors from all providers in registration order.
        // The ordering contract: builtin roots first (BuiltinLocalProvider), then local saved
        // profiles, then credential-bearing saved profiles (SavedProfileProvider).
        //
        // P1 runs providers sequentially because they are fast, offline, and synchronous.
        // P3 MUST switch to `FuturesUnordered` + per-provider timeout so that a hung Docker
        // socket or a slow kubeconfig credential plugin cannot block startup enumeration
        // (RFC-0011 §2: "concurrently and time-bounded").
        let providers: &[&dyn ConnectionProvider] = &[&BuiltinLocalProvider, &SavedProfileProvider];
        let mut raw: Vec<ConnectionDescriptor> = Vec::new();
        for provider in providers {
            raw.extend(provider.discover(ctx).await);
        }

        let mut choices: Vec<ConnectionChoice> = Vec::new();
        let mut deferred: Vec<DeferredConnection> = Vec::new();
        let mut descriptors: HashMap<ConnectionId, ConnectionDescriptor> = HashMap::new();

        for (i, mut desc) in raw.into_iter().enumerate() {
            let id = ConnectionId(self.base_id + i as u64);
            desc.id = id;

            match &desc.reachability {
                Reachability::Ready => {
                    match desc.target.clone() {
                        OpenTarget::LocalRoot(path) => {
                            // Local roots are always available; mount them directly without the opener.
                            let vfs = cairn_backend_local::LocalVfs::new(id, path);
                            registry.insert(id, Arc::new(vfs)).await;
                            let (provenance, kind) = core_projection(&desc.provenance);
                            choices.push(ConnectionChoice {
                                conn: id,
                                label: desc.display_name.clone(),
                                provenance,
                                status: ChoiceStatus::Ready,
                                kind,
                            });
                        }
                        OpenTarget::Profile(profile) => {
                            // Remote/credential-free profile (docker, k8s, or unlocked ssh/s3/…).
                            // Attempt to open; defer on `BrokerError::Locked` (the vault state
                            // might be mis-set in future call sites), drop on any other error.
                            match self.opener.open(Actor::User, id, &profile).await {
                                Ok(vfs) => {
                                    registry.insert(id, vfs).await;
                                    let (provenance, kind) = core_projection(&desc.provenance);
                                    choices.push(ConnectionChoice {
                                        conn: id,
                                        label: desc.display_name.clone(),
                                        provenance,
                                        status: ChoiceStatus::Ready,
                                        kind,
                                    });
                                }
                                Err(e) => {
                                    // A Ready-classified profile that still gets Locked is a sign
                                    // that vault_locked was mis-set by the call site (e.g. a P2
                                    // re-enumeration that passes false while the broker is locked).
                                    // Defer rather than silently dropping so the unlock flow can
                                    // retry it — same contract as the NeedsVault arm.
                                    let deferrable =
                                        matches!(e, OpenError::Broker(BrokerError::Locked));
                                    tracing::warn!(
                                        scheme = %desc.scheme,
                                        name = %desc.display_name,
                                        error = %e,
                                        deferred = deferrable,
                                        "connection profile not opened at startup"
                                    );
                                    if deferrable {
                                        deferred.push(DeferredConnection { id, profile });
                                    }
                                    // Non-deferrable (BackendNotBuilt, MissingField, …): id slot
                                    // consumed but no choice or deferred entry is added.
                                }
                            }
                        }
                    }
                }
                Reachability::NeedsVault => {
                    // Credential-bearing profile with a locked vault. Attempt the open to confirm
                    // the BrokerError::Locked and produce the same warning log as the original.
                    // The id is reserved so the profile keeps a positionally stable slot.
                    let profile = match &desc.target {
                        OpenTarget::Profile(p) => p.clone(),
                        OpenTarget::LocalRoot(path) => {
                            // P1 providers never emit a LocalRoot with NeedsVault, but a future
                            // P3 provider could violate the invariant. Log and skip — a broken
                            // descriptor does not justify crashing the whole startup sequence.
                            tracing::error!(
                                path = ?path,
                                name = %desc.display_name,
                                "local root descriptor has NeedsVault reachability; skipping"
                            );
                            descriptors.insert(id, desc);
                            continue;
                        }
                    };
                    match self.opener.open(Actor::User, id, &profile).await {
                        Ok(vfs) => {
                            // The vault was unlocked between enumeration and open (race). Mount it.
                            registry.insert(id, vfs).await;
                            let (provenance, kind) = core_projection(&desc.provenance);
                            choices.push(ConnectionChoice {
                                conn: id,
                                label: desc.display_name.clone(),
                                provenance,
                                status: ChoiceStatus::Ready,
                                kind,
                            });
                        }
                        Err(e) => {
                            let deferrable = matches!(e, OpenError::Broker(BrokerError::Locked));
                            tracing::warn!(
                                scheme = %desc.scheme,
                                name = %desc.display_name,
                                error = %e,
                                deferred = deferrable,
                                "connection profile not opened at startup"
                            );
                            if deferrable {
                                deferred.push(DeferredConnection { id, profile });
                            }
                            // Non-deferrable (e.g. missing field despite secret_ref): the id slot
                            // is consumed but no choice or deferred entry is added.
                        }
                    }
                }
            }

            descriptors.insert(id, desc);
        }

        (choices, deferred, descriptors)
    }
}

/// Convert a [`DescriptorProvenance`] to the pure-core (`ChoiceProvenance`, `ConnectionKind`)
/// pair that appears in [`ConnectionChoice`].
///
/// The mapping is:
/// - `Builtin` → `(ChoiceProvenance::Builtin, ConnectionKind::AutoDiscovered)`
/// - `Saved { id }` → `(ChoiceProvenance::Saved, ConnectionKind::Profile { id })`
/// - `Discovered { src }` → `(ChoiceProvenance::Discovered { source: src }, ConnectionKind::AutoDiscovered)`
fn core_projection(provenance: &DescriptorProvenance) -> (ChoiceProvenance, ConnectionKind) {
    match provenance {
        DescriptorProvenance::Builtin => {
            (ChoiceProvenance::Builtin, ConnectionKind::AutoDiscovered)
        }
        DescriptorProvenance::Saved { profile_id } => (
            ChoiceProvenance::Saved,
            ConnectionKind::Profile { id: *profile_id },
        ),
        DescriptorProvenance::Discovered { source } => (
            ChoiceProvenance::Discovered {
                source: source.clone(),
            },
            ConnectionKind::AutoDiscovered,
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_broker::Broker;
    use cairn_config::{Config, ConnectionProfile};
    use cairn_vfs::VfsRegistry;
    use uuid::Uuid;

    const BASE_ID: u64 = 3; // LEFT=1, RIGHT=2 → switcher starts at 3.

    fn make_coordinator() -> (ConnectionCoordinator, VfsRegistry) {
        let broker = Arc::new(Broker::locked());
        let opener = super::super::ConnectionOpener::new(broker);
        let coordinator = ConnectionCoordinator::new(opener, BASE_ID);
        let registry = VfsRegistry::new();
        (coordinator, registry)
    }

    fn ctx(config: &Config) -> DiscoveryCtx<'_> {
        DiscoveryCtx {
            config,
            vault_locked: true, // matches the startup broker state
        }
    }

    // ── No profiles ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn no_profiles_produces_builtin_roots_only() {
        let (coordinator, registry) = make_coordinator();
        let config = Config::default();
        let (choices, deferred, descriptors) = coordinator.run(&registry, &ctx(&config)).await;

        assert!(deferred.is_empty(), "no credential profiles → no deferred");
        // The filesystem root must always be present.
        let root_idx = choices.iter().position(|c| c.label == "local: /");
        assert!(root_idx.is_some(), "/ root must be in choices");

        // Id assignment: root is always the first entry → id = base.
        let root = &choices[root_idx.unwrap()];
        assert_eq!(root.conn, ConnectionId(BASE_ID));

        // All choices are Ready, Builtin, AutoDiscovered.
        for choice in &choices {
            assert_eq!(choice.status, ChoiceStatus::Ready);
            assert_eq!(choice.provenance, ChoiceProvenance::Builtin);
            assert!(
                matches!(choice.kind, ConnectionKind::AutoDiscovered),
                "builtin roots have AutoDiscovered kind"
            );
        }

        // Descriptors include exactly the choices (all were mounted successfully).
        assert_eq!(
            descriptors.len(),
            choices.len(),
            "one descriptor per choice for builtin-only configs"
        );
    }

    // ── Local profiles ───────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn local_profiles_are_appended_after_builtins() {
        let (coordinator, registry) = make_coordinator();
        let mut config = Config::default();
        let mut prof = ConnectionProfile::new("local", "work");
        prof.endpoint.insert("path".into(), "/work".into());
        let prof_id = prof.id;
        config.connections.push(prof);

        let (choices, deferred, _descriptors) = coordinator.run(&registry, &ctx(&config)).await;

        assert!(deferred.is_empty());
        // The local profile appears after the builtin root(s).
        let work_idx = choices.iter().position(|c| c.label == "local: work");
        let root_idx = choices.iter().position(|c| c.label == "local: /");
        assert!(
            root_idx.unwrap() < work_idx.unwrap(),
            "builtin / must precede local profile"
        );

        // The local profile's choice has Saved provenance and Profile kind.
        let work = &choices[work_idx.unwrap()];
        assert_eq!(work.provenance, ChoiceProvenance::Saved);
        assert_eq!(work.status, ChoiceStatus::Ready);
        assert!(
            matches!(work.kind, ConnectionKind::Profile { id } if id == prof_id),
            "local saved profile must have Profile kind with the correct UUID"
        );
    }

    // ── Credential-bearing profiles (vault locked) ────────────────────────────────────────────

    /// With the `s3` feature on, the opener can get past the scheme dispatch and reach the broker,
    /// which returns `BrokerError::Locked` — the only deferrable error. In the lean build (no
    /// backends), every credential-bearing scheme returns `BackendNotBuilt` instead of `Locked`,
    /// so deferral never fires (same as the original `register_connections`). This test is therefore
    /// gated on `s3` to match the project's existing test coverage convention (see `app.rs`).
    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn credential_profile_with_vault_locked_is_deferred() {
        let (coordinator, registry) = make_coordinator();
        let mut config = Config::default();
        let mut prof = ConnectionProfile::new("s3", "prod");
        prof.endpoint.insert("bucket".into(), "b".into());
        prof.secret_ref = Some(Uuid::new_v4());
        config.connections.push(prof.clone());

        let (choices, deferred, descriptors) = coordinator.run(&registry, &ctx(&config)).await;

        // The s3 profile must not appear in choices (vault locked → BrokerError::Locked).
        assert!(
            choices.iter().all(|c| !c.label.starts_with("s3:")),
            "vault-locked s3 profile must not appear in choices"
        );
        // It must be deferred.
        assert_eq!(deferred.len(), 1, "one deferred entry for the s3 profile");
        assert_eq!(deferred[0].profile.display_name, "prod");

        // Its id is reserved in the descriptor map even though it is not in choices.
        assert_eq!(
            descriptors.len(),
            choices.len() + 1,
            "descriptor map includes the deferred entry"
        );
        let deferred_id = deferred[0].id;
        assert!(
            descriptors.contains_key(&deferred_id),
            "deferred id must be in the descriptor map"
        );
    }

    // ── Id-assignment stability ───────────────────────────────────────────────────────────────

    /// Verify that the coordinator assigns [`ConnectionId`]s in the same order as the original
    /// `register_connections`: builtin roots → local saved profiles → credential-bearing saved
    /// profiles. The credential-bearing profile's id slot is always consumed (even in the lean
    /// build where the backend is not compiled and the open fails with `BackendNotBuilt`); we
    /// verify the reservation via the descriptor side-map rather than the deferred vec.
    #[tokio::test]
    async fn id_assignment_matches_original_register_connections() {
        // Replicate the original function's id assignment for a representative config:
        // ┌─ builtin "/"         → id 3 (base + 0)
        // ├─ builtin "$HOME"     → id 4 (base + 1)  ← only when HOME is set
        // ├─ local profile       → id N              ← base + num_builtins
        // └─ s3 profile          → id N+1            ← base + num_builtins + num_local
        let (coordinator, registry) = make_coordinator();
        let mut config = Config::default();

        let mut local_prof = ConnectionProfile::new("local", "work");
        local_prof.endpoint.insert("path".into(), "/work".into());
        config.connections.push(local_prof);

        let mut cred_prof = ConnectionProfile::new("s3", "prod");
        cred_prof.endpoint.insert("bucket".into(), "b".into());
        cred_prof.secret_ref = Some(Uuid::new_v4());
        config.connections.push(cred_prof);

        let (choices, _deferred, descriptors) = coordinator.run(&registry, &ctx(&config)).await;

        // "/" gets the base id.
        let root = choices.iter().find(|c| c.label == "local: /").unwrap();
        assert_eq!(root.conn, ConnectionId(BASE_ID));

        // The local profile gets an id immediately after the builtins.
        let num_builtins = choices
            .iter()
            .filter(|c| c.provenance == ChoiceProvenance::Builtin)
            .count();
        let work = choices.iter().find(|c| c.label == "local: work").unwrap();
        assert_eq!(work.conn, ConnectionId(BASE_ID + num_builtins as u64));

        // The credential-bearing profile's id is reserved in the descriptor map whether or not
        // it ended up in choices or deferred (the id slot is consumed regardless of open outcome).
        let expected_cred_id = ConnectionId(BASE_ID + num_builtins as u64 + 1);
        assert!(
            descriptors.contains_key(&expected_cred_id),
            "credential profile id must be reserved at position base + builtins + 1"
        );
    }

    // ── Ready/Profile deferral on Locked (fix 4) ─────────────────────────────────────────────

    /// Even when `vault_locked` is incorrectly set to `false` (or a profile is mis-classified as
    /// `Ready`), the coordinator must defer on `BrokerError::Locked` from the `Ready/Profile`
    /// arm. Without this, a stale `vault_locked: false` at a P2 call site would silently drop
    /// profiles instead of deferring them. Gated on `s3` — see the P1 deferral test above.
    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn ready_profile_that_gets_locked_error_is_deferred() {
        let (coordinator, registry) = make_coordinator(); // broker is Broker::locked()
        let mut config = Config::default();
        let mut prof = ConnectionProfile::new("s3", "prod");
        prof.endpoint.insert("bucket".into(), "b".into());
        prof.secret_ref = Some(Uuid::new_v4());
        config.connections.push(prof);

        // vault_locked: false → SavedProfileProvider classifies the profile as Ready (not
        // NeedsVault), but the broker is actually locked → opener returns BrokerError::Locked.
        let ctx_unlocked = DiscoveryCtx {
            config: &config,
            vault_locked: false,
        };
        let (choices, deferred, _descriptors) = coordinator.run(&registry, &ctx_unlocked).await;

        assert!(
            choices.iter().all(|c| !c.label.starts_with("s3:")),
            "locked profile must not appear in choices"
        );
        assert_eq!(
            deferred.len(),
            1,
            "BrokerError::Locked from Ready/Profile arm must defer, not drop"
        );
        assert_eq!(deferred[0].profile.display_name, "prod");
    }

    // ── Lean-build descriptor count contract (P2 baseline) ───────────────────────────────────

    /// In the lean build (no backend features), a `docker` profile fails with `BackendNotBuilt`
    /// — a non-deferrable error. The id slot is still consumed and the descriptor is always
    /// inserted into the side-map. P2 re-enumeration code that checks the map for live ids must
    /// account for this: a descriptor in the map does NOT imply a live registry or choice entry.
    #[cfg(not(feature = "docker"))]
    #[tokio::test]
    async fn failed_open_reserves_id_in_descriptor_map_but_not_in_choices() {
        let (coordinator, registry) = make_coordinator();
        let mut config = Config::default();
        config
            .connections
            .push(ConnectionProfile::new("docker", "local-docker"));

        let (choices, deferred, descriptors) = coordinator.run(&registry, &ctx(&config)).await;

        assert!(deferred.is_empty(), "BackendNotBuilt is not deferrable");
        // choices = builtin roots only; docker is absent.
        assert!(
            choices.iter().all(|c| !c.label.starts_with("docker:")),
            "docker profile must not appear in choices (lean build: BackendNotBuilt)"
        );
        // But the descriptor IS in the map — the id slot was consumed.
        assert_eq!(
            descriptors.len(),
            choices.len() + 1,
            "failed docker descriptor must still be in the side-map"
        );
    }

    // ── core_projection Discovered mapping (P3 baseline) ─────────────────────────────────────

    /// Locks the `Discovered` arm of `core_projection` so P3 docker/kubeconfig providers can
    /// rely on it. The arm is dead in P1 (no provider constructs `Discovered` yet).
    #[test]
    fn core_projection_discovered_maps_to_auto_discovered() {
        use super::super::descriptor::DescriptorProvenance;
        use cairn_core::DiscoverySource;

        let (prov, kind) = core_projection(&DescriptorProvenance::Discovered {
            source: DiscoverySource::Docker,
        });
        assert!(
            matches!(
                prov,
                ChoiceProvenance::Discovered {
                    source: DiscoverySource::Docker
                }
            ),
            "Discovered must map to ChoiceProvenance::Discovered with the same source"
        );
        assert!(
            matches!(kind, ConnectionKind::AutoDiscovered),
            "Discovered descriptor must have AutoDiscovered kind"
        );
    }
}
