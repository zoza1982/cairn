# RFC-0012: File Open, View & Edit — Read-Only Pager, In-Place Editor, Remote Writeback

- **Status:** Implemented (P1 + P2 + P3)
- **Author(s):** tui-engineer, software-architect, storage-engineer (synthesized)
- **Date:** 2026-07-03
- **Tracking item:** M4-7 (see `docs/IMPLEMENTATION_PLAN.md`)

## Summary

Cairn can browse every backend but has no way to *look inside* a file. This RFC specifies the
whole file-open experience in three phases:

1. **P1 (implemented): a built-in, read-only pager** (MC's `F3`) — `F3` opens the entry under the
   cursor; `Enter` on a file classifies it (text vs binary) and opens the pager in the matching
   mode (`Text` or `Hex`). No editor yet.
2. **P2 (this PR): in-place editing via `$EDITOR`, local files only.** `F4` always opens
   `$VISUAL`/`$EDITOR`/`vi` on the entry under the cursor; `Enter` on a `FileKind::Text` result now
   routes to the editor instead of the read-only pager (binary files still open the hex pager).
   Cairn suspends the terminal, hands the real TTY to the editor, and resumes on exit. Remote
   backends refuse cleanly with a pointer to P3 — this phase does not shell out over the wire.
3. **P3 (implemented): remote writeback hardening.** Conflict detection (the file changed
   underneath the editor), atomic replace-on-save semantics where the backend supports them, and a
   large-file editing limit — the from-scratch in-app buffer editor originally sketched for P2 was
   superseded by the `$EDITOR`-shell-out design once ux-engineer/tui-engineer weighed in (see "P2 —
   implemented" below); P3 makes that shell-out safe for non-local backends by downloading to a
   private temp copy, editing that, and writing it back only after confirming it is safe to do so.

This document covers the full arc so the P1 pager's data model doesn't have to be revisited when
P3 lands — the `Overlay::Pager` shape, the `AppEffect`/`AppEvent` naming, and the reducer's
sniff-then-open flow are all chosen to extend cleanly.

## Motivation

Every other dual-pane manager (Midnight Commander, Far, Total Commander) treats "look at a file"
as a first-class, keyboard-first action distinct from "open it in $EDITOR" — a pager is instant,
read-only, and safe to use on a 10 GB log or a binary you're not sure about; an editor commits to
loading the whole thing and writing it back. Cairn had neither. Concretely:

- **No way to inspect file contents at all.** The only place file bytes are visible today is the
  live log-tail overlay (`Overlay::LogViewer`), which is wired to backend-specific `Stream`
  entries (container/pod logs) via `Vfs::invoke(.., "logs", ..)` — it does not read a plain file
  via `Vfs::open_read`.
- **`Enter` on a file is a no-op.** `enter_dir`'s non-directory branch returned `Vec::new()`
  unconditionally; there was no async classification step to route "text" vs "binary" content or
  to hand off to a future editor.
- **A file could be binary.** Rendering raw bytes as text would garble the terminal (control
  codes, non-UTF-8 sequences) — the pager needs a cheap, safe classification step *before*
  deciding how to display anything, not after the fact.

## Guide-level explanation

### What changes for a user (P1)

- **`F3`** opens the entry under the cursor in the pager immediately, defaulting to `Text` mode.
  If the stream turns out to be binary (a `NUL` byte appears in the first chunk), the view flips
  to `Hex` mode automatically — no flash of garbled text.
- **`Enter` on a file** reads a small prefix and classifies it. A **binary** result opens the pager
  in `Hex` mode. A **text** result now opens the external editor (P2 — see below); `Enter` on a
  directory is unchanged (navigates into it).
- **Pager keys:** `j`/`k`/`↑`/`↓` scroll a line/row; `PageUp`/`PageDown` scroll a page; `g`/`G` (or
  `Home`/`End`) jump to the top/loaded-bottom; `q`/`Esc` closes.
- **Title bar** shows the filename, a `line/total (pct%)` position (pct only when the backend
  reports the entry's size), and a status (`Loading…` / `Ready` / `Truncated — showing first N` /
  `Error: …`).
- **Hex mode** renders classic `offset | hex bytes | ascii` rows, 16 bytes per row.

### What changes for a user (P2)

- **`F4`** always opens `$VISUAL`/`$EDITOR`/`vi` on the entry under the cursor — no text/binary
  sniff, MC-faithful ("edit this, whatever it is"). Cairn suspends its own screen, hands the
  editor the real terminal, and restores itself the moment the editor exits.
- **`Enter` on a text file** now opens the same editor (a behavioral change from P1, where it
  opened the read-only pager). `Enter` on a binary file is unchanged — still the hex pager, since
  Cairn is not a hex editor.
- **Local files only in P2.** Editing a file on a remote backend showed a P3-pointer message and
  did nothing else. **Superseded by P3** (below) — remote files are now editable too.
- After the editor exits, the active pane's listing refreshes (the file's size/mtime may have
  changed) and the status line shows the outcome (`edited <name>`, or a redacted failure).

### What changes for a user (P3)

- **`F4`/`Enter`-on-text now also works on remote files** — SSH, S3/GCS/Azure, Docker, Kubernetes,
  or any other backend whose entries aren't backed by a real local path. Behind the scenes: the
  file is downloaded to a private local temp copy, the same `$EDITOR` opens on that copy, and the
  result is written back once the editor exits — but the user experience is "press F4, edit, save,
  done," identical to editing a local file, plus a couple of new safety prompts:
  - **A no-op edit** (opened the editor, changed nothing) writes nothing back — the status line
    says so and the temp copy is discarded silently.
  - **A conflict** (the remote file changed since the download, was deleted, or the local edit came
    back completely empty while the original was not) opens a confirm overlay: **Overwrite** the
    remote anyway, **Save as** a new sibling file instead, **Keep editing** the same temp copy, or
    **Discard** the edit entirely. `Esc` behaves like Keep-editing, not Discard — the write-back
    path never silently clobbers *or* silently throws away a user's edits.
  - **Very large files are refused up front** (before any download) — see the size cap below.
- Nothing changes for local files — they still take the P2 path (no download, no temp file, no
  conflict check needed since there's no network round-trip to race).

### What doesn't change

- `Enter`/`F3` on a directory: unchanged (navigates / no-op).
- The live log-tail viewer (`Overlay::LogViewer`, `Action::OpenLogViewer`): untouched, still the
  dedicated flow for `Stream` entries (container/pod logs). The pager and the log viewer are
  deliberately separate overlays — a static file's "first N bytes, then stop" contract is the
  opposite of a live tail's "most recent N lines, keep following" contract (see Reference-level
  explanation).

## Reference-level explanation (P1 — implemented)

### Reducer stays pure

Per the project's TEA architecture (`docs/LLD.md` §5), classification is **not** done in the
reducer — reading even a bounded file prefix is I/O. The reducer only:

1. Emits `AppEffect::SniffFile { conn, path }` when `Enter` lands on a non-directory `File`/
   `Symlink` entry (`Stream` entries — container/pod logs — and `Special` nodes are left to their
   own dedicated flows and are not sniffed).
2. Opens `Overlay::Pager` immediately, in `Text`/`Loading`, and emits `AppEffect::OpenPager` when
   `Action::View` (`F3`) fires — F3 always means "view this," skipping the sniff.
3. Reacts to `AppEvent::FileSniffed`/`SniffFailed`/`PagerChunk`/`PagerDone` — all pure state
   transitions, no `.await`.

### Data model

```rust
pub enum FileKind { Text, Binary }

/// NUL-byte heuristic: any 0x00 in the sample means binary. The same convention `file(1)`, git,
/// and most pagers use.
pub fn detect_file_kind(sample: &[u8]) -> FileKind;

pub enum PagerMode { Text, Hex }

pub enum PagerStatus { Loading, Ready, Truncated, Error(String) }

pub type PagerId = u64;
pub const PAGER_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const PAGER_HEX_ROW_BYTES: usize = 16;

enum Overlay {
    // ...
    Pager {
        id: PagerId,
        title: String,
        mode: PagerMode,
        lines: VecDeque<String>,   // stored rows — see "Storage convention" below
        partial: String,           // not-yet-flushed tail of the current line/row
        byte_size: usize,          // bytes represented by `lines` + `partial`
        total_size: Option<u64>,   // from the directory listing, if known
        scroll: usize,
        status: PagerStatus,
        wrap: bool,                // Text-mode soft-wrap; no toggle keybinding yet (`// v2`)
    },
}
```

`Overlay::Pager` is modeled closely on `Overlay::LogViewer` (the existing streaming-overlay
convention: a capped, `byte_size`-tracked `VecDeque<String>` fed by chunks from an async effect) —
**with one deliberate difference**: the log viewer evicts from the *front* when its cap is hit,
because it shows a live tail (the most recent N lines matter, not the first N). The pager never
evicts — it *stops accepting bytes* once `PAGER_MAX_BYTES` is reached and shows
`"truncated — showing first N"`, because a static file's *first* N bytes are what a pager should
show. This is a deliberate divergence from the LogViewer template, not an oversight.

### Storage convention: `lines` is mode-dependent, `String`-shaped either way

To keep `Overlay::Pager` a single shape for both display modes:

- **`Text` mode:** `lines` holds complete, lossily-decoded (`String::from_utf8_lossy`) lines split
  on `\n` — identical convention to `Overlay::LogViewer`.
- **`Hex` mode:** `lines` holds raw `PAGER_HEX_ROW_BYTES`-byte (16) rows, but encoded through a
  **lossless byte↔char mapping** (`char::from(byte)` — every value `0..=255` is a valid Unicode
  scalar value) rather than lossy UTF-8 decoding. This preserves the exact bytes (a lossy decode
  would corrupt a hex dump the instant it hit an invalid sequence, which is the common case for
  binary content). The render layer (`cairn-tui`) decodes each row back
  (`raw.chars().map(|c| c as u32 as u8)`) and formats the `offset | hex | ascii` display line —
  the byte→row *formatting* stays in the render layer; the reducer's `lines` field stays raw
  either way.

### Enter-on-file flow

```
Action::Enter (non-directory entry)
  → AppEffect::SniffFile { conn, path }
  → (effect runner) reads ≤ 8 KiB, classifies, replies AppEvent::FileSniffed { kind, prefetch, .. }
  → reducer: staleness guard (cursor must still be on exactly this path — the user may have
    moved on while the sniff was in flight; mirrors the guard on AppEvent::Listed)
  → opens Overlay::Pager seeded with `prefetch` (instant first frame) in the classified mode
  → emits AppEffect::OpenPager { id, conn, path, skip: prefetch.len() } to keep streaming
