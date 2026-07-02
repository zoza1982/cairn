# RFC-0012: File Open, View & Edit — Read-Only Pager, In-Place Editor, Remote Writeback

- **Status:** Draft (P1 + P2 implemented; P3 designed, not yet implemented)
- **Author(s):** tui-engineer, software-architect (synthesized)
- **Date:** 2026-07-02
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
3. **P3 (designed, not implemented): remote writeback hardening.** Conflict detection (the file
   changed underneath the editor), atomic replace-on-save semantics where the backend supports
   them, and large-file editing limits — the from-scratch in-app buffer editor originally sketched
   for P2 was superseded by the `$EDITOR`-shell-out design once ux-engineer/tui-engineer weighed in
   (see "P2 — implemented" below); P3 now covers making that shell-out safe for non-local backends.

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
- **Local files only.** Editing a file on a remote backend (SSH, S3, a container, …) shows
  *"Editing remote files lands in P3 — copy it to a local pane to edit"* and does nothing else —
  the TUI is never disturbed for a file that can't be edited this way yet.
- After the editor exits, the active pane's listing refreshes (the file's size/mtime may have
  changed) and the status line shows the outcome (`edited <name>`, or a redacted failure).

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

## Reference-level explanation (P3 — designed, not implemented)

**Goal:** extend P2's `$EDITOR` shell-out to remote backends safely — "load fully, edit, write
fully" is unsafe if the file changed underneath you, and expensive for very large remote files.

Sketch:

- **Temp-copy-edit-upload**, not an in-app buffer: download the remote file to a local temp path
  (reusing the transfer engine's download path), run the same `spawn_editor_hardened` flow against
  the temp copy, and on a clean exit upload it back (reusing the transfer engine's upload path).
  This keeps P3 architecturally consistent with P2's "the real `$EDITOR`, on a real local file"
  model instead of reviving the previously-sketched in-app line-buffer editor.
- **Conflict detection:** capture the entry's `etag`/`modified` at download time (already present
  on `Entry`); before uploading the edited temp copy back, re-`stat` the remote original and
  compare. A mismatch surfaces a `ConfirmOverwrite`-style overlay (an existing pattern —
  `Overlay::ConfirmOverwrite` already covers the analogous transfer-collision case) rather than
  silently clobbering a concurrent change.
- **Atomic replace where the backend supports it:** local/SFTP can write to a temp path and
  rename; object stores are naturally atomic per-object (a `PUT` fully replaces); this is a
  per-backend capability, likely a new `Caps` flag, not a core reducer concern.
- **Size limits:** refuse (with a clear status message) to download-and-edit a file above some cap
  on backends without cheap ranged writes, rather than hanging on a multi-hundred-MB round trip.
  Exact threshold is a UX call for `ux-engineer` at P3 implementation time.
- **Temp-file hygiene:** the downloaded copy must live in a per-session, permission-restricted temp
  directory and be cleaned up after upload (or on error) — a security-engineer pass is required
  before this lands, per RFC-0012's Security section below.
- **Whether P3 lands before or after other backends is a v2 scheduling call**, not architecturally
  blocking — it can start as soon as P2 lands (now merged).

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
- **P3 (writeback to remote backends)** will need a fresh `security-engineer` pass before landing —
  a temp-copy-edit-upload path is new attack surface (temp-file permissions/predictability, upload
  race windows, conflict-detection correctness) that P2's local-only, no-network-I/O editor launch
  does not have.

## Unresolved questions

1. ~~Whether P2's editor is a from-scratch line-buffer widget or should defer to a "shell out to
   `$EDITOR`..." model~~ — **Resolved for P2**: shell out to `$VISUAL`/`$EDITOR`/`vi`, local files
   only; see "Rationale and alternatives" and ADR-0011.
2. The exact P3 size threshold for refusing to download-and-edit a large remote file.
3. Whether `wrap` gets a keybinding — still open, unrelated to P2's scope.

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
