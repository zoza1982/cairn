# RFC-0010: Plugin Sandbox Hardening and Brokered Host Functions

- **Status:** Draft
- **Author(s):** plugin-systems-engineer (security-engineer review required before Accepted)
- **Date:** 2026-06-30
- **Tracking item:** M8-4 (remaining: real brokered host fns), M8-5 (manifest + loader + approval UI)

## Summary

Closes the two documented security gaps in `cairn-plugin` and specifies the host-side
infrastructure needed before untrusted plugins can be loaded at runtime. Specifically:

1. **WASI-subset narrowing** — a backend plugin needs clocks, entropy, and foundational I/O types;
   it does not need sockets, filesystem preopens, blocking poll on host file descriptors, or CLI
   stdio. Replacing the single `add_to_linker_sync` call with an explicit per-interface allow-list
   removes the ambient-authority surface and most of the blocking-evasion surface.

2. **Blocking-call evasion** — confirms and analyses the documented epoch-vs-native-frame gap;
   explains how narrowing (principally dropping sockets) plus a non-blocking `MonotonicClock` stub
   close the residual threat, and states honestly what remains.

3. **Brokered `host::http-fetch`** — the host performs HTTP on the plugin's behalf (reqwest/rustls,
   per-plugin hostname allow-list, SSRF guards, size/time limits, secret redaction). The plugin never
   touches a socket.

4. **Brokered `host::use-credential`** — the plugin names a credential by opaque handle; the host
   resolves it via the broker, executes a closed-vocabulary action (sign, bearer token, etc.), and
   returns only an ephemeral artifact — the raw secret never crosses the WIT ABI.

5. **Plugin loader** — directory discovery, `plugin.toml` manifest, version/ABI compatibility, and
   install-time capability approval. Lays the groundwork for a future registry without coupling to
   one now.

## Motivation

After M8-2/3/4, the plugin host enforces:

- memory cap via `StoreLimitsBuilder`
- fuel: spins trap before exhausting the host
- epoch wall-clock backstop: the `EpochTicker` thread increments the engine every 100 ms; each
  guest op is re-armed per call

But two code-level comments mark explicit security debts:

`crates/cairn-plugin/src/component.rs`, line 143:

> SECURITY: this links the *full* WASI 0.2 surface, which includes a blocking `wasi:io/poll`.
> The epoch deadline cannot interrupt a guest blocked inside that native call.

`crates/cairn-plugin/src/epoch.rs` module doc:

> a guest blocked *inside* a host or WASI call is **not** interrupted by this mechanism. Today that
> is benign — the granted `host` imports are non-blocking (`log`/`now-secs`) or immediate
> deny-stubs — but the broadly-linked WASI surface includes a blocking `wasi:io/poll`, which a
> malicious guest could use to park the plugin thread past the deadline. Bounding that requires
> narrowing the linked WASI subset … both are gated before exposing untrusted plugins live (M8-4/M8-5).

In addition, `host::http-fetch` and `host::use-credential` are deny-stubs. Without them, plugins
cannot authenticate or make network calls — the core value proposition of a custom backend.

There is also no manifest format, no discovery mechanism, and no user-facing capability approval.
Without these, the capability model has no input: every plugin would implicitly get every grant.

This RFC authorizes implementing all four before flipping the capability model from "blocked for
untrusted use" to "safe to install from the plugin directory".

---

## Guide-level explanation

### What changes for a plugin author

A plugin is distributed as a directory (or archive) containing exactly two files:

```
my-cloud-0.2.1/
  plugin.toml          # capability manifest
  component.wasm       # the compiled cairn:plugin@1.0.0 component
```

`plugin.toml` declares what the plugin needs. Cairn shows this to the user before loading anything:

```toml
[plugin]
name        = "my-cloud"
version     = "0.2.1"
api         = "1"          # must match host WIT major; "1" → "1.x.x"
description = "My Cloud storage backend"
homepage    = "https://example.com/my-cloud"

[capabilities]
log         = true
network     = ["api.mycloud.example.com", "auth.mycloud.example.com"]
credentials = ["my-cloud-key"]

[limits]                   # optional; these are the defaults
max_memory_bytes = 67108864   # 64 MiB
fuel             = 1000000000 # 1 × 10^9 instructions
max_call_ticks   = 50         # 50 × 100 ms epoch interval ≈ 5 s per call
max_response_bytes = 8388608  # 8 MiB max http-fetch response body
```

The install flow:

```
1. User: cairn plugin install ./my-cloud-0.2.1/
2. Cairn reads plugin.toml, validates api = "1" matches host.
3. Cairn shows: "my-cloud 0.2.1 requests:
     • log: write log lines to Cairn's log
     • network: api.mycloud.example.com, auth.mycloud.example.com
     • credentials: my-cloud-key"
4. User approves/declines each grant.
5. Approved grants are stored in cairn-config (alongside the manifest).
6. On mount, Cairn builds a capability-gated Linker from the approved grants only.
```

Inside the plugin, the author uses the same WIT host interface defined in RFC-0006:

```rust
// Guest code (abridged)
let resp = host::http_fetch(HttpRequest {
    method: "GET".into(),
    url: "https://api.mycloud.example.com/v1/list".into(),
    headers: vec![("Authorization".into(), auth_value.clone())],
    body: None,
}).map_err(|e| VfsError::Connection(e))?;
```

The `auth_value` in the example above was obtained via `use-credential`:

```rust
let auth_value = host::use_credential("my-cloud-key", "bearer-token")?;
```

The plugin never holds the raw secret. The broker resolves `"my-cloud-key"` → `CredentialSecret`,
performs the credential action (`"bearer-token"` here), and returns an ephemeral token string.

---

## Reference-level explanation

### 1. WASI-subset narrowing

#### 1.1 Current state and why it matters

`PluginComponent::instantiate` currently calls:

```rust
wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
```

This links all WASI Preview 2 interfaces. The component model surfaces them as typed imports. The
full set includes:

