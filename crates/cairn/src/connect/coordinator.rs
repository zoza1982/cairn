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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use cairn_core::{ChoiceProvenance, ChoiceStatus, ConnectionChoice, ConnectionKind};
use cairn_types::ConnectionId;
use cairn_vfs::VfsRegistry;

use super::descriptor::{
    ConnectionDescriptor, ConnectionKey, DescriptorProvenance, OpenTarget, Reachability,
};
#[cfg(feature = "docker")]
use super::provider::DockerProvider;
#[cfg(feature = "k8s")]
use super::provider::KubeconfigProvider;
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
        // P3: providers run concurrently via `join_all` so that a slow socket probe or a large
        // kubeconfig does not delay other providers. Each provider is individually time-bounded
        // (RFC-0011 §2: "concurrently and time-bounded"):
        // - P1 providers (builtin, saved) complete in microseconds; the timeout is never reached.
        // - P3 providers (Docker, K8s) may block on I/O but are bounded here AND internally.
        //   Per-socket 500 ms timeouts are enforced inside `DockerProvider`. The `KubeconfigProvider`
        //   adds a 2 s `spawn_blocking` timeout. The per-provider guard here adds a final safety
        //   net so a buggy provider can't hold up the whole startup.
        //
        // Per-provider timeout: a slow/hung provider degrades to "no entries" for THAT provider
        // only, so Docker failing fast doesn't discard the builtin roots or the kubeconfig entry.
        // A single outer `join_all` timeout would be wrong: it could discard ALL results when one
        // provider is slow, leaving the switcher empty.
        const PROVIDER_TIMEOUT: Duration = Duration::from_secs(5);

        // Build the provider list. `list` is `mut` so cfg-gated providers can be pushed; the
        // `#[allow(unused_mut)]` suppresses the lint in lean builds where no pushes happen.
        let provider_list: Vec<Box<dyn ConnectionProvider>> = {
            #[allow(unused_mut)]
            let mut list: Vec<Box<dyn ConnectionProvider>> = vec![
                Box::new(BuiltinLocalProvider),
                Box::new(SavedProfileProvider),
            ];
            // Discovery providers are gated on BOTH the compiled feature AND the user's
            // `[discovery]` opt-out toggle (default on). Skipping the push here is what makes
            // `discovery.docker = false` / `discovery.kubernetes = false` actually take effect.
            #[cfg(feature = "docker")]
            if ctx.config.discovery.docker {
                list.push(Box::new(DockerProvider));
            }
            #[cfg(feature = "k8s")]
            if ctx.config.discovery.kubernetes {
                list.push(Box::new(KubeconfigProvider));
            }
            list
        };

        // Run all providers concurrently, each behind its own timeout.
        let discover_results = futures::future::join_all(provider_list.iter().map(|p| async {
            match tokio::time::timeout(PROVIDER_TIMEOUT, p.discover(ctx)).await {
                Ok(descs) => descs,
                Err(_elapsed) => {
                    tracing::warn!(
                        provider = p.source_id(),
                        timeout_secs = PROVIDER_TIMEOUT.as_secs(),
                        "provider timed out; skipping its entries"
                    );
                    Vec::new()
                }
            }
        }))
        .await;
        let mut raw: Vec<ConnectionDescriptor> = discover_results.into_iter().flatten().collect();

        // ── Saved-wins dedup ─────────────────────────────────────────────────────────────────
        // RFC §2: if the user has manually configured a saved profile for the same backend type,
        // suppress the auto-discovered entry so the switcher doesn't show duplicates.
        // We compare at the backend-type level (Docker scheme vs Docker socket, kubernetes scheme
        // vs kubeconfig/in-cluster) which covers the common case correctly.
        //
        // Deferred: exact same-socket or same-cluster comparison is impractical without querying
        // the running daemon/cluster. P4 may add explicit endpoint keys (e.g. "socket" = path)
        // to `ConnectionProfile` to enable finer-grained dedup.
        raw = dedup_discovered_vs_saved(raw);

        // ── Hidden overlay: P6 change — mark, don't drop ────────────────────────────────────────
        // Earlier phases dropped a descriptor whose stable key string appeared in
        // `config.discovery.hidden` entirely. RFC-0011 P6 adds a switcher "show hidden" toggle
        // that must be able to reveal (and un-hide) a hidden entry, which requires it to still be
        // enumerated; the `hidden` flag is now applied per-`ConnectionChoice` below (in the id
        // loop) instead of filtering `raw` here. The overlay still applies to ALL descriptor
        // types (builtin, saved, discovered) — the key strings are the same `as_key_str()` format
        // used everywhere (e.g. `"builtin:/"`, `"saved:<uuid>"`, `"docker:socket:default"`,
        // `"kube:kubeconfig"`).

        // ── Apply pinned overlay ──────────────────────────────────────────────────────────────
        // Entries in `config.discovery.pinned` float to the front of the list, in stated order.
        // Like hidden, pinned can reference any descriptor type (builtin, saved, discovered).
        // A key listed in `pinned` but not present in the discovered set is silently ignored.
        if !ctx.config.discovery.pinned.is_empty() {
            let mut pinned_descs: Vec<ConnectionDescriptor> = Vec::new();
            for pinned_key in &ctx.config.discovery.pinned {
                if let Some(pos) = raw.iter().position(|d| d.key.as_key_str() == *pinned_key) {
                    pinned_descs.push(raw.remove(pos));
                }
            }
            let mut ordered = pinned_descs;
            ordered.extend(raw);
            raw = ordered;
        }

        // Providers must not emit duplicate keys — a duplicate would cause the key→id map to
        // silently alias two descriptors onto the same ConnectionId. Assert in debug builds;
        // in release the duplicate would be overwritten by `descriptors.insert` below, which
        // is recoverable but confusing, and should never happen with well-behaved providers.
        #[cfg(debug_assertions)]
        {
            let mut seen_keys = std::collections::HashSet::new();
            for desc in &raw {
                debug_assert!(
                    seen_keys.insert(&desc.key),
                    "duplicate ConnectionKey across providers: {:?}",
                    desc.key
                );
            }
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

            // P6: pinned/hidden are pure display flags carried on every resulting
            // `ConnectionChoice`, computed once per descriptor from its stable key string so all
            // four push sites below stay in sync without repeating the lookup.
            let key_str = desc.key.as_key_str();
            let pinned = ctx.config.discovery.pinned.contains(&key_str);
            let hidden = ctx.config.discovery.hidden.contains(&key_str);

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
                        pinned,
                        hidden,
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
                        pinned,
                        hidden,
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
                        pinned,
                        hidden,
                    });
                }
                // ── P3 discovered targets: always lazy-open on selection ─────────────────────
                // Docker sockets and Kubernetes clusters never require vault credentials; they
                // are presented as NeedsOpen regardless of vault state. The effect runner opens
                // them on first selection. `NeedsVault` reachability would be a provider
                // invariant violation for these targets, but is handled defensively here.
                (_, OpenTarget::DockerSocket { .. })
                | (_, OpenTarget::KubeconfigDefault)
                | (_, OpenTarget::InCluster) => {
                    let (provenance, kind) = core_projection(&desc.provenance);
                    choices.push(ConnectionChoice {
                        conn: id,
                        label: desc.display_name.clone(),
                        provenance,
                        status: ChoiceStatus::NeedsOpen,
                        kind,
                        pinned,
                        hidden,
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
///
/// **Termination:** `claimed` is bounded by the number of live connections (a small constant in
/// practice), and `u64::MAX` (~1.8 × 10¹⁹) far exceeds any realistic claim count, so the
/// `while` loop always terminates before wrapping.
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
// Saved-wins dedup
// ---------------------------------------------------------------------------

/// Suppress auto-discovered entries that duplicate a user-saved profile for the same backend.
///
/// RFC-0011 §2 requires the coordinator to present at most one entry per logical backend so a
/// user with both a manually-saved Docker profile and an auto-discovered Docker socket doesn't
/// see two Docker entries. We compare at the *backend-type level*:
///
/// - Any saved `"docker"` profile suppresses all discovered `DockerSocket` entries.
/// - Any saved `"kubernetes"` or `"k8s"` profile suppresses all discovered
///   `KubeconfigDefault` and `InCluster` entries.
///
/// Saved and builtin entries are never suppressed by this function.
///
/// ## Deferred cases
///
/// Exact same-socket or same-cluster comparison (e.g. a saved docker profile pointing to the
/// same rootless socket as a discovered entry) is impractical without querying the running
/// daemon/cluster at discovery time. These are left for P4 when the `ConnectionProfile` schema
/// gains explicit endpoint keys (e.g. `"socket" = "/run/user/1000/docker.sock"`) that can be
/// compared directly against the discovered socket path.
fn dedup_discovered_vs_saved(raw: Vec<ConnectionDescriptor>) -> Vec<ConnectionDescriptor> {
    // Collect the schemes of all saved profiles as owned strings so we can test membership
    // without holding a borrow that would prevent moving `raw` into the filter below.
    let saved_schemes: HashSet<String> = raw
        .iter()
        .filter_map(|d| {
            if let OpenTarget::Profile(p) = &d.target {
                Some(p.scheme.clone())
            } else {
                None
            }
        })
        .collect();

    if saved_schemes.is_empty() {
        return raw;
    }

    raw.into_iter()
        .filter(|d| {
            // Builtin and Saved entries are always retained.
            if !matches!(&d.provenance, DescriptorProvenance::Discovered { .. }) {
                return true;
            }
            match &d.target {
                OpenTarget::DockerSocket { .. } => !saved_schemes.contains("docker"),
                OpenTarget::KubeconfigDefault | OpenTarget::InCluster => {
                    !saved_schemes.contains("kubernetes") && !saved_schemes.contains("k8s")
                }
                // LocalRoot and Profile targets should never appear on a Discovered descriptor,
                // but if they do, keep them (don't suppress unexpectedly).
                _ => true,
            }
        })
        .collect()
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

    /// A `Config` with auto-discovery disabled. Coordinator unit tests exercise the merge / id /
    /// overlay logic and must be hermetic — with discovery on and `--all-features`, the real Docker
    /// / kubeconfig providers would run against whatever daemon/kubeconfig the host (or CI runner)
    /// happens to have, making assertions like "builtin roots only" flaky. Real discovery is
    /// covered by the provider modules' own tests and the env-guarded integration jobs.
    fn test_config() -> Config {
        let mut c = Config::default();
        c.discovery.docker = false;
        c.discovery.kubernetes = false;
        c
    }

    // ── No profiles ─────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn no_profiles_produces_builtin_roots_only() {
        let (coordinator, registry) = make_coordinator();
        let config = test_config();
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
        let mut config = test_config();
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
        let mut config = test_config();
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
        let mut config = test_config();
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
        let mut config = test_config();

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
        let config = test_config();

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
        let mut config = test_config();

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

    // ── P3: hidden overlay ───────────────────────────────────────────────────────────────────

    /// Entries whose key string appears in `config.discovery.hidden` are marked `hidden` on their
    /// `ConnectionChoice` — RFC-0011 P6 changed this from dropping them entirely so the switcher's
    /// "show hidden" toggle can reveal (and un-hide) them.
    #[tokio::test]
    async fn hidden_overlay_marks_but_does_not_drop_matching_keys() {
        let (coordinator, registry) = make_coordinator();

        // Find the key string for the filesystem root so we can hide it.
        let mut config = test_config();
        let root_key = {
            let (choices_pre, _, _) = coordinator
                .run(&registry, &ctx(&config), &HashMap::new())
                .await;
            choices_pre
                .iter()
                .find(|c| c.label == "local: /")
                .expect("root must be present in the pre-hide run")
                .label // used below only as an existence check; key comes from descriptor
                // trick: use the first coordinator run to confirm the key, then check absence
                .clone()
        };
        // Confirm we found it.
        assert_eq!(root_key, "local: /");

        // Enumerate once (clean slate) and look up the actual key string from the descriptor map.
        let (_, _, descriptors) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;
        let root_descriptor = descriptors
            .values()
            .find(|d| matches!(&d.key, ConnectionKey::Builtin(p) if p == &std::path::PathBuf::from("/")))
            .expect("root descriptor must be present");
        let root_key_str = root_descriptor.key.as_key_str();

        // Hide the root.
        config.discovery.hidden = vec![root_key_str.clone()];
        let (choices_hidden, _, _) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;

        // P6: hidden entries are marked, not dropped — the switcher's "show hidden" toggle must
        // be able to reveal (and un-hide) them, which requires them to still be enumerated.
        let root_after = choices_hidden
            .iter()
            .find(|c| c.label == "local: /")
            .expect("hidden root must still be present in choices (marked hidden, not dropped)");
        assert!(
            root_after.hidden,
            "hidden root's ConnectionChoice.hidden must be true; hidden key: {root_key_str}"
        );
    }

    /// A key listed in `pinned` must float to the front of the choice list.
    #[tokio::test]
    async fn pinned_overlay_floats_entry_to_front() {
        let (coordinator, registry) = make_coordinator();

        // Add a credential-free saved profile so it appears as NeedsOpen (no vault dependency).
        let saved_id = Uuid::nil(); // stable, predictable UUID for test
        let mut config = Config {
            connections: vec![ConnectionProfile {
                id: saved_id,
                display_name: "my-server".to_owned(),
                scheme: "sftp".to_owned(),
                endpoint: std::collections::BTreeMap::new(),
                secret_ref: None,
            }],
            ..Config::default()
        };

        // First run without pinning: saved profile appears after builtins.
        let (choices_before, _, _) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;
        let saved_pos_before = choices_before
            .iter()
            .position(|c| c.label == "sftp: my-server")
            .expect("saved profile must appear in pre-pin run");
        assert!(
            saved_pos_before > 0,
            "without pinning, saved profile must follow the builtin roots"
        );

        // Now pin the saved profile by its key string.
        let saved_key_str = format!("saved:{saved_id}");
        config.discovery.pinned = vec![saved_key_str];

        let (choices_pinned, _, _) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;
        let saved_pos_pinned = choices_pinned
            .iter()
            .position(|c| c.label == "sftp: my-server")
            .expect("saved profile must still appear after pinning");
        assert_eq!(
            saved_pos_pinned, 0,
            "pinned entry must be the very first choice"
        );
        assert!(
            choices_pinned[saved_pos_pinned].pinned,
            "a pinned entry's ConnectionChoice.pinned must be true (P6)"
        );
    }

    /// A key listed in `pinned` that does not exist in the discovered set is silently ignored.
    #[tokio::test]
    async fn pinned_nonexistent_key_is_silently_ignored() {
        let (coordinator, registry) = make_coordinator();
        let mut config = test_config();
        config.discovery.pinned = vec!["kube:kubeconfig".to_owned()]; // k8s not built in lean

        // Must not panic or error; just produce the normal builtin-only list.
        let (choices, _, _) = coordinator
            .run(&registry, &ctx(&config), &HashMap::new())
            .await;
        assert!(
            !choices.is_empty(),
            "builtin roots must still appear even if pinned key is absent"
        );
    }

    // ── P3: saved-wins dedup ─────────────────────────────────────────────────────────────────

    /// A saved "docker" profile suppresses a Discovered Docker socket for the same type.
    ///
    /// Tests `dedup_discovered_vs_saved` directly with injected descriptors so the test is
    /// hermetic (no Docker daemon required) and deterministic in CI.
    #[test]
    fn saved_docker_profile_suppresses_discovered_docker_socket() {
        use super::super::descriptor::Reachability;
        use cairn_core::DiscoverySource;
        use std::collections::BTreeMap;

        // A discovered Docker socket entry (as DockerProvider would produce).
        let discovered = ConnectionDescriptor {
            id: ConnectionId(0),
            key: ConnectionKey::Docker {
                socket_path: "default".to_owned(),
            },
            provenance: DescriptorProvenance::Discovered {
                source: DiscoverySource::Docker,
            },
            scheme: "docker".to_owned(),
            display_name: "docker (default)".to_owned(),
            target: OpenTarget::DockerSocket { path: None },
            reachability: Reachability::Ready,
        };

        // A manually-saved Docker profile (as SavedProfileProvider would produce).
        let saved = ConnectionDescriptor {
            id: ConnectionId(0),
            key: ConnectionKey::Saved(Uuid::nil()),
            provenance: DescriptorProvenance::Saved {
                profile_id: Uuid::nil(),
            },
            scheme: "docker".to_owned(),
            display_name: "docker: my-docker".to_owned(),
            target: OpenTarget::Profile(ConnectionProfile {
                id: Uuid::nil(),
                scheme: "docker".to_owned(),
                display_name: "my-docker".to_owned(),
                endpoint: BTreeMap::new(),
                secret_ref: None,
            }),
            reachability: Reachability::Ready,
        };

        let raw = vec![discovered, saved];
        let deduped = dedup_discovered_vs_saved(raw);

        assert_eq!(
            deduped.len(),
            1,
            "discovered docker entry must be suppressed"
        );
        assert!(
            matches!(deduped[0].provenance, DescriptorProvenance::Saved { .. }),
            "the saved entry must be retained"
        );
    }

    /// A saved "kubernetes" profile suppresses discovered kubeconfig and in-cluster entries.
    #[test]
    fn saved_k8s_profile_suppresses_discovered_kubeconfig_and_incluster() {
        use super::super::descriptor::Reachability;
        use cairn_core::DiscoverySource;
        use std::collections::BTreeMap;

        let discovered_kubeconfig = ConnectionDescriptor {
            id: ConnectionId(0),
            key: ConnectionKey::Kubeconfig,
            provenance: DescriptorProvenance::Discovered {
                source: DiscoverySource::Kubeconfig,
            },
            scheme: "kubernetes".to_owned(),
            display_name: "k8s: (kubeconfig)".to_owned(),
            target: OpenTarget::KubeconfigDefault,
            reachability: Reachability::Ready,
        };

        let discovered_incluster = ConnectionDescriptor {
            id: ConnectionId(0),
            key: ConnectionKey::InCluster,
            provenance: DescriptorProvenance::Discovered {
                source: DiscoverySource::Kubeconfig,
            },
            scheme: "kubernetes".to_owned(),
            display_name: "k8s: (in-cluster)".to_owned(),
            target: OpenTarget::InCluster,
            reachability: Reachability::Ready,
        };

        let saved_k8s = ConnectionDescriptor {
            id: ConnectionId(0),
            key: ConnectionKey::Saved(Uuid::nil()),
            provenance: DescriptorProvenance::Saved {
                profile_id: Uuid::nil(),
            },
            scheme: "kubernetes".to_owned(),
            display_name: "kubernetes: my-cluster".to_owned(),
            target: OpenTarget::Profile(ConnectionProfile {
                id: Uuid::nil(),
                scheme: "kubernetes".to_owned(),
                display_name: "my-cluster".to_owned(),
                endpoint: BTreeMap::new(),
                secret_ref: None,
            }),
            reachability: Reachability::Ready,
        };

        let raw = vec![discovered_kubeconfig, discovered_incluster, saved_k8s];
        let deduped = dedup_discovered_vs_saved(raw);

        assert_eq!(
            deduped.len(),
            1,
            "both discovered k8s entries must be suppressed"
        );
        assert!(
            matches!(deduped[0].provenance, DescriptorProvenance::Saved { .. }),
            "only the saved entry must remain"
        );
    }

    /// Without any saved profiles, no discovered entries are dropped.
    #[test]
    fn dedup_is_noop_without_saved_profiles() {
        use super::super::descriptor::Reachability;
        use cairn_core::DiscoverySource;

        let discovered = ConnectionDescriptor {
            id: ConnectionId(0),
            key: ConnectionKey::Docker {
                socket_path: "default".to_owned(),
            },
            provenance: DescriptorProvenance::Discovered {
                source: DiscoverySource::Docker,
            },
            scheme: "docker".to_owned(),
            display_name: "docker (default)".to_owned(),
            target: OpenTarget::DockerSocket { path: None },
            reachability: Reachability::Ready,
        };

        let raw = vec![discovered];
        let deduped = dedup_discovered_vs_saved(raw);
        assert_eq!(
            deduped.len(),
            1,
            "without saved profiles, nothing is suppressed"
        );
    }
}
