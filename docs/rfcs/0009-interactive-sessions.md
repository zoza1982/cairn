# RFC-0009: Interactive sessions — exec, port-forward, and the TUI session pane

- **Status:** Proposed
- **Author(s):** kube-staff-engineer, tui-engineer (synthesized)
- **Date:** 2026-06-30
- **Tracking item:** M6-6 (exec + port-forward backends), M6-7 (TUI session/terminal pane)

## Summary

Defines the concrete shape of `SessionHandle` (including the `resize` channel and exit-code
signal that are absent today), the backend implementations for interactive `exec` (Docker via
bollard, Kubernetes via kube-rs) and `port-forward` (Kubernetes via `Portforwarder` + a local
`TcpListener`), the new TEA effects and events that wire a `SessionHandle` into the event loop,
and the TUI session pane that presents them. Also resolves the three open questions from
RFC-0007 §Unresolved about how indefinitely-running steps (`logs`/`exec`/`port_forward`)
integrate with `StepExecutor::execute`. Recommends a staged v1 scope (cooked exec pane +
port-forward status) with a clear upgrade path to a full raw-TTY/vt100 pane.

## Design

### 1. `SessionHandle` — current definition and proposed refinements

`cairn-vfs/src/action.rs` currently defines:

```rust
#[non_exhaustive]  // missing today — add this
pub struct SessionHandle {
    pub cancel: tokio::sync::oneshot::Sender<()>,
    pub done:   tokio::sync::oneshot::Receiver<Result<(), VfsError>>,
    pub local_port: Option<u16>,
    pub stdin:  Option<tokio::sync::mpsc::Sender<Bytes>>,
    pub stdout: Option<tokio::sync::mpsc::Receiver<Bytes>>,
}
```

Two problems: (a) it is not `#[non_exhaustive]`, so every field addition is a breaking
change for any struct-literal or exhaustive destructure; (b) it has no way to signal TTY
resize, and its `done` channel loses the exec exit code.

**Proposed shape:**

```rust
#[non_exhaustive]
pub struct SessionHandle {
    /// Send `()` (or drop) to request cancellation.
    pub cancel:     tokio::sync::oneshot::Sender<()>,
    /// Resolves once with the exit code (exec) or `0` (port-forward clean teardown),
    /// or an error on unexpected failure. Non-zero exit is `Ok(n)`, not an error.
    pub done:       tokio::sync::oneshot::Receiver<Result<i32, VfsError>>,
    /// For port-forward sessions: the local TCP port that was bound. `None` for exec.
    pub local_port: Option<u16>,
    /// Stdin pipe for an interactive exec; absent for port-forward. The consumer owns
    /// it for the session's lifetime; dropping it closes stdin on the remote side.
    pub stdin:      Option<tokio::sync::mpsc::Sender<Bytes>>,
    /// Combined stdout/stderr stream for an exec session; absent for port-forward.
    pub stdout:     Option<tokio::sync::mpsc::Receiver<Bytes>>,
    /// TTY resize sink: send `(rows, cols)` to propagate a terminal resize. Present
    /// only when `ActionCtx::Exec { tty: true }`. The backend ignores sends on error
    /// (session already ended). Absent for non-TTY exec and port-forward.
    pub resize:     Option<tokio::sync::mpsc::Sender<(u16, u16)>>,
}
```

**Change rationale:**

- `done: Result<i32, VfsError>` — exit code 0 = success for exec; port-forward backends
  send `Ok(0)` on clean teardown. The TUI can display "exit 1" rather than an opaque error.
  Backends must not expose secret material in the `VfsError` string; `VfsError::redacted()`
  applies before any display.
- `resize: Option<mpsc::Sender<(u16, u16)>>` — present iff `tty: true` was requested.
  The backend spawns a relay task that converts `(rows, cols)` into the API-specific resize
  call (Docker: `resize_exec`; K8s: write `TerminalSize` to `AttachedProcess`). The TUI only
  needs this channel when it implements raw-TTY rendering (v2 below); it can be ignored for
  the v1 cooked pane.