| Interface family | Linked today | Threat (if malicious guest) |
|---|---|---|
| `wasi:io/error` | yes | none; foundational error type |
| `wasi:io/poll` | yes | **blocking-call evasion**: guest can block inside `poll` past the epoch deadline |
| `wasi:io/streams` | yes | low; requires real resources (not present in empty ctx) |
| `wasi:clocks/wall-clock` | yes | none; non-blocking |
| `wasi:clocks/monotonic-clock` | yes | **partial blocking**: `subscribe-duration` creates a timer pollable; `poll` on it sleeps in a native frame |
| `wasi:random/random` | yes | none; safe |
| `wasi:random/insecure` + `insecure-seed` | yes | none; safe |
| `wasi:filesystem/types` | yes | ambient host FS API surface (no preopens, but surface exists) |
| `wasi:filesystem/preopens` | yes | returns empty list, but the preopens API is linked |
| `wasi:sockets/*` (all 7 interfaces) | yes | **raw sockets**: a guest can attempt socket creation; even if the WasiCtx denies it, linking the API is unnecessary and widens attack surface |
| `wasi:cli/environment` | yes | could expose host env vars if WasiCtx ever gets them set by mistake |
| `wasi:cli/exit` | yes | guest can call `proc_exit`, terminating the host process |
| `wasi:cli/stdin/stdout/stderr` | yes | unnecessary; plugins use `host::log` |
| `wasi:cli/terminal-*` | yes | unnecessary |

The `wasi:cli/exit` finding is especially serious: `proc_exit(0)` or `proc_exit(1)` called from
inside a guest WASM call currently propagates as a trap in wasmtime, but it uses a distinct trap
code (`Trap::Interrupt` or a host-triggered exit); the current `map_call_err` treats it like any
other trap. If wasmtime ever changes this behaviour, a malicious plugin could terminate Cairn.

#### 1.2 The WASI allow-list

The proposed allow-list for a backend plugin (what we link and why):

| Interface | Link? | Rationale |
|---|---|---|
| `wasi:io/error@0.2.x` | **yes** | Foundational error type; referenced by streams and other interfaces. Zero attack surface. |
| `wasi:io/streams@0.2.x` | **yes** | Buffer and byte-stream primitives. Required by `wit-bindgen-rt` and by `std` I/O traits. With an empty `WasiCtx` the only accessible streams are the null-wired stdio which return empty/error immediately. |
| `wasi:io/poll@0.2.x` | **yes (stub)** | Required by `wasi:io/streams` and `wasi:clocks/monotonic-clock`. Linked via a custom non-blocking stub implementation (§1.4) rather than the default wasmtime-wasi one. |
| `wasi:clocks/wall-clock@0.2.x` | **yes** | Non-blocking timestamp; used by `std::time::SystemTime`. Already provided by `host::now-secs`, but std-built guests import it directly. |
| `wasi:clocks/monotonic-clock@0.2.x` | **yes (stub)** | `now()` is non-blocking. `subscribe-duration`/`subscribe-instant` return pollables; with the stub clock (§1.4) these are immediately ready. |
| `wasi:random/random@0.2.x` | **yes** | Entropy; needed for UUID/nonce generation in auth flows. |
| `wasi:random/insecure@0.2.x` | **yes** | Some runtimes use this for non-security randomness. |
| `wasi:random/insecure-seed@0.2.x` | **yes** | Seeding; imported by some `rand`-crate paths. |
| `wasi:filesystem/types@0.2.x` | **no** | A plugin backend reads/writes via the VFS ABI, not the host FS. |
| `wasi:filesystem/preopens@0.2.x` | **no** | No preopened directories. Removing also removes the API surface. |
| `wasi:sockets/*` (all 7) | **no** | Network access via `host::http-fetch` only. Removing the entire socket family is the primary risk-reduction. |
| `wasi:cli/environment@0.2.x` | **no** | Plugins must not inspect host env vars; they carry credentials and configuration. |
| `wasi:cli/exit@0.2.x` | **no** | Prevents a guest from calling `proc_exit`; traps normally on `panic!` instead. |
| `wasi:cli/stdin@0.2.x` | **no** | No stdin. |
| `wasi:cli/stdout@0.2.x` | **no** | Use `host::log` instead. |
| `wasi:cli/stderr@0.2.x` | **no** | Use `host::log` instead. |
| `wasi:cli/terminal-*` (5 interfaces) | **no** | Terminal access is irrelevant for a backend. |

**Recommended implementation change in `component.rs`:**

Replace the single call `wasmtime_wasi::p2::add_to_linker_sync(&mut linker)` with explicit
per-interface additions from the `wasmtime_wasi::p2::bindings` module tree. The exact Rust
spelling depends on the wasmtime version in use at implementation time (consult Context7 for
current `wasmtime-wasi` docs); the conceptual list is:

```
wasi::io::error::add_to_linker
wasi::io::streams::add_to_linker
wasi::io::poll::add_to_linker     ← stub implementation (§1.4)
wasi::clocks::wall_clock::add_to_linker
wasi::clocks::monotonic_clock::add_to_linker   ← stub implementation (§1.4)
wasi::random::random::add_to_linker
wasi::random::insecure::add_to_linker
wasi::random::insecure_seed::add_to_linker
```

All socket, filesystem, and CLI families are omitted. `WasiCtx` is built with the same empty
builder as today — no change to the context, only to the linker.

#### 1.3 Fixture breakage risk and migration plan

The committed fixture (`tests/fixtures/backend.wasm`) was built from `tests/fixture-guest/` using
`wit-bindgen-rt` only (no `std`). Inspecting its `Cargo.toml`: it depends on `wit-bindgen-rt` and
nothing else. A `wit-bindgen-rt`-based guest on `wasm32-wasip2` only imports what the WIT world
explicitly declares. The `backend-plugin` world (RFC-0006) imports only `cairn:plugin/host`;
it does not import any `wasi:*` interface. Therefore **the existing fixture imports no WASI and will
continue to instantiate after narrowing**.

