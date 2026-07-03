# ADR-0012: Archive mount model — ephemeral connection + per-pane mount stack (RFC-0013 P4)

- **Status:** Accepted
- **Date:** 2026-07-03
- **Deciders:** storage-engineer (design; cross-checked by software-architect, security-engineer per
  CLAUDE.md §2)

## Context

RFC-0013 adds a read-only archive backend: `Enter` on a recognized (magic-byte) tar/zip should let
the user browse it like a directory, and leave it the same way they'd leave any directory. Two
design questions had to be settled before implementation could start:

1. **What *is* a mounted archive, structurally?** Cairn already has one directory-tree abstraction
   (`Vfs` + `ConnectionId` + `VfsPath`) that every existing backend implements, and a second,
   lighter-weight one (`Overlay`) for modal, transient UI state (prompts, the pager, the AI plan
   review) that never touches the connection registry. Which one fits an archive?
2. **How does `..` at the archive's root get back out?** The existing `leave_dir` reducer function
   no-ops at the VFS root (`cwd.parent() == None`) — there was previously never anywhere else to go.
   An archive mount needs exactly one more level: "leave the archive back to wherever this pane was
   browsing before."

## Decision

### An archive mount is a real, ephemeral connection in the `VfsRegistry`

`ArchiveVfs` implements the ordinary `Vfs` trait and is registered under a normal, freshly-minted
`ConnectionId`, exactly like an SSH or S3 connection — **not** a special `Overlay` variant, and not
a wrapper the pane consults conditionally.

This means every existing pane feature works on a mounted archive for free, with zero
archive-specific code in any of them: listing, cursor movement, marking, sorting, filtering,
`stat`-based rename-prompt validation, and — the concrete payoff — **copying files out**, which goes
through the transfer engine's ordinary `open_read`-based stream-through path with no special casing
at all (`ArchiveVfs::copy_within` simply stays `Unsupported`, same as it would for any backend
without server-side copy).

The alternative — a transient in-pane overlay that renders archive contents specially — was
rejected because it would have needed to re-implement (or explicitly punch a hole through) every one
of those features individually just for this one backend, for no benefit: the archive's contents
*are* a directory tree, and `Vfs` is precisely the abstraction Cairn already has for "a directory
tree, browsable, with typed capabilities."

**Where the id comes from:** the normal `ConnectionId` allocation authority is
`ConnectionCoordinator` (RFC-0011), which only runs at startup/re-enumeration and has no live handle
during the running event loop. Retrofitting archive mounts into that coordinator (re-running
enumeration, or exposing its `mint_fresh_id` machinery to the live loop) was rejected as
disproportionate: an archive mount is not a *discovered* connection with a stable identity across
config reloads, it's a one-off, session-scoped mount. Instead, `crates/cairn/src/app.rs`'s
`event_loop` keeps its own monotonic counter, seeded far above any id the coordinator could
plausibly assign (`ARCHIVE_CONN_ID_BASE = 1_000_000_000`; the coordinator starts at `RIGHT.0 + 1 =
3` and grows by roughly one per real connection — nowhere near a billion in any realistic session).
The two counters are disjoint ranges and never need to consult each other's claimed-id sets.

### Per-pane `mount_stack` for `..`-out

`PaneState` gains:

```rust
pub struct MountFrame {
    pub conn: ConnectionId,
    pub cwd: VfsPath,
}
pub mount_stack: Vec<MountFrame>, // on PaneState
```

A successful mount (`AppEvent::ArchiveMounted`) pushes the pane's **pre-mount** `(conn, cwd)` before
switching the pane to the new connection. `leave_dir` (the `Action::Leave`/`..`/Backspace handler),
which previously no-op'd unconditionally when `cwd.parent()` was `None`, now pops `mount_stack` in
that case instead — restoring the popped frame's connection and directory when one exists, and
keeping the prior no-op behavior when it doesn't (a pane that never mounted anything has an empty
stack, so this is a strict generalization, not a behavior change for existing flows).

Using a `Vec` rather than a single `Option` slot is deliberate, even though P4 has no way to *create*
a nested mount yet (no auto-descent into an archive found inside another archive): it means the
exact same pop-on-leave logic already handles that case correctly the day nested browsing lands,
with no reducer change. This is a small, free bit of forward-compatibility, not speculative
generality — the data structure costs nothing over `Option` for the common single-mount case.

### What is explicitly *not* decided here (deferred)

- **Unmount / refcounting.** The mounted connection stays in the registry for the process's
  lifetime once created; there is no "last pane left, tear it down" mechanism. Revisiting the same
  archive file mounts it again as a second, independent connection. This is an accepted v1 gap (see
  Consequences), not an oversight — a correct refcounting scheme needs to account for a mount being
  reachable from a `mount_stack` frame that isn't the currently-focused pane's, which is more design
  work than P4's scope justifies.
