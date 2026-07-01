# Cairn — Implementation Plan & Progress Tracker

> **Status:** Draft v0.1 · **Owner:** Zoran Vukmirica · **Last updated:** 2026-06-27
> **Scope:** *When* and *in what order* we build Cairn — and the **single source of truth for
> progress.** Product scope: [`PRD.md`](PRD.md). Architecture: [`LLD.md`](LLD.md). Rules:
> [`../CLAUDE.md`](../CLAUDE.md).
>
> **This document is updated in the SAME PR that does the work** (see §9). A row whose status lies is
> a bug. Produced via the team-of-agents model (CLAUDE.md §2): `project-manager` +
> `workflow-orchestrator`, synthesized.

---

## 1. At-a-glance dashboard

> Keep this block accurate on every merge — it is the first thing anyone reads.

| Field | Value |
|---|---|
| **Phase** | **"Finish the project" (2026-06-29):** hold lifted — building every remaining milestone in order, network backends feature-gated + emulator-tested in CI |
| **Design docs** | ✅ PRD · ✅ LLD · ✅ ADR-0001..0006 · ✅ RFC-0001..0008 · 🟡 RFC-0010 (plugin sandbox + brokered host fns, Draft) |
| **Current milestone** | **Foundation: feature-gated backends + lean/full CI split (ADR-0006, PR-0). Next: M3-4 credentials → M4 SSH** |
| **v0.1 target** | Deep on local + SSH + S3; functional GCS/Azure; Docker/K8s/AI/plugins behind feature flags |
| **Milestones delivered** | M0, M1, M2, M3 (lib) ✅ · M5 abstraction + M7 core & plan→confirm UI + M8 runtime, WIT RFC & keybindings + M4 SFTP-mapping + M6 Docker- & K8s-mapping ✅ · **M6-3 complete**: Docker logs + interactive exec (RFC-0009 §2, bollard `create_exec`/`start_exec`/`inspect_exec`, 4 hermetic + 1 dind test) ✅ · **M6-6 complete**: K8s logs + exec + port-forward (RFC-0009 §1+§3) ✅ · **M6-7 exec+port-forward TUI pane**: `ExecPane`/`PortForwardStatus` overlays + session TEA loop wired (PR-4) 🟡 · cloud providers + live-transport + LLM HTTP providers + WASM component bridge ⏭ · **v0.1.0 release prep** 🟡 |
| **Work items ✅ / 🟡 / ☐ / ⛔ / ⏭** | 33 / 19 / 0 / 0 / 19 |
| **Cross-platform CI green** | ✅ Linux · ✅ macOS · ✅ Windows |
| **Long-pole items** | cloud/container/plugin backends (need live services + heavy SDKs) |

> **Unblocked (2026-06-29).** The "env-deferred" hold is lifted. Network backends now build behind
> non-default Cargo features (ADR-0006) so the default cross-platform CI stays lean/hermetic and the
> full TLS build runs Linux-only; live verification uses emulators (sshd/MinIO/Azurite/fake-gcs/kind/
> dind) in a dedicated, env-guarded integration job. Build order: SSH → object-store → containers →
> AI HTTP + MCP → plugin finish → v0.1 release. The legacy note below describes the prior state.
>
> **Environment note (legacy).** Items marked **⏭ env-deferred** require live or emulated network services
> (SSH servers, MinIO/Azurite/fake-gcs, dind, `kind`) and/or very heavy SDKs (`aws-sdk-s3`, `kube`,
> `bollard`, `wasmtime`) that cannot be built and integration-tested in the current environment. Their
> **designs are complete** (LLD §8/§11, ADR-0003/0004) and the **provider-agnostic cores that *can*
> be tested hermetically are implemented** — the `Vfs`/transfer abstractions (M1/M2), the object-store
> trait + listing synthesis + `Vfs` mapping with a mock (M5-1/2), and the AI provider trait + closed
> tools + plan machine + injection defense (M7 core). Each deferred backend becomes a focused PR
> (RFC → impl → emulator integration job) once those services are available.