However, a plugin written in Rust using `std` (full `cargo-component` build with `std` enabled for
`wasm32-wasip2`) will import WASI interfaces transitively. The missing interfaces
(`wasi:filesystem/*`, `wasi:sockets/*`, `wasi:cli/*`) will cause instantiation to fail with a
clear error naming the missing import. This is the intended default-deny behaviour.

**Plugin author guidance (to be captured in the plugin author guide at M8-5):**

- Preferred: use `wit-bindgen` or `wit-bindgen-rt` directly with `#![no_std]` or
  a minimal `std` shim. This eliminates the WASI import problem entirely.
- Acceptable: use full `std` but ensure the plugin does not exercise paths that transitively import
  socket or filesystem WASI interfaces (e.g. avoid `TcpStream`, file `open`, `env::vars`). The
  narrowed linker will catch any such import at instantiation time with a diagnostic.
- A future `cairn-plugin-sdk` (M8-6) will provide a guest-side SDK that avoids the `std` problem
  by wrapping `host::http-fetch` as the only network path and exporting a `Backend` derive macro.

**Validation plan:**

1. After narrowing: run the full test suite. The fixture's component tests must pass unchanged.
2. Add a new fixture variant `tests/fixture-guest-std/` that uses Rust `std` and imports a
   deliberately removed WASI interface (e.g. `std::net::TcpStream`). The test must assert that
   `PluginComponent::instantiate` returns `PluginError::Instantiate` with a message naming the
   missing import.
3. CI: both fixtures are committed; no WASM toolchain required.

#### 1.4 Non-blocking stub for `wasi:io/poll` and `wasi:clocks/monotonic-clock`

`wasi:io/poll::poll` is the mechanism by which a guest blocks waiting for I/O readiness. Wasmtime's
default implementation delegates to the OS (via `tokio` or thread-blocking), which will genuinely
sleep for the duration of a timer pollable. This is the blocking-evasion surface.

**Proposed stub**: implement `WasiPoll` (or the equivalent trait at the linked version of
`wasmtime-wasi`) so that `poll` inspects each `Pollable` and returns it as immediately-ready
regardless of its scheduled time. This means:

- `wasi:clocks/monotonic-clock::subscribe-duration(10_000_000_000)` (10-second sleep) returns a
  pollable; `poll([that_pollable])` returns `[0]` (ready) instantly.
- A guest that tries to `thread::sleep(Duration::from_secs(10))` internally calls this path and the
  sleep returns immediately (spurious wakeup from the guest's perspective; it should be defensive
  and re-check the condition, but most do not).
- A guest that polls an I/O stream immediately gets a "ready" (or "closed") signal, causing the
  guest to attempt the read/write, which then fails cleanly because there are no underlying
  resources.

Implement `WasiMonotonicClock` (or the equivalent trait) with:
- `now()` → the real monotonic clock value
- `resolution()` → the real resolution
- `subscribe_instant(when)` → a `Pollable` that is always immediately-ready
- `subscribe_duration(ns)` → a `Pollable` that is always immediately-ready

The stub is isolated to the plugin crate; it is not a global change to wasmtime-wasi behaviour.
This implementation adds a small amount of `unsafe`-free host Rust (trait impl) but contains no
unsafe code and carries no secret material.

### 2. Blocking-call evasion: confirmed threat model and closure

#### 2.1 The gap confirmed

The `epoch.rs` documentation is correct:

- Epoch interruption fires at back-edges and function entries in **guest WebAssembly bytecode**.
- When a guest makes a host call (any `import host` or `import wasi` function), control transfers
  to a native Rust frame. The epoch counter is not checked in native frames.
- Wasmtime's design is correct: epoch is a cooperative mechanism for guest bytecode, not an OS
  preemption signal.

**Concrete attack vectors without this RFC's mitigations:**

| Attack | Via | Effect |
|---|---|---|
| Block on timer | `monotonic-clock::subscribe-duration(∞)` + `io/poll::poll` | Plugin thread parked indefinitely inside native `poll` |
| Block on socket | `wasi:sockets/tcp-create-socket` + blocking `receive` | Plugin thread parked indefinitely inside native socket recv |
| CPU spin in guest | `loop { … }` | Fuel → `OutOfFuel`; epoch traps → `Timeout` (this works correctly already) |
| Allocate unbounded memory | `memory.grow` | `StoreLimitsBuilder` caps → `memory.grow` returns -1 (works correctly) |

After this RFC:

| Attack | Closed by | Residual |
|---|---|---|
| Block on timer | Non-blocking poll stub (§1.4) | None: subscribe + poll returns immediately |
| Block on socket | Remove `wasi:sockets/*` from linker | None: instantiation fails if socket is imported |
| CPU spin | Fuel + epoch (existing) | None: already handled |
| Memory exhaustion | `StoreLimitsBuilder` (existing) | None: already handled |

#### 2.2 Residual threat model

After applying the narrowing and the poll stub:

- **A guest calling `host::http-fetch` or `host::use-credential`** will be inside a host-side
  Rust frame for the duration of the HTTP call. These calls have their own timeouts (§3.3) but are
  invisible to the epoch. An HTTP call that genuinely takes 30 seconds (e.g., a slow upstream
  server) will park the plugin thread for 30 seconds.

  Mitigation: `reqwest` connect and read timeouts are set to values ≤ `max_call_ticks ×
  epoch_interval` (the call-level ceiling). The plugin-thread architecture means a parked http-fetch
  blocks only that plugin's dedicated thread, not the runtime, not the TUI render path. This is
  acceptable: the render path never blocks (the plugin thread is separate), and the plugin will
  eventually return or time out.

- **A future host function added without timeouts** could re-introduce the gap. This RFC's design
  rule: every function linked under `host::` that performs any I/O or system call must carry an
  explicit OS-level timeout (not epoch-based) that is ≤ the configured per-call epoch ceiling. This
  is enforced by policy (code review gate) not by a compile-time check.

- **A malicious guest that never completes a VFS call** (returns from a host call but then
  re-enters a custom spin loop): still bounded by fuel + epoch, as today. The analysis is correct
  for guest-side computation.