```

`skip` exists so the prefetched window is never shown twice: `OpenPager`'s effect always opens a
*fresh* `Vfs::open_read(path, None)` (starting at byte 0, regardless of whether the sniff used a
ranged read) and discards `skip` bytes client-side before forwarding the first `PagerChunk`. This
was chosen over threading capability-detection through `OpenPager` — it costs a redundant ≤ 8 KiB
read for remote backends but needs no per-backend branching and is correct even for backends
without `Caps::RANDOM_READ`.

### F3 (`Action::View`) flow

```
Action::View
  → opens Overlay::Pager immediately (Text, Loading, no prefetch, skip: 0)
  → emits AppEffect::OpenPager
  → reducer flips mode to Hex if the first AppEvent::PagerChunk contains a NUL byte
```

F3 skips the sniff entirely (no classification round-trip) — the mode-flip-on-first-chunk keeps
the "no garbled flash" property without paying for two reads.

### Effects and events

```rust
enum AppEffect {
    SniffFile { conn: ConnectionId, path: VfsPath },
    OpenPager { id: PagerId, conn: ConnectionId, path: VfsPath, skip: u64 },
    ClosePager { id: PagerId },
}

enum AppEvent {
    FileSniffed { conn: ConnectionId, path: VfsPath, kind: FileKind, prefetch: Bytes },
    SniffFailed { message: String },
    PagerChunk { id: PagerId, bytes: Bytes },
    PagerDone { id: PagerId, error: Option<String>, truncated: bool },
}
```

`run_sniff_file_effect` and `run_pager_effect` (`crates/cairn/src/app.rs`) follow the exact
pattern `run_log_viewer_effect` established: `Vfs::open_read`/`invoke` off the render path, a
`tokio::select!` loop against a `CancellationToken` (keyed by `PagerId` in a runtime-side
`HashMap`, mirroring `log_viewer_controls`), errors redacted via `VfsError::redacted()`. When the
reducer itself hits `PAGER_MAX_BYTES` (inside the `PagerChunk` handler) it marks the view
`Truncated` **and** emits `AppEffect::ClosePager` proactively — the runner has no more bytes worth
reading.

### Rendering

`render_pager` (`crates/cairn-tui/src/render.rs`) mirrors `render_log_viewer`'s windowed
skip/take virtualization (only the on-screen rows are materialized per frame). `Text` mode uses a
`Paragraph` with optional `Wrap`; `Hex` mode uses a `List` of pre-formatted
`offset | hex | ascii` rows built by `format_hex_row`.

## Reference-level explanation (P2 — implemented)

**Goal (as shipped):** `F4` (`Action::Edit`) always opens `$VISUAL`/`$EDITOR`/`vi` on the entry
under the cursor; `Enter` on a `FileKind::Text` result routes to the same editor instead of the
read-only pager. **Local backends only** — a non-local `Vfs::local_path` result refuses cleanly
before the terminal is touched.

This supersedes the from-scratch in-app buffer editor originally sketched here (see "Rationale and
alternatives" for why) in favor of shelling out to the user's own editor, on the real terminal,
after a full design pass with `ux-engineer`/`tui-engineer`/`security-engineer`. The full design
rationale — the terminal suspend/resume sequence, the input-reader pause/resume mechanism, and the
process-hardening model — is recorded in
[ADR-0011](../adr/0011-terminal-suspend-and-editor-launch.md); this section summarizes the
data-model-level changes.

### New actions/effects/events

```rust
enum Action {
    // ...
    Edit, // F4
}

