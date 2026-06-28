# RFC-0006: Plugin host & WIT ABI

- **Status:** Accepted
- **Author(s):** plugin-systems-engineer, technical-writer (synthesized)
- **Date:** 2026-06-28
- **Tracking item:** M8-1 (this RFC); implemented by M8-3 (WIT bindings + `PluginVfsBackend`)

## Summary

Builds on ADR-0004 and the `cairn-plugin` runtime core (M8-2, already merged) to define the stable
typed contract between Cairn and untrusted WASM plugins. A `cairn:plugin@1.0.0` WIT package declares
three interfaces (`types`, `host`, `backend`) and a `backend-plugin` world. A plugin component
exports `backend`; the host wraps the wasmtime instance as `PluginVfsBackend : Vfs`, making it
indistinguishable from a built-in backend. Host functions are default-deny; grants drive which are
linked into the `Linker`. Credentials flow as opaque handles — the plugin never observes the raw
secret. All calls run off the render path; `PluginError` (fuel/trap/epoch) maps uniformly to
`VfsError::Backend`.

## Design

### The WIT package

```wit
package cairn:plugin@1.0.0;

/// Core types shared by host and guest; generated once by wit-bindgen on the host side.
interface types {
  /// Mirrors cairn_types::Caps (bitflags). Bit positions must never change across minor versions.
  flags caps {
    list, read, write, create-dir, delete, rename, rename-atomic,
    copy-server, chmod, chown, symlink, append, random-read,
    multipart, versions, watch, search-content,
  }

  enum entry-kind { file, dir, symlink, stream, special }

  record entry {
    name:           string,
    kind:           entry-kind,
    size:           option<u64>,
    modified-secs:  option<u64>,    // Unix timestamp; none = unknown
    etag:           option<string>,
    symlink-target: option<string>, // set when kind == symlink
  }

  record byte-range {
    offset: u64,
    len:    option<u64>,            // none = to EOF
  }

  record backend-error {
    code:      string,              // stable, machine-readable
    msg:       string,              // secret-free human message
    retryable: bool,
  }

  variant vfs-error {
    not-found(string),              // path string
    forbidden(string),
    already-exists(string),
    unsupported(caps),              // which capability flags are missing
    timeout-ms(u64),
    connection(string),             // secret-free description
    auth,
    conflict,
    backend(backend-error),
    cancelled,
    io(string),
  }
}

/// Host functions available to plugins. Each is linked only when the corresponding
/// grant is present — default-deny. An unlinked import causes instantiation failure.
interface host {
  use types.{vfs-error};

  record http-request {
    method:  string,
    url:     string,
    headers: list<tuple<string, string>>,
    body:    option<list<u8>>,
  }

  record http-response {
    status:  u16,
    headers: list<tuple<string, string>>,
    body:    list<u8>,
  }

  /// Write a log line at the given level (0=error … 3=debug). Requires `log` grant.
  log: func(level: u8, msg: string);

  /// Wall-clock seconds since Unix epoch. Always linked; no I/O, no secrets.
  now-secs: func() -> u64;

  /// Brokered HTTP fetch. Requires a `network` grant listing the target host.
  /// The host filters by URL allowlist before dispatch; requests to unlisted
  /// hosts are refused — the guest sees an error string, not a crash.
  http-fetch: func(req: http-request) -> result<http-response, string>;

  /// Signal the host to use a credential by its opaque handle. The host calls
  /// Broker::resolve, journals the event as Actor::Plugin, performs the brokered
  /// action (e.g. "sign-s3-request"), and returns an ephemeral artifact
  /// (a signed URL, a bearer prefix) — never the underlying secret.
  /// Requires a `credentials` grant naming the handle.
  use-credential: func(handle: string, action: string) -> result<string, string>;
}

/// The interface a plugin component must export.
interface backend {
  use types.{caps, entry, byte-range, vfs-error};

  record list-page-result {
    entries: list<entry>,
    cursor:  option<string>,  // opaque continuation token; none on the final page
    done:    bool,
  }

  // introspection
  scheme:       func() -> string;  // URI scheme served (e.g. "mycloud")
  backend-caps: func() -> caps;
  caps-at:      func(path: string) -> caps;

  // listing
  list-page: func(
    dir:            string,
    cursor:         option<string>,
    include-hidden: bool,
  ) -> result<list-page-result, vfs-error>;

  // metadata
  stat: func(path: string) -> result<entry, vfs-error>;

  // reading — resource owns the stream lifetime; host can revoke on timeout
  resource read-stream {
    read-chunk: func(max-bytes: u32) -> result<list<u8>, vfs-error>;
    close: func();
  }
  open-read: func(path: string, range: option<byte-range>) -> result<read-stream, vfs-error>;

  // writing
  resource write-sink {
    write-chunk: func(chunk: list<u8>) -> result<_, vfs-error>;
    finish: func() -> result<entry, vfs-error>;  // commit; returns final metadata
    abort:  func();                               // discard; free server-side state
  }
  open-write: func(
    path:      string,
    overwrite: bool,
    size-hint: option<u64>,
  ) -> result<write-sink, vfs-error>;

  // mutations
  create-dir: func(path: string) -> result<_, vfs-error>;
  remove:     func(path: string, recursive: bool) -> result<_, vfs-error>;
  rename:     func(from: string, to: string) -> result<_, vfs-error>;
}

world backend-plugin {
  import host;
  export backend;
}
```

