# RFC-0012: File Open, View & Edit — Read-Only Pager, In-Place Editor, Remote Writeback

- **Status:** Draft (P1 implemented; P2–P3 designed, not yet implemented)
- **Author(s):** tui-engineer, software-architect (synthesized)
- **Date:** 2026-07-02
- **Tracking item:** M4-7 (see `docs/IMPLEMENTATION_PLAN.md`)

## Summary

Cairn can browse every backend but has no way to *look inside* a file. This RFC specifies the
whole file-open experience in three phases:

1. **P1 (this PR): a built-in, read-only pager** (MC's `F3`) — `F3` opens the entry under the
   cursor; `Enter` on a file classifies it (text vs binary) and opens the pager in the matching
   mode (`Text` or `Hex`). No editor yet.
2. **P2 (designed, not implemented): in-place text editing.** `Enter` on a text file (or a new
   `F4`) opens an editable buffer; `Ctrl-S` writes it back through the same `Vfs` the pane is
   browsing.
3. **P3 (designed, not implemented): remote writeback hardening.** Conflict detection (the file
   changed underneath the editor), atomic replace-on-save semantics where the backend supports
   them, and large-file editing limits.

This document covers the full arc so the P1 pager's data model doesn't have to be revisited when
P2/P3 land — the `Overlay::Pager` shape, the `AppEffect`/`AppEvent` naming, and the reducer's
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
- **`Enter` on a file** (previously a no-op) reads a small prefix, classifies it, and opens the
  pager already in the right mode (`Text` for text, `Hex` for binary) — no flip needed, at the
  cost of a short classification round-trip before the overlay appears. `Enter` on a directory is
  unchanged (navigates into it).
- **Pager keys:** `j`/`k`/`↑`/`↓` scroll a line/row; `PageUp`/`PageDown` scroll a page; `g`/`G` (or
  `Home`/`End`) jump to the top/loaded-bottom; `q`/`Esc` closes.
- **Title bar** shows the filename, a `line/total (pct%)` position (pct only when the backend
  reports the entry's size), and a status (`Loading…` / `Ready` / `Truncated — showing first N` /
  `Error: …`).
- **Hex mode** renders classic `offset | hex bytes | ascii` rows, 16 bytes per row.
- **No editor yet.** Enter-on-text opens the same read-only pager as Enter-on-binary; there is no
  `F4`/edit action in P1. See P2.

### What doesn't change

- `Enter` on a directory: unchanged (navigates).
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

## Reference-level explanation (P2 — designed, not implemented)

**Goal:** `Enter` on a `FileKind::Text` result routes to a new `Action::Edit` instead of (or in
addition to) the read-only pager; `F4` opens the editor directly, mirroring MC.

Sketch (not implemented in this PR — every touch point below is marked `// P2:` in the P1 code):

- A new `Overlay::Editor` holding an editable line buffer (likely `Vec<String>` plus a cursor
  `(row, col)`), reusing the pager's streaming-load path to populate the initial content (bounded
  by an editable-size cap, smaller than `PAGER_MAX_BYTES` — editing an 8 MiB buffer line-by-line
  in a `TextEdit`-per-keystroke model is a different performance problem than paging one).
- `TextEdit` gains cursor-motion and line-editing variants (or the editor captures raw `KeyEvent`s
  directly, bypassing `TextEdit`'s single-line-field model — single-line assumptions run
  throughout today's `TextEdit`, e.g. `Backspace`/`Insert(char)`/`Submit`, and a multi-line buffer
  needs at minimum arrow-key motion and a newline-insert, which don't fit that shape cleanly).
- `Ctrl-S` (or a `PromptKind`-style confirm) emits a new `AppEffect::SaveFile { conn, path,
  contents }`; the effect runner calls `Vfs::open_write` and streams the buffer back.
- Binary files never route to the editor (P1's `FileKind::Binary` keeps going to the read-only hex
  pager) — Cairn is not a hex editor.
- Every backend's `Vfs::open_write` already exists (used by the transfer engine), so P2 is mostly
  TUI/reducer work plus a size-gated "load fully, then write fully" contract — no new backend
  capability is required for the *local* happy path. Remote conflict handling is P3.

## Reference-level explanation (P3 — designed, not implemented)

**Goal:** make P2's writeback safe on backends where "load fully, edit, write fully" is unsafe
(the file changed underneath you) or expensive (very large remote files).

Sketch:

- **Conflict detection:** capture the entry's `etag`/`modified` at load time (already present on
  `Entry`); before `SaveFile` commits, re-`stat` and compare. A mismatch surfaces a
  `ConfirmOverwrite`-style overlay (an existing pattern — `Overlay::ConfirmOverwrite` already
  covers the analogous transfer-collision case) rather than silently clobbering a concurrent
  change.
- **Atomic replace where the backend supports it:** local/SFTP can write to a temp path and
  rename; object stores are naturally atomic per-object (a `PUT` fully replaces); this is a
  per-backend capability, likely a new `Caps` flag, not a core reducer concern.
- **Size limits:** the editor should refuse (with a clear status message) to load a file above
  some cap on backends without cheap ranged writes, rather than hanging on a multi-hundred-MB
  load. Exact threshold is a UX call for `ux-engineer` at P3 implementation time.
- **Whether P3 lands before or after other backends is a v2 scheduling call**, not architecturally
  blocking — it can start as soon as P2 lands.

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

## Security and privacy considerations

- Pager content is file bytes, potentially containing secrets embedded in a config file, `.env`,
  or similar — it is **never forwarded to the AI layer** (no `AppEffect`/`AppEvent` in this RFC is
  reachable from `cairn-ai`'s dependency closure) and **never logged raw**; only redacted
  `error: String` messages and byte counts appear in any `Debug`/status line.
- All I/O errors are redacted via the existing `VfsError::redacted()` convention before reaching
  `AppEvent`/the status line — no host names, paths beyond what the user already navigated to, or
  credential material.
- P2/P3 (writeback) will need a fresh look from `security-engineer` before landing — a save path
  is new attack surface (e.g. symlink races on local backends) that the read-only P1 pager does
  not have.

## Unresolved questions

1. Whether P2's editor is a from-scratch line-buffer widget or should defer to a "shell out to
   `$EDITOR` on a local temp copy, then upload" model for remote backends (simpler for P2, but
   breaks the keyboard-first non-blocking-UI principle for remote files and duplicates local disk
   I/O) — needs `ux-engineer` + `tui-engineer` input before P2 starts.
2. The exact P3 size threshold for refusing to load a file into the editor.
3. Whether `wrap` gets a keybinding before or alongside P2.

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