enum AppEffect {
    // ...
    /// Not routed through the normal effect runner — special-cased inline in `event_loop`
    /// because it needs exclusive terminal + stdin ownership (see ADR-0011).
    SuspendAndEdit { conn: ConnectionId, path: VfsPath },
}

enum AppEvent {
    // ...
    EditFinished { status: String, error: bool },
}
```

### Reducer changes

- `Action::Edit` guards the entry under the cursor to `File`/`Symlink` (same restriction as the
  pager/sniff) and emits `AppEffect::SuspendAndEdit` directly — no sniff, since `F4` always means
  "edit this" regardless of content (MC-faithful).
- `AppEvent::FileSniffed`'s `Text` arm (previously: open the pager in `Text` mode) now emits
  `AppEffect::SuspendAndEdit` instead. The `Binary` arm is unchanged — still opens the read-only
  hex pager, since Cairn is not a hex editor.
- `AppEvent::EditFinished` sets the status line; on success it also re-emits the active pane's
  `List` effect (the file's size/mtime may have changed), reusing the same refresh path as
  `Action::Refresh`.

### Runtime (effect-runner) changes

`AppEffect::SuspendAndEdit` is handled inline in `event_loop`'s effect loop (`crates/cairn/src/app.rs`),
not via `dispatch` — see ADR-0011 for the full sequence (resolve `local_path` → pause the input
reader and wait for its ack → suspend the terminal → spawn the hardened editor and await it in the
foreground → resume the terminal with a full repaint → resume the reader → report the outcome).
The editor is spawned with **argv only** (never a shell), a `--` terminator before the always-
absolute path, a scrubbed environment (`env_clear()` + an explicit allow-list + a sanitized `PATH`),
its own process group, and no timeout (editing is open-ended interactive work) — modeled on the
existing `spawn_shell_action`/`sanitized_path` hardening (`docs/adr/0005-shell-command-actions.md`).

### Scope explicitly deferred to P3

- Remote backends: refused with a clear message, not shelled out to (no temp-copy-and-upload yet).
- Conflict detection (the file changed underneath the editor) and atomic replace-on-save.
- Size limits for very large files on backends without cheap ranged writes.

## Reference-level explanation (P3 — implemented)

**Goal:** extend P2's `$EDITOR` shell-out to remote backends safely — "load fully, edit, write
fully" is unsafe if the file changed underneath you, and expensive for very large remote files.

**As shipped:** temp-copy-edit-upload, not an in-app buffer, exactly as originally sketched: the
remote file is downloaded to a local temp copy via the transfer engine (`cairn_transfer::run_transfer`),
the same `spawn_editor_hardened`/terminal-suspend machinery P2 uses runs against that copy, and on
a real (non-no-op) change it is written back — also via the transfer engine — after a conflict
re-check. `crates/cairn/src/app.rs` and `crates/cairn-core/src/{state,msg,update}.rs`;
`crates/cairn-tui/src/render.rs`.

### `RemoteVersion`: detecting "did the remote change under me?"

```rust
/// crates/cairn-core/src/state.rs
pub enum RemoteVersion {
    ETag(String),
    MTimeSize { modified: SystemTime, size: u64 },
    Unknown,
}
impl RemoteVersion {
    pub fn from_entry(entry: &Entry) -> Self;      // prefers etag, falls back to modified+size
    pub fn confirmed_equal(&self, other: &Self) -> bool; // false whenever either side is Unknown
}
```

A pure function over `Entry` (unit-tested in isolation, no I/O) — **not** a new `Vfs` trait method,
since every signal it needs (`etag`, `modified`, `size`) is already on `Entry`. `confirmed_equal`
is `false` whenever either side is `Unknown` (including two `Unknown`s compared to each other) or
the two sides are different variants — an unverifiable version is *never* treated as "unchanged."

**Per-backend signal availability:**

| Backend | Signal |
|---|---|
| S3 / GCS / Azure Blob | `ETag` (object version tag) |
| SFTP, local | `MTimeSize` (no version tag; compared as a pair) |
| Docker, Kubernetes | `Unknown` **today** — their `Entry` never populates `modified`, so `from_entry` falls through past `MTimeSize` (both fields are required) straight to `Unknown`. Moot in the current release: neither backend advertises `Caps::WRITE` yet, so `begin_remote_edit` refuses them before `RemoteVersion` is ever consulted. If/when they gain write support, they will always prompt on conflict until `modified` is plumbed through their `Entry` mapping (tracked as a follow-up). |
| Anything else reporting neither `etag` nor `modified`+`size` | `Unknown` — write-back always opens the confirm overlay |

### The flow

1. **Route to remote-edit.** `run_editor_suspend` (P2, inline in `event_loop` for exclusive
   terminal ownership) already resolves `Vfs::local_path` *before* touching the terminal. Its
   `None` arm — previously an outright refusal — now calls `begin_remote_edit`, which (still before
   the terminal is touched): checks `Caps::WRITE` (refuses read-only backends, e.g. a mounted
   archive, without ever downloading), `stat`s the path for size + `RemoteVersion` (`v0`), enforces
   the size cap (below), and pre-resolves `$VISUAL`/`$EDITOR`/`vi` (fail fast before a possibly
   large download). On success it sends `AppEvent::RemoteEditNeedsDownload`.
2. **Download.** The reducer mints a `RemoteEditId` (monotonic, like `TransferId`/`PagerId`) and
   emits `AppEffect::DownloadForEdit` — a **normal dispatched effect** (spawned, never blocking the
   render loop), unlike the terminal-owning steps. **Not currently cancellable**: no
   `CancellationToken` is wired to this effect (or to `WriteBack`) yet — see "What is deferred"
   below. The runtime creates the private temp file synchronously (see below), then streams the
   remote file into it via `run_transfer` (remote `Vfs` → a `LocalVfs` rooted at the temp file's
   directory; a sentinel `ConnectionId(u64::MAX)` keeps the transfer engine's same-connection rename
   fast-path from ever matching a real backend), and SHA-256-hashes the result (`hash_file`, `sha2`
   — see "Crate impact" below) as **`download_hash`** — the stable, whole-session no-op baseline
   (see step 3's note on why this must never be replaced by a later hash).
3. **Edit.** `AppEffect::EditRemoteTemp` is special-cased inline in `event_loop`, exactly like
   `SuspendAndEdit` (same exclusive-terminal requirement) — `run_editor_suspend`'s pause/suspend/
   spawn/resume sequence was factored out into a shared `suspend_and_run_editor` helper so both P2's
   local-file path and P3's temp-file path use it verbatim. After the editor exits, the temp file is
   re-hashed and compared against **`download_hash`** (carried through every subsequent event/effect
   unchanged, never replaced):
   - **Matches `download_hash`** → `AppEvent::RemoteEditNoChange` — nothing is written back, no
     pane refresh.
   - **Differs from `download_hash`** → `AppEvent::RemoteEditModified` → the reducer emits
     `AppEffect::WriteBack` in `WriteBackMode::CheckThenWrite`. This is deliberately **not** "differs
     from the hash observed at the start of this specific invocation" — an earlier implementation
     compared against that instead, and after a `KeepEditing` re-open (step 5) whose editor
     invocation changed nothing further, that comparison came back equal and reported `NoChange`,
     silently discarding an edit that still differed from the remote. Comparing against the stable
     `download_hash` fixes this: only content that is genuinely identical to the original download
     is ever a no-op, however many edit/conflict/`KeepEditing` rounds it took to get there.
4. **Conflict check + write-back.** `AppEffect::WriteBack` (a normal dispatched effect, likewise not
   currently cancellable — see step 2) in `CheckThenWrite` mode: (a) a **zero-length guard** — if
   the temp file is now 0 bytes but the
   original was not, that is a crashed/misbehaving-editor signal, not a deliberate "empty this
   file" edit; (b) re-`stat`s the remote path for `v1` and compares against the held `v0` — this is
   the TOCTOU-sensitive step: `v0` was captured once at download time and held since; `v1` is a
   fresh `stat` taken immediately before the write decision, never re-derived from a re-opened,
   attacker-swappable path. A `NotFound` on the re-`stat` (the remote file was deleted) maps to
   `RemoteVersion::Unknown`, which `confirmed_equal` never matches. Either guard tripping emits
   `AppEvent::WriteBackConflict` (carrying *why*, `WritebackConflictReason::RemoteChanged` or
   `::ZeroLengthGuard`) instead of writing.
5. **Conflict resolution.** `AppEvent::WriteBackConflict` opens `Overlay::ConfirmWriteback` — a
   cursor-selected list (like `Overlay::Connections`), never a plain yes/no, because there are four
   meaningfully different answers:
   - **Overwrite** → `AppEffect::WriteBack` in `ForceOverwrite` mode (skips both guards — the user
     already confirmed).
   - **Save as** → `ForceOverwrite`'s sibling, `SaveAsSibling` mode: writes to a freshly chosen
     `"<stem> (edited)<ext>"` (incrementing `" (N)"` on a collision) instead of the original path —
     a small, local, bounded re-implementation of the same idea as the transfer engine's private
     `unique_name` (not directly reusable — it's module-private to `cairn-transfer`).
   - **Keep editing** → re-emits `AppEffect::EditRemoteTemp` on the *same* temp file, carrying both
     the *original* `v0` **and** `download_hash` forward unchanged (never a value updated at the
     conflict) — the question "has the remote drifted since I started editing" and "is there
     genuinely nothing to write back yet" both stay meaningful across any number of further edit
     passes. Note that choosing `KeepEditing` does **not** clear or resolve the conflict that raised
     the overlay — it only re-opens the same temp file in the editor again; the next exit re-runs
     the *entire* conflict check from scratch (re-`stat`, re-compare against `v0`, zero-length
     guard), it does not remember or skip past the earlier conflict.
   - **Discard** → `AppEffect::CancelRemoteEdit` (synchronous, no spawn — it only drops a value):
     abandons the session, deleting the temp file, without writing anything.
   - `Esc` (`Action::Cancel`) is deliberately mapped to the *same effect as Keep-editing*, not
     Discard — an escape key must never be the trigger for a destructive default.
6. **The actual write** (`write_temp_to_remote`, shared by `CheckThenWrite`'s pass-through,
   `ForceOverwrite`, and `SaveAsSibling`): where the backend advertises `Caps::RENAME` at the
   target, stages the content at a sibling name (`.cairn-edit-tmp-<name>`) via `run_transfer`, then
   `rename`s it over the target (atomic-ish; a failed *staging write* or a failed *rename* both
   best-effort clean up the staged sibling so neither leaves debris on the remote). Where it does
   not — **Docker and Kubernetes have no atomic rename** — it writes directly to the target, a real,
   documented non-atomic window (a concurrent reader could see a partially-written file mid-copy).
   **Mode preservation:** the staging-rename path writes to a brand-new inode, so without
   correction the target would come back with the staged copy's umask-default mode instead of the
   original's (e.g. a `0600` secret becoming world-readable after one remote edit) — after a
   successful rename, `orig_perms` (captured from the `v0` `stat` at download time) is restored on
   the target via `set_perms`, guarded by `Caps::CHMOD` and best-effort (a failure here does not
   fail the write-back — the content already landed correctly). The direct-overwrite path needs no
   such fix-up: truncating an existing inode in place never touches its mode.
   `AppEvent::WriteBackDone` refreshes the active pane.

### Temp-file lifecycle: who owns the RAII handle across an arbitrarily long conflict prompt?

The temp file's lifetime spans multiple async round-trips and arbitrary user think-time at the
conflict overlay — far longer than any single function call, and the reducer must hold no I/O
handle (per the project's TEA architecture). The solution mirrors the runtime's other per-session
maps (`transfer_controls`, `pager_controls`, `session_controls`): `event_loop` holds
`remote_edit_temps: HashMap<RemoteEditId, tempfile::TempDir>`, created synchronously in
`dispatch`'s `DownloadForEdit` arm (a few `std::fs` calls — negligible, exactly like the
`cancel`/`pause_tx` setup the existing `Transfer` effect already does inline) and dropped —
deleting the directory and the file in it — on every terminal event (`RemoteEditNoChange`,
`RemoteEditFailed`, `WriteBackDone`) via the same pre-`update()` cleanup convention already used
for transfers/pagers/sessions. `WriteBackConflict` is deliberately excluded from that cleanup list
— the flow continues, possibly for a long time, and the temp file must survive. As a last-resort
safety net, the whole map (and therefore every session's temp directory) drops when the event loop
itself returns (app quit), so a session left open when the app exits still gets cleaned up.

**Residual gap: a hard process abort (`SIGKILL`, a host crash, `kill -9`) skips Rust's normal
unwind/`Drop` path entirely**, so the temp directory is not cleaned up in that case — it is left on
disk (containing decrypted remote plaintext) until the next reboot/logout clears
`$XDG_RUNTIME_DIR`, or indefinitely if the OS temp dir fallback was used instead. This is the same
residual class of risk any RAII-based temp-file scheme has against an uncatchable signal; a
startup sweep of stale `.cairn-edit-*` directories would close it and is tracked as a follow-up
(see "What is deferred" below) rather than implemented in this pass.

The directory itself (not just the file) is freshly minted and non-predictable
(`new_remote_edit_dir`, mirroring `cairn-backend-archive::compressed_tar`'s pattern: prefer
`$XDG_RUNTIME_DIR`, forced `0700` regardless of the process umask, else the OS temp dir) —
`create_temp_edit_file` then creates the file *inside it* with the original leaf name (so `$EDITOR`
still sees the right extension for syntax highlighting), `0600` and `O_EXCL` (`create_new`)
explicitly rather than relying on umask. Because the directory is already private and
non-predictable, a predictable filename inside it is not a predictable *path* — this closes the
classic shared-`/tmp` predictable-temp-file attack without needing a random filename too.

### Size limit

`REMOTE_EDIT_MAX_BYTES = 100 MiB` (`cairn-core`). A deliberate UX guardrail, not a technical
ceiling — editing a multi-GB file by round-tripping it through `$EDITOR` is the wrong workflow
regardless of available disk/memory. Enforced in `begin_remote_edit`, before any download starts.

### What is deferred (follow-up, not blocking)

- **Live byte-progress/cancellation for the download and write-back phases.** Both currently show a
  single status-line message (`"Downloading <name> (<n> bytes) for editing…"`) rather than a live
  progress bar wired into the `active_transfers`/transfer-queue UI, and there is no cancel button
  mid-transfer. Reusing that UI wholesale was considered and set aside: piggybacking a transient,
  system-internal download onto the user-facing bulk-copy queue (pause/resume/reorder semantics
  that don't apply here) risked surprising interactions for a first cut; a dedicated, lighter-weight
  progress channel is the natural next step.
- **A failed write-back currently discards the local edit** rather than offering a redownload-free
  retry — the temp file is cleaned up on `RemoteEditFailed` just like any other terminal outcome.
  Keeping it around for a retry is a reasonable follow-up but adds another lifecycle branch; cut
  from this pass to keep the already-large state machine bounded.
- **A startup sweep of stale `.cairn-edit-*` temp directories** — closes the residual `SIGKILL`/
  hard-abort gap noted above (no RAII path runs on an uncatchable signal).
- **Randomize the staging sibling name** (`write_temp_to_remote`'s `.cairn-edit-tmp-<leaf>`) — a
  predictable name on the *remote* side (unlike the local temp file/directory, which already is
  non-predictable) is a smaller-but-real hardening gap for a backend where an attacker could race
  to create/observe that name.
- **Sub-second / conditional-write (`If-Match`) conflict detection for `etag` backends** — the
  current re-`stat`-and-compare window between the conflict check and the write is TOCTOU-safe
  against a *stale* read but not against a write racing in the same instant; a conditional PUT
  (where the backend supports one) would close that window entirely.
- **Wire UI cancellation for the download/write-back phases** — see the "not currently cancellable"
  notes on `AppEffect::DownloadForEdit`/`AppEffect::WriteBack` above.
- **Remote-edit transfers bypass the `[transfers] concurrency` cap** — each remote edit's download/
  write-back always runs as its own single transfer outside the bulk-copy queue, so it is not
  throttled by (or counted against) the user's configured transfer concurrency limit. Acceptable
  today (a single interactive edit is not a bulk operation), but worth revisiting if that changes.
- **Add `modified` to Docker/K8s's `Entry` mapping** before those backends gain `Caps::WRITE` — see
  the per-backend signal table above; without it they will always degrade to `RemoteVersion::Unknown`
  and always prompt on conflict.

## Drawbacks

- Two read paths for the same bytes on the `Enter`-on-file flow (the sniff's bounded prefix, then
  `OpenPager`'s full stream) cost a small redundant read for remote backends. Accepted for P1
  simplicity; see "Enter-on-file flow" above for the alternative considered.
- The pager's "stop at cap, don't evict" behavior means a truncated view of a huge file only ever
  shows its start, never its end — by design (mirrors `head`), but a user wanting the *tail* of a
  huge file still needs a different tool (out of scope; MC has the same limitation for its
  non-live pager).
- `Overlay::Pager::wrap` has no keybinding to toggle it in P1 (`// v2` in the code) — soft-wrap is
  always on. A toggle is cheap to add later and was left out to keep P1's action surface small.