The `types` interface is not imported directly into `backend-plugin`; the component toolchain
resolves it transitively through `host`'s and `backend`'s `use` declarations. The hosted world path
is `cairn:plugin/backend-plugin`.

### Host↔guest mapping

`PluginVfsBackend` wraps a persistent wasmtime `Store` holding the component instance and implements
`Vfs`. Every call is dispatched from a blocking closure (`spawn_blocking`) so it never touches the
render thread.

| `Vfs` method | WIT call(s) |
|---|---|
| `caps()` | `backend::backend-caps()` |
| `caps_at(path)` | `backend::caps-at(path)` |
| `list(dir, opts)` → `BoxStream<ListPage>` | `list-page(dir, None, opts.all)`, then loop while `!done`, passing `cursor`; each response → one `ListPage` pushed into the stream |
| `stat(path)` | `backend::stat(path)` |
| `open_read(path, range)` → `ReadHandle` | `backend::open-read(path, range)` → `read-stream`; `PluginReadStream : AsyncRead` calls `read-chunk(65536)` in `poll_read`, draining an internal buffer |
| `open_write(path, opts)` → `WriteHandle` | `backend::open-write(path, opts.overwrite, opts.size_hint)` → `write-sink`; `PluginWriteSink : WriteSink` delegates directly |
| `WriteHandle::write_chunk(chunk)` | `write-sink::write-chunk(bytes)` |
| `WriteHandle::finish()` | `write-sink::finish()` → `entry` (converted to `cairn_types::Entry`) |
| `WriteHandle::abort()` | `write-sink::abort()` |
| `create_dir(path)` | `backend::create-dir(path)` |
| `remove(path, recurse)` | `backend::remove(path, recurse == Recurse::Yes)` |
| `rename(from, to)` | `backend::rename(from, to)` |
| `copy_within`, `set_perms`, `actions_at`, `invoke` | Not in v1 ABI; `PluginVfsBackend` returns `VfsError::Unsupported(…)` |

`connection()` is not exported by the plugin; `PluginVfsBackend` synthesises a `ConnectionId` from
the plugin name and mount id at construction time.

**Cursor/page loop.** `list()` returns a `BoxStream` immediately. The first `list-page` call is lazy
(on first poll). Subsequent calls pass the cursor from the previous result. The stream ends when
`done == true`. Plugins should return at least 100 entries per page to amortise WIT round-trip
overhead (noted in ADR-0004).

**VfsError mapping** (guest WIT variant → host `VfsError`):

| `vfs-error` variant | `VfsError` |
|---|---|
| `not-found(p)` | `NotFound(VfsPath::parse(p)?)` |
| `forbidden(p)` | `Forbidden(VfsPath::parse(p)?)` |
| `already-exists(p)` | `AlreadyExists(VfsPath::parse(p)?)` |
| `unsupported(caps)` | `Unsupported(Caps::from_bits_truncate(caps.bits()))` |
| `timeout-ms(ms)` | `Timeout(Duration::from_millis(ms))` |
| `connection(msg)` | `Connection(...)` |
| `auth` | `Auth` |
| `conflict` | `Conflict` |
| `backend{code, msg, retryable}` | `Backend{code, msg, retryable}` |
| `cancelled` | `Cancelled` |
| `io(msg)` | `Io(io::Error::other(msg))` |

