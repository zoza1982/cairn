# ADR-0007: Lazy Connection Opening (RFC-0011 P2)

- **Status:** Accepted
- **Date:** 2026-06-30
- **Deciders:** zoran.vukmirica@gmail.com (human maintainer), rust-staff-engineer (AI)

## Context

RFC-0011 P1 introduced the `ConnectionCoordinator` and `DeferredConnection` mechanism: at startup
the coordinator eagerly opened every connection it could. Credential-bearing connections whose vault
was locked were placed in a "deferred" list and retried in bulk when the vault was unlocked. This
approach had several problems:

1. **Startup latency.** Opening every remote profile at startup (Docker socket, remote SSH, S3
   bucket) could delay the UI by several seconds on a cold start with many profiles.
2. **Bulk retry on unlock.** When the vault unlocked, the P1 code retried all deferred connections
   at once. A single slow or failed connection could block others, and the UI showed no progress.
3. **No switcher visibility.** Vault-locked profiles were invisible to the user until the vault
   was unlocked; there was no way to see "these connections need the vault."
4. **Re-enumeration unsafety.** Id assignment was sequential (base + i), so a config reload would
   re-assign ids, silently breaking panes that were browsing an already-open connection.

## Decision

We adopt lazy, on-select connection opening for all `Profile` targets (P2):

1. **`LocalRoot` targets only are eagerly mounted at startup.** Built-in roots (`/`, `$HOME`) and
   `scheme = "local"` config profiles are opened immediately via `LocalVfs`, exactly as before.

2. **`Profile` targets appear in the switcher immediately with a status of `NeedsOpen` or
   `NeedsVault`** — but are not opened. The coordinator records them in the descriptor side-map.

3. **Opening happens in the effect runner on user selection.** When the user selects a connection
   in the switcher the reducer emits `AppEffect::OpenConnection { conn }`. The effect runner looks
   up the `ConnectionDescriptor`, opens the backend (async, spawned), and sends back
   `AppEvent::ConnectionOpened { conn, result }`. The reducer navigates the pane on success or
   marks the connection `Unreachable` on failure.

4. **Vault unlock reconciliation: flip all, auto-open only the trigger.** When the vault unlocks,
   every `NeedsVault` entry in the switcher flips to `NeedsOpen`. Only the connection that
   triggered the vault-unlock overlay (recorded in `Overlay::VaultUnlock::pending_conn`) is
   automatically opened; the rest remain `NeedsOpen` for the user to select explicitly. This is
   more predictable than "open everything at once" and avoids surprising the user if a large number
   of profiles become available simultaneously.

5. **`DeferredConnection` is retired (always empty in P2).** The `VaultContext::deferred` field is
   removed; `run_vault_unlock_effect` no longer iterates over deferred profiles. The
   `DeferredConnection` struct is retained in `coordinator.rs` for API stability but annotated
   dead and expected to be removed in P3/P4 when the P1-compatible code path is fully gone.

6. **Stable id reuse on re-enumeration.** `ConnectionCoordinator::run` now accepts a
   `prior_descriptors: &HashMap<ConnectionId, ConnectionDescriptor>` map. For each descriptor
   whose `ConnectionKey` is already in the prior map, the existing `ConnectionId` is reused rather
   than minting a fresh sequential id. Fresh ids are minted for genuinely new keys from a counter
   that skips all claimed ids. Pass `&HashMap::new()` at startup.

## Consequences

### Positive

- **Instant startup.** No network I/O at startup; the UI becomes interactive immediately regardless
  of how many remote profiles are configured.
- **All connections visible.** `NeedsVault` entries appear in the switcher from the start, giving
  the user a complete view of what is configured.
- **Granular feedback.** Each `AppEvent::ConnectionOpened` carries its own `Result`, so a slow or
  failed open is isolated and surfaced without blocking other connections.
- **Stable ids.** Key-based id reuse prevents a config reload from repointing panes that are
  browsing a live connection.
- **Simpler vault-unlock effect.** `run_vault_unlock_effect` now only unlocks the broker and
  returns `Result<(), String>` — no registry mutations, no connection choices to return.

### Negative / trade-offs

- **First-select latency.** The first time the user selects a remote connection they see a brief
  "Opening …" status and wait for the backend to connect. This is expected and better than blocking
  startup.
- **`NeedsOpen` profiles don't self-heal.** If a `NeedsOpen` connection is selected and fails, it
  becomes `Unreachable`. Re-opening requires re-selecting it (or a future "reconnect" action in P4).
- **`DeferredConnection` API surface remains.** The struct and return type are kept for now to
  avoid a flag day; they will be fully removed in P3.

### Neutral

- The `Overlay::VaultUnlock` struct gains `pending_conn: Option<ConnectionId>` for the
  auto-open-on-unlock path; existing code uses `..` patterns to remain forward-compatible.
- The `AppState` gains `pending_conn_open: [Option<ConnectionId>; 2]` (indexed by
  `Side::index()`, Left→0 / Right→1) to track which connection is awaiting an async open per
  pane. Using per-side slots eliminates the single-slot aliasing bug present in the initial
  `Option<(ConnectionId, Side)>` design: with one slot, selecting `NeedsOpen A` on Left then
  `B` on Right before A completes would overwrite the slot; when A's `ConnectionOpened{Ok}`
  arrived the slot id (`B`) would not match `A`, so Left would silently fail to navigate.
  Per-side slots are independent and both opens proceed correctly. This is reducer state (no
  secrets, no I/O).

## Alternatives considered

- **Eager-open everything in a background task (P1 + timeout).** Rejected: network failures still
  block the startup sequence, and the id-stability problem remains.
- **Flip all `NeedsVault` → `Ready` and auto-open all of them on unlock.** Rejected: a user with
  many credential-bearing profiles could see a flood of concurrent opens, some of which might fail
  noisily. "Open only the trigger, leave the rest as NeedsOpen" is more predictable.
- **Remove `DeferredConnection` immediately.** Deferred to P3 to keep the diff reviewable.

## References

- `docs/rfcs/0011-connection-management.md` — full RFC with P0-P6 phases
- `crates/cairn/src/connect/coordinator.rs` — coordinator implementation
- `crates/cairn/src/app.rs` — effect runner (`run_open_connection_effect`)
- `crates/cairn-core/src/update.rs` — reducer routing (`apply_connections_action`)
- `crates/cairn-core/src/msg.rs` — `AppEffect::OpenConnection`, `AppEvent::ConnectionOpened`
- `crates/cairn-core/src/state.rs` — `Overlay::VaultUnlock::pending_conn`,
  `AppState::pending_conn_open`

<!--
  ADRs are immutable once Accepted. To change a decision, write a new ADR that supersedes this one
  and update the Status line above to "Superseded by ADR-XXXX".
-->