- `#[non_exhaustive]` — must be added now, before the struct is used outside `cairn-vfs`.
  All existing construction sites are in test code or the mock; the annotation is cheap
  insurance against a later forced semver break.

**Migration:** `done` changes from `Result<(), VfsError>` to `Result<i32, VfsError>`. The
only consumer today is mock test code and the wiring stubs in `cairn/src/app.rs`. All call
sites map `Ok(())` → `Ok(0)`.

---

### 2. Docker exec

#### API path

```
docker.create_exec(container_id, CreateExecOptions {
    cmd:            Some(argv),
    attach_stdin:   Some(tty || !argv_is_oneshot),
    attach_stdout:  Some(true),
    attach_stderr:  Some(!tty),   // stderr is merged into stdout by Docker when tty=true
    tty:            Some(tty),
    ..Default::default()
}).await?
```

then:

```
match docker.start_exec(&exec_id, None).await? {
    StartExecResults::Attached { mut output, input } => { … }
    StartExecResults::Detached => unreachable!(),  // we never pass detach: true
}
```

`output` is `Pin<Box<dyn Stream<Item = Result<LogOutput, BollardError>> + Send>>`.
`LogOutput` carries a `bytes::Bytes` payload with a variant tag: `StdOut`, `StdErr`,
`StdIn`, `Console` (the last appears when `tty: true`, merging all streams). `input` carries
a writable `AsyncWrite` handle for stdin.

#### Backend task (Docker)

The backend spawns a single tokio task that owns `output` and `input`. It:

1. Bridges `input: impl AsyncWrite` ← `stdin: mpsc::Receiver<Bytes>`: a sub-task drains the
   receiver and writes each chunk with `AsyncWriteExt::write_all`. Dropping the mpsc sender
   causes the receiver to return `None`, which closes stdin by dropping `input`.

2. Bridges `output` → `stdout: mpsc::Sender<Bytes>`: reads the stream, concatenates `StdOut`,
   `StdErr`, and `Console` payloads into a single interleaved byte stream sent to `stdout`.
   This matches how a real terminal works: the consumer (TUI) sees combined output without
   caring about the distinction.

3. On `cancel` signal: drop `input` and `output`, which closes the exec socket. Docker does
   not send a SIGKILL automatically; for interactive sessions this is intentional (the user
   may type `exit`). A back-channel kill is a follow-up.

4. On TTY resize: the `resize` relay sub-task reads `(rows, cols)` from the mpsc sender and
   calls `docker.resize_exec(&exec_id, ResizeExecTtyOptions { height: rows, width: cols })`
   as a best-effort fire-and-forget (errors are logged, not fatal).

5. Exit: when `output` ends (stream exhausted), call `docker.inspect_exec(&exec_id)` to
   retrieve `ExitCode`, then send `Ok(exit_code)` on `done`.

#### TTY vs non-TTY

For v1 (`tty: false`), the exec runs in cooked mode: the argv is passed as-is, output is
line-buffered, and the stdin pipe accepts whole lines terminated by `\n`. No resize is needed.
`tty: true` is reserved for v2 (raw-TTY pane). The `tty` field is forwarded directly from
`ActionCtx::Exec { tty }`.

---

### 3. Kubernetes exec and port-forward

#### K8s exec

```rust
let attached = pods
    .exec(pod_name, argv, &AttachParams {
        container: container_name,
        stdin: true,
        stdout: true,
        stderr: !tty,
        tty,
        ..AttachParams::default()
    })
    .await?;
```

`AttachedProcess` exposes:
- `.stdin()` → `Option<impl AsyncWrite + Send>` (present when `stdin: true`)
- `.stdout()` → `Option<impl AsyncRead + Send>` (present when `stdout: true`)
- `.stderr()` → `Option<impl AsyncRead + Send>` (present when `stderr: true` and `tty: false`)
- `.terminal_size()` → `Option<tokio::sync::watch::Sender<TerminalSize>>` where
  `TerminalSize { width: u16, height: u16 }`; present when `tty: true`
