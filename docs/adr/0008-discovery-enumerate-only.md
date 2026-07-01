# ADR-0008: Connection Discovery Is Enumerate-Only, Offline-Safe, and Opt-Out

- **Status:** Accepted
- **Date:** 2026-06-30
- **Deciders:** zoran.vukmirica@gmail.com (human maintainer), rust-staff-engineer (AI)

## Context

RFC-0011 P3 adds Docker socket and Kubernetes cluster auto-discovery. Discovery runs at startup
as part of `ConnectionCoordinator::run`, which is called before the first TUI frame is drawn.
Two concerns had to be settled:

1. **Safety**: Discovery must not perform actions that have side-effects or expose credentials —
   a `ConnectionProvider` is called on every startup and on every re-enumeration (future config
   reload). Making a network connection, running an exec-credential plugin, or spawning a process
   would be surprising and potentially harmful.

2. **Availability**: A slow or stalled discovery source (e.g. Docker socket unresponsive, kubeconfig
   on a network filesystem) must not block the TUI from drawing or delay the startup path for
   unrelated providers.

## Decision

**Discovery is enumerate-only, offline-safe, and opt-out.** Concretely:

### 1. `ConnectionProvider::discover` is enumerate-only

The `discover` contract (enforced by code review, not the type system) forbids:
- Network connections of any kind (TCP, UDP, HTTP).
- Running exec-credential plugins (e.g. `aws eks get-token`, `gcloud`, `kubelogin`).
- Spawning external processes.
- Writing to any persistent state.

Permitted I/O:
- Reading environment variables.
- Checking the existence of a file (e.g. SA token at `/var/run/secrets/…`).
- Parsing a local file in a `spawn_blocking` task (e.g. `~/.kube/config`).
- Attempting a Unix socket connection with a tight timeout (Docker probe).

### 2. All blocking I/O is time-bounded

Docker socket probes are wrapped in a per-probe `tokio::time::timeout` of 500 ms. The kubeconfig
YAML parse is wrapped in a per-task `tokio::time::timeout` of 2 s. All providers are additionally
wrapped in a per-provider `tokio::time::timeout` of 5 s in the coordinator.

Timeouts degrade gracefully: a timed-out provider returns an empty list for its entries only;
the results of other providers (and the builtin/saved entries) are unaffected. The coordinator
never uses a single outer timeout over `join_all` — doing so could discard all results when one
provider is slow and leave the switcher empty.

**Note on blocking threads:** `spawn_blocking` tasks that time out cannot be cancelled — the
underlying OS thread leaks until the FS responds. This is acceptable: the async task (and thus
the UI) unblocks immediately, and the thread is reclaimed once the FS completes the I/O or the
process exits.

### 3. Discovery is opt-out via `[discovery]` config

`DiscoveryConfig` in `cairn.toml` (added to `cairn-config`, `#[serde(default)]`, no schema
version bump) gives users full control:

```toml
[discovery]
docker = true          # set to false to disable Docker socket probing
kubernetes = true      # set to false to disable kubeconfig/in-cluster probing
hidden = []            # key strings of entries to suppress (any type)
pinned = []            # key strings to float to the top of the switcher (any type)
```

Toggling `docker = false` or `kubernetes = false` causes the respective provider to return an
empty list immediately without probing. This is the escape hatch for environments where even the
socket probe is undesirable (e.g. security-sensitive deployments, containers without Docker access).

### 4. Discovery providers are feature-gated

`DockerProvider` and `KubeconfigProvider` are compiled only when the `docker` / `k8s` Cargo
features are enabled. The lean build (no features) compiles and passes clippy cleanly. The
`OpenTarget::DockerSocket`, `OpenTarget::KubeconfigDefault`, and `OpenTarget::InCluster` variants
exist in all builds for match exhaustiveness; feature-gated dead-code suppression prevents
warnings in lean mode.

### 5. Reducer purity is preserved