- **Remote archive staging.** `Vfs::local_path` must resolve (`Some`) for the mount to proceed; a
  `.zip` on S3 refuses cleanly rather than being staged to a temp file automatically. Auto-staging
  is real future work (RFC-0013's "Unresolved questions") but changes the risk profile (silently
  downloading a whole file over the network) enough to warrant its own design pass.

## Consequences

### Positive

- Every existing pane operation (list, sort, filter, mark, copy, move*) works on archive contents
  with **zero** archive-specific branches anywhere in `cairn-core` or `cairn-tui` — the entire
  feature is additive at the `Vfs` boundary plus the small mount/unmount state machine described
  above. (*Move off of an archive still fails at the delete step, since `ArchiveVfs::remove` stays
  `Unsupported` — copy-then-manual-cleanup is the only path in v1, which is correct: this is a
  read-only backend.)
- The `mount_stack` generalizes to nested archives for free, as noted above.
- No change at all to `cairn-tui`'s rendering: an archive-backed pane is rendered by the exact same
  code path as any other backend's pane, since it's just another `ConnectionId`/`Vfs`.

### Negative / trade-offs

- **Registry growth for the session's lifetime.** Every archive mounted (including re-mounts of the
  same file) leaves a live `Arc<dyn Vfs>` (holding an open file handle) in the registry until the
  process exits. For the kind of usage this feature targets (peek into a handful of archives per
  session) this is a non-issue in practice; it would matter for a workflow that mounts hundreds of
  archives in one long-running session. Tracked as a known limitation, not silently accepted —
  called out here and in RFC-0013.
- **A disjoint id range is a soft invariant, not an enforced one.** Nothing prevents
  `ConnectionCoordinator`'s counter from theoretically reaching into archive-mount territory given
  an astronomically long-running process that discovers/saves on the order of a billion
  connections — a physical impossibility in practice, but worth naming as the assumption this design
  rests on rather than leaving it implicit.
- **`leave_dir`'s branch is now stateful in a new way.** Previously, "at root, `..` no-ops" was a
  pure function of `cwd` alone; it now also depends on pane-local mount history. This is a small,
  well-tested addition (see RFC-0013's test table), but it is one more piece of per-pane state to
  reason about.

### Neutral

- `Scheme::Archive` was added to the (non-`#[non_exhaustive]`-affecting-this-crate) `Scheme` enum in
  `cairn-types`; it is not otherwise matched anywhere else in the workspace today (verified by
  search), so this addition had zero blast radius outside `cairn-types` itself and this feature's
  own code.

## Alternatives considered

- **Transient `Overlay` variant instead of a registry connection.** Rejected — see "Decision" above;
  would have required re-implementing list/sort/filter/mark/copy specially for archives.
- **Route archive-mount `ConnectionId` allocation through `ConnectionCoordinator`.** Rejected as
  disproportionate for a session-scoped, non-discovered connection; see "Decision" above for the
  disjoint-counter alternative actually adopted.
- **A single `Option<MountFrame>` per pane instead of a `Vec`.** Rejected: costs nothing to use a
  `Vec` instead, and doing so means nested-archive `..`-out needs no reducer change when that
  feature lands, rather than a data-model migration then.
- **Eagerly unmount on `Action::Leave` from the archive root** (pop *and* remove the registry entry
  immediately) instead of leaving it registered for the session. Rejected for v1: a pane could
  `Enter` back into the same archive path a moment later (e.g. after checking something in the
  sibling directory), and re-indexing a large archive on every in-and-out is wasteful; the
  session-lifetime registry entry trades a small, bounded memory/FD cost for that repeat-visit
  cheapness. Proper refcounting (unmount only when no pane's `mount_stack` still references the
  connection) is the correct long-term fix and is deferred, not rejected outright.

## References

- RFC-0013 (`docs/rfcs/0013-archive-backend.md`) — the full backend design this ADR's mount-model
  section summarizes the decision from.
- RFC-0011 (`docs/rfcs/0011-connection-management.md`) and ADR-0007
  (`docs/adr/0007-lazy-connection-opening.md`) — the existing `ConnectionCoordinator`/`ConnectionId`
  allocation model this ADR deliberately does *not* extend.
- `crates/cairn-core/src/state.rs`: `PaneState::mount_stack`, `MountFrame`.
- `crates/cairn-core/src/update.rs`: `leave_dir`, the `AppEvent::ArchiveMounted` handler.
- `crates/cairn/src/app.rs`: `ARCHIVE_CONN_ID_BASE`, `run_mount_archive_effect`.
