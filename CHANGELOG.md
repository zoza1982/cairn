# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/zoza1982/cairn/commits/main