- `.join()` → future that resolves when the exec process exits

The backend task:

1. Spawns a relay for stdin: `mpsc::Receiver<Bytes>` → `attached.stdin()`. Runs concurrently
   with the stdout relay so neither blocks the other.

2. Spawns a relay for stdout+stderr: reads from `stdout()` and `stderr()` using
   `tokio::io::AsyncReadExt::read_buf` in a `tokio::select!` loop; sends chunks to the
   `stdout: mpsc::Sender<Bytes>`. When both readers return EOF, the relay ends.

3. For the resize relay: reads `(rows, cols)` from the `resize: mpsc::Receiver<(u16, u16)>`
   channel, converts to `TerminalSize`, and sends to `attached.terminal_size()`. Uses the
   `watch::Sender` directly — no polling, the watch takes the latest value.

4. Awaits `attached.join()` to get the exit status and sends on `done`.

5. On `cancel`: drops all relay tasks (cooperative via `CancellationToken` joined to the
   channel drop), then awaits `join()` with a short deadline; if the process has not exited,
   sends `Ok(-1)` on `done` (the container process may continue running — exec cancel does
   not SIGKILL the container process; the user must type `exit` or the session times out).

**Edge cases:**
- `exec` requires the pod to be `Running`; a `Pending`/`Succeeded`/`Failed` pod returns a
  404 or 400 from the API server, surfaced as `VfsError::NotFound` or `VfsError::Backend`.
- Pods with multiple containers: the caller provides the container name in `ActionCtx::Exec`
  via the `container` field (the K8s backend already resolves it from the path depth-3/4).
- The API server proxies the exec over WebSocket (SPDY for older clusters). `kube-rs` handles
  the negotiation; the backend sees only `AsyncRead`/`AsyncWrite`.

#### K8s port-forward

Port-forward is simpler than exec: the TUI is a transparent proxy, not an interactive user.
The K8s side uses `Api::<Pod>::portforward(pod_name, &[remote_port])` which returns a
`Portforwarder`. The backend then binds a local TCP socket and proxies every connection:

```rust
let listener = TcpListener::bind("127.0.0.1:local_port").await?;
let bound_port = listener.local_addr()?.port();
// SessionHandle.local_port = Some(bound_port)
```

If `local_port == 0`, the OS assigns an ephemeral port; the actual bound port is what the
TUI displays. `local_port: Some(n)` from `ActionCtx::PortForward` is used when the user
explicitly requested a port (e.g. `5432` to match a local `psql` alias).

Per-connection proxy loop (spawned per `TcpStream` accepted by `listener`):

```rust
let mut forwarder = pods.portforward(pod_name, &[remote_port]).await?;
let mut upstream = forwarder.take_stream(remote_port)?;
tokio::io::copy_bidirectional(&mut client_conn, &mut upstream).await?;
drop(upstream);
forwarder.join().await?;
```

Each new local TCP connection spawns a fresh `Portforwarder` (kube-rs allocates a WebSocket
per `portforward` call). Multiple simultaneous connections are thus fully multiplexed by the
K8s API server, which is the documented model.

**Lifecycle and cancellation:**

The backend task holds a `CancellationToken` and runs the `listener` loop with
`TcpListenerStream::take_until(cancel.cancelled())`. On cancel, the listener stops accepting,
and each in-flight proxy task is individually cancelled (the `copy_bidirectional` future is
cancelled cooperatively via `tokio::select!`). When all proxy tasks finish, `done` resolves
with `Ok(0)`.

`SessionHandle.local_port = Some(bound_port)` is set before the `Session` variant is returned
from `invoke`, so the TUI can display the address immediately before any connection arrives.

**Binding errors:** `EADDRINUSE` on an explicit port → `VfsError::Backend("port in use")`.
The TUI must surface this clearly; the user can retry with `local: 0` or a different port.