**Burn-up note:** _Delivered M0–M3 (a working dual-pane local file manager + transfer engine +
copy/move/delete with confirm + encrypted vault + broker + config), the M5 object-store abstraction
(hermetic, mock-tested), and the M7 agentic AI core (provider/tools/plan→confirm/injection-defense).
11 library crates + the binary, ~140 hermetic tests, green cross-platform CI (PRs #5–#17). Remaining
network/SDK backends are env-deferred (see note)._

### Status legend (used in every work-item table)

| Symbol | Meaning |
|---|---|
| ☐ | Not started |
| 🟡 | In progress (branch active / PR open) — append the PR `#id` |
| ✅ | Done (merged, gates passed, docs updated) — append the PR `#id` |
| ⛔ | Blocked (note `blocked-by: <item>`) |
| ⏭ | Deferred (post-v0.1 / descoped; link tracking issue) |
| 🔁 | Needs rework (a review/gate sent it back) |

---

## 2. Phasing strategy — vertical depth first, breadth second

The PRD (§11) flags "all seven backends at launch" as the top risk. We **reject** building seven
shallow backends in parallel and instead:

1. **Prove the abstraction once, end-to-end, before replicating it.** Everything rests on the `Vfs`
   trait + TEA loop + transfer engine + broker boundary. Build one complete vertical slice (local:
   browse → operate → transfer, on screen, non-blocking) so a flaw in `Vfs` is found at M1 (cheap)
   not after six backends (catastrophic).
2. **Order backends by how hard they stress the abstraction** — but promote the object store early
   because the LLD (§3.6) says it exercises the hardest paths (pagination, multipart, resume,
   server-copy): **local → SSH/SFTP → S3 (deep) → GCS + Azure (cheap via contract tests) → Docker +
   Kubernetes (the exec/logs/port-forward action model)**.
3. **Security infrastructure precedes any credentialed backend.** Vault + broker land at **M3**,
   *before* SSH/object stores: CLAUDE.md forbids plaintext secrets anywhere, and the broker boundary
   is a compile-time dependency-graph property far cheaper to establish up front than to retrofit.
4. **AI and plugins come last** — both depend only on `cairn-broker`, both are untrusted, and both
   are only meaningful once there is real functionality to drive. (The AI lane *may* start in
   parallel against a `MockBroker` once the broker API is frozen — see §6.)

**Depth-vs-breadth contract for v0.1:** deep on **local, SSH, S3**; functional on **GCS/Azure**
(parity via contract tests, lighter polish); **Docker/K8s, AI, plugins behind feature flags**,
demoable but "preview". Matches the LLD feature-flag design (default build = local + SSH).

### Milestone map

| Milestone | Theme | Demoable outcome | Primary crates |
|---|---|---|---|
| **M0** | Scaffolding & guardrails | green CI on 3 OSes; empty app opens/quits; protections + milestones live | workspace, `cairn`, CI |
| **M1** | The abstraction, proven (local slice) | dual-pane browse of local FS; sort/filter/nav; 100k-entry dir scrolls smoothly, non-blocking | `cairn-types`, `cairn-core`, `cairn-vfs`, `cairn-backend-local`, `cairn-tui` |
| **M2** | Operations & transfer engine | copy/move/delete/mkdir/rename with a live progress queue (pause/resume/cancel) | `cairn-transfer`, `cairn-core`, `cairn-tui` |
| **M3** | Secrets foundation (vault + broker) | create/unlock encrypted vault; store/list a credential; broker mediates; nothing plaintext | `cairn-secrets`, `cairn-vault`, `cairn-broker`, `cairn-config` |
| **M4** | First remote (SSH/SFTP) | connect via vault key/agent; browse, edit-in-place, transfer local↔SSH | `cairn-backend-ssh` |
| **M5** | Object storage (S3 → GCS/Azure) | browse a huge bucket; multipart + resumable upload; cross-backend local↔S3; GCS/Azure parity | `cairn-backend-object`, `cairn-transfer` |
| **M6** | Containers & clusters (action model) | browse Docker/K8s; stream logs; exec; port-forward; copy pod→S3 | `cairn-backend-docker`, `cairn-backend-k8s` |
| **M7** | Agentic AI (plan→confirm→execute) | NL request → reviewed plan → confirmed execution via broker; cloud + Ollama | `cairn-ai`, `cairn-broker`, `cairn-tui` |
| **M8** | Extensibility (WASM plugins) | load a sandboxed plugin backend, capability-gated, indistinguishable from built-in | `cairn-plugin`, `cairn-plugin-sdk` |
| **v0.1** | Release | tagged cross-platform binaries; docs/install/changelog; §2 depth-vs-breadth met | all |

Each milestone is a **GitHub Milestone**; bold work items become **GitHub Issues** as they enter
"Ready" (§9).

---

## 3. Work breakdown

> Item IDs (`M<n>-<k>`) are stable. Each item is **PR-sized (one logical change)**. "Docs" lists the
> *minimum* per CLAUDE.md §5 (every PR also needs a full description + CHANGELOG entry). Every
> functional item carries a test obligation. Append the PR `#id` to the Status cell.

### M0 — Scaffolding & guardrails

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M0-1 | Cargo workspace + crates with strict acyclic deps; lints tuned for `-D warnings`; toolchain | software-architect, rust-staff-engineer | — | ADR-0001; rustdoc stubs | workspace builds on 3 OSes; clippy `-D warnings` clean | 🟡 lint tuning + bin/types #5; crates created lazily per milestone |
| M0-2 | CI matrix (fmt, clippy -D, test, doc, deny) × Linux/macOS/Windows | devops-engineer | — | CI README; PR template; CODEOWNERS | all checks green on 3 OSes; required for merge | ✅ #1 |
| M0-3 | Branch protection, labels, GitHub Milestones M0–M8+v0.1, issue templates | project-manager, devops-engineer | M0-2 | CONTRIBUTING | `main` rejects direct push; milestones + labels exist | ✅ #1, #4 (milestones) |
| M0-4 | Binary edge: bootstrap, tracing, panic hook, `--help/--version` (redaction layer → M3) | rust-staff-engineer | M0-1 | rustdoc | launches, prints version, exits 0; CLI + panic-hook tests | ✅ #5 |
| M0-5 | `cairn-types`: `VfsPath` (rejects `..`/control), `Entry`, `Caps`, ids | rust-staff-engineer | M0-1 | rustdoc on all public items | path parse/traversal-rejection tests green | ✅ #5 |
| M0-6 | Test/QA harness: hermetic-offline policy (MockVfs lands with `cairn-vfs` in M1) | qa-engineer | M0-5 | — | `cargo test` (no features) hermetic & offline | 🟡 policy in force; MockVfs in M1 |

### M1 — The abstraction, proven (local vertical slice)

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M1-1 | `Vfs` trait set (async_trait, streaming `list`, Read/Write handles), `CapabilityProvider`, `VfsRegistry`, `MockVfs` | rust-staff-engineer, software-architect | M0-5 | confirm ADR-0001; rustdoc | object-safe (`Arc<dyn Vfs>`); MockVfs read/write/list/remove tests pass | ✅ #6 |
| M1-2 | RFC: **local backend** deep design (symlinks, perms, watch, Windows paths) | rust-staff-engineer, technical-writer | M1-1 | RFC merged | approved before M1-3 | ✅ #8 (RFC-0001) |
| M1-3 | `cairn-backend-local`: list, stat, read/write (ranged), mkdir/remove/rename/set_perms; correct `Caps` | software-engineer, rust-staff-engineer | M1-2 | rustdoc; RFC-0001 | unit + temp-dir tests green (6) | ✅ #8 |
| M1-4 | `cairn-core` TEA skeleton: `AppState`, `Msg/AppEvent/AppEffect`, pure `update()` | rust-staff-engineer, software-architect | M0-5 | confirm ADR-0001; rustdoc | `update()` unit-tested pure (10 tests); no I/O in core | ✅ #9 |
| M1-5 | Effect runner (binary): tokio rt, `VfsRegistry`, async list dispatch, bounded `event_tx` | rust-staff-engineer | M1-1, M1-4 | rustdoc | spawns listing tasks; results flow back as events (tested) | ✅ #10 |
| M1-6 | `cairn-tui` render: ratatui dual panes, titles, status bar; pure over `&AppState` | tui-engineer | M1-4 | rustdoc | renders static `AppState` (TestBackend tests); zero I/O in render | ✅ #10 |
| M1-7 | Input + keymap: blocking-thread reader, MC/vim default keymap (chords/presets later) | tui-engineer | M1-6 | rustdoc | nav/quit keys mapped (tests); input off the async runtime | ✅ #10 |
| M1-8 | Wire-up: TEA event loop in the binary; browse local FS in both panes, nav in/out, non-blocking | tui-engineer, software-engineer | M1-3, M1-5, M1-7 | rustdoc | **Demo:** `cairn` opens cwd dual-pane, navigate, Tab, marks, quit | ✅ #10 |
| M1-9 | Large-list virtualization + off-thread sort/filter, filter-as-you-type | tui-engineer, performance-tuning-engineer | M1-8 | perf note | 100k dir smooth; first page <100 ms | ✅ filter-as-you-type (#39) + **row virtualization** (#42): only the on-screen window of rows is materialized (cursor-centred `list_window`), so a 100k-dir frame is O(viewport) not O(entries). Off-thread sort/filter (the O(n) in-thread filtered scan) is a later perf pass, not a feature gap |
| M1-10 | Multi-select, sort (name/size/date/type), show/hide hidden | software-engineer, tui-engineer | M1-8 | user docs | selection + sort unit tests | ✅ marks + name/size/modified/**type** sort (`s`) + hidden toggle (`.`) done (#35, #38): cursor follows its entry across re-sort, modes configurable via `[ui.keybindings]`, status shown per pane |

### M2 — Operations & transfer engine

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M2-1 | RFC: **transfer engine** (queue, conflict, resume format, pause/cancel) | storage-engineer, software-architect, technical-writer | M1-1 | RFC merged | approved before M2-2 | ✅ #11 (RFC-0002) |
| M2-2 | `cairn-transfer` core: stream-through copy + dir-tree walk, server-copy fast path | storage-engineer, rust-staff-engineer | M2-1 | rustdoc | cross-backend copy via MockVfs (tests) | ✅ #11 |
| M2-3 | Move = rename-or-(copy→verify→delete); conflict policy (Skip/Overwrite/Rename/NewerWins/Prompt) | storage-engineer | M2-2 | rustdoc | move + all conflict modes tested | ✅ #11 |
| M2-4 | Pause/resume/cancel, retry w/ backoff+jitter, global+per-backend semaphores | storage-engineer, network-engineer | M2-2 | rustdoc | cancel mid-chunk (tested) | 🟡 engine cancellation + **UI cancel (`Esc`) wired** (#41): the in-flight transfer's `CancellationToken` is held runtime-side and fired by a `CancelTransfer` effect; cancel reports a partial-changes warning. **Pause/resume done** (#53 engine + UI): `run_transfer` takes a `watch::Receiver<bool>` and blocks at the next check-point while paused (deadlock-safe clone + `borrow_and_update` + `select!`; cancel preempts a pause). The event loop owns a `watch::Sender<bool>` per transfer, driven by a `SetTransferPaused` effect; `p` toggles pause/resume (no-op when idle), status shows `⏸ paused` and drops rate/ETA. **Concurrent transfers done** (id-keyed transfer collection #66, then bounded concurrency): up to N run at once (`[transfers] concurrency`, default 2, clamped ≥1) via per-transfer cancel/pause keyed by a stable `TransferId`; `Esc`/`p` act on all active, the rest queue and start as slots free; a panicked transfer task always releases its slot (drop-guard). Retry/backoff still deferred to M5; partial-outcome counts on cancel done (#43) |
| M2-5 | Transfer queue UI overlay: per-item progress/speed/ETA, reorder, controls | tui-engineer | M2-2, M1-6 | user docs | **Demo:** copy a big tree local→local with live queue | 🟡 live byte-progress (#40) + **sequential FIFO queue** (#45) + **queue view overlay** (#46, `Ctrl-T`) + **throughput rate** (#48) + **queue controls** (#50) + **reorder** (#51, Shift-K/J): a transfer issued while one runs is queued and auto-started; status shows bytes, rate, queue depth; the queue view can reorder or drop a specific pending transfer. **progress %/ETA done** (#52, best-effort source pre-scan → `X/Y (Z%)` + ETA, degrades gracefully). **Pause/resume + concurrent transfers done** (#53/#56/#66 + this): status aggregates N active (`⇅ 2 active · … (+K queued)`), the `Ctrl-T` view lists every active transfer + pending; idle status messages now render. Per-transfer (not just all) pause/cancel from the overlay = follow-up |
| M2-6 | Operation keys: copy (F5/c), move (F6/m), delete (F8/d) wired to engine | tui-engineer, software-engineer | M2-2 | user docs | copy/move/delete flows work; delete confirms | ✅ copy/move/delete + mkdir (F7) + rename (F2) done (#36); **overwrite-confirm** (#44): interactive copy/move asks before clobbering existing destinations (no silent overwrite), rename refuses overwrite |
| M2-7 | Confirm-dialog + overlay input interception (foundation for plan→confirm) | tui-engineer, security-engineer | M2-6 | rustdoc | destructive op cannot dispatch without confirm (tested) | ✅ #12 |

### M3 — Secrets foundation (vault + broker) · **security-review required on every item**

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M3-1 | `cairn-secrets`: `SecretString/Box`, `Zeroizing`, redaction layer (AWS/bearer/SAS/PEM/JWT) | security-engineer, rust-staff-engineer | M0-4 | rustdoc; threat-model note | no `Debug`/`Serialize` leak (compile test); redaction tests | ✅ #13 |
| M3-2 | Vault at rest (ADR-0002): XChaCha20-Poly1305, header-AAD, encrypted index, per-entry DEKs, postcard, atomic write+`.bak`+lock | security-engineer | M3-1 | confirm ADR-0002; format spec | seal/open round-trip; tamper/rollback tests; refuse unknown version | ✅ #13 |
| M3-3 | Key hierarchy + unlock: KEK in OS keychain (`keyring`), Argon2id fallback, auto-lock | security-engineer | M3-2 | rustdoc; SECURITY.md update | keychain + passphrase paths tested; auto-lock zeroizes | 🟡 **`UnlockProvider` landed (PR-C)**: trait (`passphrase`/`store`/`persists`) + `open_with`; `PassphraseUnlockProvider` (headless/prompt fallback), `KeychainUnlockProvider` over `keyring` 4.x behind the non-default `keychain` feature (Secret Service/Keychain/Cred-Manager, service `cairn`/account `vault`; `NoEntry`→`UnlockError::NotFound`, secret-free `Backend`), `MockUnlockProvider` (`test-utils`). Keychain path tested **hermetically** vs `keyring-core`'s mock store (no real keychain/dbus). Zeroizing passphrase unlock from M3-2. **Remaining**: auto-lock (zeroize on idle) + TUI unlock overlay (M3-7); SECURITY.md threat-model note for keychain trade-off |
| M3-4 | Credential model: typed `CredentialSecret` + delegation variants; `TokenCache` | security-engineer | M3-2 | rustdoc | variants seal; delegation stores no secret (test) | 🟡 RFC-0008 + **broker-api split landed** (PR-A): `cairn-broker-api` (`CredentialDirectory`/`CredentialInfo`), `cairn-ai` no longer reaches `cairn-vault` (compile-time, enforced by a `cargo metadata` dep-closure test); `CredentialId` moved to `cairn-types`. **PR-B done**: typed `CredentialSecret` (SSH variant: password/private-key/agent) sealed via a `pub(crate)` zeroizing wire-mirror, no `Debug`/`Serialize` on the public type (compile_fail-guarded), `Broker::resolve`→typed secret, `CredentialInfo` carries `CredentialShape`, vault format v2. **Next**: PR-C keychain `UnlockProvider`+auto-lock, PR-D vault-unlock TUI; other backends' variants + `TokenCache` land with M5/M6 |
| M3-5 | `cairn-broker`: `authorize`/`execute` split, capability+scope, resolve `CredentialId`→secret inside execute, journal `Actor` | security-engineer, software-architect | M3-3, M3-4 | confirm ADR-0002; rustdoc | secret never leaves `execute`; no API returns a secret (compile + review) | ✅ #14 (broker: resolve + journal; full authorize/confirm in M7) |
| M3-6 | `cairn-config`: TOML, `ConnectionProfile` (ref only, type-enforced no-secret), state dir, schema+migration | software-engineer | M0-5 | rustdoc; config docs | config↔vault boundary compile-enforced; migration round-trip | ✅ #14 |
| M3-7 | Vault TUI: create/unlock, list (labels only), add credential; on-screen redaction | tui-engineer, security-engineer | M3-3, M2-7 | user docs | **Demo:** create vault, store cred, relock — nothing plaintext | 🟡 **unlock flow landed**: `Overlay::VaultUnlock` (Ctrl-U / `vault_unlock`) with a **no-echo** masked field (`MaskedInput`: redacted `Debug`, zeroized on drop/clear; the typed secret never enters `AppState` `Debug` or logs). Submitting emits `AppEffect::UnlockVault { passphrase: SecretString }`; the runtime opens the vault off the render path (`Vault::open` via `spawn_blocking`), `broker.unlock(vault)`, then retries the credential-bearing connections **deferred at startup** because the vault was locked, adding the successes to the switcher and reporting the count. Wrong passphrase / missing vault keep the overlay open with a secret-free, retryable error. The `Arc<Broker>` + deferred profiles now live in the runtime `VaultContext` (not in `AppState`). New `[vault] path` config (default `…/cairn/vault.cvlt`). Reducer + render + effect tests (open/cancel, no-echo invariant, wrong-passphrase-retry, success transition vs a temp `fast_for_tests` vault; deferral→unlock→retry under `all-backends`). **Remaining follow-ups:** ~~vault **creation** UI~~ (landed in RFC-0011 P4.5, `feat/conn-p45-vault-create`) + add/list-credential UI, and **auto-lock-on-idle** (zeroize the broker after inactivity) |

### M4 — First remote (SSH/SFTP)

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M4-1 | RFC: **SSH/SFTP backend** (auth chain, bastion/jump, keepalive, `!Send` proxy strategy) | network-engineer, technical-writer | M1-1, M3-5 | RFC merged | approved before M4-2; `assert_send` plan documented | ✅ #19 (RFC-0003) |
| M4-2 | `cairn-backend-ssh`: connect (key/agent via broker), list/stat/read/write (streaming, RANDOM_READ) | network-engineer, rust-staff-engineer | M4-1 | rustdoc; backend README | against SSH image (CI): browse + read/write; auth via broker only | ✅ #19 SFTP→Vfs mapping + russh-sftp adapter (mock-tested); **live connect done**: `ssh_connect` (russh, `ssh` feature) — TCP→handshake→host-key (Strict/AcceptNew, changed-key always rejected; unit-tested)→auth (password/key+passphrase/agent, consumes `CredentialSecret::Ssh`)→sftp subsystem→`SftpVfs`. `assert_send_sync` guard for the russh `!Send` risk. Live full round-trip via the sshd integration job (M4-6) |
| M4-3 | SSH mutations + `exec` (remote grep→SEARCH_CONTENT), edit-in-place save-back | network-engineer | M4-2 | rustdoc | rename/delete/mkdir; exec returns Stream; edit→save round-trip | 🟡 rename/remove/mkdir done; exec + edit-save + live transport deferred |
| M4-4 | Transport resilience: timeouts, keepalive, retry/backoff, bastion/proxy-jump chain | network-engineer | M4-2 | rustdoc; resilience note | simulated stall fails+retries (no hang); jump-host test | 🟡 retry/backoff core (#31): `cairn_vfs::retry` + `RetryPolicy` (capped exponential backoff, retries only `VfsError::is_retryable`, mutations excluded), unit-tested against a flaky op; adopted on the SFTP adapter's idempotent `stat`. Keepalive, bastion/jump-host chain, and live timeouts = integration step |
| M4-5 | Connection switcher UI + new-SSH flow, profile persistence (ref-only) | tui-engineer, software-engineer | M4-2, M3-7 | user docs | **Demo:** Ctrl-K → connect → browse → transfer local↔SSH | 🟡 switcher UI (#28): `Ctrl-O` overlay lists registered connections (built-in local roots + `scheme="local"` config profiles) and re-points the active pane; reducer + render mock-tested. **Broker-backed connection opener landed** (first integration slice): `crate::connect::ConnectionOpener` resolves a profile's `secret_ref` via the `Broker`, builds the scheme's `*ConnectParams` from `endpoint` fields, and dispatches to `ssh_connect`/`s3_connect`/`gcs_connect`/`azure_connect` → `Arc<dyn Vfs>`; binary feature umbrellas `ssh`/`s3`/`gcs`/`azure` (+ `cloud`/`all-backends`) keep the default build lean (ADR-0006), and a scheme whose feature is off errors "not built in" with no I/O. Startup opens credential-bearing config profiles through it (`Actor::User`), skipping un-openable ones with a warning. Hermetic tests: dispatch, profile→params, and the resolve→connect path (S3 profile + SSH credential → `Auth` pre-network). **RFC-0011 P1 landed**: `ConnectionCoordinator` + `BuiltinLocalProvider` + `SavedProfileProvider`; descriptor side-map established; `DeferredConnection`; id-assignment contract tests. **RFC-0011 P2 landed** (ADR-0007): lazy on-select connection opening. `Profile` targets enumerated as `NeedsOpen`/`NeedsVault` at startup — no open attempt. `AppEffect::OpenConnection` / `AppEvent::ConnectionOpened` TEA round-trip: the effect runner looks up the descriptor, opens async (spawned), navigates on success. Vault-unlock reconciliation: flip all `NeedsVault→NeedsOpen`, auto-open only the triggering conn. `DeferredConnection` retired (always empty). `run_vault_unlock_effect` returns `Result<(), String>`. Stable id reuse via `prior_descriptors` map. All CI checks green (lean + all-features clippy, workspace tests, rustdoc). **Remaining:** new-remote-connection TUI (create/trigger a profile). **RFC-0011 P3 landed** (branch `feat/conn-p3-discovery`): Docker socket auto-discovery (`DockerProvider`, 3 candidates probed concurrently with 500 ms timeout via bollard, rootless + Podman sockets), Kubeconfig + in-cluster discovery (`KubeconfigProvider`, `spawn_blocking` for FS/YAML, SA-token existence check), `join_all` coordinator, `DiscoveryConfig` in `cairn-config` (`docker`, `kubernetes`, `hidden`, `pinned`), hidden/pinned overlays, `[auto]` badge in TUI, feature-gated (`docker`/`k8s`) with clean lean build, all CI checks green (lean + all-features clippy, workspace tests, rustdoc). **RFC-0011 P4 landed** (branch `feat/conn-p4-form`): in-app connection form (add/edit/delete). `Ctrl-N` opens scheme picker → field form; `e` edits selected Profile; `d` deletes. `cairn_core::forms::{FieldSpec,ProfileData,scheme_fields,KNOWN_SCHEMES}` provides a dep-free mirror of `ConnectionProfile`. `AppEffect::SaveConnection`/`DeleteConnection` + `AppEvent::ConnectionSaved`/`ConnectionDeleted` complete the TEA round-trip. `run_save_connection_effect`/`run_delete_connection_effect` in `cairn/src/app.rs` load/save `cairn.toml` and rebuild the switcher via `register_connections`. `AppState::saved_profiles` populated at startup. Credential capture deferred to P5 (one-line hint shown in form). All CI checks green. **RFC-0011 P4.5 landed** (branch `feat/conn-p45-vault-create`): vault-create-from-UI security gate. `Overlay::VaultCreate` (two `MaskedInput` fields, Tab-cycling focus, `[Ctrl-R] Remember` keychain toggle); submit path validates min-length (8 chars), compares fields without exposing bytes, emits `AppEffect::CreateVault { passphrase: SecretString, remember }`. Argon2id in `spawn_blocking`; vault written atomically at 0600; broker unlocked + `NeedsVault` connections flipped to `NeedsOpen` on success; `AppState::vault_file_exists` drives VaultCreate-vs-VaultUnlock branch. `Debug` never reveals passphrase; error strings are value-free. 17 new hermetic tests (field editing, mismatch, min-length, double-submit guard, pending-conn routing, 0600 perms, `Debug` redaction). All CI checks green (lean + all-features clippy, workspace tests, rustdoc). |
| M4-6 | Cross-backend transfer validation local↔SSH | qa-engineer, storage-engineer | M4-2, M2-2 | test docs | copy/move both directions; integrity verified | ⏭ env-deferred (live SSH server + russh SDK) |

### M5 — Object storage (S3 → GCS/Azure)

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M5-1 | `ObjectStore` trait + `ObjectStoreVfs` wrapper (ADR-0003) | storage-engineer, software-architect | M1-1 | confirm ADR-0003; rustdoc | trait+wrapper compile; `MockObjectStore` harness | ✅ #17 (ObjectStore trait, MockObjectStore, prefix→Dir merge, ObjectStoreVfs; 8 tests) |
| M5-2 | **Object-store contract test suite** (all three providers) | qa-engineer, storage-engineer | M5-1 | TESTING.md | runs against MockObjectStore + MinIO; gates each provider | 🟡 MockObjectStore + listing tests done; **MinIO integration job landed** (`integration.yml`, env-guarded `CAIRN_IT_S3`: full put→list→head→get(+range)→copy→delete round-trip vs a MinIO service). Multi-provider (GCS/Azure) emulator suite still needs those SDKs |
| M5-3 | S3 provider (`aws-sdk-s3`): list (continuation, common-prefixes), head, ranged GET | storage-engineer | M5-1 | rustdoc | against MinIO: list via bounded window; contract pass | ✅ `S3ObjectStore`/`s3_connect` behind the `s3` feature (list w/ continuation+common-prefixes, head, ranged GET, put, delete, server-copy); typed `CredentialSecret::Aws` (static/profile/default-chain); MinIO endpoint + path-style. Multipart/resume = M5-4/5 |
| M5-4 | S3 multipart upload (16 MiB threshold, 8 MiB parts, concurrent, CRC32C), abort-on-drop | storage-engineer | M5-3 | rustdoc | >16 MiB multipart; abort leaves no orphans (test) | ⏭ env-deferred (S3/GCS/Azure SDKs + emulators) |
| M5-5 | S3 resume (part-state, `list_parts` reconcile, `SourceChanged`), server-copy | storage-engineer | M5-4, M2-1 | rustdoc; resume spec | kill+resume completes; same-provider fast-path | ⏭ env-deferred (S3/GCS/Azure SDKs + emulators) |
| M5-6 | S3 integrity/consistency: conditional writes (412→Conflict), `VerifyPolicy`, broker creds (`ArcSwap`) | storage-engineer, security-engineer | M5-3, M3-5 | rustdoc | 412→conflict-policy test; presigned URLs redacted | ⏭ env-deferred (S3/GCS/Azure SDKs + emulators) |
| M5-7 | Cross-backend local↔S3 and SSH↔S3 (checksum verify) | qa-engineer, storage-engineer | M5-4, M4-2 | test docs | **Demo:** copy local→S3, SSH→S3 with verification | ⏭ env-deferred (S3/GCS/Azure SDKs + emulators) |
| M5-8 | GCS provider (`google-cloud-storage`, crc32c, generation preconds, ADC/SA via broker) | storage-engineer | M5-2, M5-6 | rustdoc | contract green vs fake-gcs-server | ✅ `GcsObjectStore`/`gcs_connect` behind the `gcs` feature (list+page-token, head, ranged read, write, delete, server-copy via rewrite; data-plane HTTP `Storage` + control-plane gRPC `StorageControl`); typed `CredentialSecret::Gcp` (service-account JSON / ADC); endpoint+anonymous for emulators. Live fake-gcs job deferred (control plane is gRPC; HTTP emulators don't cover it) — adapter type-checked, `ObjectStore`→`Vfs` mapping proven by the S3 MinIO job |
| M5-9 | Azure provider (`azure_storage_blobs`, per-block MD5, shared-key/SAS/AAD via broker) | storage-engineer | M5-2, M5-6 | rustdoc | contract green vs Azurite | ✅ `AzureObjectStore`/`azure_connect` behind the `azure` feature (list+continuation, head, ranged read, write, delete, server-copy); typed `CredentialSecret::Azure` (shared-key / SAS; AAD reserved); 0.21 SDK on rustls; **live Azurite job** (`integration.yml`, `CAIRN_IT_AZURE`). AAD adapter wiring + per-block MD5 = follow-ups |
| M5-10 | Backend-aware UX: tier badges, versioned soft-delete honesty, archive-tier cost confirm | tui-engineer, ux-engineer | M5-3 | user docs | Glacier read raises cost confirm; delete-marker messaging clear | ⏭ env-deferred (S3/GCS/Azure SDKs + emulators) |

### M6 — Containers & clusters (action model)

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M6-1 | RFC: **Docker backend** (fs via archive API, image layers, exec/logs) | container-backend-engineer, technical-writer | M1-1 | RFC merged | approved before M6-2 | ✅ (RFC-0004, #20) |
| M6-2 | `cairn-backend-docker` (`bollard`): list containers+images; browse container fs (tar); image layers RO | container-backend-engineer | M6-1, M3-5 | rustdoc; backend README | against dind: browse fs; copy in/out | ✅ **live fs browsing landed**: `BollardDocker` lists containers+images AND browses a running container's fs — `list_dir`/`stat`/`read` over the Engine archive API (`download_from_container` → tar parse), 404→NotFound; dind integration job (`CAIRN_IT_DOCKER`). Image-layer RO browsing + copy-in = follow-ups |
| M6-3 | Docker actions: `exec`, `logs` (Stream), start/stop | container-backend-engineer | M6-2 | rustdoc | exec interactive stream; logs follow | ✅ **logs + exec landed**: `DockerVfs::invoke("logs")` returns `ActionOutcome::Stream` (bollard demuxed frames, `follow`/`tail` from `ActionCtx::Logs`). `DockerVfs::invoke("exec")` with `ActionCtx::Exec { argv, tty }` returns `ActionOutcome::Session(SessionHandle)` via bollard `create_exec` → `start_exec` → relay tasks → `inspect_exec`. `ContainerOps::exec` seam on the trait; `MockDocker::exec` (echo-style, resize-drain, cancel→`Ok(-1)`, stdin-close→`Ok(0)`); `BollardDocker::exec` (stdin relay `AsyncWrite` task, resize relay `resize_exec` task, main `tokio::select!` loop over output stream, `inspect_exec` for exit code). 4 new hermetic unit tests (`invalid_ctx`, non-tty echo round-trip, tty resize+cancel, unknown-container `NotFound`) + 1 dind integration test (`docker_container_exec_round_trip`: echo `CAIRN_EXEC_MARKER`, assert marker in stdout, assert exit code 0, `CAIRN_IT_DOCKER`-gated). Workspace clippy + rustdoc clean. **Remaining:** start/stop (deferred; not in RFC-0009 scope). |
| M6-4 | RFC: **Kubernetes backend** (ctx→ns→pod→container→fs, exec/cp/logs/port-forward, auth) | kube-staff-engineer, technical-writer | M1-1 | RFC merged | approved before M6-5 | ✅ (RFC-0005, #21) |
| M6-5 | `cairn-backend-k8s` (`kube`): navigable tree, watch strategy, kubeconfig/exec-plugin auth via broker | kube-staff-engineer | M6-4, M3-5 | rustdoc; backend README | against `kind`: browse ns/pods; multi-context | ✅ **live tree landed**: `kube-rs` 4.0 adapter behind the `k8s` feature — `list_contexts`/`list_namespaces`/`list_pods`/`list_containers` (rustls, no OpenSSL); 403→Forbidden, 404→NotFound; `kind` integration job (`CAIRN_IT_K8S`). In-container fs (exec-tar), watch, and exec-plugin/broker auth = follow-ups (M6-5b/M6-6) |
| M6-5b | In-container filesystem browsing via tar-over-exec | kube-staff-engineer | M6-5 | rustdoc | `list_dir`/`stat`/`read` on a kind pod; `exec_unavailable` on no-tar container | ✅ **landed**: `list_dir`/`stat`/`read` implemented over `exec` (`tar cf - -C <path> .` / `tar cf - -C <parent> <basename>`); pure tar helpers in `tar_exec` module (11 hermetic unit tests); `exec_unavailable` returned when container lacks `tar`; kind integration test exercises `/`, `/etc`, `/etc/hostname` stat+read with graceful skip on no-tar; `tar` dep added behind `k8s` feature |
| M6-6 | K8s streaming: `logs(follow)` Stream, `exec` (tty Session), `port-forward` | kube-staff-engineer | M6-5b | rustdoc; RFC-0009 | cp out of pod completes (no stall); port-forward holds | ✅ **complete**: logs + exec + port-forward all landed. `KubeVfs::invoke("logs")` → `ActionOutcome::Stream`; `KubeVfs::invoke("exec")` with `ActionCtx::Exec` → `ActionOutcome::Session(SessionHandle)` via `Api::<Pod>::exec` + `AttachParams` (relay tasks: stdin/stdout/stderr + TTY resize); `KubeVfs::invoke("port-forward")` with `ActionCtx::PortForward` → `ActionOutcome::Session(SessionHandle)` via `Api::<Pod>::portforward` + `TcpListener` accept loop (one `Portforwarder` per connection, `copy_bidirectional` relay, ephemeral-port support, `cancel→Ok(0)`). `SessionHandle` refined per RFC-0009 §1: `done: Result<i32, VfsError>`, `resize` field, `SessionHandle::new()` constructor. `MockKube` implements echo-style exec + echo-server port-forward (real `TcpListener`). 9 new hermetic unit tests (4 exec + 4 port-forward + 1 was pre-existing); 2 kind integration tests (`k8s_exec_non_tty_round_trip` + `k8s_port_forward_binds_and_accepts_connection`, `CAIRN_IT_K8S`-gated). Workspace clippy + rustdoc clean. TUI exec/port-forward pane = M6-7 (PR-4). |
| M6-7 | Stream/Session UI: live log viewer (follow+filter), exec pane, port-forward status | tui-engineer | M6-3, M6-6 | user docs; RFC-0009 | **Demo:** stream pod logs; exec; copy pod→S3 | 🟡 **exec pane + port-forward status landed** (M6-7 PR-4): `SessionId`/`SessionRecord`/`SessionEnd` types; `Overlay::ExecPane` (cooked-mode line I/O, scroll, follow, `Ctrl-]` detach, `Ctrl-D` close-stdin); `Overlay::PortForwardStatus` (bound-port display, one-key teardown); `AppEffect::OpenExecSession`/`OpenPortForward`/`CloseSession`/`SendSessionInput`/`ResizeSession`; `AppEvent::SessionOutput`/`SessionEnded`/`PortForwardBound`; `SessionControls` + `run_exec_session_effect`/`run_port_forward_effect` in app.rs; `render_exec_pane`/`render_port_forward_status` in cairn-tui. 10 reducer unit tests + 5 render tests. **Remaining:** in-viewer log filter. |

### M7 — Agentic AI (plan→confirm→execute) · **security-review required**

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M7-1 | `LlmProvider` trait + `StreamChunk` normalization; Claude provider | ai-integration-engineer | M3-5 | rustdoc; ADR ref | `MockProvider` + Claude path; no live API in CI | 🟡 LlmProvider trait + MockProvider done; cloud/local HTTP providers + streaming deferred |
| M7-2 | Ollama + OpenAI-compat providers w/ tool degradation (Native→JsonSchema→Text) | ai-integration-engineer | M7-1 | rustdoc; local-model doc | degradation tiers tested vs MockProvider | 🟡 degradation core (#26): `ToolSupport` tier on the provider trait + `degrade` module (encode tools-vs-prompt / decode tool-call·bare-JSON·fenced-block), `request_plan` adapts to the tier; all three tiers tested vs `MockProvider`. **Live HTTP transport done** (behind the non-default `http` feature, reqwest+rustls): `AnthropicProvider` (Claude Messages API — `x-api-key`/`anthropic-version`, `Role::System`→top-level `system`, first `tool_use`→`ToolCall` else text→`Text`; Native tier) and `OllamaProvider` (`/api/chat`; robust `JsonSchema` tier — Ollama native tools are model-dependent). Non-streaming; transport/HTTP errors → secret-free `ProviderError::Transport` (never the `api_key`), bad body → `InvalidResponse`; `api_key` is a plain `String` so the secret-isolation closure test still holds. Hermetic `wiremock` coverage + opt-in `#[ignore]` live smoke (`CAIRN_IT_AI`+`ANTHROPIC_API_KEY`). OpenAI-compat provider + streaming = follow-ups |
| M7-3 | Closed tool registry (handles only; `schemars`; `ToolNotFound`) → broker | ai-integration-engineer, security-engineer | M7-1, M3-5 | rustdoc; threat-model | no tool returns/accepts a secret (compile + review) | ✅ #15 (closed set via capability_for; unknown tool rejected) |
| M7-4 | Plan state machine, engine-driven execution, per-step confirm, partial-failure surfacing | ai-integration-engineer | M7-3 | rustdoc | engine runs steps; irreversible step pauses for confirm (test) | ✅ #15 |
| M7-5 | Context `WorldSnapshot` (sanitized, budgeted, no secrets), injection defenses | ai-integration-engineer, security-engineer | M7-3 | rustdoc; injection-defense doc | snapshot carries no secret (test); heuristics flag off-scope | ✅ #16 (WorldSnapshot + untrusted-data wrapping + out-of-scope heuristic + system policy) |
| M7-6 | AI side panel + plan→confirm overlay; `[Approve all]` only when all steps Safe/Recoverable | tui-engineer, security-engineer | M7-4, M2-7 | user docs | **Demo:** "archive logs >30d to S3" plan→step-through; bulk-approve absent if destructive | 🟡 plan→confirm **and execute** (#23, #30): `Overlay::AiPlan` + reducer (step-through / reversibility-gated bulk-approve / reject / abort), ratatui overlay, Ctrl-A via offline `MockProvider`; `BinaryStepExecutor` runs approved **safe/local** steps (list/stat/read/copy/move/delete) against the backends per RFC-0007. **`exec` now routes through `Vfs::invoke`** (#34) — allow-list enforced, errors redacted, a live Stream/Session outcome rejected loudly rather than dropped; only the live backend transport remains. **Freeform prompt entry done** (#37): `Ctrl-A` opens a text prompt and the typed request drives the flow (overlay-openers suppressed while a plan is pending). **Plan-execution cancellation done** (#47): `Esc` aborts a running plan (token polled between steps); competing ops refused while executing. **Step-output summaries done** (#49, RFC-0007 Gap 1): list/stat/read/delete steps surface a short secret-free summary (count/size/kind) to the user (never to the model), shown in the plan-complete status. Live providers (M7-2) + logs/port-forward execution = integration |
| M7-7 | `cairn-mcp`: MCP client foundation (consume external MCP tools) + later expose actions as MCP server through same broker+confirm | ai-integration-engineer | M7-3 | rustdoc | external MCP client hits same confirm gate (test) | 🟡 **MCP _client_ foundation done** (`RFC-mcp-client`): standalone `cairn-mcp` crate over **`rmcp` 2.0** — `McpClient::connect_stdio`/`connect_http` (child-process + streamable-HTTP, rustls/no-OpenSSL) → `initialize`/`list_tools`/`call_tool`, mapped to secret-free `McpToolInfo`/`McpToolResult`/`McpError`. **Transport/protocol layer only**: no dep on `cairn-ai`/`broker`/`vault`/`secrets`, **not** wired into the agent's closed tool set. Hermetic in-process round-trip test (dev-only rmcp `server` feature over an in-memory duplex). **Remaining:** (1) the agent-integration + safety-model RFC (`RFC-mcp-client`) — how untrusted MCP tools meet the capability-gated, plan→confirm tool set; (2) the inverse **MCP _server_** that exposes Cairn actions through the same broker+confirm gate. |

### M8 — Extensibility (WASM plugins) · **security-review required**

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M8-1 | RFC: **plugin host & WIT ABI** (worlds, capability grants, streaming-by-polling, versioning) | plugin-systems-engineer, technical-writer | M3-5 | RFC merged; confirm ADR-0004 | approved before M8-2 | ✅ (RFC-0006, #22) |
| M8-2 | `cairn-plugin`: wasmtime host, WIT bindings, default-deny Linker, per-instance Store, ResourceLimiter, runaway-guest bound | plugin-systems-engineer | M8-1 | rustdoc | spinning plugin can't hang UI (fuel test); ungranted import fails at instantiate | ✅ #18 (wasmtime host: **fuel limit** traps a runaway guest, memory cap, default-deny imports; 6 tests). NOTE: M8-2's bound is **fuel** only; the wall-clock **epoch** deadline (owed because fuel doesn't advance during host calls) landed in M8-4 |
| M8-3 | `PluginVfsBackend` bridge: guest `backend` export → `Vfs` (chunked-poll) | plugin-systems-engineer, rust-staff-engineer | M8-2 | rustdoc | plugin backend passes MockVfs contract suite | ✅ **full `Vfs` contract done (M8-3a/b)**: `cairn:plugin@1.0.0` WIT package (RFC-0006), wasmtime component-model + generated host bindings, `PluginComponent` wrapper calling the non-streaming exports (`scheme`/`backend-caps`→`Caps`/`caps-at`/`stat`/`list-page`) with mem-cap+fuel and an empty deny-all WASI ctx; a committed guest fixture component proves it hermetically (no WASM toolchain in CI). **M8-3b read path done**: `PluginVfsBackend` implements `Vfs` for `scheme`/`connection`/`caps`/`list`(paginated stream)/`stat` via a **dedicated-thread bridge** (the `Store` is `!Send`; the `Send+Sync` backend messages it over a channel + `oneshot` replies, fuel refilled per call); granted `host` imports linked (`log`/`now-secs` real, brokered fns deny-stubbed → M8-4). Browsable as `Arc<dyn Vfs>`, tested against the committed fixture. **M8-3b PR2 done**: `open_read` bridges a guest `read-stream` resource to a `ReadHandle` (`AsyncRead`) — chunk-pulled on demand, resource owned on the plugin thread (it is `!Send`) and addressed by opaque id, freed on drop; hostile-guest hardening (per-stream byte cap `Limits::max_stream_bytes`, oversized-chunk rejection, control-char/length-capped guest error strings). **M8-3b PR3 done**: `open_write` bridges a guest `write-sink`→`WriteHandle` (chunked write → `finish`→`Entry`; drop-without-finish aborts, not commits) and `create_dir`/`remove`(recursive flag)/`rename` are wired through with per-error `VfsError` mapping — **the full `Vfs` contract**, write sinks owned on the plugin thread and freed on drop. **Remaining before live untrusted use (M8-4)**: epoch deadline before any blocking host import + real brokered host functions |
| M8-4 | Brokered creds/HTTP for plugins (UUID stand-in, host substitutes secret), journaled `Actor::Plugin` | plugin-systems-engineer, security-engineer | M8-2, M3-5 | rustdoc; threat-model; **RFC-0010** | plugin never sees secret value (test); brokered HTTP only | ✅ **RFC-0010 PR-B done**: real `host::http-fetch` and `host::use-credential` replace the deny-stubs. **http-fetch** (behind `plugin-network` feature): reqwest+rustls, per-plugin hostname allow-list, SSRF guards (IP-literal + DNS pre-flight, blocked: loopback/RFC-1918/CGNAT/link-local/ULA/IPv4-mapped), 8 MiB response cap, `Set-Cookie` stripped, sensitive headers redacted from logs, redirect re-validation per hop. **use-credential**: `CredentialBroker` trait in `cairn-broker-api` (secret-free boundary); `BrokerCredentialAdapter` in `cairn-broker` resolves vault secret, performs closed-vocabulary action (`bearer-token`/`basic-auth-header`), zeroizes secret, journals event — raw `CredentialSecret` never crosses WIT ABI. Both gated by `PluginGrants`; no grant → deny-stub without touching vault or network. dep-closure invariant verified: `cairn-plugin` depends on `cairn-broker-api` only. New public API: `PluginGrants`, `PluginHostConfig`, `PluginComponent::instantiate_with_grants`. 78 tests green (`plugin-network` feature hermetically tested via wiremock; default `cargo test` offline). **Previously done (this item)**: epoch deadline, WASI narrowing (PR-A). **Remaining before live untrusted use**: PR-C loader (M8-5) |
| M8-5 | Manifest (`plugin.toml` + wasm section), install-time capability approval UI, revocation | plugin-systems-engineer, tui-engineer | M8-2, M8-4 | user docs; plugin author guide; **RFC-0010** | **Demo:** install a sample plugin backend, approve caps, browse | 🟡 **PR-C1 done**: `PluginManifest` (`plugin.toml` via serde+toml, all structs `deny_unknown_fields`; §5.2 schema: `[plugin]`/`[capabilities]`/`[network]`/`[limits]`, semantic validation: name charset/length, SemVer, api major vs `HOST_API_MAJOR`, description length); `PluginLoader` (directory discovery, ABI check, grants intersection fail-closed, full §5.6 load pipeline); `PluginGrantsRecord`+`PluginEntry` in `cairn-config` under `[plugins."<name>@<version>".grants]` with `plugin_grants()`/`set_plugin_grants()`/`revoke_plugin_grants()` accessors; **SEC-1 fix** (issue #103, release-gating): `PinnedSsrfDnsResolver` closes DNS-rebinding TOCTOU — single resolve+validate; reqwest never re-resolves; **SEC-8**: 6to4/Teredo/NAT64 tunnel IPv4 extraction. 25 new tests (12 manifest + 7 loader + 6 config). All gates green. **Remaining**: PR-C2 (TUI approval overlay) + PR-C3 (`cairn plugin install` CLI) |
| M8-6 | `cairn-plugin-sdk` (optional) + sample guest `.wasm` fixtures for CI | plugin-systems-engineer | M8-3 | SDK docs | fixtures checked in; plugin tests need no WASM toolchain in CI | ⏭ deferred (Component Model/WIT + Vfs bridge — next layer; runtime core done in M8-2) |
| M8-7 | Declarative config extensions (keybinds, themes, shell-command actions, aliases) | software-engineer, tui-engineer | M3-6 | user docs | config-only action runs without a plugin | 🟡 config-driven **keybindings** (#24) + **themes** (#32): `[ui.keybindings]` chord→action overrides + `[ui.colors]` role→color overrides over the preset (`Theme` resolver, threaded through render), `Config::load` wired into the binary. **Shell-command actions done** (ADR-0005, security-reviewed): `[[shell_actions]]` (name/key/command/args with `{path}`/`{dir}`/`{name}`) binds a key to run a local program on the entry under the cursor. Built on **`Vfs::local_path`** (canonicalize+confine; `Caps::LOCAL_PATH`) for local-only enforcement. Hardened: argv-only **no shell**, confirm-before-run (opt-out per action), file-trust gate (ignores actions from a world/group-writable or non-owned config on Unix), env scrub (no secrets to the child), explicit cwd, stdin closed, captured+capped output, wall-clock timeout + process-group kill. Non-interactive only; output summarized to status (never echoed/sent to AI). Interactive (TUI-suspend) + aliases deferred |

### v0.1 — Release

| ID | Item | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| R-1 | Release engineering: cross-platform binaries (musl/universal-mac/Windows), Homebrew/cargo, signing | devops-engineer | M5 (M4) | release docs | tagged build attaches binaries on 3 OSes | 🟡 release workflow ready (`.github/workflows/release.yml`): on a `v*` tag (or manual dispatch) it builds `cairn --release --features all-backends` for Linux x86_64 gnu + musl, macOS aarch64 + x86_64, and Windows x86_64, packaging each binary + SHA-256 into a release asset (aws-lc-rs deps wired: CMake/musl-tools on Linux, NASM on Windows, pre-generated bindings on macOS). Tagging is the maintainer's call. Homebrew/cargo publish + signing still ⏭ |
| R-2 | Docs completeness pass: README, user guide, backend docs, glossary, `--help` | technical-writer | all | README/docs | docs match shipped features (no stale) | ⏭ deferred (follows backend milestones) |
| R-3 | Release QA: regression matrix, graceful degradation (no truecolor/Nerd-Font/narrow), session restore | qa-engineer | all | TESTING.md | crash-free smoke on 3 OSes + limited terminals | ⏭ deferred (follows backend milestones) |
| R-4 | CHANGELOG roll-up → v0.1, version bump, ADR/RFC index check | technical-writer, project-manager | R-1..R-3 | CHANGELOG | tagged `v0.1.0` from `main`; dashboard shows v0.1 ✅ | 🟡 version bumped `0.0.0`→`0.1.0` (workspace package + all internal-crate pins; workspace resolves) and `CHANGELOG.md` rolled `[Unreleased]`→`[0.1.0] - 2026-06-30` with a fresh empty `[Unreleased]`. Tag `v0.1.0` is the maintainer's outward-facing call (not created here) |

---

## 4. Critical path (the spine)

Strict order; each step unblocks the next. This is the earliest route to a demoable end-to-end slice.

1. **`cairn-types`** — `VfsPath`/`Entry`/`Caps`/ids/errors. Biggest single unblock (nothing compiles
   without it). Gate: `VfsPath::parse` rejects `..`/control chars.
2. **`cairn-vfs` ‖ `cairn-secrets`** (first parallel moment; no mutual dep). `cairn-vfs` is the
   highest-churn-risk interface — validate it on paper against every backend model (LLD §3.6) before
   any real backend. Gate: `MockVfs` passes a contract suite.
3. **`cairn-transfer`** (type stubs + local→local `copy_one`) — `cairn-core` imports its types.
4. **`cairn-core`** (pure TEA reducer; no handles, no I/O). Gate: navigate/list/cancel unit tests.
5. **`cairn-tui`** skeleton wired to `MockVfs` → **Slice 0 "it boots"** (renders, navigates, resizes).
6. **`cairn-backend-local`** (full) — Gate: contract suite on 3 OSes; 100k list smooth.
7. **`cairn` binary** (effect runner + DI) → **Slice 1 "local read-only browser"**, the earliest
   demoable end-to-end validation of the entire spine.

After step 7, multiple lanes run in parallel (§6).

## 5. Dependency DAG

```
Tier 0:  cairn-types
Tier 1:  cairn-secrets   cairn-vfs   cairn-config        (vfs blocks ALL backends/transfer/broker/plugin/core)
Tier 2:  backend-{local,ssh,object,docker,k8s} (siblings, independent)   cairn-transfer(→vfs)   cairn-vault(→secrets; NO vfs)
Tier 3:  cairn-core(→vfs,transfer)        cairn-broker(→vault,vfs,secrets)
Tier 4:  cairn-tui(→core)   cairn-ai(→broker only)   cairn-plugin(→vfs,broker)
Tier 5:  cairn-mcp(→broker,ai)   [feature-gated]
Tier 6:  cairn (binary)

Blocking edges:  vfs → {all backends, transfer, broker, plugin, core};  transfer → core;
                 vault → broker;  core → tui;  broker → {ai, plugin, mcp};  tui → first runnable binary.
Independence:    backends are siblings (cross-backend logic lives ONLY in cairn-transfer);
                 cairn-ai cannot name vault/backends (compile-time); vault sub-spine is independent
                 of the backend sub-spine until they converge in cairn-broker.
```

## 6. Parallelization lanes (after the spine)

| Lane | Owner | Blocked until | Convergence note |
|---|---|---|---|
| **A — Object stores (S3→GCS→Azure)** | storage-engineer | RFC-transfer-resume; broker for cloud creds | co-develop multipart with transfer engine; add MinIO CI job |
| **B — SSH/SFTP** | network-engineer | RFC-ssh; broker | first cross-backend transfer (local↔ssh) |
| **C — Docker** | container-backend-engineer | RFC-docker; broker (registry auth) | exec `Stream` in TUI; container→S3 |
| **D — Kubernetes** | kube-staff-engineer | RFC-k8s; broker; K8s cred types in vault | deepest nesting; `caps_at`; port-forward `Session`; "pod→bucket" |
| **E — Vault + broker** | security-engineer | secrets types (vault has NO vfs dep → start in parallel with local backend) | broker is the convergence point of the security + backend sub-spines |
| **F — AI** | ai-integration-engineer | **broker API frozen** (use `MockBroker`) | only depends on broker; can run parallel to C/D |
| **G — Transfer depth** | storage-engineer (+rust-staff) | RFC-transfer-resume; ≥2 backends for real cross-backend tests | multipart path co-developed with Lane A |
| **H — Plugins** | plugin-systems-engineer | broker + vfs stable | plugin backend wrapped as `Arc<dyn Vfs>` |

**Start Lane E (vault) in parallel with the local backend** — vault is pure crypto+disk with no vfs
dependency, so finishing it early means the broker is ready when credentialed backends need it. A
`--dev` plaintext-credential path (never in release builds) lets lanes A–D run emulator tests before
the broker lands.

## 7. RFC sequencing (RFC-before-large-impl, CLAUDE.md §5)

| RFC | Must land before | Deferrable? |
|---|---|---|
| RFC-local | M1-3 local backend impl | No — on the critical path |
| RFC-ssh | M4-2 SSH impl | No — draft early so it doesn't stall Lane B |
| RFC-transfer-resume | S3 multipart-persist (M5-5) / transfer depth | No — the format must be right the first time |
| RFC-docker | M6-2 | Short defer (Docker starts later) |
| RFC-k8s | M6-5 | Short defer (last complex backend) |
| RFC-reveal-secret-ux | reveal-in-TUI flow | Yes — stub a locked placeholder initially |
| RFC-team-vault | team-vault feature | Yes — P2, post-v1 (KEK layer extends to per-recipient `age`) |
| RFC-plugin-registry | registry/signing (not the host) | Yes — P2; host ships with local-file installs |
| RFC-0010 (plugin sandbox + brokered host fns) | M8-4 (PR-A WASI narrowing, PR-B1/B2 brokered fns), M8-5 (PR-C1/C2/C3 loader + approval) | No — gates live untrusted plugin loading |
| RFC-mcp-client | consuming external MCP tools | Yes — explicitly post-v1. _Note:_ the transport/protocol **client foundation** (`cairn-mcp`, rmcp 2.0) has landed ahead of the RFC; the RFC still owns the **safety model** — how untrusted MCP tools meet the closed, capability-gated, plan→confirm tool set. |

## 8. Vertical-slice demo checkpoints

These map onto the milestones and are the moments to show real progress (record an asciinema/
screenshot in the milestone issue): **0** it boots (MockVfs) · **1** local read-only browser
(earliest demoable) · **2** local full CRUD + queue · **3** viewer + config/session · **4** vault
unlocks · **5** +SSH (first credentialed backend; cross-backend transfer) · **6** +S3 (multipart,
pagination, resume) · **7** GCS+Azure (contract-generalized) · **8** +AI (plan→confirm) · **9**
+Docker (exec/log stream) · **10** +K8s (pod→bucket flagship) · **11** +plugins.

---

## 9. Progress-tracking design (how this stays the source of truth)

**The cardinal rule:** status is updated in the **same PR that does the work**. A PR implementing
`M5-4` flips its row 🟡→✅ in the same diff, appends the PR `#id`, and adjusts the §1 dashboard
counts. This is the only thing that keeps the tracker honest.

- **GitHub mapping:** milestones M0–M8+v0.1 → **GitHub Milestones**; each work item → a **GitHub
  Issue** (`M<n>-<k>: <desc>`, assigned to its milestone, labeled with the lead's `area:*` + a `type:*`)
  created as it enters "Ready" (§10); dependencies become issue task-list checkboxes; PRs
  `Closes #<id>`.
- **Doc ↔ GitHub:** this doc is the human-readable plan + at-a-glance status; Issues/Milestones are
  machine-tracked state; they must agree.
- **Automation (M0-3 follow-up):** a CI/Action step recomputes the §1 dashboard counts from issue
  state on merge and can fail a PR whose touched item row is left stale — making "update the tracker"
  a merge gate, not a good intention. Until that lands, the merging maintainer updates the row by hand.
- **Generated assets** (dependency diagram, burn-up) committed under `docs/assets/`.

CLAUDE.md §5 codifies the same-PR update as a documentation requirement.

## 10. Definition of Ready / Definition of Done

**Ready** (may start): a GitHub Issue under the right milestone with a lead assigned; unambiguous,
testable acceptance criteria; all dependency items ✅ (or a documented mock unblocks it); prerequisite
RFC/ADR merged; scope fits one PR; specialist(s) identified per CLAUDE.md §2.

**Done** (✅): merged via squash PR with ≥1 approval and green cross-platform CI; tests (unit;
regression for fixes; backend integration + contract where applicable); gates addressed (`bug-bot` +
`code-review`; `security-review` where applicable); docs done (rustdoc, ADR/RFC, user docs, CHANGELOG);
`fmt/clippy -D/test/doc/deny` green; no secrets introduced (redaction verified for new `#[source]`);
**tracker row updated in the same PR**.

---

## 11. Risk register

| # | Risk | L/I | Mitigation | Owner | Tripwire |
|---|---|---|---|---|---|
| R1 | Scope: 7 backends at launch (PRD §11) | H/H | vertical-depth-first; v0.1 deep on local/SSH/S3, rest "preview"; ⏭ aggressively | project-manager | M5 slips → cut GCS/Azure polish to fast-follow |
| R2 | async_trait / heavy-SDK compile times | M/M | crate-per-backend + feature flags; default build local+SSH; watch `--timings` | rust-staff, devops | cold build over target → split/gate |
| R3 | Secret-handling defect (leak/log) | L/Crit | compile-time broker boundary; mandatory `security-review`; redaction tests; `secrecy`/`zeroize` | security-engineer | any redaction-test failure blocks release |
| R4 | Cross-platform quirks (Windows, headless keychain) | H/M | 3-OS CI from M0; passphrase fallback; grapheme widths; limited-terminal tests | devops, tui | any OS red on `main` blocks merges |
| R5 | AI safety on irreversible cloud ops | M/H | plan→confirm enforced by broker/UI not model; closed registry; per-step confirm; capability containment | ai-integration, security | any path to bulk-approve a destructive step = blocker |
| R6 | WASM streaming immaturity | M/L | chunked-poll baseline; additive `stream<T>` later; epoch timeouts | plugin-systems | perf unacceptable → keep polling |
| R7 | `!Send` SDK futures (russh) | M/M | `assert_send` compile test per backend; channel-proxy isolation | rust-staff, network | `assert_send` fails → proxy that backend |
| R8 | AI cost/privacy deters users | M/M | local Ollama first-class & documented; AI fully optional | ai-integration | — |
| R9 | "Cairn" trademark conflict | L/M | clear before branding lock-in | product-branding, project-manager | conflict → rename before v0.1 marketing |
| R10 | Tracker drift | M/H | §9 same-PR rule + CI enforcement + Action-regenerated dashboard | project-manager | drift CI check red |

### Convergence/integration risks (highest-rework points)
- **Vfs trait churn** affecting all backends → validate on paper + `MockVfs` contract suite before
  replicating; **freeze the public trait after S3 + local both implement it unchanged** (Slice 6),
  guarded by a breaking-change CI check.
- **Broker API churn** affecting AI + plugins + cloud creds → freeze the broker API (review by
  security + rust-staff) before Lanes F/H start consuming it (target: by Slice 5).
- **TEA `Msg`/`Effect`/`Event` churn** → keep discriminants coarse (`AppEffect::Ai(AiCommand)`),
  all `#[non_exhaustive]`, so new subsystems add one outer variant, never explode match arms.

---

## 12. Cross-cutting workstreams (continuous; folded into feature PRs)

| Workstream | Owner | Standing obligations |
|---|---|---|
| CI / Release | devops-engineer | keep 3-OS matrix + emulator integration jobs green; cache tuning; deny/audit/Dependabot triage; release pipeline ready before v0.1 |
| Documentation | technical-writer | every feature PR ships its docs; ADRs immutable (supersede); RFCs before large impl; README/glossary/CHANGELOG current; PRD stays high-level |
| Testing / QA | qa-engineer | hermetic-offline default `cargo test`; maintain Mock{Vfs,Provider,Broker,ObjectStore}; object-store contract suite; emulator jobs; release regression |
| Security | security-engineer | `security-review` gate; redaction upkeep; living threat model; verify broker boundary holds as AI/plugins land |
| Architecture | software-architect | guard dependency-graph seams + ADR consistency; review every public API shape |
| Performance | performance-tuning-engineer | watch list/transfer/compile benchmarks as features land |
| Project management | project-manager | keep §1 dashboard + GitHub state in sync; sequence RFCs vs rollout; weekly burn-up note; escalate ⛔ |

### CI evolution (added as capabilities land)
M0 base (fmt/clippy/test/doc/deny × 3 OSes) → **+vault** (CI-safe Argon2 params) → **+ssh** (sshd
service) → **+object** (MinIO/fake-gcs/Azurite + contract job + throughput check) → **+docker** (dind)
→ **+k8s** (`kind`) → **+ai** (MockProvider always; live Claude/Ollama secrets-gated optional) →
**release** (musl/universal-mac/Windows binaries, Homebrew tap, crates.io). Default `cargo test`
stays hermetic and offline throughout; integration jobs are feature-gated.

---

## Appendix — long-poles (watch these)

- **M1-1 `Vfs` trait** gates every backend — review before replication; freeze after Slice 6.
- **M3-5 broker** gates SSH, all object stores, Docker, K8s, AI, plugins — highest fan-out; don't slip.
- **RFC items** (M1-2, M2-1, M4-1, M6-1, M6-4, M8-1) each gate their implementation.
- **M5-2 contract suite** makes GCS/Azure breadth cheap.