## Rationale and alternatives

- **Read-only pager first, editor later** (rather than one combined "open" action that always
  edits): matches MC's `F3`/`F4` split, lets P1 ship without touching writeback safety at all, and
  gives every backend a working "inspect this file" action immediately (binary files included) —
  the editor is inherently a bigger, riskier surface (data loss on a botched write) that deserves
  its own review pass.
- **NUL-byte heuristic over a mime/magic-byte library:** zero new dependencies, matches the
  heuristic every comparable tool already uses, and is exactly as fast as reading the sample
  (`.contains(&0)` is a single linear scan). A full magic-byte/mime-sniffing crate would add
  dependency surface for a P1 feature that only needs a binary/text binary decision.
- **Lossless byte↔char encoding for hex rows vs. a separate raw-bytes field:** keeps
  `Overlay::Pager` a single `VecDeque<String>` shape for both modes (simpler `Debug`, simpler
  reducer helpers) instead of adding a parallel `Vec<u8>`-shaped field that only one mode uses.
- **P2: shell out to `$VISUAL`/`$EDITOR`/`vi` over a from-scratch in-app buffer editor** (resolving
  Unresolved Question 1 below). The from-scratch widget originally sketched in this RFC would have
  needed a new multi-line text-editing model (`TextEdit` is single-line-field shaped throughout),
  its own undo/redo and syntax-agnostic rendering, and would still only give users a plain-text
  editor no better than what every terminal already has. Shelling out gives users their actual,
  already-configured editor (syntax highlighting, their keybindings, plugins, `--wait` support for
  GUI editors) at the cost of the terminal-suspend/resume complexity documented in ADR-0011 — a
  one-time cost paid once, in the runtime layer, rather than an ongoing cost of reimplementing an
  editor. This does mean P2 is local-only (P3 needed for remote), whereas an in-app buffer editor
  could in principle have worked identically over any backend from day one; the `ux-engineer`/
  `tui-engineer` consultation (Unresolved Question 1) judged the UX and correctness win of a real
  editor worth deferring remote support to P3.
