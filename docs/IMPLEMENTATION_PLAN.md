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
| **Phase** | Design complete → starting build |
| **Design docs** | ✅ PRD · ✅ LLD · ✅ ADR-0001..0004 |
| **Current milestone** | **M1 — The abstraction, proven** (🟡 in progress) |
| **v0.1 target** | Deep on local + SSH + S3; functional GCS/Azure; Docker/K8s/AI/plugins behind feature flags |
| **Milestones complete** | 0 / 9 |
| **Work items ✅ / 🟡 / ☐ / ⛔ / ⏭** | 11 / 5 / 55 / 0 / 0 |
| **Cross-platform CI green** | ✅ Linux · ✅ macOS · ✅ Windows (scaffold) |
| **Long-pole items** | `M1-1` Vfs trait · `M3-5` broker · all backend RFCs |

**Burn-up note (edit each merge):** _M0 underway: lint config tuned for velocity, `cairn-types`
(paths/entries/caps/ids, fully tested) and the binary edge (tracing, panic hook, version/help)
landed (#5). Crates are created lazily per milestone rather than 18 empty stubs up front. M1 next:
`Vfs` trait + local backend + core/TUI skeleton._

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
| M1-9 | Large-list virtualization + off-thread sort/filter, filter-as-you-type | tui-engineer, performance-tuning-engineer | M1-8 | perf note | 100k dir smooth; first page <100 ms | 🟡 ratatui scrolls selection into view; true windowing + off-thread sort/filter deferred |
| M1-10 | Multi-select, sort (name/size/date/type), show/hide hidden | software-engineer, tui-engineer | M1-8 | user docs | selection + sort unit tests | 🟡 marks + dirs-first/name sort done; size/date sort + hidden toggle deferred |

### M2 — Operations & transfer engine

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M2-1 | RFC: **transfer engine** (queue, conflict, resume format, pause/cancel) | storage-engineer, software-architect, technical-writer | M1-1 | RFC merged | approved before M2-2 | ☐ |
| M2-2 | `cairn-transfer` core: `TransferRequest/Job`, `copy_one` bounded-buffer stream-through, server-copy fast path | storage-engineer, rust-staff-engineer | M2-1 | rustdoc | local→local copy; backpressure test | ☐ |
| M2-3 | Move = copy→verify→delete; `PartialMove`; conflict via stat+conditional pre-check | storage-engineer | M2-2 | rustdoc | move/conflict tests incl. failed-delete | ☐ |
| M2-4 | Pause/resume/cancel, retry w/ backoff+jitter, global+per-backend semaphores | storage-engineer, network-engineer | M2-2 | rustdoc | cancel mid-chunk; paused job parks without blocking queue | ☐ |
| M2-5 | Transfer queue UI overlay: per-item progress/speed/ETA, reorder, controls | tui-engineer | M2-2, M1-6 | user docs | **Demo:** copy a big tree local→local with live queue | ☐ |
| M2-6 | Operation overlays: mkdir/rename/delete(confirm)/copy/move wired to engine | tui-engineer, software-engineer | M2-2 | user docs | F5–F8 flows; destructive ops confirm | ☐ |
| M2-7 | Confirm-dialog + overlay-stack input interception (foundation for plan→confirm) | tui-engineer, security-engineer | M2-6 | rustdoc | destructive effect cannot dispatch without confirm (test) | ☐ |

### M3 — Secrets foundation (vault + broker) · **security-review required on every item**

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M3-1 | `cairn-secrets`: `SecretString/Box`, `Zeroizing`, redaction layer (AWS/bearer/SAS/PEM/JWT) | security-engineer, rust-staff-engineer | M0-4 | rustdoc; threat-model note | no `Debug`/`Serialize` leak (compile test); redaction tests | ☐ |
| M3-2 | Vault at rest (ADR-0002): XChaCha20-Poly1305, header-AAD, encrypted index, per-entry DEKs, postcard, atomic write+`.bak`+lock | security-engineer | M3-1 | confirm ADR-0002; format spec | seal/open round-trip; tamper/rollback tests; refuse unknown version | ☐ |
| M3-3 | Key hierarchy + unlock: KEK in OS keychain (`keyring`), Argon2id fallback, auto-lock | security-engineer | M3-2 | rustdoc; SECURITY.md update | keychain + passphrase paths tested; auto-lock zeroizes | ☐ |
| M3-4 | Credential model: typed `CredentialSecret` + delegation variants; `TokenCache` | security-engineer | M3-2 | rustdoc | variants seal; delegation stores no secret (test) | ☐ |
| M3-5 | `cairn-broker`: `authorize`/`execute` split, capability+scope, resolve `CredentialId`→secret inside execute, journal `Actor` | security-engineer, software-architect | M3-3, M3-4 | confirm ADR-0002; rustdoc | secret never leaves `execute`; no API returns a secret (compile + review) | ☐ |
| M3-6 | `cairn-config`: TOML, `ConnectionProfile` (ref only, type-enforced no-secret), state dir, schema+migration | software-engineer | M0-5 | rustdoc; config docs | config↔vault boundary compile-enforced; migration round-trip | ☐ |
| M3-7 | Vault TUI: create/unlock, list (labels only), add credential; on-screen redaction | tui-engineer, security-engineer | M3-3, M2-7 | user docs | **Demo:** create vault, store cred, relock — nothing plaintext | ☐ |

### M4 — First remote (SSH/SFTP)

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M4-1 | RFC: **SSH/SFTP backend** (auth chain, bastion/jump, keepalive, `!Send` proxy strategy) | network-engineer, technical-writer | M1-1, M3-5 | RFC merged | approved before M4-2; `assert_send` plan documented | ☐ |
| M4-2 | `cairn-backend-ssh`: connect (key/agent via broker), list/stat/read/write (streaming, RANDOM_READ) | network-engineer, rust-staff-engineer | M4-1 | rustdoc; backend README | against SSH image (CI): browse + read/write; auth via broker only | ☐ |
| M4-3 | SSH mutations + `exec` (remote grep→SEARCH_CONTENT), edit-in-place save-back | network-engineer | M4-2 | rustdoc | rename/delete/mkdir; exec returns Stream; edit→save round-trip | ☐ |
| M4-4 | Transport resilience: timeouts, keepalive, retry/backoff, bastion/proxy-jump chain | network-engineer | M4-2 | rustdoc; resilience note | simulated stall fails+retries (no hang); jump-host test | ☐ |
| M4-5 | Connection switcher UI + new-SSH flow, profile persistence (ref-only) | tui-engineer, software-engineer | M4-2, M3-7 | user docs | **Demo:** Ctrl-K → connect → browse → transfer local↔SSH | ☐ |
| M4-6 | Cross-backend transfer validation local↔SSH | qa-engineer, storage-engineer | M4-2, M2-2 | test docs | copy/move both directions; integrity verified | ☐ |

### M5 — Object storage (S3 → GCS/Azure)

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M5-1 | `ObjectStore` trait + `ObjectStoreVfs` wrapper (ADR-0003) | storage-engineer, software-architect | M1-1 | confirm ADR-0003; rustdoc | trait+wrapper compile; `MockObjectStore` harness | ☐ |
| M5-2 | **Object-store contract test suite** (all three providers) | qa-engineer, storage-engineer | M5-1 | TESTING.md | runs against MockObjectStore + MinIO; gates each provider | ☐ |
| M5-3 | S3 provider (`aws-sdk-s3`): list (continuation, common-prefixes), head, ranged GET | storage-engineer | M5-1 | rustdoc | against MinIO: list via bounded window; contract pass | ☐ |
| M5-4 | S3 multipart upload (16 MiB threshold, 8 MiB parts, concurrent, CRC32C), abort-on-drop | storage-engineer | M5-3 | rustdoc | >16 MiB multipart; abort leaves no orphans (test) | ☐ |
| M5-5 | S3 resume (part-state, `list_parts` reconcile, `SourceChanged`), server-copy | storage-engineer | M5-4, M2-1 | rustdoc; resume spec | kill+resume completes; same-provider fast-path | ☐ |
| M5-6 | S3 integrity/consistency: conditional writes (412→Conflict), `VerifyPolicy`, broker creds (`ArcSwap`) | storage-engineer, security-engineer | M5-3, M3-5 | rustdoc | 412→conflict-policy test; presigned URLs redacted | ☐ |
| M5-7 | Cross-backend local↔S3 and SSH↔S3 (checksum verify) | qa-engineer, storage-engineer | M5-4, M4-2 | test docs | **Demo:** copy local→S3, SSH→S3 with verification | ☐ |
| M5-8 | GCS provider (`google-cloud-storage`, crc32c, generation preconds, ADC/SA via broker) | storage-engineer | M5-2, M5-6 | rustdoc | contract green vs fake-gcs-server | ☐ |
| M5-9 | Azure provider (`azure_storage_blobs`, per-block MD5, shared-key/SAS/AAD via broker) | storage-engineer | M5-2, M5-6 | rustdoc | contract green vs Azurite | ☐ |
| M5-10 | Backend-aware UX: tier badges, versioned soft-delete honesty, archive-tier cost confirm | tui-engineer, ux-engineer | M5-3 | user docs | Glacier read raises cost confirm; delete-marker messaging clear | ☐ |

### M6 — Containers & clusters (action model)

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M6-1 | RFC: **Docker backend** (fs via archive API, image layers, exec/logs) | container-backend-engineer, technical-writer | M1-1 | RFC merged | approved before M6-2 | ☐ |
| M6-2 | `cairn-backend-docker` (`bollard`): list containers+images; browse container fs (tar); image layers RO | container-backend-engineer | M6-1, M3-5 | rustdoc; backend README | against dind: browse fs; copy in/out | ☐ |
| M6-3 | Docker actions: `exec`, `logs` (Stream), start/stop | container-backend-engineer | M6-2 | rustdoc | exec interactive stream; logs follow | ☐ |
| M6-4 | RFC: **Kubernetes backend** (ctx→ns→pod→container→fs, exec/cp/logs/port-forward, auth) | kube-staff-engineer, technical-writer | M1-1 | RFC merged | approved before M6-5 | ☐ |
| M6-5 | `cairn-backend-k8s` (`kube`): navigable tree, watch strategy, kubeconfig/exec-plugin auth via broker | kube-staff-engineer | M6-4, M3-5 | rustdoc; backend README | against `kind`: browse ns/pods; multi-context | ☐ |
| M6-6 | K8s cp (tar over exec), `logs(follow)` Stream, `exec` (tty), `port-forward` (Session) | kube-staff-engineer | M6-5 | rustdoc | cp out of pod completes (no stall); port-forward holds | ☐ |
| M6-7 | Stream/Session UI: live log viewer (follow+filter), exec pane, port-forward status | tui-engineer | M6-3, M6-6 | user docs | **Demo:** stream pod logs; exec; copy pod→S3 | ☐ |

### M7 — Agentic AI (plan→confirm→execute) · **security-review required**

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M7-1 | `LlmProvider` trait + `StreamChunk` normalization; Claude provider | ai-integration-engineer | M3-5 | rustdoc; ADR ref | `MockProvider` + Claude path; no live API in CI | ☐ |
| M7-2 | Ollama + OpenAI-compat providers w/ tool degradation (Native→JsonSchema→Text) | ai-integration-engineer | M7-1 | rustdoc; local-model doc | degradation tiers tested vs MockProvider | ☐ |
| M7-3 | Closed tool registry (handles only; `schemars`; `ToolNotFound`) → broker | ai-integration-engineer, security-engineer | M7-1, M3-5 | rustdoc; threat-model | no tool returns/accepts a secret (compile + review) | ☐ |
| M7-4 | Plan state machine, engine-driven execution, per-step confirm, partial-failure surfacing | ai-integration-engineer | M7-3 | rustdoc | engine runs steps; irreversible step pauses for confirm (test) | ☐ |
| M7-5 | Context `WorldSnapshot` (sanitized, budgeted, no secrets), injection defenses | ai-integration-engineer, security-engineer | M7-3 | rustdoc; injection-defense doc | snapshot carries no secret (test); heuristics flag off-scope | ☐ |
| M7-6 | AI side panel + plan→confirm overlay; `[Approve all]` only when all steps Safe/Recoverable | tui-engineer, security-engineer | M7-4, M2-7 | user docs | **Demo:** "archive logs >30d to S3" plan→step-through; bulk-approve absent if destructive | ☐ |
| M7-7 | `cairn-mcp` (feature-gated): expose actions as MCP server through same broker+confirm | ai-integration-engineer | M7-3 | rustdoc | external MCP client hits same confirm gate (test) | ☐ |

### M8 — Extensibility (WASM plugins) · **security-review required**

| ID | Item (crate) | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| M8-1 | RFC: **plugin host & WIT ABI** (worlds, capability grants, streaming-by-polling, versioning) | plugin-systems-engineer, technical-writer | M3-5 | RFC merged; confirm ADR-0004 | approved before M8-2 | ☐ |
| M8-2 | `cairn-plugin`: wasmtime host, WIT bindings, default-deny Linker, per-instance Store, ResourceLimiter, epoch timeout | plugin-systems-engineer | M8-1 | rustdoc | spinning plugin can't hang UI (epoch test); ungranted import fails at instantiate | ☐ |
| M8-3 | `PluginVfsBackend` bridge: guest `backend` export → `Vfs` (chunked-poll) | plugin-systems-engineer, rust-staff-engineer | M8-2 | rustdoc | plugin backend passes MockVfs contract suite | ☐ |
| M8-4 | Brokered creds/HTTP for plugins (UUID stand-in, host substitutes secret), journaled `Actor::Plugin` | plugin-systems-engineer, security-engineer | M8-2, M3-5 | rustdoc; threat-model | plugin never sees secret value (test); brokered HTTP only | ☐ |
| M8-5 | Manifest (`plugin.toml` + wasm section), install-time capability approval UI, revocation | plugin-systems-engineer, tui-engineer | M8-2 | user docs; plugin author guide | **Demo:** install a sample plugin backend, approve caps, browse | ☐ |
| M8-6 | `cairn-plugin-sdk` (optional) + sample guest `.wasm` fixtures for CI | plugin-systems-engineer | M8-3 | SDK docs | fixtures checked in; plugin tests need no WASM toolchain in CI | ☐ |
| M8-7 | Declarative config extensions (keybinds, themes, shell-command actions, aliases) | software-engineer, tui-engineer | M3-6 | user docs | config-only action runs without a plugin | ☐ |

### v0.1 — Release

| ID | Item | Lead | Deps | Docs | Exit criteria | Status |
|---|---|---|---|---|---|---|
| R-1 | Release engineering: cross-platform binaries (musl/universal-mac/Windows), Homebrew/cargo, signing | devops-engineer | M5 (M4) | release docs | tagged build attaches binaries on 3 OSes | ☐ |
| R-2 | Docs completeness pass: README, user guide, backend docs, glossary, `--help` | technical-writer | all | README/docs | docs match shipped features (no stale) | ☐ |
| R-3 | Release QA: regression matrix, graceful degradation (no truecolor/Nerd-Font/narrow), session restore | qa-engineer | all | TESTING.md | crash-free smoke on 3 OSes + limited terminals | ☐ |
| R-4 | CHANGELOG roll-up → v0.1, version bump, ADR/RFC index check | technical-writer, project-manager | R-1..R-3 | CHANGELOG | tagged `v0.1.0` from `main`; dashboard shows v0.1 ✅ | ☐ |

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
| RFC-mcp-client | consuming external MCP tools | Yes — explicitly post-v1 |

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
