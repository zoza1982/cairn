# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