---

### 4. TUI session pane — v1 scope and v2 upgrade path

The TUI already has a log-viewer overlay (`Overlay::LogViewer`, `AppEffect::OpenLogViewer`,
`AppEvent::LogChunk`, `AppEvent::LogStreamEnded`, keyed by `LogViewerId`). The session pane
follows the same pattern with a new ID type and two new overlays.

#### New TEA surface

**IDs and state:**

```rust
pub struct SessionId(pub u64);     // in cairn-types
pub struct SessionRecord {          // in cairn-core AppState
    pub path:       VfsPath,
    pub title:      String,         // "exec sh /web-1/app" or "pf 127.0.0.1:5432→5432"
    pub output_buf: VecDeque<Bytes>,// ring buffer, same size policy as LogViewer (100 k lines / 4 MiB)
    pub local_port: Option<u16>,
    pub ended:      Option<SessionEnd>,
}
pub struct SessionEnd {
    pub exit_code: Option<i32>,
    pub error:     Option<String>,  // redacted
}
```

**Effects (additions to `AppEffect`):**

```rust
OpenExecSession {
    id:   SessionId,
    conn: ConnectionId,
    path: VfsPath,
    argv: Vec<String>,
    tty:  bool,
    title: String,
},
OpenPortForward {
    id:         SessionId,
    conn:       ConnectionId,
    path:       VfsPath,
    local_port: u16,        // 0 = OS-assigned
    remote_port: u16,
    title:      String,
},
CloseSession { id: SessionId },
SendSessionInput { id: SessionId, bytes: Bytes },
ResizeSession { id: SessionId, rows: u16, cols: u16 },
```

**Events (additions to `AppEvent`):**

```rust
SessionOutput   { id: SessionId, bytes: Bytes },
SessionEnded    { id: SessionId, exit_code: Option<i32>, error: Option<String> },
PortForwardBound { id: SessionId, local_port: u16 },
```

`PortForwardBound` is sent once, immediately after the `TcpListener` is bound and before the
first connection arrives, so the TUI can update the displayed address without waiting.

**Overlays (additions to `Overlay`):**

```rust
ExecPane       { id: SessionId },
PortForwardStatus { id: SessionId },
```

#### v1 — cooked exec pane

Scope: `tty: false`, line-oriented, no terminal emulation.

- **Display.** `ExecPane` renders `SessionRecord.output_buf` as scrolling text lines — the
  same approach as `LogViewer` (a `Paragraph` widget with a scroll offset). Output bytes are
  decoded lossily; each chunk is split on `\n` and appended to the buffer.

- **Input.** A single-line text field at the pane's bottom edge (reusing `cairn_tui`'s
  existing `TextEdit` widget). `Enter` emits `AppEffect::SendSessionInput` with the line bytes
  plus a `\n`. `Ctrl-D` closes stdin (drops the mpsc sender).

