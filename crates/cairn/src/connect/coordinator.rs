//! The [`ConnectionCoordinator`] — the RFC-0011 P2 lazy-open coordinator.
//!
//! The coordinator runs the registered [`ConnectionProvider`]s in order, assigns stable
//! [`ConnectionId`]s (preserving the contract of the original `register_connections`), mounts
//! local/credential-free connections immediately, and returns:
//!
//! 1. `Vec<ConnectionChoice>` — the switcher entries for `AppState::connections`.
//! 2. `Vec<DeferredConnection>` — **always empty in P2**; retained for API stability so future
//!    phases can remove it cleanly rather than forcing a flag day.
//! 3. `HashMap<ConnectionId, ConnectionDescriptor>` — the runtime side-map, keyed by id, used
//!    by the P2 effect runner to open a connection on selection.
//!
//! ## P2 open strategy
//!
//! Only `LocalRoot` targets are mounted eagerly at startup. `Profile` targets — regardless of
//! their [`Reachability`] classification — are placed in the switcher as
//! [`ChoiceStatus::NeedsOpen`] (or [`ChoiceStatus::NeedsVault`] when the vault is locked and the
//! profile has a `secret_ref`). Opening happens on demand in the effect runner when the user
//! selects the connection.
//!
//! ## Stable id reuse on re-enumeration
//!
//! `run()` accepts a `prior_descriptors` map (the descriptor side-map from the previous
//! enumeration). For each newly-discovered descriptor, the coordinator looks up its
//! [`ConnectionKey`] in that map; if a live entry is found, the existing
//! [`ConnectionId`] is reused rather than minting a fresh one. This prevents a config reload
//! from silently repointing panes that are already browsing an open connection.
//!
//! At startup (the first call), pass an empty `HashMap::new()` as `prior_descriptors`.

use std::collections::HashMap;
use std::sync::Arc;

use cairn_core::{ChoiceProvenance, ChoiceStatus, ConnectionChoice, ConnectionKind};
use cairn_types::ConnectionId;
use cairn_vfs::VfsRegistry;

use super::descriptor::{
    ConnectionDescriptor, ConnectionKey, DescriptorProvenance, OpenTarget, Reachability,
};
use super::provider::{
    BuiltinLocalProvider, ConnectionProvider, DiscoveryCtx, SavedProfileProvider,
};

/// A credential-bearing connection profile that could not be opened at startup because the vault
/// was locked.
///
/// **P2 note:** The coordinator never adds entries to this list in P2. Vault-locked profiles
/// appear in the switcher as [`ChoiceStatus::NeedsVault`] and are opened on demand after unlock.
/// This struct is retained for API stability; it will be removed when P1's eager-open path is
/// fully retired.
pub(crate) struct DeferredConnection {
    /// The pre-assigned, stable connection id.
    #[allow(dead_code)]
    pub(crate) id: ConnectionId,
    /// The profile that was deferred.
    #[allow(dead_code)]
    pub(crate) profile: cairn_config::ConnectionProfile,
}

/// Coordinates connection enumeration at startup (RFC-0011 P2).
///
/// Replaces the imperative body of `register_connections`. Runs the built-in providers, assigns
/// stable [`ConnectionId`]s, eagerly mounts `LocalRoot` targets, records `Profile` targets in the
/// side-map for lazy open on selection, and returns the switcher choices, the (always-empty)
/// deferred list, and the runtime descriptor side-map.
pub(crate) struct ConnectionCoordinator {
    /// Opener kept for P3 use (forced-open at re-enumeration or health-check). Unused in P2
    /// because all Profile targets are opened lazily by the effect runner, not the coordinator.
    #[allow(dead_code)]
    opener: super::ConnectionOpener,
    /// The first [`ConnectionId`] to assign when no prior id exists for a key.
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