- **Terminal suspend/resume design** (special-casing the one effect in `event_loop`, the
  `InputGate` pause/resume mechanism, manual re-init over `ratatui::init()`) — see ADR-0011 for the
  full alternatives analysis on that sub-decision.

## Security and privacy considerations

- Pager content is file bytes, potentially containing secrets embedded in a config file, `.env`,
  or similar — it is **never forwarded to the AI layer** (no `AppEffect`/`AppEvent` in this RFC is
  reachable from `cairn-ai`'s dependency closure) and **never logged raw**; only redacted
  `error: String` messages and byte counts appear in any `Debug`/status line.
- All I/O errors are redacted via the existing `VfsError::redacted()` convention before reaching
  `AppEvent`/the status line — no host names, paths beyond what the user already navigated to, or
  credential material.
- **P2's editor-launch hardening** (implemented, security-reviewed): argv-only spawn (never a
  shell), a `--` terminator before the always-absolute target path, `$VISUAL`/`$EDITOR` split via
  deterministic POSIX shell-word quoting (`shlex`, no glob/variable/command-substitution
  expansion), `env_clear()` + an explicit allow-list + a sanitized `PATH`, its own process group,
  and a local-only guard resolved *before* the terminal is touched. See ADR-0011 for the full
  model and `crates/cairn/src/app.rs`'s `spawn_editor_hardened` test suite for the adversarial
  cases exercised (shell-injection attempt via `$EDITOR`, command-substitution attempt, a
  flag-shaped filename, environment scrubbing, and a secret canary).