- **Focus and escape.** While `ExecPane` is the active overlay, the key router sends raw
  characters to the input field rather than dispatching to the file-manager keymap. `Ctrl-]`
  (or `Ctrl-\`) detaches the pane without killing the remote process. The overlay closes and
  the backend continues until the remote side exits, at which point `SessionEnded` arrives and
  cleans up the state entry.

- **Resize.** Not required for v1 (no TTY). The `resize` field of `SessionHandle` will be
  `None` since `tty: false`. If the terminal window is resized, the pane re-renders to the new
  dimensions without any backend communication.

This avoids any dependency on a terminal-emulation library and is entirely hermetically
testable via `MockVfs` + a mock `SessionHandle`.

#### v2 — raw-TTY pane with vt100 emulation (follow-up)

When `tty: true`, the session expects a PTY allocation and the output contains ANSI/VT100
control sequences. Rendering this correctly requires a terminal emulator.

**Recommended library: `tui-term`.** Built on top of `vt100` (the parser) and designed for
embedding in ratatui widgets. It exposes a `PseudoTerminal` widget that takes a `vt100::Screen`
reference and renders it as a `ratatui::widget::Widget`. The `vt100::Parser` processes the raw
bytes from `SessionHandle.stdout` and maintains the screen state.

**Why not `alacritty-terminal` or `wezterm-term`.** Both are heavier and carry dependencies
(OpenGL, platform-specific code) that conflict with a cross-platform TUI. `tui-term` is
minimal (it re-exports only `vt100`) and has ratatui-native rendering.

**Key input in raw mode.** Crossterm already operates in raw mode for Cairn's input loop.
When the exec pane is focused, key events are converted to escape sequences (e.g., arrow keys
→ `\x1b[A`) and sent as `AppEffect::SendSessionInput`. The `Keymap` is bypassed for all keys
except the detach chord (`Ctrl-]`).

**Resize propagation.** `Event::Resize(cols, rows)` from crossterm triggers
`AppEffect::ResizeSession { rows, cols }`. The effect runner sends on the `resize` channel of
the `SessionHandle`. For Docker: calls `docker.resize_exec(exec_id, ...)`. For K8s: writes
`TerminalSize` to the `AttachedProcess` watch sender.

**Gate: `tui-term` behind a Cargo feature.** Following ADR-0006, v2 raw-TTY support is
gated behind `features = ["vt100-session"]` in `cairn-tui`'s `Cargo.toml`. The default
build and v1 pane compile without it. The feature activates the `tui-term` dep and the
`vt100::Parser` integration; the `ExecPane` renderer switches from a simple `Paragraph` to
the `PseudoTerminal` widget.

**v1 → v2 transition.** The `OpenExecSession` effect carries `tty: bool`. When `tty: false`,
the runtime uses the v1 cooked pane. When `tty: true` and the `vt100-session` feature is
compiled in, it uses the v2 raw-TTY pane. When `tty: true` but the feature is absent, the
backend falls back to `tty: false` (the VFS call is retried with `tty: false`) and surfaces a
status note: "raw-TTY session requires the vt100-session feature".

#### Port-forward status pane

`PortForwardStatus` is a read-only pane with no input:

```
┌─ Port Forward: /default/postgres:5432 ──────────────────────┐
│  Local:  127.0.0.1:5432                                      │
│  Remote: /default/postgres → container-port 5432            │
│  Status: Active (3 connection(s) served)                     │
│  [Esc / q] Close                                             │
└──────────────────────────────────────────────────────────────┘
```

The connection counter is incremented by an `AppEvent::SessionOutput { bytes }` whose first
byte the runtime uses as an opaque counter update — or, more cleanly, a new
`AppEvent::PortForwardConnectionCount { id, count }` sent by the backend task each time the
listener accepts a new connection. This is best-effort; the count is cosmetic.

---

### 5. Safety — exec/port-forward in the plan→confirm model

RFC-0007 establishes:
- `exec` is `Irreversible` — individual plan-step approval always required.
- `port-forward` is `Recoverable` — terminable, bulk-approvable when all plan steps are
  `Safe` or `Recoverable`.

This RFC adds no changes to those reversibility classifications.

**AI-invoked path.** When `exec` or `port-forward` appears in an AI-proposed plan, the user
sees the full step in the confirm overlay (argv, path, port numbers) before any backend call.
The `SessionHandle` returned by `Vfs::invoke` is held exclusively by the TUI; the AI layer
has no handle to it. The model cannot cancel, inspect output from, or extend an already-opened
session. This is enforced by the type system: `SessionHandle` lives in the effect runner (not
in `cairn-ai`), and `cairn-ai` has no transitive dependency on `cairn-vfs`.

**User-invoked path.** When the user invokes `exec` or `port-forward` from the action menu
(`actions_at` advertises them), the TUI sends `OpenExecSession`/`OpenPortForward` directly —
no confirm gate, no AI involvement. This is the fast path. If `exec` is a shell (e.g., `sh`),
the user is implicitly trusted; Cairn is a file manager, not a sandbox.

**Confinement.** AI-invoked sessions are restricted to connections visible in
`WorldSnapshot.connections` (the two pane connections), not the full registry. This is
enforced by `BinaryStepExecutor`'s allow-list, which already excludes switcher backends. An
AI-proposed `exec` on a connection the user cannot see is rejected before confirmation is
requested.

**Session I/O and the AI layer.** `SessionRecord.output_buf` is held in `AppState` and is
accessible to `WorldSnapshot`. This RFC recommends it is **not** included in the snapshot:
session output may contain sensitive data (secrets printed to a shell, log lines with tokens),
and there is no safe redaction heuristic. The AI layer must not read session I/O. If a future
use case requires it, a dedicated RFC must address redaction and user consent.

---

### 6. Crate and dependency impact

**`cairn-vfs`:**
- `SessionHandle`: add `resize` field, change `done` type. Semver: patch version bump (no
  change to `Vfs` trait signature; `ActionOutcome::Session(SessionHandle)` is `#[non_exhaustive]`
  so existing `_ => …` match arms are unaffected).