A wasmtime trap maps to `VfsError::Backend { code: "PluginTrap", msg: <secret-free>, retryable:
false }`. `PluginError::OutOfFuel` maps to `code: "PluginFuelExhausted"`; epoch expiry to
`code: "PluginTimeout"`. Secret content must never appear in a trap description; `PluginVfsBackend`
applies `VfsError::redacted()` before forwarding to the UI or the AI layer.

### Capability model & grants

A plugin declares what it needs in `plugin.toml`, read at install time:

```toml
[plugin]
name    = "my-cloud"
version = "0.2.1"
api     = "1.0"     # semver-compat with host's "1.x" required

[capabilities]
log         = true
network     = ["api.mycloud.example.com"]
credentials = ["my-cloud-key"]
```

At install time Cairn presents these grants for user approval; declined grants are not stored. At
load time `PluginVfsBackend::load()` builds a capability-gated `Linker` on top of the M8-2
`PluginHost`:

- `log` grant → `host::log` is linked.
- `network` grant → `host::http-fetch` is linked; the host wrapper checks the request URL against the
  stored allowlist before every dispatch.
- `credentials` grant → `host::use-credential` is linked; the host wrapper checks the handle name
  against the granted set before every call.
- `now-secs` is always linked (no I/O, no secrets).
- Any unlinked import → instantiation fails (the default-deny behaviour proven in M8-2).

Plugin-advertised `Caps` (the `backend-caps`/`caps-at` export) and plugin capability grants are
orthogonal: the former tells callers which file operations the backend supports; the latter controls
which host functions the sandbox exposes. Both are independently auditable. Grants are stored
per-plugin in `cairn-config` alongside the manifest, never in `cairn-vault`.

### Credential brokering

1. During mount setup the user associates a vault credential with the plugin mount. Cairn stores the
   opaque label string (e.g. `"my-cloud-key"`) in the mount config — never the secret value.
2. The label is passed to the plugin at mount-init time as a config key (see Unresolved questions).
3. When the plugin needs to authenticate it calls `host::use-credential("my-cloud-key",
   "sign-s3-request")`.
4. The host looks up the label in the granted set, resolves the `CredentialId`, and calls
   `Broker::resolve(Actor::Plugin("my-cloud"), id)`.
5. `Broker::resolve` records a `JournalEntry { actor: Actor::Plugin("my-cloud"), action:
   "use-credential:my-cloud-key:sign-s3-request" }` — no secret material — and returns a
   `SecretString`.
6. The host performs the brokered action internally (e.g. computes an AWS SigV4 `Authorization`
   header value) and returns the resulting string to the plugin. The raw secret never crosses the
   WIT ABI.
7. The plugin uses the returned value in a subsequent `host::http-fetch` call.

The plugin observes only labels it was configured with, and only opaque results (signed values, not
secrets). The audit trail is identical in structure to the AI layer's credential use, making
`Actor::Plugin` a first-class journaled actor.

### Resource limits & cancellation

The M8-2 `Limits` apply per plugin call, with elevated defaults for backend workloads:

| Parameter | Default (backends) | Enforcement |
|---|---|---|
| `max_memory_bytes` | 64 MiB | `StoreLimitsBuilder`; `memory.grow` returns -1 past the cap |
| `fuel` | 1 × 10⁹ | `store.set_fuel`; exhaustion → `OutOfFuel` → `Backend{code:"PluginFuelExhausted"}` |
| Epoch timeout | 30 s per call | background epoch ticker; per-store deadline set at call entry |

All three are configurable per plugin in `cairn-config`, so trusted internal plugins can be granted
higher allowances without changing the defaults for untrusted ones. All `PluginVfsBackend` calls run
via `spawn_blocking`, keeping them off the async runtime and the render path; the `!Send` `Store`
lives on a dedicated thread scoped to the instance. On a trap or limit breach, wasmtime unwinds the
guest and live `read-stream`/`write-sink` resources are invalidated; a `WriteHandle` dropped without
`finish` triggers a best-effort `abort` (protocol under Unresolved questions).

### Versioning

The WIT world is `cairn:plugin@MAJOR.MINOR.PATCH`:

- **Major** — any removal, rename, or signature change of an existing export/import, or a new
  required export. The host refuses a component whose `api` major differs:
  `PluginError::Compile("incompatible API version: host=1, plugin=N")`.
