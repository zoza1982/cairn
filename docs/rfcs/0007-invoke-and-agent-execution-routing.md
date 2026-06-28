# RFC-0007: Action invocation & agent-execution routing

- **Status:** Proposed
- **Author(s):** ai-integration-engineer, rust-staff-engineer (synthesized)
- **Date:** 2026-06-28
- **Tracking item:** M6-3 (invoke path routing), M6-6 (Session variant), M7-6 (executor wiring)

## Summary

Closes two coupled gaps flagged during M6 code review, currently recorded as design notes in
`docs/IMPLEMENTATION_PLAN.md`. **Gap 1:** `Vfs::invoke` takes no path, so the Docker and
Kubernetes backends — which route everything by path segment — cannot identify the target
container or pod at invocation time. Fix: add `path: &VfsPath` as a first-class parameter and
define `ActionOutcome::Session` + `SessionHandle` for port-forward and interactive exec.
**Gap 2:** `StepExecutor::execute` receives `PlanStep.input` as opaque `serde_json::Value` with
no schema, so a real executor cannot map a step to a VFS call or transfer-engine operation. Fix:
define a typed input struct per tool and a concrete `BinaryStepExecutor` that resolves opaque
connection references against `VfsRegistry` and the transfer engine without ever touching secrets.

## Design

### Gap 1 — `Vfs::invoke` path routing

#### Revised signature

The current default method—

```rust
async fn invoke(&self, _action: ActionId, _ctx: ActionCtx) -> Result<ActionOutcome, VfsError>
```

—becomes:

```rust
async fn invoke(
    &self,
    path: &VfsPath,
    action: ActionId,
    ctx: ActionCtx,
) -> Result<ActionOutcome, VfsError> {
    let _ = (path, action, ctx);
    Err(VfsError::Unsupported(Caps::empty()))
}
```

`path` is *where* to act; `ActionCtx` is *how* (argv, tty, ports, since-time). Keeping them
orthogonal preserves the invariant that `ActionCtx` variants carry only behavioural parameters,
not routing state. The Docker backend pulls the container from the path segment; the Kubernetes
backend pulls pod and optional container from the depth-2/3/4 segments — the same depth-based
routing already used by `list`, `stat`, and `caps_at`.

#### `ActionOutcome::Session` and `SessionHandle`

`ActionOutcome` (already `#[non_exhaustive]`) gains a `Session` variant:

```rust
pub struct SessionHandle {
    /// Send `()` to cancel; dropping the sender has the same effect.
    pub cancel:     tokio::sync::oneshot::Sender<()>,
    /// Resolves when the session exits cleanly or with an error string.
    pub done:       tokio::sync::oneshot::Receiver<Result<(), VfsError>>,
    /// Local TCP port bound by the backend (port-forward only; `None` for exec sessions).
    pub local_port: Option<u16>,
    /// Stdin writer for TTY exec sessions; absent for port-forward and non-interactive exec.
    pub stdin:      Option<tokio::sync::mpsc::Sender<bytes::Bytes>>,
}

// Added to ActionOutcome:
Session(SessionHandle),
```

The TUI holds the `SessionHandle` for the lifetime of the session pane or port-forward status
widget. Dropping it is the implicit cancel signal.

#### Cancellation contract

- **`Stream`:** the caller drops the stream; the backend must observe the drop (a
  `CancellationToken` or a waker tied to the stream task) and close the underlying connection. The
  stream task must not outlive the dropped handle.
- **`Session`:** the caller sends on `cancel` or drops the sender. The backend closes the local
  listener or terminates the exec process. `done` resolves exactly once after cancellation.

#### Migration

The trait has six concrete impls (local, ssh, object-store, docker, k8s, mock). Impls that do not
override `invoke` — local, ssh, object-store, plugin — need only the mechanical signature update;
the docker and k8s impls gain the path parameter they already need. All existing `invoke` call
sites get a `path` argument added; no semantic change at those sites. `ActionOutcome` is
`#[non_exhaustive]`, so wildcard matches keep compiling; exhaustive matches in tests update.

### Gap 2 — agent execution routing

#### Tool set additions

Two entries are added to the closed tool set in `tools.rs` so reversibility is modelled correctly:

| Tool name      | Verb   | Reversibility  | Notes                                            |
|----------------|--------|----------------|--------------------------------------------------|
| `exec`         | `Exec` | `Irreversible` | existing; run a command in a container/pod       |
| `logs`         | `Exec` | `Safe`         | new; read-only log stream, bulk-approvable       |
| `port_forward` | `Exec` | `Recoverable`  | new; live listener, terminable, bulk-approvable  |

#### Per-tool typed input schema

The model references connections by the opaque `id` from `WorldSnapshot.connections` (e.g.
`"conn:1"`) and paths as absolute strings within the VFS tree. It never receives or emits
endpoints or credentials. The executor parses `"conn:N"` into `ConnectionId(N)`.

**Shared `ConnPath`** (reused across read/stat/list/delete/exec/logs/port-forward):

```rust
#[derive(serde::Deserialize)]
struct ConnPath { conn: String, path: String }
```

**Input shape per tool:**

```
list / stat / read:
  { "conn": "conn:1", "path": "/data/logs" }
copy:
  { "src": {"conn":"conn:1","path":"/data/logs/"}, "dst": {"conn":"conn:2","path":"/archive/"} }
move:
  { "src": {"conn":"conn:1","path":"/old/"}, "dst": {"conn":"conn:1","path":"/new/"} }
delete:
  { "conn": "conn:1", "paths": ["/data/old.log"], "recursive": false }
exec:
  { "conn": "conn:3", "path": "/web-1/app", "argv": ["sh","-c","…"], "tty": false }
logs:
  { "conn": "conn:3", "path": "/web-1/app", "follow": true, "since_secs": null, "container": null }
port_forward:
  { "conn": "conn:4", "path": "/default/my-pod", "local": 5432, "remote": 5432 }
open_connection:
  { "profile": "prod-s3" }
```