- Add `#[non_exhaustive]` to `SessionHandle`.

**`cairn-backend-k8s`** (behind `k8s` feature, ADR-0006):
- New methods on `KubeOps` trait: `exec` + `portforward`. `MockKube` gains stubs.
- New dep (already present for log streaming): `tokio/net` for `TcpListener`.
- No new external crates beyond what `kube` already pulls in.

**`cairn-backend-docker`** (behind `docker` feature, ADR-0006):
- New method on `ContainerOps` trait: `exec`. `MockContainerOps` gains a stub.
- No new external crates; bollard's exec API is already in scope.

**`cairn-core`:**
- `AppEffect`: add `OpenExecSession`, `OpenPortForward`, `CloseSession`, `SendSessionInput`,
  `ResizeSession`.
- `AppEvent`: add `SessionOutput`, `SessionEnded`, `PortForwardBound`, optionally
  `PortForwardConnectionCount`.
- `AppState`: add `sessions: HashMap<SessionId, SessionRecord>`.
- Pure reducer tests cover: open/close lifecycle, output append (ring-buffer eviction),
  `SessionEnded` cleanup.

**`cairn-tui`:**
- v1: no new deps. Adds `Overlay::ExecPane` + `Overlay::PortForwardStatus` renderers.
  Key-router update: while `ExecPane` is focused, bypass keymap for non-detach keys.
- v2: new optional dep `tui-term = { version = "0.5", optional = true }` behind
  `features = ["vt100-session"]`; activates `vt100::Parser` in the exec pane renderer.

**`cairn` (binary):**
- Effect runner: `OpenExecSession` → spawn task; store `(SessionHandle, CancellationToken)` in
  `HashMap<SessionId, SessionControls>`. Same pattern as `log_viewer_controls`.
- `SendSessionInput`: look up `SessionHandle.stdin` in controls; send bytes; no-op if absent.
- `ResizeSession`: look up `SessionHandle.resize`; send `(rows, cols)`; no-op if absent.
- `CloseSession`: fire cancel sender; remove from map.

**Mock `SessionHandle` for tests:**

```rust
pub fn mock_exec_session() -> (SessionHandle, MockSessionPeer) {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let (done_tx, done_rx)     = oneshot::channel();
    let (stdin_tx, stdin_rx)   = mpsc::channel(16);
    let (stdout_tx, stdout_rx) = mpsc::channel(16);
    let (resize_tx, resize_rx) = mpsc::channel(4);
    let handle = SessionHandle {
        cancel: cancel_tx, done: done_rx,
        local_port: None,
        stdin: Some(stdin_tx), stdout: Some(stdout_rx), resize: Some(resize_tx),
    };
    let peer = MockSessionPeer { cancel_rx, done_tx, stdin_rx, stdout_tx, resize_rx };
    (handle, peer)
}
```

