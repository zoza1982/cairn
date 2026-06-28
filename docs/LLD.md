# Cairn — Low-Level Design (LLD)

> **Status:** Draft v0.1
> **Owner:** Zoran Vukmirica
> **Last updated:** 2026-06-27
> **Scope:** The *how* — architecture, module boundaries, and load-bearing interfaces. Product
> scope and the *what/why* live in [`PRD.md`](PRD.md); milestone sequencing lives in the
> Implementation Plan (planned). Per-backend deep designs and any contested choices are split into
> RFCs under [`rfcs/`](rfcs/); headline decisions are recorded as ADRs under [`adr/`](adr/).

This document was produced as a team effort (CLAUDE.md §2): the foundation was designed by the
`software-architect` and `security-engineer`, then detailed by `rust-staff-engineer`,
`tui-engineer`, `storage-engineer`, `ai-integration-engineer`, and `plugin-systems-engineer`, and
synthesized here.

---

## 0. Reading guide

| If you care about… | Read |
|---|---|
| The big picture | §1 Architecture overview |
| Where code lives | §2 Workspace & crate layout |
| The central abstraction | §3 Core VFS abstraction |
| Threading / "why the UI never blocks" | §4 Async & concurrency, §5 App core |
| The screen | §6 TUI layer |
| Moving bytes | §7 Transfer engine, §8 Object-store backends |
| Credentials & the AI safety boundary | §9 Secrets/vault & security |
| The assistant | §10 AI agentic layer |
| Extensibility | §11 Plugin system |
| Errors, config, testing | §12–§14 |
| The TL;DR of every decision | §15 Decisions summary |

---

## 1. Architecture overview

Cairn is a single binary composed of library crates wired together at the edge. The organizing
principle: **the render thread is synchronous and owns the UI state; everything slow is an *effect*
executed on a tokio runtime, with results flowing back as events.** This is what structurally
guarantees CLAUDE.md §9's "the UI must never block."

### 1.1 Component diagram

```
                         ┌──────────────────────────────────────────────┐
                         │                cairn (binary)                 │
                         │   bootstrap · DI wiring · anyhow edge · CLI    │
                         └───────────────┬──────────────────────────────┘
        ┌─────────────────────────────────┼──────────────────────────────────┐
        ▼                                  ▼                                   ▼
┌───────────────┐   Msg/Cmd      ┌──────────────────┐   AppEffect    ┌──────────────────┐
│  TUI / RENDER  │◀──────────────▶│   APP CORE       │───────────────▶│  EFFECT RUNNER    │
│  (cairn-tui)   │  immutable     │  state + update  │   (intents)    │ (tokio executor) │
│  ratatui draw  │  &AppState     │  (cairn-core)    │◀───────────────│  spawns tasks    │
│  input decode  │  snapshot      │  TEA reducer     │  AppEvent      │  cancel tokens   │
└───────────────┘  (sync, never  │  pure, no I/O    │  (results)     └────────┬─────────┘
        ▲           blocks)       └──────────────────┘                          │ async calls
   terminal                                                                     ▼
   (crossterm)                                              ┌────────────────────────────────┐
                                                            │  SERVICES (async, via handles)  │
                                                            ├────────────────────────────────┤
                                                            │ VfsRegistry → dyn Vfs backends  │ cairn-vfs + backends
                                                            │ Transfer engine                 │ cairn-transfer
                                                            │ Broker  (mediation)             │ cairn-broker
                                                            │   └─ Vault (sole decryptor)     │ cairn-vault + cairn-secrets
                                                            │ AI agent                        │ cairn-ai
                                                            │ Plugin host                     │ cairn-plugin
                                                            │ Config/State                    │ cairn-config
                                                            └────────────────────────────────┘
```

### 1.2 The control loop (the shape of everything)

```rust
loop {
    let msg = select_next(&mut input_rx, &mut event_rx).await; // input OR async result OR tick
    let effects = update(&mut state, msg);   // 1. pure reducer: mutate state, emit intents
    terminal.draw(|f| render(&state, f))?;   // 2. sync render of an immutable &AppState
    effect_runner.dispatch(effects);         // 3. spawn tokio tasks; they send AppEvents back
}
```

Three channels close the loop: **input → `Msg`** (decoded on a blocking thread), **`Msg` →
`update` → `Vec<AppEffect>`** (pure, never `.await`s), **`AppEffect` → tokio task → `AppEvent`** (fed
back in as `Msg::Event`). Rendering reads an immutable borrow of state; it can drop frames but never
blocks.

### 1.3 Trust boundaries

- **`cairn-vault` is the only component that decrypts secrets.** It hands *materialized*
  credentials to backends at connect time and *opaque references* to everyone else.
- **`cairn-broker` is the sole mediator** that resolves a credential reference to a live secret, and
  the only caller of vault execution. It enforces capability/scope policy and the plan→confirm gate.
- **`cairn-ai` and `cairn-plugin` are untrusted.** They depend only on the broker, speak in opaque
  handles, and can do nothing a permissioned user action can't. This symmetry — AI and plugins
  behind one boundary — is the core security property (§9.6).

---

## 2. Workspace & crate layout

Granularity principle: **split on a stable interface seam and to quarantine heavy/optional
dependency trees** (aws-sdk, kube, bollard, wasmtime) behind crate boundaries + Cargo features, so a
default build and `cargo test` stay fast and hermetic.

```
crates/
  cairn                # binary: bootstrap, CLI, DI wiring, anyhow edge, panic/tracing setup
  cairn-types          # leaf: VfsPath, Entry, Caps, ids, error kinds. Zero heavy deps.
  cairn-core           # AppState, TEA reducer, Msg/Event/Effect. NO I/O, no .await.
  cairn-vfs            # Vfs trait set, capability model, VfsRegistry, URI parsing, MockVfs
  cairn-backend-local  # std/tokio fs
  cairn-backend-ssh    # russh / openssh-sftp
  cairn-backend-object # S3 / GCS / Azure (shared ObjectStore core + provider modules)
  cairn-backend-docker # bollard
  cairn-backend-k8s    # kube-rs
  cairn-transfer       # transfer engine: jobs, queue, streaming copy, conflict, resume
  cairn-secrets        # SecretString/Box wrappers, redaction layer, zeroizing types
  cairn-vault          # crypto, keychain bridge, vault file format, credential records
  cairn-broker         # capability mediation; resolves refs→secrets; only caller of vault exec
  cairn-config         # TOML config, dirs, session/bookmarks persistence, schema + migration
  cairn-ai             # provider-agnostic agent, tools, plan→confirm state machine
  cairn-mcp            # optional: expose Cairn actions as an MCP server (feature-gated)
  cairn-plugin         # wasmtime host, WIT bindings, capability sandbox
  cairn-plugin-sdk     # optional: Rust guest SDK for plugin authors
  cairn-tui            # ratatui widgets, layout, theming, keymap, input, render loop
```

### 2.1 Dependency direction (strict, acyclic)

```
cairn-types  ◀── (everyone)
cairn-secrets ──▶ cairn-types
cairn-vfs    ──▶ cairn-types
backends-*   ──▶ cairn-vfs, cairn-types          (siblings NEVER depend on each other)
cairn-transfer ──▶ cairn-vfs, cairn-types
cairn-vault  ──▶ cairn-secrets, cairn-types       (no dep on vfs/backends)
cairn-broker ──▶ cairn-vault, cairn-vfs, cairn-secrets, cairn-types
cairn-config ──▶ cairn-types
cairn-ai     ──▶ cairn-broker, cairn-types        (NOT vfs/vault/backends — structural)
cairn-mcp    ──▶ cairn-broker, cairn-ai, cairn-types
cairn-plugin ──▶ cairn-vfs, cairn-broker, cairn-types
cairn-core   ──▶ cairn-vfs, cairn-transfer, cairn-types   (orchestrates via traits; no concrete backend)
cairn-tui    ──▶ cairn-core, cairn-types          (renders state; no I/O)
cairn (bin)  ──▶ everything                         (the only place they all meet)
```

