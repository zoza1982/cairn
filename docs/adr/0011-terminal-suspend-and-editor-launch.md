# ADR-0011: Terminal suspend + external-editor launch (RFC-0012 P2)

- **Status:** Accepted
- **Date:** 2026-07-02
- **Deciders:** tui-engineer (design; cross-checked by rust-staff-engineer, security-engineer per
  CLAUDE.md §2)

## Context

RFC-0012 P1 shipped a read-only pager (`F3`, `Enter`-to-view). P2 adds in-place editing of local
files: a new `F4` (`Action::Edit`) always opens an external editor, and `Enter` on a
`FileKind::Text` result now routes to the editor instead of the pager. This is the highest-risk
phase of the RFC: it requires Cairn to give up exclusive control of the real terminal — raw mode,
the alternate screen, and stdin — to a foreign, long-lived, interactive child process, then get it
all back cleanly whatever that child does (exits cleanly, crashes, is killed, or Cairn itself
panics mid-edit).

Three problems had to be solved together:

1. **Where does the suspend/resume happen?** The existing effect runner (`dispatch`, in
   `crates/cairn/src/app.rs`) has no `&mut Terminal` and runs every effect concurrently via
   `tokio::spawn` — neither property is compatible with "own the terminal exclusively for the
   duration of one specific effect."
2. **Who else is reading stdin?** `spawn_input_reader` runs a blocking OS thread looping on
   `crossterm::event::poll`/`read`. Left running, it races the editor for every keystroke the user
   types into it.
3. **How do we get the terminal back correctly?** `ratatui::init()` (used once at startup) both
   enables raw mode/the alt screen *and* installs a panic hook — calling it a second time on resume
   would stack a redundant panic-hook closure on every edit (a slow memory leak over a long Cairn
   session) and re-run initialization work that doesn't need to be redone.

## Decision

We special-case exactly one effect, `AppEffect::SuspendAndEdit`, in the `event_loop`'s
per-effect loop in `crates/cairn/src/app.rs` — it is **not** routed through `dispatch`. This is the
only effect in the codebase that needs `&mut Terminal` and must run to completion before the next
effect is dispatched (every other effect either does no terminal I/O, or is a fire-and-forget
`tokio::spawn` that reports back via an `AppEvent`).

### Sequence (`run_editor_suspend`)

1. **Resolve the local path first**, entirely before touching the terminal. `Vfs::local_path`
   (via `spawn_blocking`, since it may `canonicalize`) returns `None` for a remote backend, an
   escapes-the-confined-root path, *or* a local path that won't resolve (a dangling symlink) — either
   way we refuse, sending `AppEvent::EditFinished { error: true, status: "Cannot edit this file —
   only local, resolvable files are editable (remote editing lands in P3)" }` without the TUI ever
   flickering. Resolving `$VISUAL`/`$EDITOR`/`vi` and
   splitting it (`shlex`) also happens here, before any terminal state changes, for the same
   reason: a misconfiguration is reported cleanly.
2. **Pause the input reader and wait for its ack.** A new `InputGate` (`Arc<Mutex<ReaderState>>` +
   `Condvar`, three states: `Running`/`PauseRequested`/`Paused`) is shared between
   `spawn_input_reader`'s blocking thread and the async suspend path.
   `InputGate::request_pause()` sets `PauseRequested`, notifies, and performs a real
   `Condvar::wait` loop until the reader flips to `Paused` — run inside `spawn_blocking` since this
   is genuine OS-thread blocking, never `.await`ed directly on a tokio worker. The reader checks
   the gate once per loop iteration, strictly between a `poll`/`read` pair and the next `poll` —
   never mid-`read()` — so a pause request can never interrupt an in-flight read; on
   `PauseRequested` it acks (`Paused`) and parks in `cv.wait` with zero stdin access until
   `resume()`.