Discovery results arrive as `Vec<ConnectionDescriptor>` from providers. The coordinator converts
them to `Vec<ConnectionChoice>` (pure data, no I/O). The reducer receives only `ConnectionChoice`
values — it never sees a `ConnectionDescriptor`, never calls a provider, and never opens a
connection. The reducer stays pure per the TEA architecture.

### 6. Saved entries win over discovered entries (dedup)

When the user has a manually-saved profile for a backend type that is also auto-discovered (e.g.
a saved `"docker"` profile + a discovered Docker socket), the saved entry is retained and the
discovered entry is suppressed. This comparison is at the backend-type level (not per-socket) and
is applied in the coordinator after providers run, before hidden/pinned overlays.

## Consequences

### Positive

- **Instant startup.** Discovery completes asynchronously in parallel with the first frame; no
  single slow source blocks others.
- **No credential exposure.** Discovery never reads vault secrets, runs plugin processes, or
  makes network calls beyond a local socket probe.
- **Predictable.** Every timeout is documented; degradation is local to the timed-out provider.
- **Opt-out.** Users or operators who don't want auto-discovery can disable it per-backend or
  suppress individual entries via `hidden`.

### Negative / trade-offs

- **Blocking threads may leak on timeout.** `spawn_blocking` tasks cannot be cancelled by
  `tokio::time::timeout`. A stalled kubeconfig read leaks a thread until the FS responds or the
  process exits. Accepted: Tokio's blocking thread pool is sized generously (512 threads by
  default), and the leak is bounded by the number of providers with blocking I/O (currently one).
- **Backend-type-level dedup is coarse.** A user with two saved Docker profiles won't get the
  default socket auto-discovered even if neither saved profile refers to it. Finer-grained dedup
  (per-socket path comparison) is deferred to P4.
- **`source_id()` is currently unused.** The method is defined on `ConnectionProvider` for
  logging and future extensibility (P4 provenance filtering), but the coordinator only uses it for
  `tracing::warn!` on timeout. If source_id is never used further by P5, consider removing it.

### Neutral

- `cairn-core` gains no new secret or vault dependencies; the isolation test remains green.
- The `DeferredConnection` struct (always-empty in P2/P3) is retained for API stability;
  expected removal in P4.

## Alternatives considered

- **Discovery with network calls (e.g. ping the Kubernetes API server at startup).** Rejected:
  violates the startup-latency and credential-safety constraints. Network probes belong in the
  open effect, not in discovery.
- **Single outer timeout over `join_all`.** Rejected: if one provider is slow, all results are
  discarded, leaving the switcher empty. Per-provider timeouts degrade locally.
- **No timeout on `spawn_blocking`.** Rejected: a `~/.kube/config` on a stalled NFS mount would
  block the TUI indefinitely. The 2 s timeout is the same order of magnitude as the Docker probe
  timeout and acceptable for a config parse.
- **Inline the providers into the coordinator (no trait).** Rejected: the trait makes it easy to
  add future providers (P4: WASM plugin sources, SSH config, `/etc/hosts`) without touching the
  coordinator logic.

## References

- `docs/rfcs/0011-connection-management.md` — full RFC with P0-P6 phases and P3 spec (§3 Docker,
  §4 Kubernetes, §7 config schema)
- `crates/cairn/src/connect/provider.rs` — `ConnectionProvider` trait + P3 providers
- `crates/cairn/src/connect/coordinator.rs` — `ConnectionCoordinator::run`, per-provider timeout,
  dedup, hidden/pinned overlays
- `crates/cairn-backend-docker/src/discovery.rs` — `probe_sockets()` (500 ms per-socket timeout)
- `crates/cairn-backend-k8s/src/discovery.rs` — `probe_kubeconfig()` (2 s spawn_blocking timeout)
- `crates/cairn-config/src/lib.rs` — `DiscoveryConfig`
- ADR-0006 — feature-gated backends and lean/full CI split
- ADR-0007 — lazy connection opening (P2 base)

<!--
  ADRs are immutable once Accepted. To change a decision, write a new ADR that supersedes this one
  and update the Status line above to "Superseded by ADR-XXXX".
-->