**Conclusion**: after narrowing + the poll stub, the only remaining blocking surface is within
deliberately designed host functions (http-fetch, use-credential) that carry their own timeouts.
The epoch gap is closed for WASI. This meets the bar for loading plugins from untrusted authors.

### 3. Brokered `host::http-fetch`

#### 3.1 HTTP client choice

Use `reqwest` with `rustls` (the TLS stack already chosen throughout the project; no OpenSSL
dependency). Reuse the `reqwest` crate that will be a dev/test dependency for the integration
CI — pulling it behind the `network` feature gate in `cairn-plugin` is the pattern, matching
ADR-0006 (feature-gated backends).

**Feature gate:** `cairn-plugin` grows a non-default `plugin-network` Cargo feature. Without it,
`http-fetch` remains a deny-stub (the current behaviour). Integration tests for http-fetch are
gated behind `CAIRN_IT_PLUGIN_NETWORK` in the CI matrix, following the existing emulator-test
pattern. Default `cargo test` stays hermetic.

#### 3.2 Allow-list enforcement (SSRF defence)

Before the `reqwest` client is invoked, the host must validate the request URL against the plugin's
approved `network` grant. The grant is a list of hostnames (not full URLs); the host matches the
parsed `authority` of the request URL (scheme-stripped).

Validation steps, in order — **all must pass** for the request to be dispatched:

1. **URL parse**: reject malformed URLs.

2. **Scheme check**: only `https://` is permitted by default. A plugin that needs `http://` must
   declare `[network] allow-http = true` in its manifest and the user must grant it explicitly at
   install time. Note: `file://`, `data://`, `ftp://`, and other schemes are always rejected,
   unconditionally.

3. **Hostname allow-list**: the URL's hostname (after stripping brackets from IPv6 literals) must
   exactly match one of the plugin's granted hostnames or be a subdomain of one. The match uses
   the public-suffix list to prevent subdomain abuse (e.g. `evil.example.com` must not match a
   grant for `example.com.evil.net`). Wildcard entries (`*.example.com`) are not permitted in
   v1 grants; they may be added in a minor-version expansion.

4. **IP address SSRF guard**: if the URL resolves (via a pre-flight DNS check with a short timeout)
   to a private or link-local address, the request is rejected with a clear error. The blocked
   ranges are: loopback (127.0.0.0/8, ::1), private (10.0.0.0/8, 172.16.0.0/12,
   192.168.0.0/16, fc00::/7), link-local (169.254.0.0/16, fe80::/10), and
   documentation/reserved (100.64.0.0/10, 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24,
   240.0.0.0/4). Note: DNS resolution is inherently TOCTOU; this guard is defense-in-depth
   alongside the hostname allow-list, not a sole guarantee.

5. **Redirect handling**: follow up to 5 redirects. Re-apply the allow-list check at each redirect
   target URL. A redirect to an ungranted hostname or a private IP is rejected (no cross-origin
   redirect leakage via SSRF chaining).

6. **Method allow-list**: `GET`, `HEAD`, `POST`, `PUT`, `DELETE`, `PATCH`, `OPTIONS` are permitted.
   `CONNECT` and `TRACE` are rejected unconditionally.

#### 3.3 Size and time limits

| Limit | Default | Source | Config key |
|---|---|---|---|
| Connect timeout | 5 s | `reqwest::ClientBuilder::connect_timeout` | `[limits] http_connect_timeout_secs` |
| Request timeout (per call) | 30 s | `reqwest::ClientBuilder::timeout` | `[limits] http_request_timeout_secs` |
| Max response body bytes | 8 MiB | streaming read + byte counter | `[limits] max_response_bytes` |
| Max redirect hops | 5 | built-in | — |
| Max request header size | 32 KiB | input validation before dispatch | — |
| Max request body size | 32 MiB | input validation before dispatch | `[limits] max_request_body_bytes` |

Both timeouts must be ≤ the per-call epoch ceiling (`max_call_ticks × 100 ms`, default 5 s).
If a plugin configures `max_call_ticks` higher than the default, the http timeouts must be
configured proportionally (documented in the plugin author guide).

The response body is streamed from `reqwest` into a `Vec<u8>` with a running byte count. If the
count exceeds `max_response_bytes`, the connection is dropped and the host returns an error to the
guest without materializing the full body in memory.

#### 3.4 Redaction and logging

The host logs `[plugin::http] method={} host={}` at DEBUG level before dispatch. It does NOT log:
- Full URLs (may contain tokens in the query string)
- Request bodies (may contain credentials)
- Response bodies (may contain sensitive data)
- `Authorization`, `Cookie`, `Set-Cookie`, `X-Api-Key`, `X-Auth-Token`, or `Proxy-Authorization`
  headers (redacted to `[REDACTED]` in any log output)

`Set-Cookie` response headers are stripped from the `HttpResponse` returned to the guest. The
plugin must not accumulate cookie state that could enable session fixation or cross-origin leakage.

Error strings returned to the guest via `Err(String)` are sanitised (control characters stripped,
length-capped at 512 bytes) matching the pattern in `bridge::sanitize_msg`.

#### 3.5 `CompState` extension

The real http-fetch implementation adds a `reqwest::Client` to `CompState`:

```rust
struct CompState {
    limits:  StoreLimits,
    wasi:    WasiCtx,
    table:   ResourceTable,
    // Added:
    network_grants: Arc<Vec<String>>,   // approved hostnames; empty = deny all
    http_client:    Option<Arc<reqwest::Client>>,  // None when plugin-network feature off
    http_limits:    HttpLimits,         // from Limits (timeouts, max_response_bytes)
}
```

The `http-fetch` host function (in `impl cairn::plugin::host::Host for CompState`) checks
`http_client.is_some()` (feature gate), then validates the URL against `network_grants`, then
dispatches. It must call `reqwest` synchronously (no `.await`) because `PluginComponent` runs in a
synchronous wasmtime call context on a dedicated thread; use `tokio::runtime::Handle::current()` +
`block_on` — the same pattern used by `wasmtime-wasi`'s sync linker.