Why these seams:
- **Backends are leaf siblings.** Cross-backend transfer composes them only in `cairn-transfer`
  through the `Vfs` trait — that is what makes "pod → S3" tractable and keeps each backend ignorant
  of the others.
- **`cairn-ai` cannot name the vault or backends.** The secret-isolation guarantee is a compile-time
  property of the dependency graph, not a convention.
- **`cairn-core` holds `Arc<dyn Vfs>` from the registry**, never a concrete backend, so feature flags
  add/remove backends without touching the core.

### 2.2 Feature flags

Heavy SDKs are quarantined. Default build = local + SSH only.

```toml
# cairn (binary) features
default = ["local", "ssh"]
s3 = ["cairn-backend-object/s3"]; gcs = ["…/gcs"]; azure = ["…/azure"]
docker = ["dep:cairn-backend-docker"]; k8s = ["dep:cairn-backend-k8s"]
ai = ["dep:cairn-ai"]; plugins = ["dep:cairn-plugin"]; mcp = ["cairn-ai", "dep:cairn-mcp"]
full = ["s3","gcs","azure","docker","k8s","ssh","ai","plugins"]
```

Each backend registers itself at startup behind its `#[cfg(feature)]`. `cargo test` (no features)
compiles only the lean core; per-backend integration tests run under their own feature in dedicated
CI jobs (§14).

---

## 3. Core VFS abstraction

The single most important interface. Every backend implements it; the capability model expresses
what each can and cannot do so the UI offers only valid operations and failures are legible.

### 3.1 Path / URI model

A pane is `(ConnectionId, VfsPath)`. Connections (scheme + profile + endpoint) are separate from
in-backend locations.

```rust
// cairn-types
pub struct ConnectionId(u64);            // opaque; resolved by the registry

pub struct VfsPath {                     // normalized, backend-agnostic, always '/'-separated
    segments: Vec<SmolStr>,              // [] == backend root
    trailing_slash: bool,                // significant for object stores (object vs prefix)
}
impl VfsPath {
    pub fn parse(s: &str) -> Result<Self, PathError>;   // rejects "..", control chars
    pub fn parent(&self) -> Option<VfsPath>;
    pub fn join(&self, name: &str) -> Result<VfsPath, PathError>;
    pub fn file_name(&self) -> Option<&str>;
    pub fn as_str(&self) -> String;                      // canonical "/a/b"
}

pub struct VfsUri { pub scheme: Scheme, pub authority: String, pub path: VfsPath }
// e.g. "s3://prod-backups/2026/", "k8s://prod/payments/api-7f9c/"
```

Paths are normalized but **not** resolved — `..` is rejected at parse time, closing path-traversal
across the trust boundary (important for plugins and the AI). `trailing_slash` is preserved because
on object stores `foo` (object) and `foo/` (prefix) are different entities.

### 3.2 Entry metadata

One struct for every backend; backend-specific facts live in a typed extension enum, never a
stringly-typed map.

```rust
// cairn-types
pub struct Entry {
    pub name: SmolStr,
    pub kind: EntryKind,
    pub size: Option<u64>,             // None = unknown / streaming / N/A
    pub modified: Option<SystemTime>,
    pub perms: Option<UnixPerms>,      // None where backend has no perm model
    pub symlink_target: Option<VfsPath>,
    pub etag: Option<SmolStr>,         // object stores: integrity/conflict
    pub ext: EntryExt,
}
pub enum EntryKind { File, Dir, Symlink, Stream, Special }   // Dir covers prefixes & pods-as-dirs
#[non_exhaustive]
pub enum EntryExt {
    None,
    Object   { storage_class: Option<SmolStr>, version_id: Option<SmolStr> },
    Container{ id: SmolStr, state: ContainerState, image: SmolStr },
    Image    { id: SmolStr, layers: u32, tags: Vec<SmolStr> },
    Pod      { phase: PodPhase, ready: (u16, u16), node: Option<SmolStr> },
    K8sResource { kind: SmolStr, namespace: Option<SmolStr> },
}
```

### 3.3 Capability model

```rust
bitflags! {
    pub struct Caps: u64 {
        const LIST=1<<0; const READ=1<<1; const WRITE=1<<2; const CREATE_DIR=1<<3;
        const DELETE=1<<4; const RENAME=1<<5; const RENAME_ATOMIC=1<<6; const COPY_SERVER=1<<7;
        const CHMOD=1<<8; const CHOWN=1<<9; const SYMLINK=1<<10; const APPEND=1<<11;
        const RANDOM_READ=1<<12; const MULTIPART=1<<13; const VERSIONS=1<<14;
        const WATCH=1<<15; const SEARCH_CONTENT=1<<16;
    }
}
pub trait CapabilityProvider {
    fn caps(&self) -> Caps;                                   // backend-wide baseline
    fn caps_at(&self, _path: &VfsPath) -> Caps { self.caps() }// k8s/docker refine by depth
}
```

The UI queries capabilities to decide what to *offer*; backends return `Unsupported(Caps)` if asked
anyway (a first-class, non-scary error — §12).

### 3.4 The trait — async dispatch decision

