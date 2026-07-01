# RFC-0011: Connection Management — Discovery, the Connection UI, and Credential Provisioning

- **Status:** Draft
- **Author(s):** software-architect, container-backend-engineer, kube-staff-engineer, tui-engineer,
  ux-engineer, security-engineer (synthesized) — `security-review` required before Accepted
- **Date:** 2026-06-30
- **Tracking item:** M4-5 (new-connection TUI), plus connection auto-discovery and credential
  provisioning (the missing onboarding path for remote backends)

## Summary

Today every backend (SSH, S3, GCS, Azure, Docker, Kubernetes) is implemented and reachable, but a
user can only open one by hand-editing `config.toml`, and credential-bearing backends cannot be set
up at all because there is no way to put a secret into the vault from the UI. This RFC specifies the
end-to-end connection-management experience:

1. **Auto-discovery** — Docker and Kubernetes connections appear in the switcher with zero
   configuration (Docker socket/Podman; merged kubeconfig contexts; in-cluster), enumerate-only and
   non-blocking.
2. **A unified connection model** — a `ConnectionProvider` abstraction and a `ConnectionCoordinator`
   that merges built-in roots, saved profiles, and discovered connections into one switcher list with
   *provenance* and *reachability status*, replacing today's eager-open `register_connections`.
3. **Lazy, on-select opening** — enumerate cheaply at startup; open a backend only when the user
   selects it (discovery can surface dozens of contexts; eager-opening them would block startup).
4. **An in-app connection UI** — `Ctrl-N` to add, plus edit/remove, with a per-scheme form whose
   fields are derived from the single source of truth in `connect.rs`.
5. **Credential provisioning** — capture a secret in a zeroizing field and either **reference an
   existing OS credential source** (ssh-agent, `~/.aws` profile, gcloud ADC, Azure AD) or copy it
   into the encrypted vault, with vault-create-from-UI as the security gate. The raw secret never
   enters `cairn-core` state, logs, or the AI/plugin layers.

The two load-bearing decisions, both approved: **reference-first** credentials (copy into the vault
only on explicit raw entry), and a **full phase-ordered** delivery (P0→P6).

## Motivation

The connection switcher (`Ctrl-O`) currently lists only built-in local roots plus whatever
`[[connections]]` profiles the user has hand-written; credential-bearing profiles also require a
vault credential that nothing can create. So:

- **Docker/Kubernetes** work but are invisible until the user writes a profile by hand — even though
  the daemon socket and kubeconfig are right there to be discovered.
- **SSH/S3/GCS/Azure** cannot be configured end-to-end at all: `connect.rs` resolves a profile's
  `secret_ref` through the broker, but there is no UI/CLI to *store* a credential, and
  `run_vault_unlock_effect` explicitly refuses to *create* a vault.

The result is that v0.1's backends are unreachable to a normal user. This RFC closes that gap while
preserving the project's hard invariants: the reducer stays pure, startup never blocks on the
network, `ConnectionProfile` stays secret-free, and secret material never traverses `cairn-core` or
the AI/plugin boundary.

## Guide-level explanation

### What changes for a user

- **Docker and Kubernetes just appear.** Launch Cairn, press `Ctrl-O`, and a `DISCOVERED` section
  lists your local Docker daemon (and rootless/Podman if present) and your kubeconfig (one `k8s`
  entry; navigate into it to pick a context). Nothing to configure. An in-cluster entry appears when
  running inside a pod.
- **The switcher is sectioned**: `SAVED` (your profiles), `DISCOVERED` (Docker/K8s), `LOCAL`
  (`/`, `~`). A status column shows reachability (`up` / `?` / unreachable / `auth` / `🔒 locked`),
  filled in lazily so opening the switcher is instant.
- **Add a connection with `Ctrl-N`.** Pick a scheme → fill a short per-backend form → choose how to
  authenticate → optionally test → save. Edit or remove from the switcher (`e` / `d`).
- **Credentials, reference-first.** If you already have ssh-agent, an `~/.aws` profile, gcloud ADC,
  or Azure AD, Cairn references it — nothing is copied. Only when you paste a raw key/password does
  Cairn store it in its encrypted vault. The first time a secret needs a home, Cairn walks you
  through creating the vault (a master passphrase). A browse-only user never sees the vault.