`MockSessionPeer` is owned by test code and simulates the remote process: read from
`stdin_rx`, write to `stdout_tx`, fire `done_tx` with an exit code. This enables full
TUI-layer tests without a real backend.

---

### 7. Resolving RFC-0007 open questions

**"StepExecutor result for streaming steps."** `StepExecutor::execute` returns
`Result<StepOutput, String>` where `StepOutput` gains an optional `outcome` field:

```rust
pub struct StepOutput {
    pub summary: Option<String>,           // text summary shown in plan-complete status
    pub outcome: Option<ActionOutcome>,    // Session/Stream carried out of the executor
}
```

The executor posts the `ActionOutcome` back via a `Sender<(SessionId, ActionOutcome)>` channel
owned by the effect runner. The plan step is marked `Done` immediately (the user approved it;
the session runs independently). This avoids blocking plan execution on an indefinitely-running
session.

**"Outcome storage."** The `SessionHandle` lives in the effect runner's `session_controls`
map. The plan step carries only a `SessionId` reference in `StepOutput.summary` (e.g.,
`"port-forward session:7 started on 127.0.0.1:5432"`). The AI layer never receives the handle.

**"Sequential vs concurrent execution."** Session and stream steps spawn background tasks and
resolve `execute` immediately. The plan runner remains strictly sequential for the step
ordering, but an indefinite session does not block subsequent steps. If two session steps
appear in one plan, both run concurrently (each on its own background task), which is correct:
a plan that opens two port-forwards is valid.

---

## Drawbacks / deferred

- **In-container copy (kubectl cp semantics)** — tar-over-exec for reading (M6-5b) is
  complete; write (copy-in) is a follow-up unrelated to this RFC.
- **exec kill on cancel** — dropping the exec connection does not SIGKILL the container
  process (it continues running). A `SIGKILL` would require a separate `docker kill`/`kubectl
  kill` call; this is deferred to avoid the ambiguity of a silent process kill. The user is
  in control of termination (they type `exit`).
- **Docker port-forward** — Docker does not have a native port-forward primitive comparable to
  K8s. For Docker, users can reach container ports via published ports or network attach.
  Deferring `port-forward` for Docker; `actions_at` will not advertise it on Docker paths.
- **Port-forward byte counting** — the connection count and bytes-forwarded metric are
  best-effort and cosmetic; accuracy is not guaranteed across multiple concurrent connections.
- **vt100 session pane (v2)** — deferred to a post-M6-7 PR; `tui-term` evaluation
  (rendering correctness for common terminal apps: vim, htop, fish) happens then.
- **Ephemeral/init containers in exec** — `AttachParams.container` carries the container name
  extracted from the path; ephemeral containers are in scope because `list_containers` already
  flags them via `ContainerInfo`. No additional exec-path work needed.
- **SPDY vs WebSocket** — kube-rs negotiates the subprotocol (`spdy/3.1` vs
  `v4.channel.k8s.io`) transparently. Older API servers (<1.20) that use SPDY exclusively are
  handled by kube-rs; this RFC makes no assumption about the wire format.

---

## Rationale & alternatives

- *Expose `AsyncWrite` / `AsyncRead` directly on `SessionHandle` instead of mpsc channels.*
  Rejected: `AsyncWrite` and `AsyncRead` are not `Send + Sync + Clone`; the handle must be
  storable in `AppState`-adjacent runtime state and passed between the effect runner and TUI
  callbacks. Mpsc channels are the natural TEA boundary.

- *Use a single `Vec<u8>` or `String` for output buffering instead of `mpsc<Bytes>`.* Rejected:
  the ring-buffer policy (100 k lines / 4 MiB cap) is the same as `LogViewer` and must
  be enforced incrementally, not on a growing allocation.

- *Allocate a PTY (via `pty-process` or `tokio-pty`) for the local side of the exec.*
  Considered for v1. Rejected: a PTY is only meaningful for true interactive programs that
  detect `isatty()`. For v1's cooked pane the added complexity (platform-specific pty APIs,
  pty ownership in async context) is not justified. It remains an option for v2.