**Decision (ADR-0001): `#[async_trait]` + `Arc<dyn Vfs>`.** We must hold heterogeneous backends
behind one object-safe handle (plugins register backends at runtime), and native `async fn` in
traits is not object-safe on stable without experimental shims. The per-call `Box<dyn Future>`
allocation is noise next to network/disk latency. **Exception:** `list` is *not* `async` — it returns
a `BoxStream` synchronously; declaring it `async fn -> BoxStream<'_>` creates an unsatisfiable
lifetime (the `'async_trait` future would have to also be the stream's borrow). Listing is a stream
(not a `Vec`) for backpressure and first-paint-before-complete.

```rust
// cairn-vfs
#[async_trait::async_trait]
pub trait Vfs: CapabilityProvider + Send + Sync + 'static {
    fn scheme(&self) -> Scheme;
    fn connection(&self) -> ConnectionId;

    fn list<'a>(&'a self, dir: &'a VfsPath, opts: ListOpts)
        -> BoxStream<'a, Result<ListPage, VfsError>>;          // NOT async — returns a stream

    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError>;
    async fn open_read(&self, path: &VfsPath, range: Option<ByteRange>) -> Result<ReadHandle, VfsError>;
    async fn open_write(&self, path: &VfsPath, opts: WriteOpts) -> Result<WriteHandle, VfsError>;

    // Mutations default to Unsupported so backends opt in.
    async fn create_dir(&self, _p: &VfsPath) -> Result<(), VfsError> { Err(VfsError::Unsupported(Caps::CREATE_DIR)) }
    async fn remove(&self, _p: &VfsPath, _r: Recurse) -> Result<(), VfsError> { Err(VfsError::Unsupported(Caps::DELETE)) }
    async fn rename(&self, _f: &VfsPath, _t: &VfsPath) -> Result<(), VfsError> { Err(VfsError::Unsupported(Caps::RENAME)) }
    async fn set_perms(&self, _p: &VfsPath, _m: UnixPerms) -> Result<(), VfsError> { Err(VfsError::Unsupported(Caps::CHMOD)) }
    async fn copy_within(&self, _f: &VfsPath, _t: &VfsPath) -> Result<(), VfsError> { Err(VfsError::Unsupported(Caps::COPY_SERVER)) }

    fn actions_at(&self, _p: &VfsPath) -> Vec<ActionDescriptor> { vec![] }
    async fn invoke(&self, _a: ActionId, _ctx: ActionCtx) -> Result<ActionOutcome, VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }
    fn watch(&self, _dir: &VfsPath) -> Option<BoxStream<'_, WatchEvent>> { None }
}

pub struct ListPage { pub entries: Vec<Entry>, pub cursor: Option<PageCursor>, pub done: bool }
```

**Handles.** `ReadHandle` implements `tokio::io::AsyncRead` (plugs into the whole tokio I/O
ecosystem). `WriteHandle` is a **chunk API**, not `AsyncWrite`, because `finish()` must return
metadata (the multipart ETag/version) that `AsyncWrite::shutdown` cannot:

```rust
pub struct ReadHandle { /* impl AsyncRead; len hint, content-type, etag */ }
pub struct WriteHandle { /* hides single-shot vs multipart state machine */ }
impl WriteHandle {
    pub async fn write_chunk(&mut self, b: Bytes) -> Result<(), VfsError>;  // awaits on backpressure
    pub async fn finish(self) -> Result<Entry, VfsError>;                   // commit; returns final etag
    pub async fn abort(self);                                               // cancel multipart (avoid orphans)
}
```

`WriteHandle`'s `Drop` fires a detached `abort` as a safety net so a dropped/panicked upload doesn't
leave (billable) orphaned multipart parts.

### 3.5 Special actions through a uniform interface

exec / logs / port-forward stay **off** the core trait (keeping it small and object-safe). Pattern:
**discover → describe → invoke**, with typed-but-extensible context/outcome so the UI renders a
generic action menu plus a few rich handlers for streams/sessions.

```rust
pub struct ActionDescriptor { pub id: ActionId, pub label: SmolStr, pub kind: ActionKind, pub destructive: bool }
pub enum ActionKind { OneShot, Stream, Session, Interactive }
pub enum ActionCtx {
    None,
    Exec { argv: Vec<String>, tty: bool, stdin: Option<BoxStream<'static, Bytes>> },
    Logs { follow: bool, since: Option<SystemTime>, container: Option<SmolStr> },
    PortForward { local: u16, remote: u16 },
}
pub enum ActionOutcome {
    Done, Text(String),
    Stream(BoxStream<'static, Result<Bytes, VfsError>>),   // logs(follow), exec(tty)
    Session(SessionHandle),                                // port-forward, exec session
}
```

`destructive` feeds straight into the plan→confirm and per-action confirm gates. New backends (and
plugins) add actions without changing the trait.

### 3.6 How the hard backends map

| Backend | List | Read/Write | Notable actions |
|---|---|---|---|
| **Object (S3/GCS/Azure)** | prefix+delimiter; `cursor`=continuation token; common-prefixes→`Dir` | multipart `WriteHandle`; `copy_within`=server CopyObject | — (RENAME_ATOMIC off → engine does copy+delete; CREATE_DIR off) |
| **SSH/SFTP** | readdir, single page | streaming; RANDOM_READ on | `exec` (remote grep → SEARCH_CONTENT) |
| **Docker** | root=containers+images as `Dir`; inside container=its fs (tar) | fs via archive API; image layers read-only | `exec`, `logs`, `start/stop` |
| **Kubernetes** | contexts→namespaces→pods→containers→fs (nested `Dir`) | fs via `cp` (tar over exec) | `exec`, `logs(follow)` (Stream), `port-forward` (Session) |

Local, SSH, Docker, and Kubernetes backends each get a follow-up RFC for deep design; this LLD fixes
the abstraction they implement and validates it against object stores (§8) which exercise the
hardest streaming/pagination/multipart paths.

---

## 4. Async runtime & concurrency model

**Decision: tokio multi-thread runtime; a fully synchronous render path.**

```
main thread: select! over { input_rx, event_rx, tick } → update() → render() → dispatch(effects)
   │ spawn                                                              ▲ AppEvent (bounded mpsc)
   ▼                                                                    │
tokio pool: VFS calls, transfers, AI, vault ops. Each task carries a CancellationToken,
            sends coalesced progress + guaranteed results back via event_tx.
```

- **Input on a blocking std thread** forwarded over a channel — never poll input on the runtime, and
  never let a slow render starve input.
- **Render takes `&AppState` and does zero I/O / zero `.await`.** Structural enforcement of §9.
- **Cancellation:** every effect gets a `tokio_util::sync::CancellationToken` stored by `TaskId`;
  `Msg::Cancel(id)` cancels cooperatively; chunked loops use `tokio::select! { biased; cancel … }`
  so cancellation is checked every iteration even under a fast source.
- **Backpressure:** `event_tx` is **bounded**. Progress is coalesced (~10 Hz, latest-wins, dropped if
  full); results are never dropped (guaranteed lane / `send().await`). A standardized
  `Progress { task, done, total: Option<u64>, rate, phase }` covers unknown-length streams.
- **No `block_on`** except at the top of `main`; backends are pure async. Local-fs uses `tokio::fs`
  (never blocking `std::fs` inside async). `clippy::disallowed_methods` bans `std::thread::sleep` and
  stray `tokio::spawn` outside the effect runner.

A known risk (rust-staff): some SDKs (`russh` historically) expose `!Send` futures. A compile-time
`assert_send` test guards each backend; an offending SDK is isolated behind a channel proxy.

---

## 5. Application core — state & the TEA loop

**Decision: Elm/TEA `Model → Msg → update → effects`.** A pure reducer is trivially testable and
cleanly separates "what changed" (`Msg`) from "go do I/O" (`AppEffect`) from "results arrived"
(`AppEvent`). Components are used for *render* composition only (§6), not for holding state.

```rust
// cairn-core — holds NO service handles, NO Arc<dyn Vfs>; only plain data.
pub struct AppState {
    pub panes: [Pane; 2],
    pub focus: Focus,                       // Left | Right | Overlay(OverlayId)
    pub overlays: Vec<Overlay>,             // stack; top gets input first
    pub transfers: TransferView,
    pub connections: ConnRegistryView,
    pub tasks: IndexMap<TaskId, TaskState>, // status only (cancel tokens live in the runner)
    pub keymap: Keymap,
    pub theme: Theme,
    pub terminal_size: (u16, u16),
}
pub struct Pane { pub tabs: Vec<Tab>, pub active_tab: usize }
pub struct Tab {
    pub conn: ConnectionId,
    pub cwd: VfsPath,
    pub entries: Arc<Vec<Entry>>,           // large list behind Arc → cheap render borrow
    pub raw_entries: Arc<Vec<Entry>>,       // pre-sort/filter source
    pub filter_indices: Option<Arc<Vec<usize>>>,
    pub cursor: usize, pub scroll_offset: usize, pub viewport_height: u16,
    pub sort: SortSpec, pub filter: Option<String>,
    pub listing: ListingState,              // Loading | Ready | Error
}

pub enum Msg { Input(InputEvent), Event(AppEvent), Cancel(TaskId), Tick }

// Results FROM async world (carry originating TaskId).
pub enum AppEvent {
    Listed { task: TaskId, pane: PaneRef, page: ListPage },
    Stat   { task: TaskId, entry: Result<Entry, VfsError> },
    Progress(Progress), Transfer(TransferEvent),
    AiPlan(AiEvent), Stream { task: TaskId, chunk: Result<Bytes, VfsError> },
    Connected { conn: ConnectionId, result: Result<(), VfsError> },
    Failed { task: TaskId, error: CairnError },
}
// Intents OUT to async world. update() returns these; it does not run them.
pub enum AppEffect {
    List { pane: PaneRef, conn: ConnectionId, dir: VfsPath },
    Read { task: TaskId, conn: ConnectionId, path: VfsPath, sink: ReadSink },
    Transfer(TransferRequest), InvokeAction { conn: ConnectionId, action: ActionId, ctx: ActionCtx },
    Connect { selector: ConnSelector }, Ai(AiCommand), Persist(PersistRequest),
}

pub fn update(state: &mut AppState, msg: Msg) -> Vec<AppEffect>;  // pure: no I/O, no .await
```

**State ownership (rust-staff).** `AppState` holds no live handles. Service handles live in the
**effect runner** (in the binary crate): `VfsRegistry`, `Arc<dyn Broker>`, `TransferEngine`,
`event_tx`, and the `TaskId → (JoinHandle, CancellationToken)` table. `AppEffect` carries cheap value
types (`ConnectionId`, `VfsPath`); the runner resolves them to `Arc<dyn Vfs>` via the registry
(`RwLock<HashMap<…>>`, connect-on-miss). The sequential loop means `render` borrows `&AppState`
uncontended; `Arc<Vec<Entry>>` makes that borrow O(1). No `Arc<Mutex<AppState>>`, no snapshots, no
persistent-data-structure crate needed in v1.

Overlays are a stack; input routes to the top first. This is what lets the plan→confirm overlay
*intercept* destructive effects before they're dispatched.

---

## 6. TUI layer (`cairn-tui`)

**Stack: `ratatui` over `crossterm`.** ratatui diffs two cell buffers per `draw`, writing only
changed cells — so event-driven rendering is cheap and there is no fixed frame clock; input and
coalesced async events drive redraws.

### 6.1 Render model

`render(&AppState, &mut Frame)` is a tree of **pure render functions** (no widget-held state): root →
breadcrumbs, panes, preview/AI panel, command line, status line, function-key bar, then the overlay
stack last (each preceded by a `Clear`). All ephemeral view state (scroll offset, text cursor,
split ratio) lives in `AppState`, satisfying the TEA constraint that render is a pure function of
state. `compute_layout(state, area)` runs each frame (cache static rects, recompute on resize);
below 80 cols the right pane stacks, below 40 the function bar abbreviates.

Performance rules (tui-engineer): pre-compute per-row display strings when entries arrive (no
`format!` in the row loop), let ratatui batch one write per frame (no manual `flush`/`execute!` in
render), and cache layout rects.

### 6.2 Input & keymap

`crossterm::Event` is decoded on the input thread into `InputEvent { Key, Paste, Resize, Mouse,
Focus }`. A configurable `Keymap` resolves `(ModalContext, KeySequence) → Action`:

```rust
pub enum KeyPreset { Mc, Vim, Custom }
pub enum ModalContext { PaneList, CommandLine, SearchBar, CommandPalette, ConfirmDialog, AiInput, LogViewer, FileViewer, TransferQueue }
pub type KeySequence = SmallVec<[KeyEvent; 3]>;   // heap-free for 1–2 key chords
```

Resolution order: active overlay context → pending chord (prefix trie, ~1 s timeout shown in the
status bar) → pane context → global. **MC preset** is compiled-in default (F-keys, Tab, Insert);
**Vim** (`hjkl`, `gg`/`G`, `dd`); **Custom** merges `keymap.toml` over the chosen preset. A load-time
trie detects conflicts (a binding that is a strict prefix of another in the same context). Mouse
events hit-test into the same `Action`s (click→`SetCursor`, wheel→`Scroll`, divider drag→resize), so
the mouse is strictly optional.

### 6.3 Large-list virtualization

Only visible rows render; `cursor`/`scroll_offset` are indices. Listing pages stream in from
`BoxStream<ListPage>`; a runner task accumulates and emits `Arc<Vec<Entry>>` batches at ~10 Hz so the
first page paints in milliseconds while the rest loads. Sort and filter run off-thread
(`spawn_blocking`; filter debounced ~30 ms, fuzzy via `nucleo`) and return new `Arc`s — the UI shows
a "sorting…/filtering…" hint and never blocks. 100k entries ≈ ~1 MiB/pane; the stream is never fully
buffered for truly huge backends (object stores cap a sliding window).

### 6.4 Overlays & the plan→confirm UI

```rust
pub enum Overlay { CommandPalette(_), ConnectionSwitcher(_), ConfirmDialog(_), AiPanel(_), TransferQueue(_), GoToPath(_), FindFiles(_), KeyPresetChooser(_) }
```

The command palette (Ctrl-K) serves actions *and* connection switching with fuzzy search. The
plan→confirm overlay renders each step with a capability/reversibility badge:

```
1. select 14 files older than 2026-05-28   [safe]
2. copy → s3://prod-backups/logs/          [safe]
3. verify checksums                        [safe]
4. delete local originals                  [!!]   ← red, irreversible
```

**Security-enforced in the TUI, independently of the AI engine:** the `[Approve all]` control is
*rendered and reachable only when every step is Safe/Recoverable*. If any step is `Irreversible`
(or Delete/Exec), the bulk control is absent entirely (no keybinding path to it); the user must
navigate to each such step and confirm it individually (ASCII degradation: `[ok] [! ] [!!]`).

### 6.5 Live streams & viewer

`ActionOutcome::Stream` (follow-logs, exec) binds to a `StreamBuffer` (`VecDeque<Arc<str>>` with a
byte cap and semaphore backpressure) filled by a runner task; follow-mode just pins `scroll_offset`
to the tail. The F3 viewer reads any backend via `open_read`'s `AsyncRead` in 64 KiB chunks; syntax
highlighting uses **`two-face`** (bundled syntect) for the visible window + lookahead, with
checkpointed highlighter state every ~500 lines for backward scroll; highlighting auto-disables
above ~10 MiB. In-viewer search is `regex` on a `spawn_blocking` task.

### 6.6 Theming & compatibility

Truecolor themes degrade to 256/16/no-color (detected via `COLORTERM`/`TERM`/`NO_COLOR`, with
tmux-aware downgrade). Nerd-Font icons fall back to Unicode then ASCII (`CAIRN_ICONS=auto|always|
never`). Widths use `unicode-width`; truncation uses grapheme clusters (`unicode-segmentation`);
double-width glyphs at a pane boundary are space-padded to avoid half-cell corruption. crossterm
handles Windows VT setup; a compatibility table covers kitty/wezterm/iTerm2/tmux/screen/Windows
Terminal/conhost/SSH. Optional terminal-graphics image preview via `viuer` behind a feature.

Crates: `ratatui`, `crossterm`, `unicode-width`, `unicode-segmentation`, `two-face` (syntect),
`nucleo`, `smallvec`; optional `viuer`, `tree-sitter` (feature).

---

## 7. Transfer engine (`cairn-transfer`)

**Position: above `Vfs`, composing two `Arc<dyn Vfs>` (src, dst).** The only place cross-backend
logic lives; "pod → S3" is identical in code to "local → local".

```rust
pub struct TransferRequest {
    pub src: (ConnectionId, Vec<VfsPath>),   // multi-select
    pub dst: (ConnectionId, VfsPath),
    pub op: TransferOp,                       // Copy | Move
    pub conflict: ConflictPolicy,            // Skip|Overwrite|Rename|NewerWins|Prompt
    pub verify: VerifyPolicy,                // None | Size | Checksum
}
pub struct Job { pub id: JobId, pub state: JobState, pub progress: JobProgress, /* cancel, pause */ }
```

Per-item core (never stages whole files on disk):

```rust
async fn copy_one(src: &dyn Vfs, dst: &dyn Vfs, from: &VfsPath, to: &VfsPath, ctx: JobCtx) -> Result<(), VfsError> {
    if src.connection() == dst.connection() && src.caps_at(from).contains(Caps::COPY_SERVER) {
        return src.copy_within(from, to).await;                 // server-side fast path
    }
    let mut r = src.open_read(from, None).await?;
    let mut w = dst.open_write(to, WriteOpts::from_hint(&r)).await?;
    // bounded mpsc(8) of ~1 MiB chunks between a reader and writer task = backpressure
    while let Some(chunk) = r.next_chunk(BUF).await? {
        ctx.pause.wait_if_paused().await;
        ctx.cancel.err_if_cancelled()?;                         // → w.abort()
        w.write_chunk(chunk).await?;
        ctx.report(chunk.len());                                // coalesced progress
    }
    let entry = w.finish().await?;                              // multipart complete / fsync
    ctx.verify(entry).await
}
```

Decisions:
- **Bounded buffer = backpressure**: a slow upload throttles a fast read; memory is capped per job.
- **Move = copy → verify → delete**; the delete runs only after `finish()`+verify. A failed delete
  surfaces `PartialMove { copied, deleted }` ("both copies exist; resolve manually") — never a silent
  partial state.
- **Pause/resume** via a `PauseToken` gate checked between chunks. **Resume across restarts** persists
  multipart part-state to the state dir (§13), re-validated against the server's part list on resume
  (§8.5); only where `Caps::MULTIPART`.
- **Conflict** resolved via `stat(to)` + conditional requests *before* streaming; `Prompt` parks one
  item without blocking the job/queue.
- **Concurrency**: global + per-backend semaphores; reorderable queue; retryable `VfsError`s retried
  with exponential backoff + jitter, cancel/pause honored during backoff.
- **Progress** is coalesced (100 ms window, EMA rate) before hitting the UI event channel.

---

## 8. Object-store backends (`cairn-backend-object`)

### 8.1 SDK decision (ADR-0003): official per-provider SDKs, **not** `opendal`

We hand-roll three provider modules on `aws-sdk-s3`, `google-cloud-storage`, `azure_storage_blobs`
behind a shared `ObjectStore` trait. Rationale: the transfer engine must drive **part-level**
multipart (independent retry, per-part checksums, upload-id persistence for resume), provider-specific
CAS primitives (S3 conditional writes, GCS generation preconditions, `crc32c`), and presign — all of
which `opendal` abstracts away; and we want a stable 1.x SDK floor rather than a 0.x meta-library at
this correctness-critical layer. Cost: ~3× provider code; S3-compat (MinIO/R2/B2) handled by
`endpoint_url` + `path_style` on the S3 provider.

```rust
#[async_trait]
pub trait ObjectStore: Send + Sync + 'static {
    fn capabilities(&self) -> Caps;
    fn provider_id(&self) -> ProviderId;                 // kind+region+endpoint → fast-path eligibility
    async fn list_page(&self, bucket:&str, prefix:&str, delim:Option<&str>, token:Option<&str>, max:u32) -> Result<ObjectListPage, VfsError>;
    async fn head(&self, bucket:&str, key:&str) -> Result<ObjectMeta, VfsError>;
    async fn get_range(&self, bucket:&str, key:&str, range:Option<Range<u64>>) -> Result<BoxStream<'static, Result<Bytes, VfsError>>, VfsError>;
    async fn put(&self, bucket:&str, key:&str, opts:PutOpts, body:Bytes) -> Result<PutReceipt, VfsError>;
    async fn multipart_create(&self, …) -> Result<UploadId, VfsError>;
    async fn multipart_put_part(&self, …, part:u16, body:Bytes, checksum:Option<PartChecksum>) -> Result<CompletedPart, VfsError>;
    async fn multipart_list_parts(&self, …) -> Result<Vec<CompletedPart>, VfsError>;   // resume
    async fn multipart_complete(&self, …, parts:Vec<CompletedPart>) -> Result<PutReceipt, VfsError>;
    async fn multipart_abort(&self, …) -> Result<(), VfsError>;
    async fn copy_object(&self, …, opts:CopyOpts) -> Result<PutReceipt, VfsError>;     // server-side
    async fn delete(&self, …) -> Result<(), VfsError>;
    async fn delete_batch(&self, …) -> Result<BatchDeleteResult, VfsError>;
    async fn presign(&self, …, op:PresignOp, ttl:Duration) -> Result<Url, VfsError>;
}
```

A single `ObjectStoreVfs` wraps `Arc<dyn ObjectStore> + BucketRoot` and implements `Vfs`.

### 8.2 Listing at scale

`list` is an `unfold` over the continuation token; `ListPage.cursor` carries the provider token
verbatim (S3 `ContinuationToken` / GCS `pageToken` / Azure `NextMarker`). Common-prefixes become
synthetic `Dir` entries (no size/mtime). Zero-byte trailing-slash marker objects are de-duplicated
against common-prefixes. The pane holds a bounded window (default ~5 000 entries); the stream is
pulled on demand near the scroll tail and evicted entries re-fetched on scroll-back — millions of
objects never enter memory.

### 8.3 Uploads / downloads

Threshold **16 MiB**: single PUT with full-body checksum below; multipart above. Default part size
**8 MiB** (auto-scaled for >40 GiB to stay under 10 000 parts). `MultipartWriteHandle` buffers to part
size, uploads parts concurrently under a per-object semaphore (backpressure on `write_chunk`),
retries idempotent parts, and on `finish` completes with the part list (ordered `BTreeMap`); `abort`
calls `AbortMultipartUpload`. Recommended bucket lifecycle rule `AbortIncompleteMultipartUpload: 3d`
as a cost safety-net. Large downloads use a `ParallelReadHandle` (sliding window of ranged GETs,
memory ≤ parallelism × part).

### 8.4 Integrity & consistency

S3: request **CRC32C additional checksums** per part + on complete (never trust multipart ETag for
content). GCS: `crc32c` (canonical). Azure: per-block `Content-MD5`. `VerifyPolicy` defaults:
same-provider server copy → `EtagMatch`; cross-provider stream → `Checksum`. Overwrite/create use
**conditional requests** (`If-None-Match:*` / `x-goog-if-generation-match:0` / `If-Match:<etag|gen>`)
to make conflict checks atomic (no TOCTOU); a 412 maps to `Conflict` and re-enters the conflict
policy. All three are strongly consistent for single-object ops; retried PUT/complete are idempotent.

### 8.5 Resume state, caps, auth

Per-item JSON in `…/state/cairn/transfers/<job>/item-<id>.json` (atomic write): src etag/size,
provider, upload-id, part size, completed parts (number+etag+crc), bytes. On resume: re-`stat` source
(etag/size mismatch → `SourceChanged`, restart), `multipart_list_parts` as ground truth, recompute
next part + byte offset. Caps per provider include `MULTIPART | RANDOM_READ | COPY_SERVER | PRESIGN`
(+`VERSIONS` if enabled; S3-compat probes adjust, e.g. R2 cross-bucket copy off). Credentials arrive
**resolved from the broker** (AWS key/STS/SSO/profile chain; GCP SA-JSON/ADC→OAuth; Azure
shared-key/SAS/AAD) held in `ArcSwap` for refresh without client rebuild; presigned URLs and signed
query params are always redacted from logs and never reach the AI.

Versioned soft-deletes (delete markers) are surfaced honestly ("data still exists; remove all
versions to purge"); archive-tier reads (Glacier/Archive) raise a cost/latency confirmation before
retrieval.

---

## 9. Secrets, vault & security (`cairn-vault`, `cairn-secrets`, `cairn-broker`)

Design tenets (PRD §7.4, CLAUDE §9/§11): no plaintext secrets on disk ever; redacted in logs/`Debug`
(type-enforced); the AI never sees raw secrets and operates through the same permissioned action
layer as the user; plan→confirm with individual confirmation of irreversible actions.

### 9.1 Vault at rest (ADR-0002)

**AEAD: XChaCha20-Poly1305** (`chacha20poly1305`) — 192-bit nonce makes random nonces safe with no
coordination, constant-time in software on every target incl. ARM/musl, no AES-NI dependency. (AES-256-GCM
rejected: 96-bit nonce reuse risk on a frequently-rewritten file + risky software fallback. `age`
reused only for the optional export/sync path.) **One vault file** with an authenticated header + an
**encrypted index** + **per-entry sealed records** (each with its own DEK) → least plaintext resident
in RAM, selective decryption, and metadata hiding (credential labels are encrypted too).

```
vault.cvlt: MAGIC | format_version | kdf_params? | wrapped-KEK | vault_id | generation
            ‖ AEAD(index: entry_id→offset/len/label/kind)
            ‖ [ nonce ‖ AEAD(CredentialRecord) ‖ tag ] *
```

Header (version, vault_id, generation) is bound as **AAD** → detects truncation, reordering,
splicing, and rollback (monotonic `generation` remembered in-session). Versioned format with a
forward migration chain; refuse-to-open on unknown higher version. **Atomic writes**: temp → fsync →
rename → fsync dir; keep one encrypted `.bak`; advisory file lock (`fd-lock`) blocks concurrent
instances. Serialization via `postcard` (deterministic, hostile-input-safe).

### 9.2 Key hierarchy & unlock

Three tiers: **unlock secret → KEK → (index key + per-entry DEKs) → plaintext**. Default
(keychain mode): a random 32-byte **KEK lives in the OS keychain** (`keyring`: macOS Keychain / Linux
Secret Service / Windows Credential Manager), so unlock is fast and no KDF runs. Fallback (no
keychain, e.g. headless Linux — detected and offered automatically): `KEK = Argon2id(passphrase,
salt)` with calibrated params (default m=256 MiB, t=3, p=1; floor 64 MiB) stored in the header. The
KEK layer means rotating the unlock factor re-wraps one key, not the whole vault. Auto-lock on idle
(default 15 min) zeroizes KEK/DEKs; OS-sleep and explicit `:lock` also lock; passphrase mode adds
failed-attempt backoff.

### 9.3 Credential model

An entry = non-secret metadata (in the encrypted index) + a typed secret payload (sealed record):

```rust
enum CredentialSecret {                 // never Debug/Display/Serialize-to-log
    Ssh(SshCredential), Aws(AwsCredential), Gcp(GcpCredential),
    Azure(AzureCredential), Kubernetes(K8sCredential), Docker(DockerCredential),
}
```

Per-backend variants cover keys/agents/passwords (SSH), access-key/STS/SSO/`ProfileRef` (AWS),
SA-JSON/ADC/impersonation (GCP), shared-key/SAS/AAD/`AadDefault` (Azure), bearer/client-cert/
exec-plugin/`KubeconfigRef` (k8s), registry-auth/identity-token (Docker). **Delegation variants
(`ProfileRef`, `Adc`, `AadDefault`, `ExecPlugin`, `AgentRef`) store no secret** — only how to obtain
one at use-time; preferred when available. Short-lived derived tokens (STS/SAS/SSO/exec output) live
in a separate `TokenCache` keyed by credential id with `expires_at`, auto-refreshed. Config/session
reference only an opaque `CredentialId` (`cred:<uuid>`), so config files, bookmarks, and screenshots
are safe.

### 9.4 Import & external managers

Opt-in importers (never auto-scrape) with a redacted TUI preview for `~/.aws/credentials`,
`~/.ssh/config`, kubeconfig, gcloud ADC, az — preferring delegation variants, sealing inline secrets,
and offering to neuter the plaintext source. External managers implement one trait so the built-in
vault is just the default provider:

```rust
#[async_trait] trait SecretProvider {           // LocalVault | HashiCorpVault | Aws/Gcp/AzureSecretManager
    fn id(&self) -> ProviderId;
    async fn resolve(&self, r: &SecretRef) -> Result<CredentialSecret, ProviderError>;
    async fn health(&self) -> ProviderHealth;
    fn supports_rotation(&self) -> bool;
}
```

The bootstrap secret for an external manager is itself stored in the local vault (no plaintext
bootstrap on disk).

### 9.5 Secrets in memory

`secrecy::{SecretString, SecretBox}` (no `Debug`/`Display`/`Serialize`, zeroize on drop) for every
sensitive field; `Zeroizing<Vec<u8>>` for decrypted buffers; `expose_secret()` is the only reader
(grep-able for audit). A `tracing` redaction layer scrubs known patterns (AWS keys, bearer tokens,
SAS `sig=`, `X-Amz-Signature`, PEM, JWT) before any sink; SDK errors pass through it. Best-effort
hardening: disable core dumps (`setrlimit`), `mlock` key pages, pass subprocess creds via stdin/memfd
not argv/env. SECURITY.md is honest that a root attacker on a live unlocked session can win.

### 9.6 The AI / plugin permission boundary (critical)

```
 ┌──────────┐  tool calls (handles only)  ┌───────────────┐ resolve(CredentialId) ┌────────────┐
 │ AI / WASM│ ───────────────────────────▶│ cairn-broker   │──────────────────────▶│ vault /     │
 │ plugin   │  ◀── redacted results ────── │ capability +   │   (SecretString stays │ provider    │
 └──────────┘   model/plugin ctx has       │ scope + confirm│    inside execute)    └────────────┘
                NO secrets, ever           └───────┬────────┘                              │
                                                   ▼ authorized + user-confirmed only      ▼
                                                                                    VFS backend op
```

The AI and plugins link only against `cairn-broker`, never the vault. They speak opaque handles;
there is deliberately **no tool/host-call that returns or accepts a secret**, so prompt injection or
a malicious plugin has no mechanism to read one. `broker.authorize(action)` validates handle
existence + scope (current panes/selection) and computes reversibility without mutating;
`broker.execute(authorized)` resolves credentials *inside* execution and runs the op; results are
redacted before return. **plan→confirm→execute is enforced by the broker/UI, not the model** — the
model only proposes; execution requires a human confirm it has no tool for, and Irreversible/Delete/
Exec steps confirm individually (§6.4). Every brokered action is journaled with its `Actor`
(User/Ai/Plugin).

### 9.7 Threat model (summary)

| Adversary | Key mitigations |
|---|---|
| Offline disk theft | XChaCha20 sealing; KEK in keychain or Argon2id-256MiB; no plaintext config secrets |
| Live-session memory/swap | zeroize, mlock, no core dumps, auto-lock; honest that root wins |
| Malicious WASM plugin | brokered handles only, no vault path, WASM sandbox (no ambient fs/net), capability-checked + journaled, irreversible→confirm |
| Prompt injection (hostile file/log content) | capability containment (primary) + plan→confirm + data/instruction separation + brokered egress (no arbitrary HTTP, no secret to exfiltrate) |
| Shoulder-surfing | on-screen redaction; no-echo passphrase; reveal requires explicit audited action |
| Vault tamper/rollback | AAD-bound header + monotonic generation + per-record tags + advisory lock |

---

## 10. AI agentic layer (`cairn-ai`)

Provider-agnostic; cloud default Claude, local Ollama drop-in, bring-your-own endpoint. Default
models: Opus 4.8 `claude-opus-4-8` (heavy reasoning), Sonnet 4.6 `claude-sonnet-4-6` (balanced),
Haiku 4.5 `claude-haiku-4-5-20251001` (cheap summarization).

### 10.1 Provider abstraction

```rust
#[async_trait] pub trait LlmProvider: Send + Sync + 'static {
    async fn chat_stream(&self, req: ChatRequest, cancel: CancellationToken)
        -> Result<BoxStream<'static, Result<StreamChunk, ProviderError>>>;
    fn capabilities(&self) -> &ProviderCapabilities;
    fn model_id(&self) -> &ModelId;
}
pub enum StreamChunk { TextDelta(String), ToolCallDelta{…}, ToolCallComplete{id,name,input}, Done{stop_reason,usage} }
pub enum ProviderConfig { Claude{api_key:SecretString, model}, Ollama{base_url, model}, OpenAiCompat{base_url, api_key:Option<SecretString>, model} }
```

The provider struct normalizes tool-definition shape (Claude `input_schema` vs OpenAI
`function.parameters`), tool-call/result representation (content blocks vs `tool_calls`/`tool` role),
system-prompt placement, prompt-caching (`cache_control` for Claude, stripped elsewhere), and SSE
framing — the engine only ever sees `StreamChunk`.

### 10.2 Tools & the closed registry

Tools operate on **handles only**: `list/stat/read/copy/move/delete/exec/open-connection`, plus a
meta-tool **`propose_plan`**. JSON schemas are generated via `schemars` and validated before any
call. A tool call → `ToolRegistry::build_action` → `broker.authorize`. **The set is structurally
closed**: there is no `ReadEnvVar`, `HttpFetch`, `ReadVaultEntry`, or `EscalatePermissions`; a
hallucinated call gets `ToolNotFound` before the broker is consulted. Capability containment, not
model compliance, is the boundary.

### 10.3 Plan state machine

```
Drafting ─propose_plan→ Proposed ─approve→ Executing ⇄ AwaitingConfirm(step) ─→ Done
   any ─cancel/reject→ Aborted        Executing ─step fails→ Failed (no rollback; prior steps ran)
```

The model emits one ordered `Plan` (steps carry `capability{verb, reversibility}` + a human
description). Execution is **engine-driven**, not model-driven: the engine runs steps deterministically,
pausing for per-step confirmation where required; step results feed back into the model for
multi-step commentary; partial failure aborts the remainder and reports completed steps (cross-backend
atomicity is unachievable, so no auto-rollback — surface state, let the user decide). Maps onto TEA
as `AppEffect::Ai(AiCommand{StartTask|ConfirmPlan|ConfirmStep|Reject|Cancel})` and
`AppEvent::AiPlan(AiEvent{StreamToken|PlanProposed|StepPendingConfirm|StepCompleted|PlanDone|…})`.

### 10.4 Context, injection defense, streaming

Context is a sanitized `WorldSnapshot` (panes, cwd, trimmed listings, selection, prior step results)
within a token budget — listings truncated, history trimmed oldest-first then summarized via a cheap
model, **never any secret**. Prompt caching pins the (stable) system policy and unchanged world
snapshot for Claude. Untrusted file/log content is wrapped (`<untrusted_data trust="none">…`) with a
standing system policy that such content is data, not instructions; `InjectionHeuristics` flags
out-of-scope paths / off-plan calls / mass-delete (defense-in-depth behind capability containment).
The `AiEngine` runs on its own task; text deltas use `try_send` (drop-on-full; text also accumulates
engine-side) so the render tick never blocks; cancellation is honored mid-generation (drop the
provider stream) and between steps. A `MAX_AGENTIC_ROUNDS` cap bounds tool loops.

### 10.5 MCP & local models

`cairn-mcp` (feature-gated, separate crate) can expose Cairn's actions as an MCP server — external
clients get the *same* broker + confirm gate. Consuming external MCP tools is deferred post-1.0.
The Ollama provider degrades gracefully across `NativeTools` → `JsonSchemaConstrained` (`format:json`
+ validate/retry) → `TextFallback` (parse numbered steps; cap reversibility at Recoverable, max 5
steps). Local path keeps everything on-device (documented prominently); recommended `qwen2.5-coder`,
`llama3.2`. Testing uses a `MockProvider` + `MockBroker`; no live API in CI (live/Ollama tests are a
secrets-gated optional job).

---

## 11. Plugin system (`cairn-plugin`)

### 11.1 Runtime (ADR-0004): `wasmtime` + Component Model / WIT

Typed, language-agnostic guest interfaces (`wit-bindgen` generates host bindings), first-class async,
strong security record, explicit WIT versioning. (Hand-rolled ABI: unmaintainable. `extism`:
bytes-in/out, wrong shape for multi-method backends. `wasmer`: weaker governance/license history.)

### 11.2 WIT interface & the Vfs bridge

Host **imports** are capability-scoped: `logging`, `progress` (always); `brokered-vfs`,
`brokered-http`, `credential-broker`, `plugin-config` (only if granted). Plugin **exports** one or
more worlds: `backend` (connect → resource with list/stat/open-read/open-write/actions),
`viewer` (can-view + render-chunk), `action` (describe + invoke). The host wraps a guest `backend`
export in a `PluginVfsBackend` that implements `Vfs` — so a plugin backend is indistinguishable from
a built-in one to the rest of Cairn.

Streaming across the boundary uses **chunked polling** today (page-token loops for `list`;
`read-chunk(max)`/`write-chunk(data)` resource methods adapted to `AsyncRead`/chunk-write), because
native WIT `stream<T>` is not yet stable. When it stabilizes it is added as an *additive* v0.2
interface; polling stays as the compat baseline.

### 11.3 Capability sandbox, isolation, lifecycle

Default-deny: the `Linker` only gets host functions for granted capabilities, so an ungranted import
fails at instantiation. **WASI ambient fs/sockets/process/env are withheld**; all I/O is brokered and
capability-checked at the host boundary and journaled as `Actor::Plugin`. **Brokered credentials:** a
plugin calls `acquire-token(secret-key)` and receives a session-scoped UUID stand-in; the host
substitutes the real secret only at HTTP dispatch — the plugin never sees the value (must be a
header, not URL-embedded). Isolation: per-instance `Store`, `ResourceLimiter` memory cap (e.g. 64 MiB
backend / 8 MiB viewer), **epoch-interruption** timeout (~5 s/call, reset per page) so a spinning
plugin can't hang the UI, run off the render path (`spawn_blocking`), trap→poison→reinstantiate, and
disable-after-N-crashes. Manifest (`plugin.toml`, also embedded as a wasm custom section) declares
id (reverse-DNS), version, targeted ABI, exported interfaces, and requested capabilities; install
shows the capability request for per-item approval (stored in config, individually revocable);
lazy instantiation keeps startup fast. WIT world is semver'd: major = breaking (refused), minor =
additive (`@since`-gated). Signing/registry are stubbed groundwork for a later phase.

### 11.4 Config vs plugin

Rule: **stateless, logic-free, no-I/O extensions are config; everything else is a plugin.** Config
(`cairn.toml`) covers custom keybindings, theme overrides, connection profiles, declarative
shell-command actions, filter/sort presets, and path aliases. A new VFS backend, a preview renderer,
or an action with logic/network/state requires a WASM plugin.

---

## 12. Error handling

Typed `thiserror` enums per library crate; `anyhow` only in the binary edge.

```rust
#[derive(thiserror::Error, Debug)] #[non_exhaustive]
pub enum VfsError {
    #[error("not found: {0}")] NotFound(VfsPath),
    #[error("permission denied: {0}")] Forbidden(VfsPath),
    #[error("already exists: {0}")] AlreadyExists(VfsPath),
    #[error("operation not supported ({0:?})")] Unsupported(Caps),
    #[error("timed out after {0:?}")] Timeout(Duration),
    #[error("connection failed")] Connection(#[source] BoxError),
    #[error("authentication failed")] Auth(#[source] AuthError),   // never embeds secrets
    #[error("conflict / precondition failed")] Conflict,
    #[error("backend error: {code}")] Backend { code: SmolStr, msg: String, retryable: bool },
    #[error("cancelled")] Cancelled,
    #[error("io")] Io(#[source] std::io::Error),
}
impl VfsError { pub fn is_retryable(&self) -> bool; pub fn redacted(&self) -> RedactedError<'_>; }
```

`CairnError` wraps subsystem errors (`Vfs|Vault|Transfer|Ai|Plugin|Config`) for the UI, which
pattern-matches for presentation (icon/color/retry affordance). Key rules: **`Unsupported(Caps)` is
non-scary** (capabilities prevent offering it in the first place; it's the safety net);
**retryability is carried** so engine and UI share one truth; **secrets never enter error values** —
SDK errors are wrapped in a `SanitizedError` at the backend boundary (the one place the raw error is
logged, at debug) and `redacted()` is applied before logging or any path to the AI. This is a
mandatory `security-review` checkpoint for any new `#[source]`.

---

## 13. Config & session persistence (`cairn-config`)

TOML config (human-editable) + a machine-managed state store + the separate vault. Paths via
`directories`.

| Data | Store |
|---|---|
| Keybind preset, theme, AI provider/endpoint, defaults | `config.toml` |
| Connection profiles (host/bucket/region/profile *name* + `VaultRef`) — **no secrets** | `config.toml` |
| Bookmarks / hotlist | `config.toml` |
| Navigation history, last session (panes/tabs/cwd), transfer-resume | state dir |
| Credentials/tokens/passphrases | **vault only** |

```rust
pub struct ConnectionProfile {           // CANNOT hold a secret — only a reference
    pub id: ConnectionId, pub scheme: Scheme, pub display_name: String,
    pub endpoint: ConnEndpoint,          // host/bucket/region/context — non-secret
    pub secret_ref: Option<VaultRef>,    // opaque; resolved by the broker at connect
}
```

The config↔vault boundary is type-enforced. Config is schema-versioned with a forward migration at
load (unknown future keys preserved on round-trip); config and state use atomic temp-write+rename.

---

## 14. Cross-cutting

- **Observability:** `tracing` with the redaction layer (§9.5); a panic hook restores the terminal
  and writes a crash report rather than corrupting the screen. `tokio-console` available in dev.
- **Testing (CLAUDE §8):** default `cargo test` is hermetic and offline. `MockVfs`, `MockProvider`,
  `MockBroker`, and a `MockObjectStore` live behind `cfg(test)`/`test-utils`. Integration tests are
  feature-gated and run against local emulators in dedicated CI jobs: MinIO/Azurite/fake-gcs-server
  (object), an SSH server image (sftp), dind (docker), `kind` (k8s), Ollama (AI). Object stores get a
  **contract test** suite run against all three providers to catch behavioral divergence. Plugin tests
  use tiny pre-compiled guest `.wasm` fixtures (no WASM toolchain needed in CI).
- **Cross-platform:** static musl on Linux; universal macOS; native Windows (VT setup, ACL caveat for
  the vault file). Headless/SSH is first-class (keychain-absent → passphrase fallback).
- **Lints:** workspace-wide `forbid(unsafe_code)`, clippy pedantic, deny `unwrap/expect/panic`,
  `disallowed_methods` (`std::thread::sleep`, stray `tokio::spawn`, blocking `std::fs` in async).

---

## 15. Decisions summary

| # | Decision | Choice | Rationale | ADR |
|---|---|---|---|---|
| 1 | VFS dispatch | `#[async_trait]` + `Arc<dyn Vfs>`; sync-returning streaming `list`; chunked `WriteHandle` | object-safety for heterogeneous + plugin backends; streams for backpressure; `finish()` returns metadata | ADR-0001 |
| 2 | Runtime / UI | tokio multi-thread; fully sync render path; bounded coalesced events; cancellation tokens | structural "UI never blocks" | ADR-0001 |
| 3 | App architecture | Elm/TEA pure reducer; overlay stack for confirm gating; state holds no handles | testable; clean async result routing | ADR-0001 |
| 4 | Crate layout | many small crates; backends are leaf siblings; AI/plugins depend only on broker | quarantine heavy SDKs; structural secret isolation | ADR-0001 |
| 5 | Vault crypto | XChaCha20-Poly1305 + per-entry DEKs; Argon2id fallback; KEK in OS keychain | safe random nonces; fast unlock; least plaintext in RAM | ADR-0002 |
| 6 | Security boundary | `cairn-broker` mediates; AI & plugins speak handles; plan→confirm enforced by broker/UI | injection/plugin blast radius ≤ what the user approves | ADR-0002 |
| 7 | Object stores | official per-provider SDKs, **not** opendal | need part-level multipart/resume/CAS/presign control | ADR-0003 |
| 8 | Transfer engine | above-VFS, stream-through bounded buffer, server-copy fast path, resumable multipart | one code path for all backends; no whole-file staging | — |
| 9 | TUI | ratatui + crossterm; pure render fns; virtualized lists; two-face highlighting | cheap diffed redraws; 100k-entry smoothness | — |
| 10 | AI | provider-agnostic trait; closed tool registry; engine-driven plan execution | Claude+Ollama drop-in; structural injection containment | ADR-0002 |
| 11 | Plugins | wasmtime + Component Model/WIT; capability sandbox; chunked-poll streaming | typed language-agnostic ABI; safe-by-default | ADR-0004 |
| 12 | Errors | thiserror per lib + anyhow edge; `Unsupported(Caps)` first-class; `redacted()` | legible, secret-safe, capability-aware | — |

---

## 16. Open items → RFCs

Deep designs intentionally deferred to RFCs (write & review before large implementation, per
CLAUDE §5): per-backend designs for **local**, **SSH/SFTP**, **Docker**, and **Kubernetes**; the
**transfer-engine resume format** details; **team/shared vaults** (the KEK-wrapping layer is designed
to extend to per-recipient `age` wrapping later); the **reveal-secret UX** (re-auth, clipboard
auto-clear); the **plugin registry & signing** phase; and **MCP client** consumption. The
Implementation Plan will sequence these against the v1 backend rollout.

---

## Appendix — primary dependencies (indicative)

`tokio`, `futures`, `bytes`, `async-trait`, `thiserror`/`anyhow`, `tracing`, `serde`,
`smol_str`/`smallvec`, `bitflags`, `directories`, `uuid`, `arc-swap`, `tokio-util` ·
TUI: `ratatui`, `crossterm`, `unicode-width`, `unicode-segmentation`, `two-face`, `nucleo` ·
crypto: `chacha20poly1305`, `argon2`, `zeroize`, `secrecy`, `keyring`, `fd-lock`, `postcard`,
`getrandom`, optional `age` · object: `aws-sdk-s3`, `google-cloud-storage`, `azure_storage_blobs`,
`crc32c` · ssh: `russh`/`openssh-sftp-client` · docker: `bollard` · k8s: `kube` · AI: provider HTTP
via `reqwest`, `schemars` · plugins: `wasmtime`, `wasmtime-wasi`, `wit-bindgen`, `async-stream`.
All must pass `cargo-deny` (permissive licenses only).