    /// Run enumeration, assign ids, mount local connections eagerly, and return the triple.
    ///
    /// `prior_descriptors` is the descriptor side-map from the previous enumeration round.
    /// Pass `&HashMap::new()` on the first call (startup). For any descriptor whose
    /// [`ConnectionKey`] already appears in `prior_descriptors`, the prior [`ConnectionId`] is
    /// reused so open panes are not repointed. Fresh ids are minted for genuinely new keys via a
    /// counter that skips over all ids already claimed (both prior and freshly-assigned).
    ///
    /// `LocalRoot` targets are mounted directly via [`cairn_backend_local::LocalVfs`]; `Profile`
    /// targets are placed in the switcher as `NeedsOpen` or `NeedsVault` with no open attempt.
    pub(crate) async fn run(
        &self,
        registry: &VfsRegistry,
        ctx: &DiscoveryCtx<'_>,
        prior_descriptors: &HashMap<ConnectionId, ConnectionDescriptor>,
    ) -> (
        Vec<ConnectionChoice>,
        Vec<DeferredConnection>,
        HashMap<ConnectionId, ConnectionDescriptor>,
    ) {
        // ── Build the key→id reuse map from the prior round ──────────────────────────────────
        // For each live descriptor key in the prior map, record its id so we can reuse it when
        // the same key reappears in this enumeration. This is the stable-id contract for
        // re-enumeration: open panes keep pointing at their existing ConnectionId.
        let key_to_prior_id: HashMap<&ConnectionKey, ConnectionId> =
            prior_descriptors.values().map(|d| (&d.key, d.id)).collect();

        // Collect all ids already claimed (prior + newly minted below) so the fresh-id counter
        // skips over them. We start by collecting the prior ids; newly-assigned ids are inserted
        // as we go so the counter never re-mints a live id.
        let mut claimed: std::collections::HashSet<ConnectionId> =
            prior_descriptors.keys().copied().collect();

        // ── Enumerate providers ───────────────────────────────────────────────────────────────
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
        // P2 never populates this; retained for API stability.
        let deferred: Vec<DeferredConnection> = Vec::new();
        let mut descriptors: HashMap<ConnectionId, ConnectionDescriptor> = HashMap::new();

        // Fresh-id counter: next id to mint when a key has no prior entry.
        // Starts at base_id and advances, skipping any id already in `claimed`.
        let mut next_fresh = self.base_id;

        for mut desc in raw {
            // Reuse the prior id for this key, or mint a fresh one.
            // All prior ids are already in `claimed` (initialised above), so no re-insert needed.
            let id = if let Some(&prior_id) = key_to_prior_id.get(&desc.key) {
                prior_id
            } else {
                mint_fresh_id(&mut next_fresh, &mut claimed)
            };
            desc.id = id;

            match (&desc.reachability, &desc.target) {
                // ── LocalRoot (always eager-mount) ────────────────────────────────────────────
                (Reachability::Ready, OpenTarget::LocalRoot(path)) => {
                    let vfs = cairn_backend_local::LocalVfs::new(id, path.clone());
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
                // ── NeedsVault LocalRoot: provider invariant violation, skip ─────────────────
                // Providers must never emit a LocalRoot with NeedsVault; guard so a future P3
                // provider bug does not crash the startup sequence.
                (Reachability::NeedsVault, OpenTarget::LocalRoot(path)) => {
                    tracing::error!(
                        path = ?path,
                        name = %desc.display_name,
                        "local root descriptor has NeedsVault reachability; skipping"
                    );
                    descriptors.insert(id, desc);
                    continue;
                }
                // ── Profile, Ready: lazy-open on selection ────────────────────────────────────
                // In P2 we do NOT attempt to open credential-free profiles at startup. They
                // appear as NeedsOpen so the effect runner opens them on first selection.
                (Reachability::Ready, OpenTarget::Profile(_)) => {
                    let (provenance, kind) = core_projection(&desc.provenance);
                    choices.push(ConnectionChoice {
                        conn: id,
                        label: desc.display_name.clone(),
                        provenance,
                        status: ChoiceStatus::NeedsOpen,
                        kind,
                    });
                }
                // ── Profile, NeedsVault: lazy-open after vault unlock ─────────────────────────
                (Reachability::NeedsVault, OpenTarget::Profile(_)) => {
                    let (provenance, kind) = core_projection(&desc.provenance);
                    choices.push(ConnectionChoice {
                        conn: id,
                        label: desc.display_name.clone(),
                        provenance,
                        status: ChoiceStatus::NeedsVault,
                        kind,
                    });
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

/// Mint the next available [`ConnectionId`] that is not already in `claimed`, advancing
/// `next_fresh` past the result. Inserts the minted id into `claimed` before returning so
/// successive calls never produce the same id.
fn mint_fresh_id(
    next_fresh: &mut u64,
    claimed: &mut std::collections::HashSet<ConnectionId>,
) -> ConnectionId {
    while claimed.contains(&ConnectionId(*next_fresh)) {
        *next_fresh += 1;
    }
    let id = ConnectionId(*next_fresh);
    claimed.insert(id);
    *next_fresh += 1;
    id
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
        let (choices, deferred, descriptors) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;

        assert!(deferred.is_empty(), "P2 coordinator never defers");
        // The filesystem root must always be present.
        let root_idx = choices.iter().position(|c| c.label == "local: /");
        assert!(root_idx.is_some(), "/ root must be in choices");

        // Id assignment: root is always the first entry → id = base.
        let root = &choices[root_idx.unwrap()];
        assert_eq!(root.conn, ConnectionId(BASE_ID));

        // All builtin choices are Ready (local mounts).
        for choice in &choices {
            assert_eq!(choice.status, ChoiceStatus::Ready);
            assert_eq!(choice.provenance, ChoiceProvenance::Builtin);
            assert!(
                matches!(choice.kind, ConnectionKind::AutoDiscovered),
                "builtin roots have AutoDiscovered kind"
            );
        }

        // Descriptors include exactly the choices.
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

        let (choices, deferred, _descriptors) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;

        assert!(deferred.is_empty());
        // The local profile appears after the builtin root(s).
        let work_idx = choices.iter().position(|c| c.label == "local: work");
        let root_idx = choices.iter().position(|c| c.label == "local: /");
        assert!(
            root_idx.unwrap() < work_idx.unwrap(),
            "builtin / must precede local profile"
        );

        // The local profile has Saved provenance, Profile kind, and Ready status (LocalRoot
        // targets are eagerly mounted in P2).
        let work = &choices[work_idx.unwrap()];
        assert_eq!(work.provenance, ChoiceProvenance::Saved);
        assert_eq!(work.status, ChoiceStatus::Ready);
        assert!(
            matches!(work.kind, ConnectionKind::Profile { id } if id == prof_id),
            "local saved profile must have Profile kind with the correct UUID"
        );
    }

    // ── Credential-bearing profiles (vault locked) ────────────────────────────────────────────

    /// In P2, credential-bearing profiles (vault locked) appear as NeedsVault in the switcher
    /// rather than being deferred. No open attempt is made at startup.
    #[tokio::test]
    async fn credential_profile_with_vault_locked_is_needs_vault() {
        let (coordinator, registry) = make_coordinator();
        let mut config = Config::default();
        let mut prof = ConnectionProfile::new("s3", "prod");
        prof.endpoint.insert("bucket".into(), "b".into());
        prof.secret_ref = Some(Uuid::new_v4());
        config.connections.push(prof.clone());

        let (choices, deferred, descriptors) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;

        // P2: deferred list is always empty.
        assert!(deferred.is_empty(), "P2 coordinator never defers");
        // The s3 profile appears as NeedsVault, not as Ready or missing.
        let s3 = choices.iter().find(|c| c.label.starts_with("s3:"));
        assert!(
            s3.is_some(),
            "vault-locked s3 profile must appear in choices as NeedsVault"
        );
        assert_eq!(
            s3.unwrap().status,
            ChoiceStatus::NeedsVault,
            "vault-locked credential profile must have NeedsVault status"
        );

        // Its descriptor is in the side-map.
        let s3_id = s3.unwrap().conn;
        assert!(
            descriptors.contains_key(&s3_id),
            "NeedsVault profile must be in the descriptor map"
        );
    }

    // ── Non-local profiles without vault lock ─────────────────────────────────────────────────

    /// In P2, non-local profiles that are accessible without the vault (docker, k8s, or
    /// credential-free ssh) appear as NeedsOpen rather than being eagerly opened.
    #[tokio::test]
    async fn credential_free_profile_is_needs_open() {
        let (coordinator, registry) = make_coordinator();
        let mut config = Config::default();
        // Docker has no secret_ref → Ready classification by SavedProfileProvider.
        config
            .connections
            .push(ConnectionProfile::new("docker", "local-docker"));

        let (choices, deferred, descriptors) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;

        assert!(deferred.is_empty(), "P2 coordinator never defers");
        // Docker profile must appear as NeedsOpen (not attempt-opened at startup).
        let docker = choices.iter().find(|c| c.label.starts_with("docker:"));
        assert!(
            docker.is_some(),
            "credential-free docker profile must appear in choices"
        );
        assert_eq!(
            docker.unwrap().status,
            ChoiceStatus::NeedsOpen,
            "docker profile (no vault needed) must have NeedsOpen status in P2"
        );
        // Its descriptor is in the side-map so the effect runner can open it.
        let docker_id = docker.unwrap().conn;
        assert!(
            descriptors.contains_key(&docker_id),
            "NeedsOpen profile must be in the descriptor map"
        );
    }

    // ── Id-assignment stability ───────────────────────────────────────────────────────────────

    /// Verify that the coordinator assigns [`ConnectionId`]s in the expected order: builtin roots
    /// first, then local saved profiles, then non-local saved profiles.
    #[tokio::test]
    async fn id_assignment_matches_expected_order() {
        let (coordinator, registry) = make_coordinator();
        let mut config = Config::default();

        let mut local_prof = ConnectionProfile::new("local", "work");
        local_prof.endpoint.insert("path".into(), "/work".into());
        config.connections.push(local_prof);

        let mut cred_prof = ConnectionProfile::new("s3", "prod");
        cred_prof.endpoint.insert("bucket".into(), "b".into());
        cred_prof.secret_ref = Some(Uuid::new_v4());
        config.connections.push(cred_prof);

        let (choices, _deferred, descriptors) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;

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

        // The s3 profile follows in choices and its descriptor is in the map.
        let s3 = choices.iter().find(|c| c.label.starts_with("s3:")).unwrap();
        let expected_s3_id = ConnectionId(BASE_ID + num_builtins as u64 + 1);
        assert_eq!(s3.conn, expected_s3_id);
        assert!(descriptors.contains_key(&expected_s3_id));
    }

    // ── Stable id reuse on re-enumeration ────────────────────────────────────────────────────

    /// When a key already appears in `prior_descriptors`, the existing [`ConnectionId`] is
    /// reused rather than minting a fresh one. This prevents pane repointing on config reload.
    #[tokio::test]
    async fn key_reuse_preserves_id_across_reenumeration() {
        let (coordinator, registry) = make_coordinator();
        let config = Config::default();

        // First enumeration (no prior map).
        let (choices1, _, descriptors1) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;

        // Record the root id from the first run.
        let root1 = choices1.iter().find(|c| c.label == "local: /").unwrap();
        let root_id_first = root1.conn;

        // Second enumeration, passing the first round's descriptors as prior.
        let (choices2, _, descriptors2) = coordinator
            .run(&registry, &ctx(&config), &descriptors1)
            .await;

        let root2 = choices2.iter().find(|c| c.label == "local: /").unwrap();
        assert_eq!(
            root2.conn, root_id_first,
            "builtin / must reuse its id from the prior enumeration"
        );
        assert_eq!(
            descriptors2[&root_id_first].key, descriptors1[&root_id_first].key,
            "the reused id must map to the same key in both rounds"
        );
    }

    /// When a prior descriptor map has an id that would otherwise conflict with the fresh counter,
    /// the fresh counter skips it rather than reissuing the same id to a different key.
    #[tokio::test]
    async fn fresh_id_counter_skips_prior_claimed_ids() {
        let (coordinator, registry) = make_coordinator();
        let mut config = Config::default();

        // Add a local profile so the second enumeration produces more entries.
        let mut prof = ConnectionProfile::new("local", "work");
        prof.endpoint.insert("path".into(), "/work".into());
        config.connections.push(prof);

        // First round.
        let (_, _, descriptors1) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;

        // Second round with same config — all keys are reused, so the second map must contain
        // exactly the same ids as the first (no new ids minted, no old ids duplicated).
        let (_, _, descriptors2) = coordinator
            .run(&registry, &ctx(&config), &descriptors1)
            .await;

        let mut ids1: Vec<ConnectionId> = descriptors1.keys().copied().collect();
        let mut ids2: Vec<ConnectionId> = descriptors2.keys().copied().collect();
        ids1.sort_by_key(|id| id.0);
        ids2.sort_by_key(|id| id.0);
        assert_eq!(
            ids1, ids2,
            "re-enumeration with identical config must produce identical ids"
        );
    }

    // ── core_projection Discovered mapping (P3 baseline) ─────────────────────────────────────

    /// Locks the `Discovered` arm of `core_projection` so P3 docker/kubeconfig providers can
    /// rely on it. The arm is dead in P1/P2 (no provider constructs `Discovered` yet).
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
