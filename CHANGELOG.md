# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **MC-style transfer progress dialog**: copying or moving files now auto-opens a dialog (retitled
  " Transfer ") showing each active transfer as a label, a text progress bar (`█`/`░`, with an
  indeterminate `--%` bar when the total size isn't known yet), and a byte/rate/ETA line, above the
  existing pending-queue list. `b` sends it to the background (the transfer keeps running and the
  status line keeps its compact summary); `Ctrl-T` brings it back to the foreground; `p`
  pauses/resumes every active transfer from inside the dialog; `Esc` now aborts all active transfers
  (like the no-overlay binding) rather than just closing, degrading to a plain close when only
  pending (not yet started) items remain. The dialog dismisses itself once the last transfer
  finishes and nothing is left queued. The dialog is intentionally non-blocking: queued transfers
  keep draining into free slots while it's open, and an overwrite conflict still surfaces its prompt
  over it instead of being silently swallowed.

- **Pane connection header**: each pane's top border now shows *which* backend it is browsing, so a
  remote pane is instantly distinguishable from the local filesystem. A remote pane renders a full
  `scheme://user@host:path` locator (e.g. `ssh://root@dietpi6:/home`, `s3://bucket:/prefix`) in a
  distinct accent color (`remote` theme role, default yellow); a local pane shows just its path as
  before. The identity is joined at render time from the pane's connection id against the
  connection/profile registries, with a graceful path-only fallback when it can't be resolved (e.g.
  inside an archive mount). Add `remote = "<color>"` under `[ui.colors]` to recolor it.