- **P3 (writeback to remote backends, implemented, security-reviewed):** the temp-copy-edit-upload
  path's specific new attack surface and how it's addressed:
  - **Temp-file permissions/predictability**: a freshly-minted, non-predictable, `0700` directory
    (preferring `$XDG_RUNTIME_DIR` — a per-user tmpfs, so plaintext ideally never touches stable
    storage) containing a single `0600`, `O_EXCL`-created file. The directory's non-predictability
    is what closes the classic shared-`/tmp` predictable-path attack even though the *filename*
    inside it (the original leaf name, kept for `$EDITOR` syntax highlighting) is predictable.
  - **Deleted on every exit path**: success, no-op, editor failure, write-back failure, explicit
    discard, and (as a last-resort net) whole-process shutdown — see "Temp-file lifecycle" above.
    Never left behind holding remote plaintext. **Residual gap**: a `SIGKILL`/hard-abort skips every
    `Drop` path and leaves the directory on disk until logout/reboot clears it (or indefinitely on
    the OS-temp-dir fallback) — accepted for this pass, tracked as a startup-sweep follow-up.
  - **Conflict-detection TOCTOU**: `v0` is captured once at download time and held in the effect/
    event payloads (plain data, never a live handle); `v1` is a fresh `stat` immediately before the
    write decision, never re-derived from a re-opened path. `RemoteVersion::Unknown` is never
    "confirmed equal" to anything, including itself, so an unverifiable version always prompts.
  - **No-op-edit decision correctness**: the "is there anything to write back" check compares the
    post-edit hash against `download_hash` — the hash captured once, right after the original
    download, carried unchanged through every subsequent effect/event including any number of
    `KeepEditing` loops. An earlier implementation compared against the *latest* (per-round) hash
    instead, which silently discarded a real edit that survived a `KeepEditing` re-open unchanged
    (caught in gate review before merge; see `decide_remote_edit_outcome`'s doc and its regression
    tests). Treated as a correctness/data-loss bug, not merely a UX nit, given the whole point of
    the conflict flow is to never lose an edit silently.
  - **Zero-length/truncated-save guard**: a crashed or misbehaving editor that empties a previously
    non-empty file is caught before it reaches the remote, not after.
  - **Remote file mode preservation**: the staging-rename write path writes to a brand-new inode, so
    without correction the target would silently pick up the staged copy's umask-default mode
    instead of the original's — a real permission-loosening bug for e.g. a `0600` credentials file
    (caught in gate review; see `write_temp_to_remote`'s mode-restoration and its regression test).
    Fixed via a best-effort `set_perms` restore after a successful rename, gated on `Caps::CHMOD`.
  - **Staging cleanup on every failure mode, not just a failed rename**: a failed *staging write*
    (not only a failed rename) now also best-effort removes the partial staged object, so a one-off
    network failure mid-copy doesn't leave a `.cairn-edit-tmp-*` object on the remote (caught in
    gate review).
  - **Editor spawn**: reuses P2's hardened `spawn_editor_hardened` verbatim (argv-only, env-scrubbed,
    `--` terminator) — the temp path is the only argument; no new spawn surface.
  - **Never reaches the AI layer**: the remote-edit effects/events live entirely in `cairn-core`/
    `cairn` (the binary edge); the temp path and content are never passed to `cairn-ai`.
  - **Never logged raw**: `RemoteEditFailed`/`WriteBackConflict` carry only redacted messages
    (`VfsError::redacted()`/`TransferError::redacted()`); the temp path appears in the
    `ConfirmWriteback` overlay (shown to the user, who already knows their own filesystem) but never
    in a log line.