### 4. Brokered `host::use-credential`

#### 4.1 The secret-never-crosses-ABI invariant

The `use-credential(handle: string, action: string) -> result<string, string>` host function must
uphold the invariant from RFC-0006 and RFC-0008: the raw `CredentialSecret` never serialises into
or out of the WIT ABI. The returned `string` is always an ephemeral *artifact* derived from the
secret (a signed header value, a short-lived token) — never the secret itself.

This is enforced the same way as in the AI layer: by a compile-time dep-closure property. The
`cairn-plugin` crate depends on `cairn-broker-api` (which provides `CredentialDirectory` —
secret-free) but not on `cairn-broker` (which provides `Broker::resolve` — secret-returning). The
host function implementation lives in a new shim that does have a `cairn-broker` dependency, wired
in at the binary edge (as `Box<dyn CredentialBroker>`). This keeps `cairn-plugin` itself
vault-free, matching the existing dep-closure CI test in RFC-0008.

**`CredentialBroker` trait (proposed, in `cairn-broker-api`):**

```rust
pub trait CredentialBroker: Send + Sync {
    /// Perform a brokered credential action. The actor is the plugin's canonical name.
    /// Returns an ephemeral, secret-free artifact string, never the raw credential.
    /// The raw secret is resolved, used, and dropped within this call.
    fn use_credential(
        &self,
        actor: &str,
        handle: &str,
        action: &CredentialAction,
    ) -> Result<String, CredentialBrokerError>;
}
```

The concrete `BrokerCredentialAdapter` (in `cairn-broker`) implements this trait by calling
`Broker::resolve(Actor::Plugin(actor), id)` internally, performing the action, and returning
the artifact. `cairn-plugin` receives `Arc<dyn CredentialBroker>` — no `cairn-vault` in its dep
graph.

#### 4.2 Action vocabulary (closed set, v1)

Actions are identified by a structured string. The host validates the action string before
dispatching; unknown actions return `Err("unknown action")` to the guest without attempting
credential resolution.

| Action | Credential types | Returns | Description |
|---|---|---|---|
| `"bearer-token"` | AWS (STS/SSO), GCP (ADC/SA), Azure (AAD), OAuth2 | Short-lived access token | Resolved via `TokenCache`; refreshed if within 60 s of expiry. A plugin can call this repeatedly; the cache ensures the underlying credential is used minimally. |
| `"basic-auth-header"` | Any with `(username, password)` | `base64(user:pass)` | HTTP Basic auth value (without the `Basic ` prefix). The credential's `password` field is used for the encoding; the raw bytes never leave the host. |
| `"sign-aws-request:{method}:{url}:{body-sha256}"` | `CredentialSecret::Aws(AccessKey)` | `Authorization` header value | Computes AWS SigV4 over the provided method, URL, and body hash. The signing key is derived from the secret access key inside the host; only the `Authorization` value is returned. |
| `"gcp-access-token:{scope}"` | `CredentialSecret::Gcp` | OAuth2 access token | Calls GCP token endpoint with the service-account key; returns a short-lived access token. Scope must be in the plugin's granted scope list (TBD: scopes sub-grant). |
| `"azure-access-token:{resource}"` | `CredentialSecret::Azure(Aad{..})` | Azure AD access token | Acquires token for the specified resource; cached in `TokenCache`. |