### Manifest of the user-visible surface

- New keybinding `Ctrl-N` = new connection (also available as `[Ctrl-N] New` hint inside the switcher; `e`/`d` contextually edit/delete inside the switcher).
- New `[discovery]` section in `config.toml` (opt-out, hidden/pinned entries) — see below.
- New overlays: the connection form, the vault-create prompt, the remove-connection confirm.

## Reference-level explanation

### 1. Unified connection model

Two layers separated by the `cairn-core` boundary so secrets and open-thunks never enter the pure
core.

**Runtime descriptor (binary side, `crates/cairn/src/connect/`):**

```rust
struct ConnectionDescriptor {
    id:           ConnectionId,    // assigned per enumeration by the coordinator
    key:          ConnectionKey,   // STABLE, content-derived identity (see below)
    provenance:   Provenance,      // Builtin | Saved { profile_id } | Discovered { source }
    scheme:       String,
    display_name: String,
    target:       OpenTarget,      // LocalRoot(PathBuf) | Profile(ConnectionProfile) | DockerSocket{..} | Kubeconfig{..}
    reachability: Reachability,    // Unknown | Reachable | Unreachable(redacted) | NeedsVault
}
enum Provenance { Builtin, Saved { profile_id: Uuid }, Discovered { source: DiscoverySource } }
enum DiscoverySource { Docker, Kubeconfig }
```

`ConnectionKey` is a stable, content-derived identity (`builtin:/path`, the saved profile `Uuid`,
`docker:socket:<path>`, `kube:context:<name>`) used to persist hidden/pinned state and to reuse a
live `ConnectionId` across re-enumeration. `OpenTarget` carries only non-secret open instructions;
the secret is still resolved through the broker at open time exactly as `ConnectionOpener::open`
does now.

**Pure core projection (`cairn-core`, additive to `ConnectionChoice`):**

```rust
struct ConnectionChoice {
    conn:       ConnectionId,
    label:      String,
    provenance: ChoiceProvenance,  // Builtin | Saved | Discovered { source }   (for the icon/badge)
    status:     ChoiceStatus,      // Ready | NeedsOpen | NeedsVault | Unreachable
    kind:       ConnectionKind,    // Profile { id } (editable) | AutoDiscovered (display-only)
}
```

`status` is a pure data projection — no thunks, no secrets. It is what lets the reducer route a
selection purely:

