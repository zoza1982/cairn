# ADR-0005: Shell-command actions (declarative config, no shell)

- **Status:** Accepted
- **Date:** 2026-06-28
- **Deciders:** maintainer (with security-engineer + software-architect review)

## Context

M8-7 calls for declarative config extensions including **shell-command actions**: a user binds a key
to run a local program against the entry under the cursor (e.g. checksum, compress, lint). This is
the first feature in Cairn that executes arbitrary local processes, so it must be designed
security-first.

Forces and constraints:

- Cairn is a secrets-handling tool; the bar for process execution is high (CLAUDE.md §9/§11).
- The model is virtual: every pane is an `Arc<dyn Vfs>` over `VfsPath`. Only local backends map to a
  real OS path; a shell command needs that real path.
- `#![forbid(unsafe_code)]` workspace-wide.
- CI runs `--all-features` tests on Linux/macOS/**Windows**; exec tests must be hermetic and offline.
- Interactive programs (an editor) require suspending the TUI and a real TTY, which cannot be verified
  hermetically — out of scope for this slice.

## Decision

We add **non-interactive** shell-command actions with the following load-bearing decisions:

1. **argv-only, no shell — ever.** Programs run via `std::process::Command`/`tokio::process::Command`
   with an argv vector; never `sh -c`/`cmd /c`, never a single command-line string. There is no
   `command`-string field. Expanded placeholders (`{path}`, `{dir}`, `{name}`) are substituted *within*
   an argv element and never re-split or re-parsed, so a filename containing shell metacharacters is an
   inert literal argument. This invariant is security-critical; relaxing it requires a new
   security-review.
2. **Local-only via `Vfs::local_path`** (ADR-adjacent, see PR #57): an action runs only where
   `local_path` returns `Some` — i.e. a local backend whose canonical target is confined under its
   root. Non-local backends and symlink-escapes return `None` and are refused. The canonical (real)
   path is passed to argv.
3. **Config file-trust gate.** Because `config.toml` can now define executable actions, it is no
   longer "always safe to share". `Config::secure_shell_actions` drops the `[[shell_actions]]` section
   (with a warning) when the file is group-/world-writable or not owned by the current user (Unix).
4. **Confirm before run, by default.** Each action shows a confirm overlay (program name + target)
   before executing; `confirm = false` is an explicit per-action opt-out.
5. **Process sandbox.** `env_clear()` then a minimal non-secret allow-list (sanitized `PATH` with no
   `.`/empty entries, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_*`, `TZ`, `TMPDIR`, plus `CAIRN_ACTION`
   set to the action name — never secrets/vault material); explicit cwd (the entry's directory);
   `stdin` closed; stdout/stderr
   captured (not inherited) and byte-capped; a wall-clock timeout after which the whole process group
   is killed (Unix `process_group(0)` + `kill_process_group`; elsewhere `kill_on_drop`).
6. **Program rules.** Absolute path or bare name (resolved via the sanitized PATH); a relative path
   with a separator (`./x`, `bin/x`) and `.bat`/`.cmd` are rejected at config validation.
7. **Output is never echoed or sent to the AI layer.** It may contain secrets; only a redacted
   summary (exit code / byte count / "timed out") reaches the status line.
8. **Index alignment.** The keymap (`Action::RunShellAction(idx)`), `AppState::shell_actions`, and the
   runtime's `ShellActionDef` list are all built from one validated list, in order, so the index can
   never address the wrong action.

The pure reducer stays pure: it validates "something is selected", opens the confirm overlay or emits
`AppEffect::RunShellAction { index, conn, target }`, and never resolves real paths or command details.
The runtime resolves `local_path`, expands templates, and spawns.

## Consequences

### Positive
- Useful extensibility (checksum/compress/lint/…) with no plugin or recompile.
- Defense-in-depth: no shell, local-only + confinement, file-trust gate, env scrub, confirm, timeout,
  output caps — each closes a distinct class of attack.
- `Vfs::local_path` is reusable for any future feature needing a confined real path.

### Negative / trade-offs
- Non-interactive only: editors and pagers (the most-wanted actions) are deferred.
- The file-trust gate and process-group kill are Unix-only for now; Windows enforces less.
- Output is a byte-count summary, not shown — a viewer overlay is a follow-up.
- Accepted residual risks: argv reaching the *target program's own* option parser (mitigated with
  `--`/absolute paths + docs, not eliminated); TOCTOU between `local_path` and spawn; the chosen
  program is trusted once selected; non-UTF-8 paths are passed lossily.

### Neutral
- Adds a small `rustix` (unix, `process`) dependency — already in the tree — for a safe `getuid` and
  process-group kill without `unsafe`.

## Alternatives considered

- **Allow `sh -c` / a command string** — rejected: reintroduces shell-injection via filenames; the
  single most dangerous choice.
- **Runtime `conn → root` map or downcast to `LocalVfs`** instead of `Vfs::local_path` — rejected
  (PR #57): the containment authority must live with the backend that owns the root; a map can desync
  and a downcast is brittle and bypasses the per-path check.
- **No confirm (config keybind = consent, the vim/ranger/lf model)** — softened: confirm defaults on
  but is opt-out per action, keeping the security default while honoring the config-as-consent model.
- **Gate on vault-locked state** — rejected: an unlocked vault is normal; non-exposure is enforced
  structurally (env scrub, no secrets in argv), so a vault interlock adds no security and hurts UX.
- **Interactive (TUI-suspend) in this slice** — deferred: cannot be verified hermetically/offline.

## References

- PR #57 (`Vfs::local_path` capability), this PR (shell actions).
- `docs/IMPLEMENTATION_PLAN.md` M8-7; CLAUDE.md §9/§11.
- security-engineer + software-architect design notes (synthesized in the implementing PR).

<!--
  ADRs are immutable once Accepted. To change a decision, write a new ADR that supersedes this one
  and update the Status line above to "Superseded by ADR-XXXX".
-->