Adding a new action is a **minor-version expansion** of RFC-0010 (tracked with a `BREAKING CHANGE:`
footer if it alters an existing action's semantics). The guest validates the returned artifact
format but must not assume secrets.

#### 4.3 Grant and journal

Before dispatching, the host checks:
1. The `handle` string is in the plugin's approved `credentials` grant. If not, returns `Err` without
   calling the broker.
2. The `action` string is in the closed vocabulary. If not, returns `Err` without touching the vault.

The broker call journals (following RFC-0008's `Actor::Plugin`):

```
JournalEntry {
    actor:  Actor::Plugin("my-cloud"),
    action: "use-credential:my-cloud-key:bearer-token",
    ts:     <now>,
    // No secret material in the journal.
}
```

The journal entry is visible in the credential-use audit log (a future TUI feature; the data
structure is already defined by RFC-0008).

#### 4.4 `CompState` extension

```rust
struct CompState {
    // Existing:
    limits: StoreLimits, wasi: WasiCtx, table: ResourceTable,
    // From §3.5:
    network_grants: Arc<Vec<String>>,
    http_client:    Option<Arc<reqwest::Client>>,
    http_limits:    HttpLimits,
    // Added:
    credential_grants: Arc<Vec<String>>,     // approved credential handles
    credential_broker: Option<Arc<dyn CredentialBroker>>,  // None = deny-stub
    plugin_name:       String,               // for Actor::Plugin journal entry
}
```

### 5. Plugin loader

#### 5.1 Discovery

Plugins are loaded from a configurable directory. The default is
`${cairn_data_dir}/plugins/` (e.g. `~/.local/share/cairn/plugins/` on Linux,
`~/Library/Application Support/Cairn/plugins/` on macOS,
`%APPDATA%\Cairn\plugins\` on Windows). Configurable via `[plugins] dir = "..."` in `cairn.toml`.

Each plugin is a subdirectory of the form `<name>-<version>/` or a future `.tar.xz` archive
(archives deferred; directory layout is the v1 format). A valid plugin directory contains:
- `plugin.toml` (required)
- `component.wasm` (required)
- Any other files (ignored; plugins may bundle static assets in their WASM component)

Discovery is on-demand (at mount time for backend plugins) and at startup for plugins declared in
connection profiles. A future background re-scan is post-M8.

#### 5.2 Manifest schema (`plugin.toml`)

```toml
[plugin]
name        = "my-cloud"          # machine identifier; only [a-z0-9-_], max 64 chars
version     = "0.2.1"             # SemVer
api         = "1"                 # WIT major version; must match host's MAJOR
description = "..."               # user-visible; max 256 chars; not a secret
homepage    = "https://..."       # optional
authors     = ["Alice <a@b.com>"] # optional

[capabilities]
log         = true                # may write to cairn's log
network     = [                   # approved request hostnames (HTTPS by default)
  "api.mycloud.example.com",
  "auth.mycloud.example.com",
]
credentials = ["my-cloud-key"]    # credential label names from the vault

[network]                         # optional; adjust the http-fetch defaults
allow-http        = false         # true = also permit http:// (requires user grant)
max_response_bytes = 8388608      # 8 MiB
http_connect_timeout_secs = 5
http_request_timeout_secs = 30

[limits]                          # optional; all have safe defaults
max_memory_bytes  = 67108864      # 64 MiB
fuel              = 1000000000    # 1 × 10^9
max_call_ticks    = 50            # 50 × 100 ms ≈ 5 s per call
max_stream_bytes  = 4294967296    # 4 GiB per read stream
```

Manifest parsing: `serde` + `toml` (already dependencies elsewhere in the workspace). Unknown
fields are **rejected** (deny-unknown) to prevent a future version's field from silently being
granted under a host that doesn't understand it. Version skew is surfaced clearly:
`PluginError::Compile("unknown manifest key: [network].allow-http-proxy")`.

#### 5.3 ABI version compatibility

The `api = "1"` field is the WIT world major. The host extracts this from the parsed manifest
before attempting to instantiate the component.

- **Major mismatch** (`api = "2"` and host supports `"1"`): `PluginError::Compile` with a clear
  message. The component bytes are not loaded.
- **Exact major match**: proceed. The WIT world is `cairn:plugin@1.x.x`; minor differences
  (new optional imports on the host side) are handled by the default-deny linker — an older plugin
  simply doesn't import the new optional function.
- **Future**: a host that supports both `1.x` and `2.x` would maintain two separate `Linker`
  configurations and dispatch on `api`. This is post-v1.

This is consistent with RFC-0006's versioning section.

#### 5.4 Capability approval storage

Approved capabilities are stored in `cairn-config` (not `cairn-vault`) because they are not
secret — they are the user's intent record. The storage key is `plugins.<name>.<version>.grants`:

```toml
# cairn.toml (managed section; user should not hand-edit)
[plugins."my-cloud@0.2.1".grants]
log         = true
network     = ["api.mycloud.example.com"]  # may be a subset of what was requested
credentials = ["my-cloud-key"]
```

Declined grants are simply absent. At load time, the loader reads the grants for the installed
version. If a plugin's manifest requests a capability not in the stored grants, the loader treats
it as a warning and proceeds without that capability (the import is absent from the linker, causing
instantiation to fail with `PluginError::Instantiate` naming the missing import, which is surfaced
to the user as "plugin requires the `network` grant — re-run install to approve").

#### 5.5 Install flow

Designed for the future `cairn plugin install <path>` CLI command (or a TUI equivalent in M8-5):

1. Parse `plugin.toml` and validate schema.
2. Check `api` major against host.
3. Display capabilities to the user (a clear, non-technical summary).
4. Accept/decline each capability group (network, credentials, log). The user may narrow the
   network grant (e.g. remove one of the two hostnames) but not expand it.
5. Write approved grants to `cairn-config`.
6. Copy the plugin directory to `${cairn_data_dir}/plugins/<name>-<version>/`.

Revocation: `cairn plugin revoke my-cloud` removes the grants from config (the plugin directory is
retained until `cairn plugin remove my-cloud` is also run). On next load, missing grants = deny.

#### 5.6 Load flow (at mount time)

```
PluginLoader::load(name, version, linker_config) -> Result<PluginVfsBackend, PluginError>

1. Locate <name>-<version>/ in the plugins directory.
2. Parse plugin.toml; re-validate api major.
3. Read grants from cairn-config.
4. Instantiate PluginComponent with a capability-gated Linker:
     - always link: wasi subset (§1.2), host::log, host::now-secs
     - if grants.log:         link host::log (already always linked; log grant controls rate-limiting in the future)
     - if grants.network:     link real host::http-fetch with the grant's hostname list
     - if grants.credentials: link real host::use-credential with the grant's handle list
5. Call component.scheme() and component.caps() to read the plugin's self-declared identity.
6. Construct PluginVfsBackend(component, limits, connection_id).
```

---

## Drawbacks

- **WASI narrowing adds a new "denied import" failure mode** for std-built plugins. Authors who
  build a Rust plugin with full `std` and import `std::net::TcpStream` will see an instantiation
  error at load time, not at build time. The error message from wasmtime is clear (`missing import
  wasi:sockets/tcp@0.2.0:tcp::create-tcp-socket`), but it is a runtime error. The plugin author
  guide must document this clearly. A future `cairn plugin validate ./my-plugin/` subcommand could
  catch it pre-install.

- **`reqwest` adds build weight** behind the `plugin-network` feature. Already the pattern for
  cloud SDKs in this project; consistent.

- **The `CredentialBroker` trait** adds one more abstraction layer in the broker chain. The
  alternative (having the plugin crate directly depend on `cairn-broker`) would violate the
  dep-closure isolation guarantee. The cost is worth it.

---

## Rationale and alternatives

### WASI narrowing approach

**Alternative considered: use a custom `WasiView` that panics on every interface**. Rejected:
`add_to_linker_sync` adds each interface against a specific trait implementation; providing a
panicking implementation would compile but cause runtime panics (not clean errors) and would require
maintaining a full implementation of every WASI trait.

**Alternative considered: remove `wasi:io/poll` entirely**. Partially rejected for v1: while the
fixture doesn't need it, `std`-built guests do. Removing it makes the "acceptable" std-built path
described in §1.3 impossible. The non-blocking stub achieves the same security property (poll never
blocks) without breaking the std-built path.

**Alternative considered: async WASM linker**. Wasmtime's async linker would allow yielding to the
executor during host calls, enabling epoch-based cancellation even of native frames. Rejected for
now: the plugin bridge intentionally uses the sync linker on a dedicated thread (the `Store` is
`!Sync`; the bridge is explicitly designed for this); switching to async would require redesigning
the bridge and the handle lifetime model. Revisit post-v1 when WIT async streams stabilise.

### HTTP client

**Alternative: `ureq`** (lighter, no tokio dependency). Rejected: the project already uses
`reqwest`/`rustls` across cloud SDKs and the AI HTTP provider; a second HTTP client adds
dep-tree bloat and two sets of TLS configuration to audit.

**Alternative: expose a raw socket or TUN/TAP interface**. Rejected categorically: the capability
model's value is that a plugin cannot make arbitrary network calls. `http-fetch` is the ONLY
brokered network access point.

### `use-credential` action vocabulary

**Alternative: expose a raw secret (bearer token only)**. Rejected: returning even a short-lived
token constitutes the secret crossing the ABI boundary. For token-based credentials (OAuth2,
STS, AAD), the token is the secret. The action vocabulary abstracts over this by computing the
artifact the plugin actually needs (an `Authorization` header value), not the secret that produces
it.

**Alternative: plugin-specific action strings (freeform)**. Rejected: a freeform action string
allows a plugin to probe for secret types by exhaustive enumeration (`"get-raw-value"`,
`"dump-secret"`, etc.). The closed vocabulary ensures the host is the only interpreter of actions
and cannot accidentally return raw secret material.

### Credential broker trait location

The `CredentialBroker` trait in `cairn-broker-api` (rather than `cairn-broker`) is load-bearing:
`cairn-plugin` can receive `Arc<dyn CredentialBroker>` from the binary without having `cairn-vault`
in its dep graph. This is the same isolation property as `CredentialDirectory` (RFC-0008 §3).

---

## Security and privacy considerations

### Threat model

| Threat | Control |
|---|---|
| Malicious plugin exfiltrates host FS | `wasi:filesystem` not linked (§1.2) |
| Malicious plugin opens raw sockets | `wasi:sockets` not linked (§1.2) |
| Malicious plugin blocks plugin thread indefinitely | Poll stub + http/credential timeouts (§1.4, §3.3) |
| Malicious plugin reads host env vars | `wasi:cli/environment` not linked (§1.2) |
| Malicious plugin calls `proc_exit` | `wasi:cli/exit` not linked (§1.2) |
| Malicious plugin exhausts memory | `StoreLimitsBuilder` memory cap (existing M8-2) |
| Malicious plugin spins forever | Fuel + epoch (existing M8-2/4) |
| Plugin makes SSRF request | Hostname allow-list + private-IP guard + scheme check (§3.2) |
| Plugin exfiltrates secret via http-fetch URL | Hostname allow-list (§3.2); URL params are logged at DEBUG only in non-secret contexts |
| Plugin extracts raw credential via use-credential | Action vocabulary returns only ephemeral artifacts; raw `CredentialSecret` stays in the broker (§4.1) |
| Plugin learns which credentials exist | Grant check: plugin only knows the handle labels in its own grant (§4.3) |
| Plugin flood-calls http-fetch | `max_call_ticks` per-call ceiling; rate-limiting is a future enhancement |
| Installing a plugin with malicious `plugin.toml` | Schema deny-unknown-fields; capability approval is user-driven; no code executes until approval |
| Version skew: plugin declares `api = "99"` | Major mismatch rejected before instantiation (§5.3) |
| Plugin leaks secret via `host::log` | `log` messages are sanitised (control chars stripped, length-capped); they arrive in the tracing subscriber, not in the AI context or the TUI |
| Terminal injection via plugin-generated entry names | `bridge::sanitize_msg` + `valid_leaf_name` (existing M8-3) |

### Secret flow

The secret flow through a `use-credential` call:

```
plugin guest
  → host::use-credential("my-cloud-key", "bearer-token")
     [WIT boundary]
  → host: check handle in credential_grants ✓
  → host: check action in vocabulary ✓
  → broker: resolve(Actor::Plugin("my-cloud"), CredentialId("my-cloud-key"))
             → vault: unseal CredentialSecret::Aws(AccessKey { id, secret, token })
                      ↓ (stays in vault/broker; never serialised)
  → host: TokenCache::get_or_refresh(id, "sts:sts.amazonaws.com") → ephemeral_token (String)
  → host: drop CredentialSecret (zeroized on drop via SecretBox)
  → host: journal { actor: Plugin("my-cloud"), action: "use-credential:my-cloud-key:bearer-token" }
  → return Ok(ephemeral_token) → WIT boundary → guest receives: "eyJhbGci..."
```

The `ephemeral_token` is a bearer token. It is short-lived (minutes). The plugin uses it in a
subsequent `http-fetch` in the `Authorization` header. The raw `AccessKey.secret` never crosses
the WIT boundary. The journal carries only ids, not secrets — matching the pattern established in
RFC-0008 §3.

### Dep-closure test extension

The existing CI test (`cargo metadata` dep-closure check, RFC-0008 §3) must be updated to also
assert `cairn-broker` is not in the dependency closure of `cairn-plugin`. This is the same
property, now explicitly tested for `cairn-plugin` as well as `cairn-ai`.

---

## Unresolved questions

1. **`wasmtime-wasi` per-interface linker API at the version pinned in `Cargo.toml`.** The exact
   Rust symbols for adding individual WASI interfaces depend on the wasmtime version. The
   implementer must consult the current `wasmtime-wasi` docs (via Context7) to identify the
   correct per-interface `add_to_linker` symbols or alternative mechanism. The API has changed
   across wasmtime 22–28.

2. **Non-blocking `WasiMonotonicClock` trait bound.** The wasmtime-wasi trait for the monotonic
   clock may require a different bound in the component model `p2` path vs the old `preview2` path.
   Confirm the trait name before implementing the stub.

3. **`CredentialBroker` placement.** This RFC proposes adding `CredentialBroker` to
   `cairn-broker-api`. Confirm with `security-engineer` and `software-architect` that this doesn't
   introduce a transitivity issue (does `CredentialBroker`'s method signature need to name
   `CredentialSecret`? If so, it cannot be in `cairn-broker-api`). The intent is for the return
   type to be `String` (the ephemeral artifact), so `CredentialSecret` need not appear in the
   trait's public API. Resolve before PR-B1.

4. **GCP scope sub-grants.** The `"gcp-access-token:{scope}"` action passes a scope string from
   the guest. Should the scope string also appear in the manifest's capability declaration (a
   `scopes = ["storage.readonly"]` sub-field under `credentials`)? This reduces what a plugin can
   request even if it holds a credential. Defer to M8-5 if it complicates the v1 manifest.

5. **Credential handle → `CredentialId` mapping.** The `use-credential` handle is a user-chosen
   label string. The host must map it to a `CredentialId` in the vault. The mapping is stored at
   mount-init time (the user associates the label with a vault credential during the connection
   setup flow). The exact protocol for passing this at `configure()` call time (RFC-0006
   Unresolved questions §2) is still open — resolve before PR-B2.

6. **http-fetch in the epoch gap.** The HTTP call runs in a native frame (not in guest WASM), so
   the epoch deadline cannot interrupt it. The request-level timeout (§3.3) bounds it, but if the
   plugin thread is parked inside a slow http-fetch for 30 s, the calling async side (the
   `PluginVfsBackend` channel sender) will timeout only after the `oneshot` channel also drops.
   Define the cancellation protocol: should `PluginVfsBackend` close the `mpsc::Sender` (killing
   the thread) on a timeout, or should it send a `CancelHttpFetch` message? Decide before PR-B1.

---

## Crate, dependency, and feature impact

| Crate | Change |
|---|---|
| `cairn-plugin` | Narrow WASI linker; add `plugin-network` feature; extend `CompState`; add non-blocking stub impls; wire `CredentialBroker` behind the `plugin-network` feature |
| `cairn-broker-api` | Add `CredentialBroker` trait + `CredentialBrokerError` |
| `cairn-broker` | Add `BrokerCredentialAdapter: CredentialBroker`; update dep-closure CI test to include `cairn-plugin` |
| `cairn` (binary) | Wire `BrokerCredentialAdapter` into `PluginLoader` at startup; add `PluginLoader` struct |
| `cairn-config` | Add `[plugins]` schema section (grants storage) |
| New type: `PluginLoader` | Lives in `cairn-plugin`; orchestrates the load flow (§5.6) |
| New type: `PluginManifest` | Parsed `plugin.toml` model; lives in `cairn-plugin` |

**New dependencies (all behind features):**

| Dep | Feature | Purpose |
|---|---|---|
| `reqwest` (rustls) | `plugin-network` | HTTP client for http-fetch |
| `publicsuffix` or `addr` | `plugin-network` | Public-suffix list for hostname matching |
| `ipnet` | `plugin-network` | CIDR matching for private-IP SSRF guard |

---

## Phased PR plan

The narrowing (§1) is a prerequisite security fix; the brokered functions (§3/§4) build on it.
The loader (§5) is independent of the brokered functions but requires §1.

### Phase A — WASI narrowing (prerequisite; gates live untrusted plugins)

**PR-A (M8-4 remainder, security-review required):**

1. Replace `wasmtime_wasi::p2::add_to_linker_sync` with the explicit per-interface allow-list (§1.2).
2. Add the non-blocking `WasiMonotonicClock` stub (§1.4).
3. Update `CompState`'s `wasi` field if the stub requires a different context builder.
4. Add `tests/fixture-guest-std/` with a deliberate socket import; assert instantiation fails.
5. All existing tests pass. `cargo test --all-features` green.
6. Update M8-4 status in `IMPLEMENTATION_PLAN.md`: WASI narrowing ✅.

### Phase B — Brokered host functions (enable real plugin network + credential use)

**PR-B1 (http-fetch; env-gated integration test):**

1. Add `plugin-network` Cargo feature to `cairn-plugin`.
2. Implement real `host::http-fetch` in `CompState` (reqwest/rustls, allow-list, SSRF, limits, redaction).
3. Hermetic unit tests: URL validation (blocked schemes, private IPs, unlisted hosts), response body truncation.
4. Integration test behind `CAIRN_IT_PLUGIN_NETWORK` + a local echo server.
5. Update `IMPLEMENTATION_PLAN.md`.

**PR-B2 (use-credential; broker wiring):**

1. Add `CredentialBroker` trait to `cairn-broker-api`; add `BrokerCredentialAdapter` to `cairn-broker`.
2. Implement `host::use-credential` (closed action vocabulary: `bearer-token`, `basic-auth-header`, `sign-aws-request`).
3. Extend dep-closure CI test: `cairn-broker` ∉ `cairn-plugin` dep closure.
4. Hermetic tests with a `MockCredentialBroker` (action dispatch, journal, unknown-handle/action rejection, denied-grant rejection).
5. Update `IMPLEMENTATION_PLAN.md`.

### Phase C — Plugin loader and approval UI (M8-5)

**PR-C1 (loader):**

1. `PluginLoader`: directory scan, `PluginManifest` parsing (serde + toml, deny-unknown-fields), version check, grants-from-config, capability-gated `PluginComponent::instantiate`.
2. Update `cairn-config` schema with `[plugins]` grants storage.
3. Tests: valid manifest round-trip, unknown-field rejection, major-mismatch error, missing-grant instantiation error.

**PR-C2 (approval UI — TUI overlay):**

1. `Overlay::PluginApproval { manifest, pending_grants }`: shows plugin name/version/description/requested capabilities.
2. User navigates to approve/decline each capability group; submits → grants written to config.
3. Reducer + render + effect tests (approval, decline, partial grant).

**PR-C3 (install subcommand; deferred to M8-5 if TUI-first):**

1. `cairn plugin install <path>` CLI subcommand or TUI install-from-path flow.
2. Integrates PR-C1 + PR-C2 into a single user-facing action.