- `Ready` → `navigate_to_conn` (today's path; eagerly-mounted local roots).
- `NeedsOpen` → emit `AppEffect::OpenConnection { conn }`.
- `NeedsVault` → emit the unlock overlay instead of erroring.

The authoritative `HashMap<ConnectionId, ConnectionDescriptor>` lives runtime-side (next to the
other control maps in `event_loop`); the reducer only ever emits `OpenConnection { conn }` and the
runtime looks the descriptor up.

### 2. The `ConnectionProvider` abstraction and coordinator

```rust
#[async_trait]
trait ConnectionProvider: Send + Sync {
    fn source_id(&self) -> &'static str;            // "builtin" | "saved" | "docker" | "kubeconfig"
    /// Enumerate-only. MAY do cheap, offline-safe IO (read kubeconfig, ping a local socket).
    /// MUST NOT open backends over the network, run credential plugins, or block startup.
    async fn discover(&self, ctx: &DiscoveryCtx) -> Vec<ConnectionDescriptor>;
}
```

`ConnectionCoordinator` replaces the body of `register_connections`: it runs all providers
concurrently and time-bounded, merges and de-duplicates by `ConnectionKey` (Saved wins over
Discovered), applies the `[discovery]` config overlay (drop `hidden`, float `pinned`), assigns
`ConnectionId`s **reusing the id for any key already mounted in a pane**, eager-mounts only `Ready`
connections, and returns `(Vec<ConnectionChoice>, HashMap<ConnectionId, ConnectionDescriptor>)`.

Providers: `BuiltinLocalProvider` and `SavedProfileProvider` (architecture-owned), `DockerProvider`
(in `cairn-backend-docker`, feature-gated) and `KubeconfigProvider` (in `cairn-backend-k8s`).

### 3. Docker discovery

`DockerProvider::discover` probes, concurrently, each candidate with a `tokio::time::timeout`
(~500 ms) `ping`:

- `connect_with_local_defaults()` (reads `$DOCKER_HOST`; defaults to `/var/run/docker.sock` /
  `//./pipe/docker_engine`) → `"Docker (local)"`.
- rootless `$XDG_RUNTIME_DIR/docker.sock` → `"Docker (rootless)"`.
- Podman `$XDG_RUNTIME_DIR/podman/podman.sock` → `"Podman (local)"`.

Unreachable/absent candidates are debug-logged and surfaced with `Unreachable`/dropped (UX shows a
status badge rather than hard-hiding). Docker contexts (`~/.docker/config.json`) and remote
`DOCKER_HOST` over TLS are deferred. A new `BollardDocker::connect_with_socket(path)` constructor is
required (the existing `connect_local()` stays for profile-driven opens).

### 4. Kubernetes discovery

`KubeconfigProvider::discover` is **purely kubeconfig parsing — no network, no client construction,
no exec-credential plugins**:

- Resolve `$KUBECONFIG` (via `std::env::split_paths` for cross-platform separators), else
  `~/.kube/config`; merge; missing files skipped silently; malformed → redacted warning, skip.
- Emit **one** `"k8s"` entry per merged kubeconfig (the backend already models
  `/<context>/<namespace>/<pod>/<container>` and `list_contexts()` returns contexts at depth 0 with
  no network). Per-context switcher entries are a v2 opt-in via a pinned-context profile.
- Emit a second `"k8s: (in-cluster)"` entry when `KUBERNETES_SERVICE_HOST` is set and the
  service-account token is readable; `open_k8s` gains a `source = "in-cluster"` branch that calls
  `Config::incluster()`.

Connectivity (and therefore any exec-credential plugin) happens only at navigation time. The whole
`discover` call is wrapped in `spawn_blocking` (YAML parsing). `KubeRsOps::list_contexts` must be
audited to never build a `kube::Client`.

### 5. The connection form (TUI)

A new `Overlay::ConnectionForm { stage, scheme, values, focus, field_errors, editing_id,
existing_secret_ref, .. }` with two stages: `SchemePicker` (a list navigator, does not capture text)
and `Fields` (captures text). The field set is a **static, pure schema** derived from `connect.rs`:

```rust
// cairn-core/src/forms.rs
struct FieldSpec { key: &'static str, label: &'static str, secret: bool, required: bool,
                   hint: Option<&'static str>, placeholder: &'static str }
fn scheme_fields(scheme: &str) -> &'static [FieldSpec]
// ssh   → [name*, host*, user*, port, known_hosts, host_key, credential*]
// s3    → [name*, bucket*, region, endpoint, root, force_path_style, credential*]
// gcs   → [name*, bucket*, endpoint, credential*]
// azure → [name*, account*, container*, endpoint, root, credential*]
```

`FieldValue` is `Plain(String)` or `Secret(MaskedInput)`; its `Clone` returns an empty
`MaskedInput` for the secret variant and its `Debug` prints `Secret(<redacted>)`. `TextEdit` gains
`NextField`/`PrevField` (bound to `Tab`/`BackTab` while capturing in the form; no-ops elsewhere).
`Action` gains `NewConnection` (`Ctrl-N` global; also reachable via `[Ctrl-N] New` hint in the switcher), `EditConnection`,
`DeleteConnection` (contextual in the switcher; no-ops on `AutoDiscovered` rows). The reducer's
`capturing_text` and `advance_queue` block-list are extended for the `Fields` stage. Submission
validates required fields purely, then extracts non-secret fields into `ConnectionProfile.endpoint`
and the secret via `MaskedInput::take_secret()` into the provisioning effect. Render is driven
entirely off `scheme_fields` — no per-scheme branching in the renderer.

### 6. Credential provisioning (reference-first) — the security gate

The non-negotiable rule: the raw secret exists only as a `MaskedInput` (core state) → a typed
**`CredentialDraft`** carrying `SecretString` fields + non-secret identifiers (an owned effect with
redacting `Debug`) → the typed `CredentialSecret` assembled **at the binary edge** (the effect
runner, which already depends on `cairn-vault`) → vault ciphertext. `cairn-core` never names
`cairn-vault`; `AppEffect` never carries `CredentialSecret` (which has no `Debug` by design — it
carries `SecretString`/`CredentialDraft` instead). This keeps the dep-closure isolation test intact.

**Reference-first.** `CredentialSecret` already encodes both copy and reference for every scheme:
copy variants hold `SecretString`; delegation variants (`Ssh::Agent`, `Aws::Profile`/`DefaultChain`,
`Gcp::ApplicationDefault`, `Azure::AzureAd`) hold **zero** material. The form defaults to the
delegation variant when the OS source is detected (enumerating *names/existence only*, never reading
secret bytes), and copies into the vault only when the user explicitly provides raw material that has
no managed home (a bare SSH password, a SAS token, a static MinIO keypair).

**Four gaps to close:**

1. `Broker::store(actor, label, secret) -> CredentialId` (execution-layer only; `vault.add` +
   `save`; journal records *kind + label only*) and a `BrokerError::Io`.
2. **Vault-create-from-UI** — a new `Overlay::VaultCreate` (passphrase + confirm via `MaskedInput`,
   optional "remember on this device" → OS keychain) and `AppEffect::CreateVault`, lifting the
   `run_vault_unlock_effect` "not yet available" block. Storing requires an unlocked vault, so this
   is a hard prerequisite for credential provisioning, deferred to the moment the first secret needs
   a home.
3. A new SSH **`PrivateKeyFile { path, passphrase: Option<SecretString> }`** `CredentialSecret`
   variant so an on-disk key is *referenced*, not copied (`#[non_exhaustive]`, non-breaking).
4. The capture form (§5).

**Per-scheme shapes:** SSH → agent | key file | inline PEM | password; AWS → profile/default-chain |
static (access-key-id [plain] + secret-access-key + optional session-token); GCP → ADC |
service-account JSON (load-from-file preferred over paste); Azure → AzureAd | shared key | SAS |
connection-string (parsed in the effect runner, raw string dropped).

### 7. Config schema evolution

```toml
[discovery]            # all fields optional; defaults preserve current behavior
docker     = true      # opt-out
kubernetes = true      # opt-out
hidden     = []        # ConnectionKeys the user hid
pinned     = []        # ConnectionKeys floated to top, in order
```

Additive `#[serde(default)]` fields on `Config`; **no `SCHEMA_VERSION` bump** (the existing
`version > SCHEMA_VERSION` guard still protects against genuinely newer files). Default-on is safe
because discovery is enumerate-only and offline. **No trust gate** is added (these toggles are
non-secret and non-executable, unlike `shell_actions`). `ConnectionProfile` stays secret-free
(`secret_ref` only).

## Drawbacks

- Replacing eager-open with lazy-open is a behavioral change (a connection isn't mounted until
  selected); it gets its own ADR.
- A second `ConnectionId` allocation pass per re-enumeration must reuse ids for live panes or a
  refresh could repoint a pane (called out as an implementation invariant).
- Reference-first depends on ambient OS state being present/correct (the "confused deputy" risk);
  mitigated by showing which identity a row references and offering vault-copy as the fallback.

## Rationale and alternatives

- **One `k8s` entry vs one-per-context:** the backend already trees contexts; a single entry avoids
  switcher clutter for users with many contexts and matches the Docker "one entry, not one per
  container" model. Per-context pinning is a v2 opt-in.
- **Show-with-status vs hide-unreachable:** showing every discovered/saved row with a lazy status
  badge beats silently dropping rows — discoverability over secrecy, and it unifies Docker's
  cheap-probe model with K8s's no-probe model.
- **Copy-only vault** was rejected (see the approved decision): referencing wins on blast radius and
  single-source-of-truth, the properties that matter most for a secrets tool.

## Security and privacy considerations

`security-review` is mandatory on the provisioning phases. Invariants:

- Secret lives only as `MaskedInput` → `SecretString`/`CredentialDraft` (redacting `Debug`) → vault
  ciphertext; never a plain `String`, never in `Debug`/logs, never in `cairn-core`, never to
  `cairn-ai`/`cairn-plugin` (the `cargo metadata` isolation test stays green).
- Vault: Argon2id (`recommended()`), AEAD, atomic temp-file+rename, file mode `0600` (explicit).
- Recommend the binary set `RLIMIT_CORE=0` at startup so a crash mid-entry can't dump heap secrets;
  prefer load-from-file for PEM/GCP-JSON to avoid clipboard retention.
- All error paths use fixed-category, value-free messages (`VfsError::redacted()` /
  `OpenError`/`VaultError`); the broker journal records actor + id + label + kind only.
- Reference variants store zero material, so a vault compromise yields nothing for them.

## Unresolved questions

1. SSH `known_hosts` / host-key TOFU defaults in the form (`accept-new` vs `strict`) — confirm with
   `network-engineer` before P5.
2. Whether the in-cluster K8s entry should appear alongside a kubeconfig entry or replace it when
   both are present (lean: both).
3. "Test connection" credential path needs a small `ConnectionOpener::open_with_credential` (bypass
   the vault for an ephemeral probe) — finalize its shape in P6.
4. Editing a saved credential when it is referenced by multiple profiles (delete-shared warning).

## Crate, dependency, and feature impact

- `cairn-core`: new `forms.rs`; additive `Overlay`/`Action`/`AppEffect`/`AppEvent`/`TextEdit`
  variants; extended `ConnectionChoice`. No new deps.
- `cairn-config`: `DiscoveryConfig`. No new deps.
- `cairn/src/connect/`: `descriptor`, `coordinator`, `provider` submodules; lazy-open in `app.rs`.
- `cairn-backend-docker` (feat `docker`): `discovery.rs` + `connect_with_socket`.
- `cairn-backend-k8s` (feat `k8s`): `discovery` + `source = "in-cluster"`; audit `list_contexts`.
- `cairn-broker`: `store` + `BrokerError::Io`. `cairn-vault`: `PrivateKeyFile` variant, vault-create
  wiring, explicit `0600`. No new external dependencies anticipated.

## Phased PR plan

Delivery is **full phase order** (approved). Each phase is a PR (or small PR set) with the §6 gates;
the credential phases also require `security-review`.

| Phase | Scope | Vault? | Gate |
|---|---|---|---|
| **P0** | This RFC + additive `provenance`/`status`/`kind` on `ConnectionChoice` (defaults preserve current behavior; no runtime change) | — | code-review |
| **P1** | `ConnectionProvider` + `ConnectionCoordinator`; refactor `register_connections` into it with `BuiltinLocalProvider` + `SavedProfileProvider`; establish the runtime side-map. Identical observable output. | — | bug-bot, code-review |
| **P2** | Lazy open: `OpenConnection`/`ConnectionOpened`, `NeedsOpen`/`NeedsVault` status routing, selection opens/unlocks. Behavioral ADR. | — | bug-bot, code-review |
| **P3** | `DockerProvider` ∥ `KubeconfigProvider`; `[discovery]` config; switcher provenance badges + lazy reachability status. | no | bug-bot, code-review |
| **P4** | Connection form (`Ctrl-N`) + edit/remove for `local` and credential-less profiles; persist + re-enumerate. | no | bug-bot, code-review |
| **P4.5** | **Vault-create-from-UI** (`Overlay::VaultCreate`, `CreateVault`, lift the block). | — | bug-bot, code-review, **security-review** |
| **P5** | Credential provisioning: `CredentialDraft`, `Broker::store`, reference-first per-scheme capture, `PrivateKeyFile` variant; SSH/S3/GCS/Azure end-to-end. | yes | bug-bot, code-review, **security-review** |
| **P6** | Edit endpoint fields, "test connection", remove-with-credential-cleanup, reachability sweep tuning, discovered-entry rename/pin/hide. | — | bug-bot, code-review |

Critical paths: `P0→P1→P2→P3` (discovery) and `P0→P1→P4→P4.5→P5→P6` (provisioning); P3 ∥ P4. The
security gate is `P4.5→P5` — credential provisioning cannot land until vault-create-from-UI does and
`security-review` passes.

### Companion ADRs (accepted as phases land)

- ADR: `ConnectionProvider` abstraction + unified descriptor/key model.
- ADR: lazy, on-select opening replaces eager mounting.
- ADR: discovery is enumerate-only, offline-safe, opt-out via config.
- ADR: credential provisioning — secrets never traverse `cairn-core`; reference-first posture.