## Unresolved questions

1. ~~Whether P2's editor is a from-scratch line-buffer widget or should defer to a "shell out to
   `$EDITOR`..." model~~ — **Resolved for P2**: shell out to `$VISUAL`/`$EDITOR`/`vi`, local files
   only; see "Rationale and alternatives" and ADR-0011.
2. ~~The exact P3 size threshold for refusing to download-and-edit a large remote file~~ —
   **Resolved for P3**: `REMOTE_EDIT_MAX_BYTES = 100 MiB`, a deliberate UX guardrail (see the P3
   reference-level section).
3. Whether `wrap` gets a keybinding — still open, unrelated to P2/P3's scope.
4. Live byte-progress/cancellation for the P3 download/write-back phases, and retry-without-
   redownload after a failed write-back — both deferred as documented follow-ups (see the P3
   reference-level section's "What is deferred").

## Crate, dependency, and feature impact

- `cairn-core`: additive `Action::View`; additive `AppEffect::{SniffFile,OpenPager,ClosePager}`;
  additive `AppEvent::{FileSniffed,SniffFailed,PagerChunk,PagerDone}`; additive
  `Overlay::Pager`; new `FileKind`/`PagerMode`/`PagerStatus`/`PagerId`/`PAGER_MAX_BYTES`/
  `PAGER_HEX_ROW_BYTES`/`detect_file_kind` in `state.rs`. New dependency: `bytes` (already a
  transitive dependency of `cairn-vfs`/`cairn`; used here purely as a data holder — no I/O in
  `cairn-core`).
- `cairn-tui`: `render_pager`/`format_hex_row`; `F3` keybinding; `"view"` config action name.
- `cairn`: `run_sniff_file_effect`/`run_pager_effect` + `pager_controls` control map in
  `event_loop`/`dispatch`, mirroring `log_viewer_controls`.
- No new backend capability, no `Caps` flag, no config schema change in P1.
- **P2 additive:** `cairn-core`: `Action::Edit`; `AppEffect::SuspendAndEdit`;
  `AppEvent::EditFinished`. `cairn-tui`: `F4` keybinding; `"edit"` config action name.
  `cairn`: `run_editor_suspend`/`spawn_editor_hardened`/`resolve_editor_argv`/`InputGate`/
  `EditorRestoreGuard` in `app.rs` (see ADR-0011). New dependency: `shlex` (already a transitive
  dependency present in `Cargo.lock`; pure-Rust, small, audited-by-widespread-use POSIX
  shell-word splitter — chosen over hand-rolling quote/escape parsing for a security-sensitive
  input). No new backend capability, no `Caps` flag, no config schema change in P2 — P2 is
  entirely TUI/runtime work plus the existing `Vfs::local_path` capability the local backend
  already provides.
- **P3 additive:** `cairn-core`: `RemoteVersion`/`RemoteEditId`/`ContentHash`/`WritebackChoice`/
  `WritebackConflictReason`/`REMOTE_EDIT_MAX_BYTES` in `state.rs`; `WriteBackMode` and
  `AppEffect::{DownloadForEdit,EditRemoteTemp,WriteBack,CancelRemoteEdit}` /
  `AppEvent::{RemoteEditNeedsDownload,RemoteEditDownloaded,RemoteEditNoChange,RemoteEditModified,
  RemoteEditFailed,WriteBackConflict,WriteBackDone}` in `msg.rs`; `Overlay::ConfirmWriteback` in
  `state.rs`. `cairn-tui`: `render_confirm_writeback`; a `confirm-writeback` scenario/snapshot
  (no new keybinding — the overlay reuses the existing cursor/confirm/cancel actions). `cairn`:
  `begin_remote_edit`/`run_download_for_edit`/`run_remote_edit_temp`/`run_writeback`/
  `write_temp_to_remote`/`unique_sibling_path`/`new_remote_edit_dir`/`create_temp_edit_file`/
  `hash_file`/`decide_remote_edit_outcome` (+ its `RemoteEditOutcome` result type — the pure,
  directly-tested no-op-vs-modified decision the gate-fix regression tests pin) in `app.rs`;
  `suspend_and_run_editor` factored out of `run_editor_suspend` so P2 and P3 share the terminal
  pause/suspend/spawn/resume sequence; a `remote_edit_temps` control map in `event_loop`, mirroring
  `transfer_controls`/`pager_controls`/`session_controls`. Every remote-edit `AppEffect`/`AppEvent`
  additionally carries `orig_perms: Option<UnixPerms>` (captured from the `v0` `stat`, restored on
  the target after a staging-rename write-back) and `download_hash: ContentHash` (the stable,
  whole-session no-op baseline — see "The flow" above). Two new dependencies in `cairn`: `tempfile`
  (promoted from dev-only — already a normal dependency of `cairn-backend-archive`, same temp-file
  pattern reused here) and `sha2` (already resolved transitively via `wasmtime`/`cranelift` for
  `cairn-plugin`, so this adds zero new crates to the dependency tree, just a new direct consumer).
  No new config schema change; the atomic-vs-direct write-back choice reads the existing
  `Caps::RENAME`, and the mode-restoration fix-up reads the existing `Caps::CHMOD`.