3. **Suspend:** `ratatui::restore()` — leaves the alternate screen and disables raw mode.
4. **Spawn the hardened editor and await it in the foreground.** This is the one deliberate,
   documented exception to "the render path never blocks" (CLAUDE.md §9): the whole point of this
   effect is that the editor exclusively owns the TTY while it runs. The runtime is confirmed
   multi-threaded (`tokio::runtime::Builder::new_multi_thread`), so no `block_in_place` is needed —
   the `.await` simply parks this one task; other spawned tasks (an in-flight transfer's progress,
   say) keep running and their `AppEvent`s queue in the bounded channel, replayed once the loop
   resumes.
5. **Resume:** manual `enable_raw_mode()` + `execute!(stdout(), EnterAlternateScreen)` +
   `terminal.clear()` — **not** a second call to `ratatui::init()` (see Alternatives). The full,
   non-diffed `clear()` matters because the terminal may have been resized while the editor had it,
   and the editor's own screen contents are sitting in a blind spot ratatui's diff buffer doesn't
   know about — a plain `terminal.draw()` without `clear()` first would diff against stale
   assumptions and could leave stray fragments on screen.
6. Resume the input reader (`InputGate::resume()`), then send `AppEvent::EditFinished{status,
   error}`. The reducer sets the status line and, on success, re-emits the active pane's `List`
   effect (the file's size/mtime may have changed) via the same refresh path `Action::Refresh` uses.

### RAII restore guard

`EditorRestoreGuard` is constructed immediately after the suspend (step 3) and disarmed only after
the explicit resume (step 5) has run. If anything between those two points panics or returns early,
its `Drop` re-enters raw mode + the alternate screen (best-effort, no `Terminal::clear()` since
Drop holds no `&mut Terminal`) and — critically — always calls `InputGate::resume()`. Skipping the
latter would park the reader thread forever: the *only* source of terminal input events, meaning
the whole app goes permanently deaf to the keyboard — a hang, not a cosmetic glitch. This is the
invariant the guard exists to make unconditional.

We accept one known, narrow edge case: on an actual panic, `install_terminal_panic_hook`'s hook
runs *before* unwinding begins and calls `ratatui::restore()` (idempotent — harmless whether or not
we'd already suspended) and prints the panic message. If the panic occurred inside the guarded
region, the guard's `Drop` then fires *during* unwind, *after* the hook, and would re-enter raw
mode/alt-screen right before the process exits — potentially leaving the user's terminal in a
raw-mode state the OS pty doesn't reset by itself. This is a pre-existing class of risk for any
raw-mode TUI that suspends around a foreign child process, not something introduced or made worse
by this design; we chose to keep the guard's input-reader-resume unconditional (preventing a
guaranteed hang) rather than special-case "don't touch the terminal if we're unwinding from a
panic" (which needs cross-cutting panic-state detection this crate doesn't have). Tracked as an
accepted trade-off, not a follow-up — a genuine panic here has no other code path exercising it
today, and `spawn_editor_hardened`/the argv-resolution helpers have no panicking paths.

### Security hardening (`spawn_editor_hardened`)

Modeled on the existing `spawn_shell_action`/`sanitized_path` hardening
(`docs/adr/0005-shell-command-actions.md`), adapted for a long-lived, TTY-inheriting child instead
of a piped, timeout-bounded one:

- **argv only, never a shell**: `Command::new(program).args(fixed_args).arg("--").arg(abs_path)`.
  `program` and `fixed_args` come from splitting `$VISUAL`/`$EDITOR` via `shlex::split` — POSIX
  shell-word quoting only (so `code --wait` and `emacs -nw` work), never glob/variable/command-
  substitution expansion. A value like `vi "$(id)"` splits to the literal argv `["vi", "$(id)"]`;
  `$(id)` is inert text, never executed. `shlex::split` returns `None` on malformed quoting
  (unterminated quote/trailing backslash), which we surface as a clear, non-panicking error.
- **`--` terminator + an always-absolute path**: `abs_path` is the canonical path from
  `Vfs::local_path`, and `--` precedes it, so a file literally named `-c :!sh` is passed as a plain
  filename argument, never parsed as a flag.
- **`env_clear()` + an allow-list** (`EDITOR_ENV_ALLOWLIST`: `HOME, USER, LOGNAME, LANG, LC_*, TZ,
  TMPDIR, TERM, COLORTERM, TERMINFO, SHELL, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_CACHE_HOME,
  XDG_RUNTIME_DIR`) plus a sanitized `PATH` (`sanitized_path()`, reused as-is — strips `.`/empty
  entries so the editor can't resolve a bare program name against the directory being browsed).
  Everything else — `AWS_*`, `GOOGLE_*`, `AZURE_*`, `GITHUB_TOKEN`, `VAULT_*`, `CAIRN_*`,
  `SSH_AUTH_SOCK`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_*` — is dropped and never reachable by the
  child; vault material is never assembled into an editor-launch effect in the first place (this
  path only ever carries a `ConnectionId` + `VfsPath`).
- **Shares Cairn's process group** (i.e. we deliberately do *not* call `process_group(0)` for the
  editor, unlike `spawn_shell_action`). The editor inherits the controlling TTY and *reads* it; a
  process in a **background** process group that reads the terminal is sent `SIGTTIN` (default:
  stop). Reparenting the editor into its own group — without a full `tcsetpgrp` foreground handoff —
  would leave it in the background and freeze it (and Cairn, parked in `wait().await`) on the first
  keystroke. `spawn_shell_action` can safely reparent only because its child never touches the TTY
  (piped stdio). The editor therefore stays in Cairn's group, which is the terminal's foreground
  group, and reads without `SIGTTIN`. Interactive editors put the terminal into their own raw mode
  (ISIG off), so a Ctrl-C during editing reaches the editor as a keystroke, not a `SIGINT`; the only
  residual exposure is the sub-millisecond cooked-mode windows around spawn/exit — the same accepted
  trade-off every program that shells out to `$EDITOR` (e.g. `git`) lives with.
- **The `--` end-of-options terminator is Unix-only.** Unix editors honor it (so a file named like a
  flag can't inject), but Windows GUI editors (`notepad.exe`) don't and would open a file literally
  named `--`; the target path is always absolute (`C:\…`) on Windows anyway, so it can't be mistaken
  for a flag there.
- **`cwd` = the file's own parent directory** (from the resolved, canonical path) — never an
  attacker-influenced working directory.
- **Inherited stdio, no timeout**: unlike `spawn_shell_action` (piped output, capped, wall-clock
  timeout), the editor needs the real TTY and is expected to run indefinitely while the user edits.
- **No `unwrap`/`expect`/`panic!`** on any reachable path here: an unset `$EDITOR` on Windows, a
  missing `vi` on a minimal Unix install, malformed quoting, or a spawn failure all become a typed
  `Err(String)` surfaced as `AppEvent::EditFinished{error: true, ..}` — the TUI stays alive and
  fully restored either way.

## Consequences

### Positive

- The editor gets a completely normal, unmediated terminal session — full-screen editors (vim,
  emacs, nano, VS Code's `--wait` mode) work exactly as they would from a shell, because that is
  effectively what they're getting.
- No new attack surface beyond what `spawn_shell_action` already established and this PR's tests
  exercise the same class of hardening (argv-not-shell, deterministic split, `--` terminator,
  env-scrub, secret-canary) against the new, TTY-inheriting spawn path.
- The input-reader pause/resume is a general mechanism (`InputGate`) — any future feature that
  needs exclusive TTY ownership (e.g. a `!`-shell-out command, RFC-0012 P3's remote-edit flow) can
  reuse it verbatim.

### Negative / trade-offs

- **One documented blocking `.await` in `event_loop`.** For the duration of an edit, the event
  loop cannot process the *next* effect or redraw — by design, since the editor owns the TTY. Other
  spawned tasks keep running; their events queue (bounded channel, 256 deep) and are replayed once
  the loop resumes. Because `run_editor_suspend` runs inline on the sole `event_rx` consumer, it must
  never itself *await* a send on that bounded channel — if the channel were momentarily full, the
  send would park waiting for capacity that only this same (parked) task can free: a permanent
  deadlock. All its status/`EditFinished` sends therefore go through `send_event_detached`, which
  spawns the send on a separate task so `run_editor_suspend` returns, the loop resumes and drains,
  and the detached send completes. A background event storm during a long edit can still transiently
  fill the channel, but that only delays (never deadlocks) the resume, and `try_send` producers
  (transfer progress) already tolerate drops.
- **Reader-thread death is handled, not hung.** If the input-reader thread exits (a `poll`/`read`
  error, or the controlling terminal dropping), it marks the `InputGate` `Dead`; a subsequent edit's
  `request_pause` then returns `Err` and the edit aborts cleanly instead of blocking forever on an
  acknowledgement that can never arrive.
- **The panic-during-suspend edge case** described above under "RAII restore guard" is accepted,
  not fully closed.
- P2 is **local-only**: a remote file refuses cleanly with a message pointing at P3. Users editing
  a remote file today must copy it to a local pane first.

### Neutral

- `InputGate` uses `std::sync::{Mutex, Condvar}`, not tokio's async equivalents — deliberate, since
  the reader is a real OS thread doing blocking I/O, not a tokio task.

## Alternatives considered

- **Re-call `ratatui::init()` on resume** instead of manual `enable_raw_mode` +
  `EnterAlternateScreen` + `terminal.clear()`. Rejected: `init()` also calls
  `std::panic::set_hook`, so every edit would stack a new closure around the previous one — a
  slow memory leak over a session with many edits, plus N redundant terminal restores chained in
  the panic path by the time the Nth edit happens. `install_terminal_panic_hook` is called exactly
  once at startup; `ratatui::restore()`/manual re-init is the correct suspend/resume pair when the
  hook is already installed.
- **Route `SuspendAndEdit` through the normal `dispatch`/`tokio::spawn` effect runner**, having the
  spawned task somehow signal back for exclusive terminal access. Rejected: `dispatch` has no
  `&mut Terminal` to hand out, and effects are designed to run concurrently — retrofitting mutual
  exclusion (a mutex around the terminal, cross-task signaling to pause rendering) would be far
  more complex than special-casing the one effect that structurally needs different treatment,
  directly in the one place (`event_loop`) that already owns both the terminal and the loop
  sequencing.
- **Leave the input reader running and rely on the editor's own terminal handling to "win" the
  race for keystrokes.** Rejected outright — this is a real, user-visible bug class (dropped/
  duplicated/misrouted keystrokes between Cairn and the child), not a theoretical one; every
  suspend/resume TUI pattern in the ecosystem (e.g. how shells suspend for `$PAGER`/`$EDITOR`) deals
  with this by ensuring only one reader owns stdin at a time.
- **Shell out to `$EDITOR` on a local temp copy, then upload** (the RFC-0012 "unresolved question"
  for remote backends) — out of scope for P2 (local-only); revisit for P3 alongside conflict
  detection.
- **Hand-roll a minimal shell-word splitter instead of the `shlex` crate.** Rejected: `shlex` is
  already a transitive dependency (present in `Cargo.lock` via existing build tooling), pure-Rust,
  small, and has exactly the API needed (`shlex::split(&str) -> Option<Vec<String>>`, `None` on
  malformed quoting) — reimplementing that correctly (backslash escapes, single/double quote
  semantics, comment stripping) is exactly the kind of security-sensitive parsing better left to an
  audited, widely-used crate than reinvented for this PR.

## References

- RFC-0012 (`docs/rfcs/0012-file-open-view-edit.md`) — P1 (read-only pager, merged), P2 (this
  ADR), P3 (remote writeback, still designed only).
- `docs/adr/0005-shell-command-actions.md` — the `spawn_shell_action`/`sanitized_path` hardening
  pattern this design extends to a TTY-inheriting, untimed child.
- `crates/cairn/src/app.rs`: `run_editor_suspend`, `spawn_editor_hardened`, `InputGate`,
  `EditorRestoreGuard`, `resolve_editor_argv`.
- `crates/cairn-core/src/msg.rs`: `Action::Edit`, `AppEffect::SuspendAndEdit`,
  `AppEvent::EditFinished`.