- **Test a connection, pin/hide discovered entries** (RFC-0011 P6): the connection switcher
  (`Ctrl-O`) gains three new keys. `t` probes the highlighted entry's reachability without opening
  it into a pane or switching any pane — it reuses the same vetted per-scheme open path (so a real
  SSH/S3/GCS/Azure probe performs genuine credential resolution + a network handshake; Docker
  additionally pings the daemon) but never mounts the result, and a vault-locked entry reports
  "needs unlock" instead of forcing the vault overlay open. `P` pins an entry to the top of the
  list; `H` hides it from the default view — both persist to the existing (RFC-0011 P3)
  `[discovery].pinned`/`.hidden` config fields; what's new is the switcher UI writing to them, not
  the fields themselves. `S` toggles showing hidden entries for the current session
  so a hidden entry can always be found again and un-hidden (hiding is never a one-way trap).
  Pin/hide apply to any switcher entry (built-in, saved, or auto-discovered), not just discovered
  ones. Renaming a discovered entry is deferred (see the RFC's P6 section for why).

- **Edit remote files** (RFC-0012 P3, `M4-7`): `F4`/`Enter`-on-text now works on any backend, not
  just local files — a remote file is downloaded to a private temp copy (0600 permissions, `O_EXCL`,
  a freshly-minted non-predictable directory preferring `$XDG_RUNTIME_DIR`), edited through the same
  `$EDITOR` shell-out as local files, and written back once the editor exits (a no-op edit is
  detected by content hash and skipped — nothing is uploaded). Before writing back, the remote
  version is re-checked against a snapshot taken at download time (`etag` on S3/GCS/Azure;
  `modified`+`size` on SFTP/local; Docker/K8s report neither today, so they degrade to `Unknown` —
  moot for now since neither yet advertises write support; anything reporting neither always
  prompts); a mismatch — including the remote file having been deleted, or the local edit coming
  back empty while the original was not — opens a confirm overlay offering Overwrite / Save-as /
  Keep-editing / Discard, never a silent clobber. The actual write stages to a sibling name and
  renames it into place when the backend supports atomic rename (restoring the original file's
  permissions afterward, since staging writes to a new inode), or a direct overwrite otherwise —
  Docker/K8s (no rename) get a direct overwrite instead (documented non-atomic window). Files above
  100 MiB are refused up front with a clear message — editing a multi-GB file through `$EDITOR` is
  the wrong workflow. See RFC-0012's P3 section.

- **tmux end-to-end tests.** `crates/cairn/tests/tmux_e2e.rs` drives the real binary inside a tmux
  pane (`send-keys` / `capture-pane`) to cover the genuine raw-mode / alternate-screen / TTY handoff
  that a headless `TestBackend` can't — startup + clean quit, directory navigation, the F3 pager, and
  the crown-jewel `$EDITOR` suspend→edit→resume round-trip (driving an interactive editor that reads
  stdin, which validates the foreground-process-group / SIGTTIN behavior end to end). Env-guarded by
  `CAIRN_IT_TMUX=1` so the default `cargo test` stays hermetic; runs in a dedicated CI `tmux-e2e` job.

- **Browse tar/zip archives read-only** (RFC-0013 P4, `M4-8`): `Enter` on a local `.tar` or `.zip`
  file — recognized by magic bytes, never by extension — mounts it and browses it like any other
  directory. `..` at the archive's root leaves back to wherever the pane was before. Copying files
  out of the archive works through the normal copy/move flow. New `cairn-backend-archive` crate
  (`ArchiveVfs`, read-only: `Caps::LIST | Caps::READ | Caps::RANDOM_READ`), gated behind a new
  `archive` Cargo feature (included in `all-backends`). Hardened against untrusted archive bytes:
  bounded per-member and per-session decode caps, a zip compression-ratio (bomb) guard, an
  entry-count cap, path-traversal/absolute-path/UNC/drive-letter rejection, and inert (never
  followed) symlink/hardlink members. See RFC-0013 and ADR-0012.

- **Browse compressed tar archives** (RFC-0013 P5): `.tgz`/`.tar.gz`, `.tbz2`/`.tar.bz2`, and
  `.tzst`/`.tar.zst` now mount and browse exactly like a plain `.tar` — the whole file is
  decompressed once, up front, into a private (owner-only, RAII-deleted, preferring
  `$XDG_RUNTIME_DIR` when available) temp file, then indexed by the same tar indexer P4 already
  ships, so every existing tar guard applies unchanged. New decompression-bomb defenses: an
  incrementally-enforced absolute decoded-byte cap and a compression-ratio guard (generalized from
  P4's zip-only check), both aborting mid-decode the instant they trip rather than after the fact.
  A separate guard refuses (rather than silently truncates) concatenated multi-stream/multi-frame
  bzip2/zstd input that only partially decodes. All three decoders (gzip via `flate2`'s
  `MultiGzDecoder`, zstd via `ruzstd`, bzip2 via `bzip2-rs`) are pure-Rust — no C/FFI parsing
  untrusted compressed bytes; see ADR-0013 for the per-codec selection rationale and disclosed
  trade-offs. `.txz`/`.tar.xz` is recognized (magic-sniffed) but **not decoded** in this release —
  refused with a clear, typed error — because the only pure-Rust xz decoder evaluated is not
  memory-bounded against a decompression bomb; tracked as a follow-up.

- **Deterministic TUI snapshot testing.** A `scenarios` catalog in `cairn-tui` renders every screen
  (dual-pane, pager text/hex, log/AI-plan/transfer/connection/vault overlays, …) to a plain-text
  frame via ratatui's headless `TestBackend`. `insta` snapshot tests assert each scenario at 80×24
  and 40×12 (narrow-layout coverage), so a rendering regression is a readable `.snap` diff. A new
  `cairn --frame-dump <scenario> [WxH]` flag (and `--frame-dump-list`) prints one rendered frame to
  stdout for headless inspection — no TTY required. The catalog is the single source of truth shared
  by the snapshots and the dump flag.

### Changed

- **`cairn.toml` writes are now atomic** (temp file in the same directory + rename, fsynced before
  the rename — mirroring `cairn-vault`'s `atomic_write`). Applies to every config save path
  (connection add/edit/delete, and the new pin/hide toggles), not just the new RFC-0011 P6 code.
- **A hidden discovered connection is marked, not dropped** (RFC-0011 P6): the switcher previously
  removed a `[discovery].hidden` entry from enumeration entirely; it is now still enumerated (with
  `ConnectionChoice::hidden = true`) so the switcher's "show hidden" toggle (`S`) can reveal — and
  un-hide — it. The on-disk `[discovery]` schema is unchanged.

### Fixed

- **Transfer speed now shows the *current* rate, not a lifetime average.** With two transfers
  running at once, the first would drop to a few KB/s and appear to "never recover" even after the
  second finished — because the displayed rate was cumulative bytes ÷ total elapsed, which stays
  depressed long after real throughput returns. The rate (and ETA) are now measured over a short
  trailing window (~3 s), so they track actual speed and recover promptly. Removes the per-transfer
  pause-time accumulator task the old average needed.

- **Compressed tarballs (`.tar.gz`/`.tgz`, `.tar.bz2`, `.tar.zst`) now open and browse like folders
  on `Enter`.** The archive backend already supported mounting them, but the pure front-end
  file-kind sniff (`detect_file_kind`, which decides whether `Enter` mounts an archive) only
  recognized zip and *uncompressed* tar, so a `.tar.gz` fell through to the hex pager instead. It now
  recognizes the four outer-compression magics (gzip/bzip2/xz/zstd) at offset 0, matching the
  backend's `sniff_format`. `.tar.xz` is recognized but reports a clear "unsupported" message
  (xz decoding remains disabled for OOM-safety) rather than opening as binary.

- **Creating the vault no longer fails with "io error" on a fresh install.** The vault's config
  directory (e.g. `~/.config/cairn`) does not exist until something writes there, and vault creation
  (triggered the first time you save a credential — e.g. adding an SSH host with a password) failed
  because the atomic write couldn't create its temp file in a missing directory. Vault writes now
  create the parent directory first (owner-only `0700` on Unix). Regression test added.

- **Panes now root at the OS filesystem root** (`/` on Unix, drive root on Windows) so `..`
  navigation is unrestricted all the way up. Cairn still opens at the launch directory, but
  the user can navigate above it without hitting an artificial ceiling. The `LocalVfs` base is
  now the platform root derived from `std::path::Component` rather than `current_dir()`.

- **MC-style `..` parent entry**: every non-root directory listing now shows a synthetic `..`
  entry at position 0. Arrowing onto it and pressing Enter (or the existing Backspace/h/Left
  bindings) navigates to the parent directory. The `..` entry is never markable, never a target
  for copy/move/delete/rename, never included in filtered-away results (it stays visible at the
  top regardless of the active name filter), and never stored as a `VfsPath` containing `..` —
  it is a pure UI affordance that routes through the existing `leave_dir` path.

### Security

- **DNS connection pinning and IPv6 tunnel classification** (SEC-1 / issue #103, RFC-0010 PR-C1,
  M8-5): closed the DNS-rebinding TOCTOU race in the plugin HTTP sandbox. Replaced the
  pre-flight `check_ssrf_via_dns` call (which allowed reqwest to re-resolve the hostname
  independently at connect time) with a `PinnedSsrfDnsResolver` that implements
  `reqwest::dns::Resolve` and is installed as the sole DNS authority via
  `ClientBuilder::dns_resolver()`. The resolver resolves the hostname once via
  `tokio::net::lookup_host`, validates every returned `IpAddr` with `is_ssrf_blocked_ip`, and
  hands reqwest only the approved `SocketAddr` set — reqwest does not perform a second DNS
  lookup. This closes the rebind window entirely.  SEC-8 (IPv6 tunnels): 6to4 (`2002::/16`),
  Teredo (`2001::/32`), and NAT64 (`64:ff9b::/96`) prefixes are now classified by extracting
  the embedded IPv4 address and recursing through `is_ssrf_blocked_ip`.

- **Editor launch is hardened like the existing shell-action runner** (RFC-0012 P2, ADR-0011): the
  resolved `$VISUAL`/`$EDITOR` command is split via deterministic POSIX shell-word quoting
  (`shlex`) — never a shell, never glob/variable/command-substitution expansion — and spawned as a
  plain argv with a `--` terminator before the always-absolute target path (so a file named like a
  flag is never misparsed), a scrubbed environment (`env_clear()` + an explicit allow-list + a
  sanitized `PATH` — secrets such as `AWS_*`/`GITHUB_TOKEN`/`CAIRN_*`/`LD_PRELOAD` never reach the
  child), and its own process group.

### Added

- **Read-only file pager** (RFC-0012 P1, M4-7): `F3` opens the entry under the cursor in a
  scrollable, read-only pager; `Enter` on a file (previously a no-op) now reads a bounded prefix,
  classifies it as text or binary (a NUL-byte heuristic), and opens the pager in the matching
  `Text`/`Hex` mode. Binary content renders as classic `offset | hex | ascii` rows. The title bar
  shows the filename, a line/percentage position, and a status (`Loading…` / `Ready` /
  `Truncated — showing first N` / `Error: …`); the view is capped at ~8 MiB. `j`/`k`/arrows,
  `PageUp`/`PageDown`, `g`/`G`/`Home`/`End`, and `q`/`Esc` all work as in the existing log viewer.

- **Edit files with `$EDITOR`** (RFC-0012 P2, M4-7, ADR-0011): `F4` always opens
  `$VISUAL`/`$EDITOR`/`vi` on the entry under the cursor (MC-faithful — no text/binary sniff);
  `Enter` on a text file now opens the same editor instead of the read-only pager (`Enter` on a
  binary file is unchanged — still the hex pager). Cairn suspends its own screen, hands the editor
  the real terminal (a new `InputGate` pauses the blocking input-reader thread and waits for its
  ack so it never races the editor for keystrokes), and resumes with a full repaint on exit. Local
  files only for now — a file on a remote connection shows *"Editing remote files lands in P3 —
  copy it to a local pane to edit"* without disturbing the TUI. After the editor exits, the active
  pane's listing refreshes and the status line shows the outcome.

- **Docker image content browsing** (M6-2 follow-up, ADR-0010): entering `/images/<tag>` in the
  Docker backend now browses the image's actual rootfs instead of silently showing an empty
  listing. `DockerVfs` resolves a tag or raw image id to an ephemeral, **never-started** container
  (`docker create` only) on first access, keyed by the image's canonical id so tag and raw-id
  aliases of the same image share one container (digest references, e.g. `nginx@sha256:…`, are not
  yet resolved to the same container — tracked follow-up), and reuses the existing
  container-filesystem archive/tar `list_dir`/`stat`/`read` path against it unchanged. Two cleanup
  tiers keep the daemon clean: an idle-TTL reaper (5 min, with a re-check-under-lock immediately
  before evicting so a browse that resumes mid-reap keeps its container — see
  `evict_if_still_idle`) and a label+age crash-safety sweep (containers labeled
  `cairn.role=image-browse-ephemeral` older than 30 min and not tracked live by the current
  process are force-removed on real connection-open and every 10 min thereafter). A graceful-
  shutdown hook, and a fix for cross-instance sweeps reaping another instance's >30-min browse
  session, are deferred (see ADR-0010's Negatives). An image with neither `CMD` nor `ENTRYPOINT`
  configured (e.g. some `FROM scratch` images) still can't be browsed this way, but now fails with
  a clear `VfsError::Backend { code: "image_no_command", .. }` instead of an opaque daemon error.
  `EntryExt::Image.layers` is now populated from `inspect_image`'s `RootFS.Layers` count instead
  of a hardcoded `0`. `ContainerOps` gains `ephemeral_for_image` and `resolve_image_id` (the
  latter a cheap tag/id-only lookup used on every image-browse navigation step, so the
  `list_images`-only-needed-for-`layers` cost isn't paid per step). Extensive hermetic test
  coverage: routing (list/stat/read, tag-and-id resolve to the same container, a `/`-containing
  repo tag — e.g. `myorg/app:v1` — and an untagged `sha256:…` id are both listed/browsed by image
  id, unknown image → `NotFound`), reaper race protection (`evict_if_still_idle`'s snapshot vs.
  resumed-browse and slot-reuse cases), the tier-2 sweep's live-vs-orphan filtering, the
  no-command error mapping, and the `OnceCell` retry-after-failed-create contract — plus one
  `CAIRN_IT_DOCKER`-gated dind integration test.

- **RFC-0011 Phase P5 — reference-first credential provisioning** (branch
  `feat/conn-p5-credentials`): users can now configure SSH/S3/GCS/Azure credentials through the
  connection form. Key details:
  - The connection form gains two new stages: **CredentialMethodPicker** (choose SSH agent / key
    file / inline PEM / password, or the AWS/GCP/Azure equivalent) and **CredentialFields**
    (fill required fields for secret-bearing methods).
  - **Delegation sources** (SSH agent, AWS default chain / named profile, GCP ADC, Azure AD) are
    preferred — no key material is stored in the vault. Picking a delegation method saves the
    profile immediately with a delegation marker.
  - **`SshPrivateKeyFile`** stores only the path (non-secret) and optional passphrase; the key
    bytes are read from disk at connect time so key rotation is reflected immediately.
  - **Vault gating:** non-delegation methods gate on vault availability. If the vault is locked
    or absent, the credential draft is held in `pending_save` on the `VaultUnlock` / `VaultCreate`
    overlay and completes automatically after unlock/create.
  - **OS-source detection** (`AppEffect::DetectOsSources`) runs at startup to default the
    credential picker to the most-likely-working option (SSH agent if `SSH_AUTH_SOCK` is set,
    named AWS profile if `~/.aws/credentials` has entries, etc.).
  - **Edit mode** shows "Keep existing credential" at the top of the picker so re-editing
    endpoint fields does not inadvertently clear a configured vault secret.
  - **`cairn-core` isolation** is preserved: `CredentialDraft` (not `CredentialSecret`) travels
    through `AppEffect`; the fully-typed `CredentialSecret` is assembled only at the binary edge
    in `cairn/src/app.rs` where `cairn-vault` is available.
  - GCS service-account JSON and Azure shared-key / SAS / connection-string are present in the
    picker but field-capture is deferred to a future update (the profile saves without vault
    credentials; the backend will prompt on first open).
  - Added ADR-0009 documenting the reference-first design decision.
  - **Post-review fixes** (quality gate findings addressed in the same PR): `AwsProfile`
    removed from `is_delegation()` — it now correctly requires field entry for the profile name
    and stores it in the vault; deferred methods (`GcpServiceAccountJson`, `AzureSharedKey`,
    `AzureSasToken`, `AzureConnectionString`) now emit their correct `CredentialDraft` variants
    instead of misidentified placeholders; vault gate applies to all methods including delegation
    (which store a non-secret marker); `debug_assert!` in `initial_effects` extended to allow
    `AppEffect::DetectOsSources`; `PrivateKeyFile` PEM bytes now wrapped in `zeroize::Zeroizing`
    to wipe key material after decode; `detect_aws_profiles` streams line-by-line via `BufReader`
    so secret access-key values never accumulate in a heap `String`; cross-platform home-dir
    fallback (`USERPROFILE` on Windows, `APPDATA` for GCP ADC path); `spawn_blocking` panic in
    OS-source detection now logged at `warn` rather than silently swallowed.

- **RFC-0011 Phase P4.5 — vault-create-from-UI** (branch `feat/conn-p45-vault-create`): closes
  the security gate that allows a first-run user to create the encrypted vault from inside Cairn,
  unblocking the credential-provisioning phase (P5). Key details: `Overlay::VaultCreate` with
  two `MaskedInput` passphrase fields (new + confirm), Tab-cycling focus, `[Ctrl-R] Remember`
  keychain toggle, and a submit path that validates minimum length (8 chars), compares the two
  fields without ever exposing the bytes (length check first; `take_secret()` + `expose_secret()`
  only when lengths match, both values zeroized on mismatch), then emits
  `AppEffect::CreateVault { passphrase: SecretString, remember: bool }`. The Argon2id KDF
  (`KdfParams::recommended()`: 19 MiB / 2 passes) runs in `tokio::task::spawn_blocking` so the
  render path is never blocked. The vault file is written atomically via `tempfile::NamedTempFile`
  at mode 0600. On `VaultCreated { Ok(()) }` the broker is unlocked immediately (and the
  passphrase optionally stored in the OS keychain), all `NeedsVault` connections are flipped to
  `NeedsOpen`, and `AppState::vault_file_exists` is set to `true` so subsequent `Ctrl-U` / vault
  selections open the unlock overlay rather than create. `AppState`/`AppEffect` `Debug` never
  reveals the passphrase. Error strings crossing to logs are value-free (no path, no passphrase).
  New hermetic tests: field editing, Tab/Esc, mismatch rejection, min-length enforcement,
  double-submit guard, remember flag, pending-conn routing, `VaultCreated` TEA round-trip, Unix
  0600 file permission, and a `Debug` redaction assertion.

- **RFC-0011 Phase P4 — in-app connection form (add / edit / remove)**: users can now create,
  edit, and delete connection profiles from within the TUI without manually editing the cairn
  config file. `Ctrl-N` opens the scheme picker; `e` edits the selected profile in the switcher;
  `d` deletes it (Profile entries only — auto-discovered entries are read-only). The form collects
  per-scheme endpoint fields (SSH host/user/port, S3 bucket/region, GCS/Azure equivalents, local
  path) and persists the profile to the user config file. Credential capture is deferred to P5;
  the form shows a one-line hint explaining this. A single hint line in the switcher footer shows
  available actions for the selected entry type. `Tab`/`Shift-Tab` and `↑`/`↓` navigate fields;
  `Enter` saves; `Esc` goes back to the scheme picker (new connections) or cancels (edits). Note:
  newly created connections require a restart to appear in the switcher. Deleted and edited
  connections are reflected immediately. New `AppEffect::SaveConnection`/`DeleteConnection` and
  `AppEvent::ConnectionSaved`/`ConnectionDeleted`/`ConnectionOpFailed` complete the TEA round-trip.

- **RFC-0011 Phase P3 — Docker + Kubernetes auto-discovery** (RFC-0011 §3–§4, §7): Cairn now
  auto-discovers Docker sockets and Kubernetes clusters from the environment at startup without
  blocking the UI or making any network connections during discovery. Discovered entries appear in
  the connection switcher with an `[auto]` provenance badge (dimmed, non-editable). Three Docker
  socket candidates are probed concurrently with a 500 ms timeout each: the platform default
  (via `connect_local()`), the rootless Docker socket (`$XDG_RUNTIME_DIR/docker.sock`), and the
  Podman socket (`$XDG_RUNTIME_DIR/podman/podman.sock`). For Kubernetes, the merged kubeconfig
  (`$KUBECONFIG` / `~/.kube/config`) is parsed in a `spawn_blocking` task; a second in-cluster
  entry is emitted if `KUBERNETES_SERVICE_HOST` is set and the SA token file exists. Discovered
  connections open lazily on selection (`NeedsOpen`), exactly like P2 saved profiles. New
  `DiscoveryConfig` section in `cairn.toml` (additive, `#[serde(default)]`, no schema bump):
  `docker = true`, `kubernetes = true`, `hidden = []`, `pinned = []`. The `hidden` list suppresses
  individual entries by key string; `pinned` floats entries to the top of the switcher. Provider
  loop is now concurrent (`join_all`) across all providers. All providers are feature-gated
  (`docker` / `k8s` features); the lean build (no features) compiles and passes clippy cleanly.

- **RFC-0011 Phase P2 — lazy, on-select connection opening** (RFC-0011 §2, ADR-0007): remote and
  credential-bearing connection profiles are no longer opened at startup. They appear in the
  connection switcher immediately as `NeedsOpen` (credential-free remote) or `NeedsVault`
  (vault-locked credential), and are opened asynchronously on the first user selection via the new
  `AppEffect::OpenConnection` / `AppEvent::ConnectionOpened` TEA round-trip. `LocalRoot` targets
  (built-in roots + `scheme = "local"` profiles) continue to mount eagerly, preserving zero-latency
  local navigation. Vault-unlock reconciliation: all `NeedsVault` entries flip to `NeedsOpen` on
  unlock; only the connection that triggered the unlock overlay is auto-opened. Stable id reuse:
  `ConnectionCoordinator::run` accepts a `prior_descriptors` map so re-enumeration (P3 config
  reload) preserves live `ConnectionId`s, preventing pane repointing. `DeferredConnection` and
  `VaultContext::deferred` are retired (always empty in P2). `run_vault_unlock_effect` now returns
  `Result<(), String>` — no connections are opened at unlock time.

- **RFC-0011 Phase P1 — connection provider/coordinator abstraction** (RFC-0011 §1–§2): replaces
  the imperative body of `register_connections` with a `ConnectionCoordinator` backed by
  `BuiltinLocalProvider` and `SavedProfileProvider`. Observable behavior is identical — same
  switcher entries, same id assignment order, same eager-mount / vault-lock-defer split. New
  additive fields on `ConnectionChoice` (`provenance: ChoiceProvenance`, `status: ChoiceStatus`,
  `kind: ConnectionKind`) allow the reducer and renderer to display richer connection metadata
  in future phases. New `ConnectionDescriptor` side-map stored in `event_loop` (unused in P1,
  established for P2 lazy open on selection). Broker-isolation test remains green.
  **Micro-fix (deliberate, not pure refactor):** a `scheme = "local"` profile with
  `endpoint.path = ""` (explicitly empty string) is now skipped with a warning, matching the
  existing treatment of a missing key. The original code silently mounted a broken `LocalVfs`
  at an empty path and consumed a `ConnectionId`, shifting ids of any following profiles.

- **Plugin loader and manifest parser** (RFC-0010 PR-C1, M8-5): new `cairn-plugin::manifest`
  module with `PluginManifest` (parsed from `plugin.toml` via `serde`+`toml`, all structs
  `deny_unknown_fields`) covering the `[plugin]`, `[capabilities]`, `[network]`, and `[limits]`
  sections from RFC-0010 §5.2. Semantic validation: plugin name charset `[a-z0-9_-]` ≤ 64
  chars, inline SemVer, `api.major` must equal `HOST_API_MAJOR = "1"`, description ≤ 256 chars.
  New `cairn-plugin::loader` module with `PluginLoader`: directory discovery
  (`<name>-<version>/plugin.toml` + `component.wasm`), ABI major-version check (§5.3),
  grants intersection (manifest-requested ∩ config-stored, fail-closed; §5.4), and the full
  §5.6 load pipeline. New `cairn-config` types `PluginGrantsRecord` and `PluginEntry` stored
  under `[plugins."<name>@<version>".grants]` in `cairn.toml`, with `plugin_grants()`,
  `set_plugin_grants()`, and `revoke_plugin_grants()` accessors. 7 loader tests, 12 manifest
  tests, 6 config tests. Default `cargo test` remains hermetic and offline.
  Deferred: PR-C2 (TUI approval overlay) and PR-C3 (`cairn plugin install` CLI).

- **Brokered host functions** (RFC-0010 PR-B, M8-4): replaced the deny-stubs for
  `host::http-fetch` and `host::use-credential` with real, capability-gated implementations.
  `http-fetch` performs HTTP on the plugin's behalf using reqwest+rustls (no OpenSSL); every
  call is gated by a per-plugin hostname allow-list, SSRF-guarded via pre-flight DNS resolution
  and IP-literal classification (loopback / RFC-1918 / CGNAT / link-local / ULA / IPv4-mapped
  blocked), response-capped at 8 MiB, `Set-Cookie` stripped from responses, and sensitive
  request headers (`Authorization`, `Cookie`, etc.) redacted from logs. Redirects are
  re-validated per hop. `use-credential` resolves a vault credential by opaque handle via the
  `CredentialBroker` trait — the raw `CredentialSecret` never crosses the WIT ABI; only an
  ephemeral artifact (e.g. STS bearer token or base64-encoded Basic-Auth value) is returned.
  Both functions are capability-gated: a plugin without a `network` or `credentials` grant in
  `PluginGrants` receives a deny-stub error at runtime without touching the vault or the network.
  Secrets are zeroized after use and never appear in error strings, logs, or journal entries.
  New `cairn-broker-api` boundary types: `CredentialAction`, `CredentialBroker`, and
  `CredentialBrokerError`. New `cairn-broker` adapter: `BrokerCredentialAdapter`. New
  `cairn-plugin` module: `http_fetch` (gated by the `plugin-network` feature). New public types:
  `PluginGrants`, `PluginHostConfig`, `PluginComponent::instantiate_with_grants`. The
  `plugin-network` feature keeps default `cargo test` hermetic and offline. 78 tests pass in
  both configurations.

- **WASI subset narrowing** (RFC-0010 PR-A, M8-3b): replaced the blanket
  `wasmtime_wasi::p2::add_to_linker_sync` with an explicit per-interface allow-list. Only
  `wasi:io/{error,streams,poll}`, `wasi:clocks/{wall-clock,monotonic-clock}`, and
  `wasi:random/{random,insecure,insecure-seed}` are registered; `wasi:sockets/*`,
  `wasi:filesystem/*`, and `wasi:cli/*` are absent. A component importing any excluded interface
  fails instantiation (default-deny). Non-blocking stubs for `wasi:io/poll` and
  `wasi:clocks/monotonic-clock` close the epoch-vs-blocking-WASI evasion gap: the stubs return
  all pollables immediately-ready without entering the Tokio scheduler, so a malicious guest
  cannot park the plugin thread inside a native frame that epoch cannot interrupt. The fixture
  guest is rebuilt as `#![no_std]` (uses `dlmalloc` + `core::arch::wasm32::unreachable()` panic
  handler) so the committed `backend.wasm` imports no `wasi:cli/*` interfaces and can be
  instantiated with the narrowed linker. All 44 unit + integration tests pass.

### Added

- **TUI exec/port-forward session pane** (M6-7, RFC-0009 PR-4): `SessionId`, `SessionRecord`,
  `SessionEnd`, `SESSION_OUTPUT_MAX_LINES`, and `SESSION_OUTPUT_MAX_BYTES` are added to
  `cairn-types`/`cairn-core`. The reducer gains `Overlay::ExecPane` (cooked-mode line I/O with
  scroll, follow, and a detach-without-kill `Ctrl-]` binding) and `Overlay::PortForwardStatus`
  (bound-port display with one-key teardown). Six new `AppEffect` variants —
  `OpenExecSession`, `OpenPortForward`, `CloseSession`, `CloseStdin`, `SendSessionInput`,
  `ResizeSession` — and three new `AppEvent` variants — `SessionOutput`, `SessionEnded`,
  `PortForwardBound` — complete the TEA loop. `Action::OpenExecSession` and `open_exec_session()`
  create session records via the same reducer path used in production, minting `SessionId` and
  inserting `SessionRecord`. The app effect runner adds `SessionControls` (cancel token + stdin
  relay channel) and the `run_exec_session_effect`/`run_port_forward_effect` async functions.
  Quality-gate fixes applied post-review: follow-mode scroll now fills the viewport correctly
  (renderer uses `total.saturating_sub(viewport)` when `follow=true`, not the last-line index);
  `output_partial` (incomplete last line) is rendered dimmed below complete lines; `Ctrl-D`
  emits `AppEffect::CloseStdin` (EOF to stdin) rather than `AppEffect::CloseSession` (hard kill)
  so the process can exit gracefully; `PortForwardStatus` `Enter`/`Confirm` are no-ops — only
  `Esc`/`Cancel` tears down a live forward; `advance_queue` no longer stalls the transfer queue
  while `ExecPane`/`PortForwardStatus`/`LogViewer` are open (passive overlays coexist with
  transfers); trailing stdout is drained after the `done` receiver resolves; `SessionEnded`
  cleans up the `sessions` map immediately when no overlay is displaying the session; scroll is
  adjusted for front-eviction in the ring buffer to prevent view drift. A second quality-gate
  pass (bug-bot + code-review) found eight additional items, all addressed: the `output_partial`
  OOM bypass (no-newline output growing the partial buffer past the byte cap without triggering
  eviction) is plugged by a force-flush in the new shared `append_to_ring` helper, which
  unifies `append_log_chunk` and `append_session_output` into one place; the post-`done` stdout
  drain switches from `try_recv` to `recv().await` with a 5 s timeout so in-flight final chunks
  are never lost; UTF-8 multibyte chars split across chunk boundaries are correctly stitched via
  a per-session `utf8_carry: Vec<u8>` buffer in the relay task (`AppEvent::SessionOutput` now
  carries `text: String` instead of `bytes: Vec<u8>`, matching the `LogChunk` pattern); `Submit`
  is a no-op when `rec.ended.is_some()` so no bytes are sent to a dead process; `SendSessionInput`
  `try_send` failure is surfaced via `tracing::warn` rather than silently discarded;
  `SessionDoneGuard` (RAII, mirroring `TransferDoneGuard`) emits a synthetic `SessionEnded` on
  relay-task panic so the `ExecPane` overlay cannot freeze permanently; the misleading "80 %"
  comment in `render_exec_pane` is corrected to "80-column-wide". New tests:
  `append_to_ring_oom_guard_caps_partial`, `append_to_ring_scroll_drift_corrected`, and
  `map_input_routes_ctrl_bracket_and_ctrl_d_when_exec_pane_active`. 657 tests pass across the
  workspace.

- **Docker interactive exec** (M6-3, RFC-0009 §2): `DockerVfs::invoke("exec")` with
  `ActionCtx::Exec { argv, tty }` now returns `ActionOutcome::Session(SessionHandle)` backed by
  bollard 0.21's `create_exec` → `start_exec` → relay-task pipeline. `ContainerOps::exec` is
  added to the trait seam; `BollardDocker::exec` spawns three sub-tasks — a stdin relay writing to
  the exec's `AsyncWrite` handle, a resize relay calling `resize_exec` (TTY sessions only), and a
  main `tokio::select!` loop that drains the `LogOutput` stream and selects against the cancel
  oneshot. When the output stream closes naturally, `inspect_exec` retrieves the numeric exit code
  and resolves `done` with `Ok(exit_code)`; on cancel, `done` is `Ok(-1)`. `attach_stderr` is
  wired for non-TTY sessions (Docker merges stderr into stdout when `tty: true`). `MockDocker::exec`
  is an echo-style implementation with a resize-drain sub-task, cancellation support, and
  `VfsError::NotFound` for unknown containers. Four new hermetic unit tests: wrong-ctx returns
  `invalid_ctx`, non-tty echo round-trip (stdin→stdout, no resize, `done Ok(0)`), tty resize
  channel + cancel→`Ok(-1)`, unknown-container `NotFound`. One new env-guarded dind integration
  test (`docker_container_exec_round_trip`, `CAIRN_IT_DOCKER`-gated): execs `echo CAIRN_EXEC_MARKER`
  in busybox, asserts the marker appears in stdout and exit code is 0.

- **Kubernetes port-forward** (M6-6, RFC-0009 §3): `KubeVfs::invoke("port-forward")` with
  `ActionCtx::PortForward { local, remote }` now returns `ActionOutcome::Session(SessionHandle)`
  backed by `kube 4.0`'s `Api::<Pod>::portforward`. `KubeRsOps::port_forward` binds a
  `TcpListener` on `127.0.0.1:<local>` (or an OS-assigned ephemeral port when `local == 0`)
  and for each accepted TCP connection opens a fresh `Portforwarder` WebSocket (one per
  connection — the documented kube-rs pattern) and relays bytes with
  `tokio::io::copy_bidirectional`. The accept loop runs in a spawned Tokio task and exits on
  `cancel`; `done` resolves `Ok(0)` on clean teardown. `SessionHandle.local_port` is set
  immediately (before any connection arrives) so the TUI can display the address at once.
  `MockKube::port_forward` binds a real `TcpListener` and runs an echo server per connection
  (bytes written by a client are reflected back), enabling fully hermetic testing. `tokio/net`
  added to the `k8s` feature and dev-dependency feature set. New hermetic unit tests (4):
  `invoke_port_forward_with_wrong_ctx_returns_invalid_ctx`,
  `invoke_port_forward_on_shallow_path_returns_not_available`,
  `invoke_port_forward_ephemeral_port_and_echo_round_trip` (TCP round-trip + cancel→Ok(0)),
  `invoke_port_forward_unknown_pod_returns_not_found`. New env-guarded kind integration test
  (`k8s_port_forward_binds_and_accepts_connection`, `CAIRN_IT_K8S`-gated): forwards to a
  kube-system pod's port 10250 with an ephemeral local port, asserts the bind succeeds,
  asserts a TCP connect is accepted, and asserts cancel resolves `done` with `Ok(0)`; skips
  gracefully when no Running pod is available or RBAC denies the portforward subresource.
  Workspace `cargo clippy` and `RUSTDOCFLAGS="-D warnings" cargo doc` are green. M6-6 is now
  complete (logs + exec + port-forward); TUI exec/port-forward pane is M6-7 (PR-4).

- **Kubernetes interactive exec** (M6-6, RFC-0009 §1 + §3): `KubeVfs::invoke("exec")` with
  `ActionCtx::Exec { argv, tty }` now returns `ActionOutcome::Session(SessionHandle)` backed by
  `kube 4.0`'s `Api::<Pod>::exec` + `AttachParams`. The `SessionHandle` shape is refined per
  RFC-0009 §1: `done` carries `Result<i32, VfsError>` (exit code, not `Result<()>`; non-zero exit
  is `Ok(n)`), a new `resize: Option<mpsc::Sender<(u16, u16)>>` field is added (present only when
  `tty: true`), and a `SessionHandle::new()` constructor is added (required because
  `#[non_exhaustive]` forbids struct-literal construction outside `cairn-vfs`). `KubeRsOps::exec`
  spawns a task owning the `AttachedProcess` and wires three relay sub-tasks: stdin (`mpsc::Receiver
  → AsyncWrite`), stdout+stderr (`AsyncRead → mpsc::Sender`, interleaved; stderr absent when TTY),
  and a resize relay (`mpsc::Receiver<(u16,u16)> → futures::channel::mpsc::Sender<TerminalSize>`,
  TTY only, uses `try_send` for best-effort non-blocking forwarding). The main task `select!`s
  between `cancel` (→ `Ok(-1)`) and the `take_status()` future (→ Kubernetes `Status.causes`
  ExitCode extraction). `MockKube::exec` is an echo-style implementation that relays stdin to
  stdout and exits with `Ok(0)`. New hermetic tests (5): `invoke_exec_with_wrong_ctx_returns_invalid_ctx`,
  `invoke_exec_on_shallow_path_returns_not_available`, `invoke_exec_non_tty_returns_session_with_echo`,
  `invoke_exec_tty_has_resize_channel`, `invoke_exec_cancel_by_dropping_sender_resolves_done`. Kind
  integration test extended (`CAIRN_IT_K8S`): `k8s_exec_non_tty_round_trip` runs
  `["sh", "-c", "echo CAIRN_EXEC_MARKER"]` in a kube-system Running pod, asserts the marker in
  stdout, and asserts `done == Ok(0)`; skips gracefully when no suitable container is found.
  Workspace `cargo clippy` and `RUSTDOCFLAGS="-D warnings" cargo doc` are green across all crates.
  Remaining RFC-0009 work: Docker exec (PR-5), port-forward (PR-3), TUI exec pane (PR-4).

- **TUI log-stream viewer overlay** (M6-7): `L` opens a live log-stream overlay for the entry
  under the cursor. The overlay buffers up to 100 000 lines / 4 MiB (ring-buffer; oldest lines
  drop when the cap is reached), auto-scrolls in follow mode (`[follow]` indicator in the border),
  and supports manual scroll with `j`/`k` (cursor up/down), `g`/`G` (top/bottom), and
  `PgUp`/`PgDn`. Any upward scroll disables follow; `G` or `CursorBottom` re-enables it. Status
  indicators (`Streaming…` / `Done` / `Error: …`) are shown in the bottom-right of the overlay
  border. `Esc` / `n` closes the overlay and cancels the stream (fires `AppEffect::CloseLogViewer`
  with a `CancellationToken`). New keybindings: `L` → `open_log_viewer`, `PageUp` → `page_up`,
  `PageDown` → `page_down`. The runtime drives the stream through `Vfs::invoke("logs", …)` in
  follow mode, forwarding each lossily-decoded chunk as `AppEvent::LogChunk`; backends without log
  streaming surface `VfsError::Unsupported` as an in-overlay error.

- **Kubernetes pod/container log streaming** (M6-6 first slice): `KubeVfs::invoke("logs")`
  now returns `ActionOutcome::Stream(BoxStream<'static, Result<Bytes, VfsError>>)` backed by
  `kube 4.0`'s `Api::<Pod>::log_stream`. The `log_stream` method returns an
  `impl futures::io::AsyncBufRead + use<K>` (a hyper body wrapped in `IntoAsyncRead`); since
  the concrete type is not guaranteed to be `Unpin`, `Box::pin` is applied to produce a
  `Pin<Box<T>>` which is always `Unpin`, enabling `futures::io::AsyncReadExt::read`. Chunks
  are forwarded over a bounded `mpsc` channel (capacity 64) from a spawned Tokio task, producing
  a `'static` stream with natural cancellation when the consumer drops it. `follow` and `tail`
  are read from `ActionCtx::Logs`; defaults are `follow: false / tail: Some(100)` so history
  reads and integration tests terminate without a timeout. `KubeOps::logs` is a new seam method
  (non-async, returns `BoxStream`) implemented by `KubeRsOps` (live) and `MockKube` (canned
  two-line output). `bytes` is now an unconditional dep of `cairn-backend-k8s` (the trait and
  mock need it regardless of the `k8s` feature); `tokio/rt` and `tokio/sync` are added to the
  `k8s` feature for the spawned task and `mpsc` channel. New hermetic unit tests (5):
  `invoke_logs_pod_path_returns_stream_with_canned_output`,
  `invoke_logs_container_path_returns_stream`,
  `invoke_logs_ctx_follow_is_forwarded`,
  `invoke_logs_on_navigation_path_errors`,
  `invoke_exec_and_port_forward_are_deferred`. The kind integration test
  (`CAIRN_IT_K8S`) is extended with log streaming: `ops.logs` (non-follow, tail 10) is driven
  to completion and asserts zero stream errors; a non-empty result is expected for active
  kube-system pods. `exec` (interactive TTY `Session`) and `port-forward` (`Session`) remain
  M6-6 follow-ups — they are advertised by `actions_at` but return `not_implemented`.

### Changed

### Fixed

## [0.1.0] - 2026-06-30

### Added
- **Kubernetes in-container filesystem browsing** (M6-5b): `KubeRsOps` now implements
  `list_dir`/`stat`/`read` for paths inside a container's filesystem via **tar-over-exec**
  (`kubectl cp` semantics). `list_dir(path)` execs `tar cf - -C <path> .` and parses the tar
  stream for immediate children; `stat(path)` and `read(path)` exec `tar cf - -C <parent> <basename>`
  and examine the first header entry. A new `tar_exec` module provides pure helper functions
  (`parse_list_dir`, `parse_stat_tar`, `parse_read_tar`, `tar_parent`, `tar_basename`) that are
  fully unit-testable without a cluster — 11 new hermetic unit tests cover empty tars, flat
  directories, deep-descendant deduplication, directory/file stat/read, and NotFound/Unsupported
  edges. When the container lacks `tar`, all three methods return `VfsError::Backend { code:
  "exec_unavailable" }` rather than a misleading `NotFound`. The kind integration test
  (`CAIRN_IT_K8S`) is extended to exercise `list_dir`/`stat`/`read` on `/`, `/etc`, and
  `/etc/hostname`, with a graceful skip when the target container lacks `tar`. The `tar` crate is
  added as an optional dep under the `k8s` feature.
- **Docker container log streaming** (M6-3 first slice): `DockerVfs::invoke("logs")` now returns
  `ActionOutcome::Stream(BoxStream<'static, Result<Bytes, VfsError>>)` backed by bollard's
  `Docker::logs` endpoint. Bollard's `LogOutput` decoder handles Docker's 8-byte multiplexed stream
  header (present when no TTY) so callers receive plain payload bytes without hand-parsing wire
  frames. The implementation spawns a Tokio task owning the Docker client clone + bollard stream and
  forwards frames over a bounded `mpsc` channel (capacity 64), producing a `'static` `BoxStream`
  and providing natural cancellation when the consumer drops the stream. `follow` and `tail` are
  read from `ActionCtx::Logs`; defaults are `follow: false / tail: "100"` so a hermetic or dind
  test terminates without a timeout. `exec` (interactive `Session`) remains deferred (M6-3/M6-6
  follow-up). `ContainerOps::logs` is a new seam method implemented by both `BollardDocker` and
  `MockContainerOps` (canned two-line output). `bytes` is now an unconditional dep of
  `cairn-backend-docker` (the trait and mock use it; `bollard`/`tar` remain `docker`-feature-only).
  New hermetic tests: `invoke_logs_returns_stream_with_canned_output`,
  `invoke_logs_on_non_container_path_errors`, `invoke_exec_is_deferred_not_implemented`,
  `invoke_logs_ctx_follow_is_forwarded`. New env-guarded dind test: `docker_container_log_streaming`
  (creates a busybox container that emits `CAIRN_LOG_STREAM_MARKER`, collects the `invoke("logs")`
  stream, and asserts the marker is present).
- **Docker & Kubernetes connection-opener arms** (M4-5): the binary's `ConnectionOpener` now also
  opens `docker` and `kubernetes`/`k8s` profiles (no vault credential — local socket / kubeconfig),
  behind new `docker`/`k8s` binary features and a `containers` umbrella (`all-backends` now =
  `ssh` + `cloud` + `containers`). The lean default build stays SDK-free.
- **Vault-unlock TUI flow** (M3-7): the runnable `cairn` binary can now unlock the encrypted secrets
  vault and bring credential-bearing connections online. A new `Overlay::VaultUnlock` (open with
  **Ctrl-U**, or the `vault_unlock` keybinding) presents a **no-echo** passphrase field backed by a
  new `MaskedInput` type — it renders as bullets (`•`), its `Debug` is redacted, and its buffer is
  zeroized when cleared, when the secret is taken, and on drop, so the typed passphrase never reaches
  `AppState`'s `Debug` or any log. Submitting emits `AppEffect::UnlockVault { passphrase }` (the
  passphrase rides in a zeroizing `SecretString`); the effect runner opens the vault off the render
  path (`Vault::open` via `spawn_blocking` — Argon2id is CPU-bound), installs it into the shared
  `Broker` (`broker.unlock(vault)`), then retries the credential-bearing connections that were
  **deferred at startup** because the vault was locked, registering the successes in the switcher and
  reporting how many connected. A wrong passphrase or a missing vault file keeps the overlay open with
  a clear, secret-free, retryable message. The `Arc<Broker>` and the deferred profiles now live in a
  runtime-side `VaultContext` (the secret-free `AppState` gains only two plain flags:
  `vault_unlocked` and `has_locked_connections`). A new `[vault] path` config setting selects the
  vault file (default `…/cairn/vault.cvlt`, via `cairn_config::default_vault_path`). Hermetic tests
  cover the reducer transitions (open/cancel, the no-echo/`Debug` invariant, empty- and
  wrong-passphrase keeping the overlay, and a successful unlock adding connections), the masked render
  (passphrase never drawn), and the effect against a temp `fast_for_tests` vault (unlock succeeds and
  unlocks the broker; wrong passphrase leaves it locked; deferral→unlock→retry under `all-backends`).
  **Deferred follow-ups:** the vault **creation** UI (an absent vault file currently surfaces a clear
  non-blocking message) and add/list-credential management UI, plus **auto-lock-on-idle** (zeroizing
  the broker after inactivity).
- **Broker-backed connection opener** (M4-5, first integration slice): the runnable `cairn` binary
  can now turn a saved `ConnectionProfile` into a live `Vfs` backend. A new `ConnectionOpener`
  (`crates/cairn/src/connect.rs`) resolves the profile's vault credential reference through the
  `Broker`, builds the scheme's `*ConnectParams` from the profile's non-secret `endpoint` fields, and
  dispatches to the matching connector (`ssh_connect`/`s3_connect`/`gcs_connect`/`azure_connect`),
  returning an `Arc<dyn Vfs>`. The binary gains per-backend feature umbrellas — `ssh`, `s3`, `gcs`,
  `azure`, plus convenience `cloud = [s3, gcs, azure]` and `all-backends = [ssh, cloud]` — each
  pulling its backend crate (an optional dep) and live-transport feature. The **default build stays
  lean** (no backends, no cloud/SSH SDKs), preserving the cross-platform lean build (ADR-0006); a
  profile whose scheme's feature is off fails fast with a "not built into this binary" error rather
  than attempting any I/O. At startup, credential-bearing config profiles are opened through this
  path (journaled as `Actor::User`) and registered in the switcher; a profile that can't be opened
  (locked vault, missing field, or a backend not built in) is skipped with a warning so startup never
  breaks. Hermetic tests cover scheme dispatch, profile→params construction, and the resolve→connect
  credential path (an S3 profile pointing at an SSH credential is rejected as `Auth` before any
  network I/O). **Deferred follow-ups:** the Docker/K8s opener (they connect without a vault
  credential), the new-connection TUI (M4-5), and the `Overlay::VaultUnlock` flow (M3-7) — until the
  latter lands the broker is wired *locked*. The assistant's `open_connection` tool also stays
  deferred (it requires the M7 authorize→confirm mediation).
- **MCP client foundation** (M7, `RFC-mcp-client`): a new standalone `cairn-mcp` crate — a thin,
  documented wrapper over the official **`rmcp` 2.0** SDK that lets Cairn act as a **client** of an
  external Model Context Protocol server. `McpClient` connects over **stdio** (`connect_stdio`, spawning
  a server as a child process) or **streamable HTTP** (`connect_http`, a URL), performs the MCP
  `initialize` handshake, then `list_tools` (→ `McpToolInfo`: name + description + input JSON schema) and
  `call_tool(name, args)` (→ `McpToolResult`: joined text content + `is_error` + structured output).
  Non-object/non-null arguments are rejected locally as `McpError::InvalidArguments` without contacting
  the server. Every operation (handshake + each request) is bounded by a timeout (default 30s,
  configurable via `with_timeout`) so an unresponsive server can never block the caller forever
  (`McpError::Timeout`), and the spawned stdio server's **stderr is discarded** so it cannot corrupt
  Cairn's terminal. `McpError` (thiserror, `#[non_exhaustive]`) keeps **secret-free** top-level messages
  and attaches the rmcp error as `source` (the source chain/`Debug` may still carry transport detail, so
  callers should log only the `Display`). rmcp is pulled with default features **off**, enabling only
  `client` + `transport-child-process` + `transport-streamable-http-client-reqwest` + `reqwest`, which
  pins TLS to **rustls** (no OpenSSL). This is the **transport/protocol layer only**: the crate does
  **not** depend on `cairn-ai`/`cairn-broker`/`cairn-vault`/`cairn-secrets` and is **not** wired into the
  agent's closed, capability-gated tool surface — how untrusted MCP tools interact with the
  plan→confirm→execute safety model is a deliberate **follow-up RFC** (`RFC-mcp-client`). Tested
  **hermetically**: a tiny in-process `rmcp` echo server (dev-only `server` feature) over an in-memory
  duplex stream exercises a full connect → `initialize` → `list_tools` → `call_tool` round-trip, plus
  tests for null/array argument handling, a client-side timeout against an unresponsive server, and
  result/error mapping. No network or child process in `cargo test`.
- **Live LLM HTTP providers** (M7-2, RFC-0008): `cairn-ai` gains two concrete `LlmProvider`
  implementations behind the **non-default `http` feature** — `AnthropicProvider` (Claude Messages
  API: `x-api-key`/`anthropic-version` headers, system-message folding into top-level `system`, first
  `tool_use` block → `ToolCall` else text → `Text`; advertises tools natively) and `OllamaProvider`
  (local models via `/api/chat`; advertises the robust `JsonSchema` tool tier since Ollama's native
  function-calling is model-dependent). Both are non-streaming, inject the `reqwest::Client`
  (rustls, no OpenSSL) so construction is panic-free, and take a configurable `base_url`. HTTP/transport
  failures map to `ProviderError::Transport` with a **secret-free** message that never embeds the
  `api_key`; unparseable bodies map to `InvalidResponse`. The `api_key` is a plain `String` — the crate
  still depends only on `cairn-broker-api` (no vault/secrets), and the `cairn-broker-api` dependency-
  closure test keeps that boundary enforced. Tested hermetically with `wiremock` (correct path/headers/
  body, canned `tool_use`/`text`, 401/500 → `Transport` with no key leak, malformed → `InvalidResponse`);
  an opt-in `#[ignore]` live smoke test is gated by `CAIRN_IT_AI` + `ANTHROPIC_API_KEY`. The `cairn-mcp`
  server (M7-7) remains a follow-up.
- **Vault unlock providers** (M3-3, ADR-0002/ADR-0006): `cairn-vault` gains an `UnlockProvider` trait
  that supplies the vault passphrase, plus a small `open_with(path, &dyn UnlockProvider)` convenience.
  Three implementations: `PassphraseUnlockProvider` (the always-available headless / prompt fallback),
  `KeychainUnlockProvider` (reads/writes the passphrase in the OS keychain — Secret Service on Linux,
  Keychain on macOS, Credential Manager on Windows — via `keyring` 4.x, behind the **non-default
  `keychain` feature** so the lean/headless build doesn't pull the platform secret-store stack), and a
  test-only `MockUnlockProvider` (under `cfg(test)` / the new `test-utils` feature). A new `UnlockError`
  maps a missing keychain entry to `NotFound` (so callers can fall back to prompting) and every other
  keychain failure to a fixed, secret-free `Backend` message; `VaultError` gains an `Unlock` variant.
  The keychain provider is tested **hermetically** against `keyring-core`'s in-memory mock store (no
  real OS keychain, no dbus). Auto-lock and the TUI unlock overlay (M3-7) remain follow-ups.
- **Kubernetes navigable tree, live** (M6-5, RFC-0005): the k8s backend's live `kube-rs` adapter
  (behind the non-default `k8s` feature) makes a cluster browsable — `list_contexts` (from the
  kubeconfig, no cluster call), `list_namespaces`, `list_pods` (phase / ready / node), and
  `list_containers` (regular + init + ephemeral). RBAC 403 maps to `VfsError::Forbidden`, a missing
  pod to `NotFound`. The kube stack is pinned to rustls (no OpenSSL). In-container filesystem
  browsing (`list_dir`/`stat`/`read` via exec-tar) and watch/streams remain follow-ups. A `kind`
  integration job (env-guarded by `CAIRN_IT_K8S`) exercises the tree against a real cluster.
- **Docker container-filesystem browsing, live** (M6-2, RFC-0004): the Docker backend's live
  `bollard` adapter (`BollardDocker`, behind the non-default `docker` feature) now implements
  `list_dir`/`stat`/`read` over the Docker Engine **archive API** — `download_from_container` returns
  a tar stream of a path, which is parsed with the `tar` crate to browse a running container's
  filesystem (list a directory's immediate children, stat a path, read a file). A missing path maps
  to `VfsError::NotFound`. Container/image listing was already live; this makes a container pane
  actually browsable. A dind integration job (env-guarded by `CAIRN_IT_DOCKER`, runs against the CI
  runner's own daemon) exercises it end-to-end. Live `exec`/`logs` streaming remains a follow-up.
- **Azure Blob object-store live backend** (M5-9, ADR-0003): `cairn-backend-object` gains
  `AzureObjectStore` and `azure_connect` (behind the non-default `azure` feature), an
  `azure_storage_blobs` adapter implementing the same `ObjectStore` seam — list (prefix/delimiter +
  continuation), head, ranged read, write, delete, and server-side copy. Credentials come from the
  broker as the typed `CredentialSecret::Azure`: a storage-account shared key or a SAS token (Azure
  AD is reserved in the vault but not yet wired in the adapter). The 0.21 SDK line is used (the 1.0
  rewrite is AAD-only and can't talk to Azurite) on the rustls stack (no OpenSSL). A live **Azurite**
  integration job (env-guarded by `CAIRN_IT_AZURE`) exercises the full round-trip — the third backend
  with a real emulator gate (alongside the S3 MinIO job). This completes the v0.1 "functional on
  GCS/Azure" contract.
- **Azure credential variant** (M5 / M3-4, RFC-0008): the vault's typed `CredentialSecret` gains an
  `Azure` family — `SharedKey { account, key }`, `SasToken`, and `AzureAd` (delegation) — sealed
  through the same `pub(crate)` zeroizing wire-mirror as the other variants.
- **GCS object-store live backend** (M5-8, ADR-0003): `cairn-backend-object` gains `GcsObjectStore`
  and `gcs_connect` (behind the non-default `gcs` feature), a `google-cloud-storage` adapter
  implementing the same provider-agnostic `ObjectStore` seam — list (prefix/delimiter + page tokens),
  head, ranged read, write, delete, and server-side copy (GCS `rewrite`). It uses the SDK's two
  clients (HTTP data-plane `Storage` for read/write, gRPC control-plane `StorageControl` for the
  rest) and supports a custom endpoint (anonymous auth) for local emulators. Credentials come from
  the broker as the typed `CredentialSecret::Gcp`: a service-account JSON key, or Application Default
  Credentials. The SDK rides the rustls/ring stack (no OpenSSL). The live emulator path is deferred
  (the GCS control plane is gRPC; HTTP emulators like fake-gcs don't cover it) — the adapter is
  type-checked and the `ObjectStore`→`Vfs` mapping is exercised by the S3 MinIO job and the
  `MockObjectStore` contract.
- **GCP credential variant** (M5 / M3-4, RFC-0008): the vault's typed `CredentialSecret` gains a
  `Gcp` family — `ServiceAccountKey` (the JSON key, entirely secret) and `ApplicationDefault`
  (delegation) — sealed through the same `pub(crate)` zeroizing wire-mirror as the SSH/AWS variants.
- **S3 object-store live backend** (M5-3, ADR-0003): `cairn-backend-object` gains `S3ObjectStore`
  and `s3_connect` (behind the non-default `s3` feature), an `aws-sdk-s3` adapter implementing the
  provider-agnostic `ObjectStore` seam — delimiter listing with continuation tokens and common
  prefixes, `head`, ranged `GET`, `PUT`, `DELETE`, and server-side `COPY`. It also drives
  S3-compatible stores (e.g. MinIO) via an endpoint override + path-style addressing
  (`S3ConnectParams`). Credentials come from the broker as the typed `CredentialSecret::Aws`: static
  keys (incl. STS session tokens), a named profile, or the SDK default provider chain. The AWS SDK
  (aws-lc-rs crypto, hyper) is feature-gated, so the lean cross-platform build never compiles it; a
  MinIO integration job (env-guarded by `CAIRN_IT_S3`) exercises the live round-trip. Multipart and
  resumable upload (M5-4/M5-5) remain follow-ups.
- **AWS credential variant** (M5 / M3-4, RFC-0008): the vault's typed `CredentialSecret` gains an
  `Aws` family — `Static { access_key_id, secret_access_key, session_token }`, `Profile`, and
  `DefaultChain` — sealed through the same `pub(crate)` zeroizing wire-mirror as the SSH variant
  (no `Debug`/`Serialize` on the public type; access key id is treated as a non-secret identifier).
- **SSH/SFTP live backend** (M4, RFC-0003): the first network backend can now establish a real
  connection. `cairn-backend-ssh` gains `ssh_connect` (behind the non-default `ssh` feature) — TCP →
  russh handshake → host-key verification (`HostKeyPolicy::Strict` / `AcceptNew` TOFU; a changed key
  is always rejected, and there is no "accept anything" mode) → authentication (password / private
  key + passphrase / SSH agent) → open the `sftp` subsystem → a ready `SftpVfs`. It consumes the
  typed `CredentialSecret::Ssh` from the broker. The `russh` stack is feature-gated so the lean,
  cross-platform build (and Windows/macOS CI) never compiles it; the `russh-sftp` adapter stays
  unconditional. Connection pooling/keepalive, jump hosts, and `~/.ssh/config` parsing are follow-ups.
- **Typed credential model** (M3-4, RFC-0008): the vault now stores a typed `CredentialSecret`
  (starting with the SSH variant: password / private-key+passphrase / agent) instead of a flat
  string. The public secret type implements neither `Debug` nor `Serialize` — a `compile_fail` test
  guards the no-`Debug` invariant — and the only serializable form is a `pub(crate)` zeroizing
  wire-mirror used solely inside the seal/open path, so an unlocked secret is wiped from memory on
  drop (lock). `Broker::resolve` now returns the typed secret (key vs password vs agent), and
  `CredentialInfo` carries a non-secret `CredentialShape` (family + variant + delegation). The vault
  on-disk format is bumped to v2 (clean break; no released users). Other backends' credential
  variants land with their backend PRs; the cloud `TokenCache` lands with M5.
- **`cairn-broker-api` — compile-time AI/plugin secret isolation** (M3-4, RFC-0008): a new crate
  exposing only the secret-free credential boundary (`CredentialDirectory` + `CredentialInfo`). The
  AI and plugin layers now depend on it instead of `cairn-broker`, so `cairn-ai` no longer pulls
  `cairn-vault` into its dependency graph and cannot even *name* a secret-returning API. A
  `cargo metadata` dependency-closure test fails CI if `cairn-broker`/`cairn-vault`/`cairn-secrets`
  ever re-enter the `cairn-ai`/`cairn-plugin` graph — turning the "AI never sees raw secrets" property
  from a convention into a guarantee. The non-secret `CredentialId` moved to `cairn-types`
  (re-exported from `cairn-vault` for compatibility).

### Changed
- **Feature-gated network backends + lean/full CI split** (ADR-0006): the heavy/TLS-bearing backend
  SDKs now sit behind non-default Cargo features so the default build stays lean and cross-platform.
  As the first step, the Docker adapter (`bollard`) moved behind a `docker` feature (off by default).
  CI now builds the lean default on Linux/macOS/Windows and `--all-features` on Linux only, and runs
  `cargo-deny` on both the default and full dependency graphs — keeping the aws-lc/TLS stack off
  Windows/macOS while still type-checking every adapter on Linux. Groundwork for the SSH/S3/GCS/Azure/
  Kubernetes backends, each verified against an emulator in a dedicated integration job.

### Added
- **WASM plugin wall-clock deadline** (M8-4): guests now have an **epoch** time limit alongside the
  fuel (instruction) limit. An `EpochTicker` advances the engine's epoch on a fixed interval and each
  guest op arms a deadline (`Limits::max_call_ticks`, ≈5 s by default), trapping a guest that *spins*
  past it as a `plugin_timeout` error. The ticker is tied to the instance lifetime and holds only a
  weak engine handle, so it never leaks a thread or keeps an engine alive. Note: epoch only interrupts
  guest wasm, not a guest *blocked inside a host/WASI call* (e.g. `wasi:io/poll`); narrowing the
  linked WASI surface to bound that is gated before exposing untrusted plugins live (M8-5).
- **WASM plugin backend — streaming writes + mutations** (M8-3b): `PluginVfsBackend` now implements
  the **full `Vfs` contract**. `open_write` bridges a guest `write-sink` resource to a `WriteHandle`
  (chunked write → `finish` returns the resulting `Entry`); a handle dropped without `finish` aborts
  the write rather than silently committing a partial one. `create_dir`/`remove` (with the recursive
  flag) /`rename` are wired through to the guest, each error mapped to the matching `VfsError`. Like
  reads, write sinks are owned on the plugin thread and freed on drop. The entry name a guest returns
  from `finish` (and `stat`) is now validated as a leaf name — rejecting traversal/control-char
  injection — and leaf names are length-bounded, matching the `list` defense. (The `cargo-component`
  fixture is rebuilt and committed.) Remaining before live untrusted use: an epoch deadline + the real
  brokered host functions (M8-4).
- **WASM plugin backend — streaming reads** (M8-3b): `PluginVfsBackend::open_read` now bridges a
  guest `read-stream` resource to a `tokio::io::AsyncRead` (`ReadHandle`), so a plugin-backed file
  reads (and ranged-reads) like any built-in backend. The resource is owned on the plugin thread
  (it is `!Send`) and addressed across calls by an opaque id; a `PluginReadHandle` pulls chunks on
  demand and frees the resource on drop. Hardened against a hostile guest: a per-stream byte cap
  (`Limits::max_stream_bytes`) cuts off a stream that never reports EOF, chunks larger than the host
  requested are rejected, and guest-supplied error strings are stripped of control characters and
  length-capped before reaching logs/UI. `open_write`/mutations still report `Unsupported` (PR3).
- **WASM plugin backend as a `Vfs` — read path** (M8-3b): a plugin component can now be browsed
  through the `Vfs` trait via `PluginVfsBackend`, indistinguishable from a built-in backend for
  listing and metadata. Because a wasmtime `Store` is `!Send`/`!Sync`, the instance runs on a
  dedicated thread and the `Send + Sync` backend messages it over a channel (one request + `oneshot`
  reply per op, fuel refilled per call). Implements `scheme`/`connection`/`caps`/`list` (paginated
  stream)/`stat`. The granted `host` interface is linked: `log` and
  `now-secs` are real; `http-fetch`/`use-credential` are deny-stubs until the broker wiring (M8-4).
- **WASM plugin backend bridge — foundation** (M8-3a): the `cairn-plugin` host can now load a
  **component** that exports the `cairn:plugin/backend` interface (the `cairn:plugin@1.0.0` WIT
  package from RFC-0006) and call its non-streaming introspection/metadata ops (`scheme`,
  `backend-caps` → `Caps`, `caps-at`, `stat`, `list-page`). Built on wasmtime's component model with
  generated host bindings, a per-instance memory cap + fuel limit, and an **empty, ambient-authority-
  free WASI context** — no preopened directories, no environment, null stdio, no network (the WASI
  interfaces are linked but grant nothing). A committed guest fixture
  component exercises it end-to-end, so CI needs no WASM toolchain. The streaming read/write
  resources, mutations, the granted `host` import interface, and the full `PluginVfsBackend: Vfs`
  async bridge are the next slice (M8-3b).
- **Concurrent transfers** (M2): up to N transfers now run at once (default **2**, set via
  `[transfers] concurrency` in config; clamped to ≥ 1). The status line shows an aggregate while
  several run (`⇅ 2 active · 2.0 MiB at 1.0 MiB/s (+1 queued)`), and the `Ctrl-T` queue view lists
  every active transfer plus the pending ones. `p` pauses/resumes and `Esc` cancels *all* active
  transfers; the rest queue (FIFO) and start as slots free. A transfer task that dies unexpectedly
  always releases its slot. Idle status messages (e.g. "Transfer paused") are now shown on the status
  line instead of being silently dropped.
- **Shell-command actions** (M8-7): bind a key in config to run a local program against the entry
  under the cursor — e.g. `[[shell_actions]]` with `name`/`key`/`command`/`args` (placeholders
  `{path}`/`{dir}`/`{name}`). **Security-first** (see ADR-0005): argv-only with **no shell**
  interpretation, **local backends only** (via `Vfs::local_path`, which canonicalizes and confines the
  path), a **confirm prompt** before each run (opt-out per action), a **scrubbed environment** (no
  secrets reach the child), explicit cwd, closed stdin, captured+capped output, and a wall-clock
  timeout that kills the process group. The `[[shell_actions]]` section is ignored when `config.toml`
  is writable by other users or not owned by you (Unix). Non-interactive only for now; output is
  summarized to the status line (never echoed or sent to the AI). Interactive/TUI-suspending programs
  are deferred.
- **`Vfs::local_path` capability** (M8 groundwork): a new `Vfs` trait method `local_path(&VfsPath) ->
  Option<PathBuf>` returns the real, canonical OS path backing a virtual path — but only for backends
  with a local filesystem identity. It defaults to `None` (every remote backend denies it), and
  `LocalVfs` implements it by canonicalizing and confining the result under its root, so a symlink
  whose target escapes the root yields `None`. This is the single sanctioned virtual→real-path bridge
  and the enforcement point for features that shell out (forthcoming shell-command actions). New cap
  `Caps::LOCAL_PATH` advertises it.

### Changed
- Dependencies: adopted the pending breaking bumps — `chacha20poly1305` 0.10→0.11 and `getrandom`
  0.2→0.4 (cairn-vault) and `rustix` 0.38→1 (config/binary). The vault migration is API-only (same
  XChaCha20-Poly1305 algorithm; `new_from_slice`/`XNonce::try_from`), so existing vault files remain
  readable — covered by the round-trip and tamper-detection tests. Also bumped `ratatui` 0.29→0.30,
  `toml` 0.8→1, and `directories` 5→6 (no code changes needed).
- Internal: the transfer model moved from a single in-flight transfer to a collection keyed by a
  stable `TransferId` (`AppState::active_transfers`, per-transfer cancel/pause, a `concurrency_limit`
  defaulting to 1), groundwork for concurrent transfers. No user-visible behaviour change yet.
- **BREAKING** (`cairn-transfer`): `TransferError::Cancelled` now carries the partial
  `TransferOutcome` completed before cancellation (`Cancelled(TransferOutcome)`), so a cancelled
  transfer reports how much already happened (e.g. "Transfer cancelled after 3 file(s), 1 dir(s);
  partial changes may remain") rather than implying nothing changed. Match it as `Cancelled(_)`.
- AI executor **`exec` routing** (M7-6 / RFC-0007 Gap 1): the `exec` tool no longer returns a
  hardcoded "not yet available" stub — it now resolves its `conn:N` handle (allow-list enforced) and
  routes through `Vfs::invoke(path, ActionId::EXEC, ActionCtx::Exec{argv,tty})`, reaching whichever
  backend the connection maps to. Local backends still report `Unsupported` and the container/cluster
  backends `not_implemented` (no live process spawns yet), but the routing is real and errors are
  redacted. A live `Stream`/`Session` outcome is rejected loudly rather than silently dropped, so an
  interactive/streaming exec can't masquerade as success before its output channel exists.
  `open_connection` remains deferred pending the broker-backed opener.

### Fixed
- **Transfer rate/ETA no longer skews low after a pause** (#55): the throughput rate shown in the
  status line is now computed over *effective* (non-paused) elapsed time. A lightweight accumulator
  task tracks the wall-time of each pause interval (true→false on the pause watch) into an
  `AtomicU64`; the `rate_bps` closure subtracts it before calling `avg_rate`. Rate and ETA snap back
  to accuracy immediately on resume instead of gradually recovering as fresh progress dilutes the
  distorted average.
- **`ConfirmShellAction` no longer misleads about the target path** (#58): the confirm overlay now
  annotates that the path shown is the *virtual* VFS path — the real OS path is resolved by the
  effect runner (via `Vfs::local_path`) immediately after the user confirms. Approach (a) from the
  issue: the real path is not yet available at confirm time so we clarify rather than pre-resolve.
- **Status bar transient messages are visible** (#54): `render_status` already applied the correct
  priority (live transfer > transient `AppState::status` > help string); this PR extends test
  coverage to explicitly verify that a live transfer takes priority even when a status message is
  set simultaneously, closing the untested arm of the precedence chain.
- **Interactive copy/move no longer silently overwrites** (M2-6): a UI copy/move that would clobber
  an existing destination now opens an "Overwrite?" confirm (showing how many collide) instead of
  overwriting silently. Confirm re-runs with overwrite enabled; cancel abandons the transfer leaving
  destinations untouched. (The AI executor already refused such overwrites.)

### Added
- **Transfer pause/resume** (M2): press `p` to pause or resume the active transfer (no-op when none
  is running). The status line shows `⏸ paused` and drops the rate/ETA while stopped; `Esc` still
  cancels a paused transfer immediately. Built on the engine plumbing below — the event loop owns a
  `watch::Sender<bool>` per transfer and drives it via a `SetTransferPaused` effect.
- **Transfer pause/resume — engine plumbing** (M2): the transfer engine now takes a
  `paused: &watch::Receiver<bool>` and blocks at the next check-point (between items, tree nodes, and
  mid-file between chunks) while it holds `true`, resuming when it flips back to `false`. Waiting is
  deadlock-safe (cloned receiver + `borrow_and_update` + `select!` on `changed()` vs cancel) and
  cancellation takes priority over a pause, so `Esc` aborts a paused transfer immediately.
- **AI step output** (M7-6 / RFC-0007 Gap 1): an executed plan's read-style steps now surface a
  short, secret-free summary instead of being validate-only — `list → 12 entries`, `stat → file,
  1.2 KiB`, `read → 1.2 KiB`, `delete → removed 3`. The summaries appear in the plan-complete status;
  they are shown to the **user only** and never fed back to the model (no file contents, just counts/
  sizes/kinds).
- **AI plan-execution cancellation** (M7-4/M7-6): `Esc` aborts an approved plan that is executing —
  the runtime polls a cancellation token between steps, so already-run steps stay applied and the
  remainder is skipped (`Plan cancelled after N step(s)`). While a plan executes, competing
  operations (a second plan, copy/move/delete, overlays) are refused so nothing mutates the
  filesystem concurrently or orphans the cancel token. Cancellation only *stops* execution — it
  cannot bypass the approval/allow-list/redaction model.
- **Transfer queue** (M2-5): a copy/move issued while one is already running is now **queued** and
  started automatically (FIFO) when the active transfer finishes, instead of being refused. The
  status line shows the queue depth (`⇅ transferring… 3.4 MiB (+2 queued)`); cancelling or completing
  the active transfer (or dismissing its overwrite prompt) drains the next one. `Ctrl-T` (config
  `open_queue`) opens a **queue view** showing the active + pending transfers; navigate with the
  cursor, `Shift-K`/`Shift-J` reorder the selected pending transfer, `d` drops it, `x` clears them
  all. The status line shows live **progress, throughput, and ETA** — `⇅ transferring… 3.4 / 10.2 MiB
  (33%) at 512 KiB/s, ETA 14s` — from a best-effort pre-scan of the source size (it degrades to a
  byte+rate display when the size can't be determined, and is skipped for instant same-pane moves).
- **Large-list row virtualization** (M1-9): only the on-screen window of rows is materialized each
  frame (the cursor is kept roughly centred), so browsing a directory with tens of thousands of
  entries costs O(viewport) instead of O(entries) per render.
- **Transfer cancellation** (M2-4): `Esc` aborts an in-flight copy/move — the engine's cooperative
  `CancellationToken` is now held on the runtime side and signalled by a `CancelTransfer` effect.
  Cancellation reports a non-error completion warning that partial changes may remain (a mid-flight
  move may have already moved earlier items).
- **Live transfer progress** (M2-5): the copy/move engine's progress is now surfaced — the status
  line shows a running byte total (`⇅ transferring… 3.4 MiB`) while a transfer runs. Progress is
  coalesced and delivered best-effort (non-blocking `try_send`, so it never stalls the transfer), and
  a dedicated `TransferDone` event clears the indicator so an unrelated op finishing mid-transfer
  can't wipe it. One transfer runs at a time (a second is refused while one is in flight).
- **Filter-as-you-type** (M1-9): `/` filters the active pane's listing by a case-insensitive name
  substring, updating live as you type — `Enter` keeps the filter, `Esc` clears it. The cursor and
  marks index the filtered view, and copy/move/delete/rename/enter act only on visible entries;
  changing directory resets the filter. The active filter shows in the pane's bottom-left. (Configurable
  as `filter`.) Large-list virtualization / off-thread filtering remains deferred.
- **AI freeform prompt** (M7-6): `Ctrl-A` now opens a text prompt for a natural-language request
  instead of sending a hardcoded demo string — the entered text drives the plan → confirm → execute
  flow. The freeform prompt accepts arbitrary input (paths, spaces); while the assistant is preparing
  a plan, actions that would open a competing overlay are suppressed so the proposal can't clobber
  another modal. (Live LLM providers remain the integration step; the offline `MockProvider` still
  builds the plan.)
- **Text-input overlay + mkdir/rename** (M2-3): a reusable single-line prompt (`Overlay::Prompt`)
  with a terminal-agnostic `TextEdit` message, driving two first consumers — `F7` creates a directory
  and `F2` renames the entry under the cursor (configurable as `make_dir` / `rename`). While a prompt
  is open the event loop routes keystrokes to the field (`Ctrl-C` still quits); names are validated
  (non-empty, not `.`/`..`, no `/`) and `VfsPath` parsing independently blocks traversal. Rename
  refuses to overwrite an existing destination (and aborts rather than risk a clobber on a non-
  not-found stat error). Completing any mutating op now clears stale positional marks.
- Per-pane **sort modes & hidden-file toggle** (M1-8): `s` cycles the active pane's sort
  (name → size → modified → type) and `.` toggles whether hidden entries (dotfiles) are listed. Directories
  always sort first; size/modified order the most-relevant first (largest / newest) with unknown
  values last and a case-insensitive name tiebreak. Cycling re-orders in place (no re-list) and keeps
  the cursor on the same entry; the hidden toggle re-lists via the backend's `ListOpts::all`. The
  active mode and hidden state show bottom-right in each pane. Both actions are configurable under
  `[ui.keybindings]` as `cycle_sort` / `toggle_hidden`.
- Config-driven **theme colors** (M8-7): `[ui.colors]` overrides individual render roles
  (`focused_border`/`unfocused_border`/`dir`/`error`/`status`/`selection_bg`/`selection_fg`) over the
  built-in `dark` preset, using color names or `#rrggbb`. A `Theme` is resolved from config and
  threaded through the renderer; unknown roles / unparseable colors are skipped with a warning.
- `cairn-vfs` **retry/backoff** (M4-4): `retry` + `RetryPolicy` re-run an operation while its
  `VfsError` is retryable, with capped exponential backoff (`backoff_delay`); the schedule is a pure,
  unit-tested function and non-retryable errors fail fast. Adopted on the SFTP adapter's idempotent
  `stat` (mutating ops are intentionally not auto-retried). Keepalive, bastion/jump-host chains, and
  live timeouts remain the integration step.
- AI **plan execution** (M7-6): a `BinaryStepExecutor` (RFC-0007) runs an approved plan's steps
  against the registered backends — the safe/local tools (`list`/`stat`/`read`/`copy`/`move`/
  `delete`) execute now via the VFS/transfer engine, resolving the model's opaque `conn:N` handles to
  backends; `exec`/`logs`/`port_forward`/`open_connection` report "not yet available" until the live
  invoke path (RFC-0007 Gap 1) lands. `Ctrl-A` now drives the full plan → confirm → **execute** loop.
- RFC-0007 (action invocation & agent-execution routing): resolves the two routing design gaps the
  review gates flagged — adds `path` to `Vfs::invoke` and defines `ActionOutcome::Session`/
  `SessionHandle` (Gap 1), and a typed per-tool input schema plus a `BinaryStepExecutor` that maps
  approved AI plan steps to `VfsRegistry`/transfer/`invoke` calls using opaque `conn:N` references
  (Gap 2). Unblocks the live invoke (M6-3/M6-6) and agent-execution (M7-6) work.
- **Connection switcher** (M4-5): `Ctrl-O` opens an overlay listing the available connections — the
  built-in local roots (`/`, `$HOME`) plus any `scheme = "local"` profiles from config — and selecting
  one re-points the active pane to it at its root. The reducer (`Overlay::Connections`) and overlay
  render are unit-tested; opening a *new* remote connection (SSH/cloud) is the integration step.
- Container/cluster **action surface** (M6-3/M6-6): the Docker and Kubernetes backends now advertise
  their backend-specific actions via `actions_at` — Docker exposes `exec`/`logs` across a container's
  subtree; Kubernetes exposes `logs`/`port-forward` on a pod and `logs`/`exec` on a container (by
  path depth). The actions are discoverable and unit-tested; live invocation (streaming/sessions over
  the engine/cluster API) remains the integration step, so `invoke` still returns `Unsupported`.
- `cairn-ai` **tool-call degradation** (M7-2): a `ToolSupport` tier on the provider trait
  (Native → JsonSchema → Text) and a `degrade` module that adapts how a plan is requested
  (native tool vs a JSON-object / fenced-block instruction in the prompt) and parsed back
  (structured tool call, bare JSON object, or fenced ```json block, with a string-aware brace
  matcher). `request_plan` now adapts to the provider's declared tier; all three tiers are tested
  against `MockProvider`. The concrete Ollama / OpenAI-compatible HTTP transport is the integration
  step.
- Config-driven **keybindings** (M8-7): a `[ui.keybindings]` map of chord → action (e.g.
  `"ctrl+a" = "ai_propose"`) layered over the built-in scheme. `cairn-tui::Keymap` parses chords
  (`ctrl+`/`alt+`/`shift+`, named keys, `f1`–`f12`) and action names, warns on (but skips) bad
  entries, and resolves overrides-then-default; the binary loads the user config at startup and falls
  back to defaults if it is missing or unreadable. Themes and shell-command actions are deferred (the
  latter is process execution and needs a security review).
- AI **plan → confirm** overlay (M7-6): `Ctrl-A` asks the assistant to propose a plan, which opens a
  review overlay showing each step with its reversibility. Approve step-by-step (`↵`), reject (`x`),
  or — only when no step is irreversible — bulk-approve (`a`); `Esc` aborts. The reducer
  (`Overlay::AiPlan`) is pure and unit-tested; the overlay renders via ratatui (`TestBackend` tests).
  Plans are produced from an offline `MockProvider` until the HTTP providers (M7-2) land; step
  execution against backends is the next integration step.
- RFC-0006 (plugin host & WIT ABI): the `cairn:plugin@1.0.0` WIT package (`types`/`host`/`backend`
  interfaces, `backend-plugin` world), the host↔guest mapping onto the `Vfs` trait, the
  capability-grant and credential-brokering model, resource limits/cancellation, and WIT semver
  rules — building on ADR-0004 and the M8-2 runtime core (M8-1).
- `cairn-backend-k8s`: the Kubernetes backend's `Vfs` mapping over a `KubeOps` transport seam — a
  navigable cluster tree (`/<context>/<namespace>/<pod>/<container>/<path…>`) with capabilities that
  vary by depth (list-only navigation; file read inside a container), fully unit-tested against an
  in-memory mock. Read-only mapping core; the live `kube-rs` adapter (auth via the broker,
  tar-over-`exec` filesystem) and the action surface (logs/exec/port-forward) are the integration
  step. RFC-0005 (M6-4/M6-5). Surfaces init/ephemeral containers via the new
  `EntryExt::KubeContainer` variant (`cairn-types`).
- `cairn-vfs`: `join_abs_path`, a shared helper for backends that map a subtree onto a remote
  filesystem (used by the Docker and Kubernetes backends).
- `cairn-backend-docker`: the Docker/OCI backend's `Vfs` mapping over a `ContainerOps` transport
  seam — a navigable tree (`/containers/<name>/…` browses a container's filesystem, `/images/<tag>`
  lists images), read-only, fully unit-tested against an in-memory mock, plus a `bollard` adapter
  that lists containers and images live. In-container filesystem access (tar/exec) and a live daemon
  are the integration step; RFC-0004 (M6-1/M6-2).
- `cairn-backend-ssh`: the SSH/SFTP backend's `Vfs` mapping over an `SftpOps` transport seam
  (list/stat/ranged-read/write/mkdir/rename/recursive-remove), fully unit-tested against an
  in-memory mock, plus a `russh-sftp` adapter; RFC-0003. Live SSH transport is the integration
  step (M4-1/M4-2).
- `cairn-plugin`: the sandboxed WASM plugin host on `wasmtime` — instantiate untrusted modules
  with a memory cap and execution-fuel limit (a runaway guest traps instead of hanging the host),
  and default-deny host imports (capability-gated). Hermetic, WAT-module tests (M8-2).
- `cairn-backend-object`: the provider-agnostic object-store core — the `ObjectStore` trait,
  prefix/delimiter listing synthesis (object-vs-prefix, directory-marker folding), an
  `ObjectStoreVfs` mapping onto `Vfs`, and an in-memory `MockObjectStore`. Cloud providers
  (S3/GCS/Azure) are feature-gated and not yet implemented (M5-1/M5-2).
- `cairn-ai` context & injection defense: a secrets-free `WorldSnapshot` for the model, an
  `<untrusted_data>` wrapper that neutralizes break-out attempts, an out-of-scope heuristic, and
  the standing system policy (M7-5).
- `cairn-ai`: the agentic AI core — a provider-agnostic `LlmProvider` trait + `MockProvider`, the
  closed tool surface (capability per tool; unknown tools rejected), and the plan→confirm→execute
  state machine (bulk-approve only when every step is safe/recoverable; failure aborts the rest).
  Hermetic (M7-3/M7-4; trait+mock for M7-1).
- `cairn-broker`: the capability broker — the sole mediator that resolves a credential id to a
  secret (journaled), with a secret-free world view for untrusted actors (M3-5).
- `cairn-config`: TOML configuration with connection profiles that reference a vault credential
  by id and cannot hold a secret by construction (M3-6).
- `cairn-secrets`: zeroizing secret types and a log-redaction helper.
- `cairn-vault`: encrypted secrets vault (XChaCha20-Poly1305 + Argon2id, header bound as AAD,
  atomic writes); passphrase unlock; 7 hermetic crypto tests (M3-1/M3-2).
- File operations in the TUI: copy (`c`/F5), move (`m`/F6), and delete (`d`/F8, with a confirm
  dialog) of the marked/current entries between panes, via the transfer engine; status feedback
  and auto-refresh (M2-6/M2-7).
- `cairn-transfer`: the transfer engine — stream-through copy/move across any two backends,
  directory-tree walk, server-side-copy fast path, conflict policies, size verification, and
  cooperative cancellation; RFC-0002. 6 hermetic tests (M2-1..4).
- `cairn-tui` (ratatui dual-pane render + keymap) and the binary event loop / effect runner:
  `cairn` now opens an interactive dual-pane local-filesystem browser — navigate, switch panes,
  mark, refresh, quit; non-blocking (M1-5/6/7/8). Graceful message when not run in a TTY.
- `cairn-core`: the Elm/TEA app model and pure `update` reducer (panes, cursor/marks, navigate,
  list events) — no I/O, 10 tests (M1-4).
- `cairn-backend-local`: the local filesystem backend over `tokio::fs` (list, stat, ranged
  read/write, mkdir/remove/rename, Unix perms), with RFC-0001 (M1-2/M1-3).
- Engineering rule: use the Context7 MCP for current library/API docs (CLAUDE.md §10, §13).
- `cairn-vfs`: the `Vfs` trait (object-safe, `#[async_trait]`, streaming `list`), capability
  provider, `ReadHandle`/`WriteHandle`, `VfsRegistry`, the action interface, and `MockVfs` (M1).
- `cairn-types` crate: `VfsPath` (normalized, traversal-safe), `Entry`/`EntryKind`/`EntryExt`,
  the `Caps` capability model, and `ConnectionId`/`Scheme` — the shared leaf vocabulary (M0).
- Binary edge: tracing init (`CAIRN_LOG`), a panic hook, and `--version`/`--help` (M0).
- Project scaffolding: engineering rules (`CLAUDE.md`), contribution and governance docs,
  GitHub issue/PR templates, CI workflow, and a minimal Cargo workspace.

### Changed
- `cairn-vfs`: `Vfs::invoke` now takes the target `path` (RFC-0007 Gap 1) so path-routed backends
  (Docker/Kubernetes) can identify the container/pod an action targets, and a new
  `ActionOutcome::Session` + `SessionHandle` model long-lived sessions (port-forward / interactive
  exec). The API is ready; the live engine/cluster streams remain the integration step.
- Tuned workspace clippy lints for velocity under CI's `-D warnings`: deny `clippy::all` + forbid
  unsafe + require rustdoc, but drop the over-broad `pedantic`/`unwrap_used`/`expect_used` lints
  (advisory via review per CLAUDE.md §9, not a hard test-breaking gate).
- Product Requirements Document (`docs/PRD.md`).
- Team-of-agents working model in `CLAUDE.md` §2: every feature and significant decision is
  run past the relevant specialist agent(s), with a domain→agent mapping.
- Vendored specialist agents under `.claude/agents/` so every contributor shares the same team.
  Includes Cairn-specific agents authored for this project: `tui-engineer`,
  `ai-integration-engineer`, `plugin-systems-engineer`, `container-backend-engineer`,
  `technical-writer`, plus client-backend-focused `kube-staff-engineer`, `network-engineer`,
  `storage-engineer`, and a Rust-focused `code-reviewer`.
- Low-Level Design (`docs/LLD.md`): architecture, the core async VFS abstraction + capability
  model, tokio/TEA app model, transfer engine, object-store backends, secrets vault + AI/plugin
  broker boundary, AI agentic layer, and WASM plugin system.
- ADRs recording the load-bearing decisions: core architecture (ADR-0001), vault crypto + broker
  boundary (ADR-0002), object-store SDKs (ADR-0003), WASM plugin runtime (ADR-0004).
- Implementation Plan & living progress tracker (`docs/IMPLEMENTATION_PLAN.md`): milestones M0–M8 +
  v0.1, work breakdown with status, critical path & dependency DAG, parallelization lanes, RFC
  sequencing, risk register, and a same-PR status-update rule (CLAUDE.md §5).
- GitHub Milestones M0–M8 + v0.1 backing the tracker.

### Changed
- Renumbered `CLAUDE.md` sections to accommodate the new team-of-agents model (§2).

[Unreleased]: https://github.com/zoza1982/cairn/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/zoza1982/cairn/releases/tag/v0.1.0