- **Minor** — purely additive: new optional host functions, new error variants, new record fields. An
  older-minor component loads unmodified; functions it does not import are simply absent from its
  `Linker` slot (default-deny already handles this).
- **Patch** — docs/toolchain only; ABI unchanged.

Breaking the world is a major event requiring a migration guide, a deprecation period (the host may
support both majors via separate `Linker` builds), and an ADR superseding ADR-0004.

## Drawbacks / deferred

- **No `copy_within`/`set_perms`/`actions_at`/`invoke` in v1.** Actions (exec, logs, port-forward)
  have bidirectional-stream/session shapes that do not map cleanly to chunked polling; candidates for
  a minor-version addition once the action surface stabilises.
- **Native WIT `stream<T>` deferred.** The `list-page` cursor loop is a conscious shim, superseded
  additively when wasmtime stabilises WIT async streams.
- **Viewer/action-only worlds deferred.** `viewer-plugin` and `action-plugin` are post-M8, as
  separate worlds in the same package.
- **Plugin registry / signing.** Discovery, signature verification, and auto-update are post-v0.1;
  the manifest and `cairn-config` storage are designed to accommodate them without a schema change.

## Rationale & alternatives

- *Byte-in/byte-out (`extism`).* Rejected in ADR-0004: the `Vfs` trait is multi-method; routing
  through `call(name, bytes) -> bytes` reimplements dispatch, typing, and streaming by hand.
- *Resources vs repeated plain calls.* Resources own their lifetime: the host can revoke them on
  timeout, and `wit-bindgen` generates typed handle wrappers on both sides with no index bookkeeping.
- *Pull (`list-page`) vs push callbacks.* Pull composes with `BoxStream` and gives natural
  back-pressure; push would require re-entering the guest during a guest call.
- *Shared `types` interface vs inlining.* `types` is used by both `host` and `backend`; one
  definition generates the Rust types once with no orphan-rule friction.
- *Persistent `Store` vs fresh per call.* The M8-2 API creates a fresh store per call (clean but
  expensive for the many `list-page` calls of a stream); `PluginVfsBackend` uses a persistent store
  with per-call fuel refills — a deliberate evolution noted under Unresolved questions.

## Security & privacy

- **Default-deny sandbox.** An ungranted import fails instantiation (proven by M8-2). No path to
  network/filesystem/vault exists except through a linked, capability-checked host function.
- **Credential opacity.** The plugin receives only a user-chosen label; the host resolves it via
  `cairn-broker`, journals `Actor::Plugin`, and returns only an ephemeral signed artifact. The raw
  secret stays in host memory, zeroized on drop.
- **Network allowlist.** `http-fetch` is URL-filtered before dispatch (HTTPS only unless
  `insecure-http` is explicitly granted); unlisted targets return an error without a network call.
- **Path sanitisation.** Paths arrive as WIT `string`; `PluginVfsBackend` runs them through
  `VfsPath::parse`, which rejects `..` traversal and control characters.
- **Secret-free errors.** `connection`/`io` payloads are expected secret-free; `PluginVfsBackend`
  applies `VfsError::redacted()` to anything forwarded to the TUI or AI layer.
- **Hard limits & no ambient WASI.** Memory/fuel/epoch limits terminate a runaway plugin below its
  own code; `wasi:cli`/`wasi:filesystem` are not imported into `backend-plugin`.

## Unresolved questions

- **Persistent `Store` protocol.** One persistent store per instance (per-call fuel refills + epoch
  reset) vs a pool — necessary for streaming, complicates cancellation. Resolve before M8-3.
- **Mount-init protocol.** How the plugin receives mount config (root URL, cred handles, options) — a
  `configure: func(config: list<tuple<string,string>>) -> result<_, string>` export is the natural
  shape; spec it before generating bindings.
- **`write-sink::abort` on timeout.** Define the exact best-effort-abort protocol (session id at
  `open-write`, reused in abort) before implementing `open_write`.
- **Fuel vs wall-time accounting.** A plugin awaiting many brokered HTTP calls spends little fuel but
  holds the store; define how fuel refills and epoch deadlines interact across many `list-page` calls.
- **Viewer/action plugin worlds.** Their WIT shapes are needed before the extension point is
  complete; deferred pending a stable action model from M6.