- *Use `alacritty-terminal` or `wezterm-term` instead of `tui-term` for v2.* Both are more
  complete terminal emulators, but they carry non-trivial platform dependencies and are not
  designed for embedding in a ratatui widget. `tui-term` is the ratatui-native choice and
  supports the escape sequences needed for interactive programs (colors, cursor movement,
  alternate screen).

- *Port-forward: one `Portforwarder` per listener instead of one per accepted connection.*
  Rejected: kube-rs's `Portforwarder` is a single WebSocket connection to the API server; a
  single-portforwarder model would multiplex all connections over one WebSocket, but kube-rs
  exposes only one stream per port via `take_stream` and does not support adding new streams
  to an existing forwarder. One forwarder per connection is the documented pattern (kube-rs
  `pod_portforward_bind.rs` example) and is correct.

- *`done: Result<(), VfsError>` with exit code in a `VfsError::ExecExited(i32)` variant.*
  Rejected: misuses the error channel for a non-error condition (exit 0 is `Ok(())` today;
  `VfsError` is for failures). `Result<i32, VfsError>` is cleaner and avoids special-casing
  `VfsError` in the TUI's display path.

---

## Security & privacy

- **exec is as powerful as shell access.** A user who can invoke `exec sh` on a pod has full
  container-level access. This is intentional and documented. Cairn is a file manager, not a
  sandbox. The plan→confirm gate ensures AI-invoked exec requires explicit approval.
- **Port-forward binds on loopback only** (`127.0.0.1`). Binding `0.0.0.0` would expose the
  forward to the network; the backend must reject any `local_addr` other than `127.0.0.1` (or
  `::1` on IPv6) when the request comes from the AI layer. User-invoked port-forward may allow
  a configurable bind address as a future option, but the default must be loopback.
- **Session output is not shown to the AI.** `SessionRecord.output_buf` is explicitly excluded
  from `WorldSnapshot`. Implementing a future "AI reads session output" feature requires a
  dedicated RFC, user consent, and a redaction layer.
- **Credentials in exec I/O.** There is no technical barrier to a user running `env` or
  `cat /var/run/secrets/kubernetes.io/serviceaccount/token` in an exec session. Cairn cannot
  and does not attempt to redact exec output — this is the container's responsibility. The AI
  layer is isolated from this I/O by the type system.
- **Backend authentication.** `Vfs::invoke` is called with the same `Arc<dyn Vfs>` that holds
  the already-authenticated K8s/Docker client; no credential re-resolution is needed for exec
  or port-forward. Tokens are managed inside `kube::Client`/`bollard::Docker` and never touch
  `SessionHandle`.

---

## Unresolved questions

- **Detach vs kill semantics for the UX.** Should `Ctrl-]` (detach) leave the remote process
  running indefinitely, or should there be an explicit "kill" keybinding that sends SIGKILL
  via a separate API call? Deferred to the UX RFC/design for M6-7 detailed implementation.
- **Multiple exec sessions.** The design allows `sessions: HashMap<SessionId, SessionRecord>`,
  supporting multiple concurrent exec panes. How does the TUI expose session switching (e.g.,
  a session list overlay, numbered tabs)? Deferred to the TUI design for M6-7.
- **Port-forward on Docker.** Docker Desktop and CLI users often rely on
  `-p <host>:<container>` at container start. Decide whether to expose a "republish-port"
  action (which would require a container restart) or document the limitation and defer.
- **`tui-term` version pin.** `tui-term` is relatively young; API stability between minor
  versions is not yet guaranteed. Confirm before landing the v2 PR.
- **Exit code for cancelled sessions.** This RFC proposes `Ok(-1)` for a session cancelled
  while the remote process may still be alive. Confirm this is acceptable to the TUI (it could
  display "detached" rather than "exit -1").