`open_connection` names a config profile by label — never an endpoint or credential. The broker
resolves credentials internally; the resulting `ConnectionId` becomes `"conn:N"` in subsequent
steps of the same plan run.

#### `BinaryStepExecutor`

```rust
pub struct BinaryStepExecutor {
    registry: Arc<VfsRegistry>,
    transfer: Arc<TransferEngine>,
    broker:   Arc<Broker>,   // only for open_connection
}
```

`StepExecutor::execute` dispatches on `step.tool` after parsing `step.input`:

| Tool              | Dispatch target                                                            |
|-------------------|---------------------------------------------------------------------------|
| `list`            | `vfs.list(path, ListOpts::default())`                                      |
| `stat`            | `vfs.stat(path)`                                                           |
| `read`            | `vfs.open_read(path, None)`                                                |
| `copy`            | `transfer.copy(src_vfs, src_path, dst_vfs, dst_path)`                      |
| `move`            | same-backend → `vfs.rename`; cross-backend → transfer move                 |
| `delete`          | `for p in paths { vfs.remove(p, recurse) }`                               |
| `exec`            | `vfs.invoke(&path, ActionId::EXEC, ActionCtx::Exec{argv,tty})`           |
| `logs`            | `vfs.invoke(&path, ActionId::LOGS, ActionCtx::Logs{..})`                  |
| `port_forward`    | `vfs.invoke(&path, ActionId::PORT_FORWARD, ActionCtx::PortForward{..})`   |
| `open_connection` | `broker.open_connection(profile)` → `registry.insert` → new `conn:N`      |

`registry.get(id)` resolves `"conn:N"` → `Arc<dyn Vfs>`, returning an error if absent. Unknown
tool names are unreachable (already rejected by `Plan::from_proposed`/`capability_for`); the
executor guards with an `"unroutable tool"` error for defence in depth.

#### Composition with plan→confirm

`Plan::execute` already verifies every step is `Approved` before calling the executor — unchanged.
`exec`/`delete` remain `Irreversible` (individual approval); `logs`/`port_forward` are
bulk-approvable. The executor maps tool → VFS/transfer; the confirm gate decides *when*. This makes
**local** plan execution (list/copy/move/delete against local backends) implementable and testable
hermetically now; exec/logs/port-forward execution still needs the live Docker/k8s integration.

## Drawbacks / deferred

- **Live stream/session integration.** `exec`/`logs`/`port_forward` emit `Stream`/`Session`
  outcomes; routing those into the TUI session pane (M6-7) and signalling completion via
  `SessionHandle.done` is the integration step. The execute contract may need to widen (see below).
- **`logs`/`port_forward` tool additions** expand the closed set and the schema advertised to the
  model; landed in the M6-6/M7-6 implementation PRs.
- **Cloud transfer SDK.** `copy`/`move` across object stores route through the transfer engine; its
  multipart internals are M2/M5, not this RFC.

## Rationale & alternatives

- *Add path to `ActionCtx` instead of `invoke`.* Rejected: `ActionCtx` encodes *how*; the target is
  a separate concern. Every variant would need a redundant `path` field.
- *Per-backend `invoke` dispatch outside the trait (downcast `Arc<dyn Vfs>`).* Rejected: breaks the
  object-safe uniform surface the TUI and AI layer rely on.
- *Opaque `input: Value` with runtime duck-typing.* Rejected: type errors would surface only at
  execution time, after the user has already approved a mutating step. Typed structs validated at
  parse/approve time catch hallucinated fields before approval is requested.
- *Model references connections by endpoint string.* Rejected: endpoints carry secret material;
  opaque `conn:N` handles from `WorldSnapshot` are the only reference the model may use.

## Security & privacy

- **No new model capabilities.** `capability_for` is the boundary; adding `logs`/`port_forward` is
  additive and a model cannot name a tool outside `TOOLS`.
- **Path as target, not secret.** `VfsPath::parse` already rejects `..` and control characters, so
  adding `path` to `invoke` opens no new injection surface; argv for `exec` is passed as a vector,
  never an interpolated shell string.
- **Broker never bypassed.** `open_connection` goes through the broker (journaled), which resolves
  credentials internally; credentials never appear in a plan step, the confirm UI, or a serialized
  plan.
- **`exec` stays `Irreversible`** — individual approval regardless of how benign the argv looks.
- **Session lifetime is TUI-owned.** The model proposes a session step; the `SessionHandle` is held
  by the TUI, not any AI crate, and the model has no tool to cancel or inspect a running session.

## Unresolved questions

- **`StepExecutor` result for streaming steps.** `execute` returns `Result<(), String>` and
  resolves synchronously, but `logs`/`port_forward` run indefinitely. Decide: widen the outcome to
  carry an optional `ActionOutcome`, or post the `SessionHandle` out of band and mark the step
  `Done` immediately.
- **Outcome storage.** If the TUI must show port-forward status, the `SessionHandle` must outlive
  `execute` — decide whether `PlanStep` grows an `outcome` field or the executor posts handles via a
  channel.
- **Connection-id propagation.** When an `open_connection` step yields a new `conn:N` used by a
  later step, define the protocol for threading the new id into downstream step inputs at run time.
- **Sequential vs concurrent execution.** `Plan::execute` is strictly sequential; an indefinite
  `logs` step would block the rest. Decide whether stream/session steps run on a background task or
  whether the plan model gains explicit parallelism.
