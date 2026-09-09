<div align="center">

# 🪨 Cairn

**A modern terminal file manager for every filesystem.**

A Midnight Commander successor, written in Rust — where every dual-pane is a
virtual filesystem: local disk, SSH/SFTP, S3, GCS, Azure Blob, Docker, and Kubernetes.
With a secure secrets vault and an agentic AI assistant.

[![CI](https://github.com/zoza1982/cairn/actions/workflows/ci.yml/badge.svg)](https://github.com/zoza1982/cairn/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue.svg)](#license)
[![Status](https://img.shields.io/badge/status-pre--alpha-orange.svg)](#status)

</div>

> **Status:** 🚧 Alpha. **Working today:** an interactive dual-pane **local** file manager (browse,
> navigate, sort, mark) with **configurable keybindings**, a cross-backend **transfer engine**
> (copy/move/delete with a confirm dialog), an encrypted **secrets vault** (XChaCha20-Poly1305 +
> Argon2id) behind a capability **broker**, TOML **config**, an **AI plan → confirm** overlay, a
> read-only **file pager** + in-place `$EDITOR` editing (RFC-0012), and read-only **archive
> browsing** — `Enter` on a `.tar`/`.zip`/`.tgz`/`.tbz2`/`.tzst` mounts it like a directory
> (RFC-0013, behind the `archive` feature; `.txz` is recognized but not yet decoded).
> **Backend mapping cores** for **SSH/SFTP**, **object stores** (S3/GCS/Azure-shaped), **Docker**, and
> **Kubernetes** are implemented against a transport seam and fully unit-tested with in-memory mocks,
> and the **WASM plugin host** (wasmtime, resource-limited, default-deny) runs sandboxed modules.
> **SFTP reads and writes stream** (bounded memory, progress that tracks the wire), verified against
> a real OpenSSH `sftp-server`; object stores still buffer whole objects until multipart lands.
> **Still integration-bound** (need live services + heavy SDKs/TLS): the live SSH/cloud/cluster
> transports, the HTTP LLM providers, and the WASM component-model bridge. See
> [`docs/PRD.md`](docs/PRD.md), [`docs/LLD.md`](docs/LLD.md), and the live
> [`docs/IMPLEMENTATION_PLAN.md`](docs/IMPLEMENTATION_PLAN.md).

---

## Why Cairn?

Terminal users juggle a fragmented toolbox: `mc` for local files, `aws s3`/`gsutil`/`az` for
object stores, `kubectl` for clusters, `docker` for containers, `scp`/`sftp` for remotes — each
with its own syntax and auth model. Moving a file from a Kubernetes pod to an S3 bucket means
stitching several tools together by hand.

**Cairn unifies these workflows behind one consistent, modern, keyboard-driven TUI**, with
credentials handled safely and an AI layer that turns intent into reviewed, executed actions.

## Highlights (planned)

- 🗂️ **Dual-pane, MC-faithful UX** — familiar muscle memory, switchable vim/custom keybinds.
- 🌐 **Every pane is a backend** — local, SSH/SFTP, S3, GCS, Azure Blob, Docker, Kubernetes.
- 🔁 **Cross-backend operations** — copy/move/diff between *any* two backends (pod → bucket, etc.).
- 🔐 **Secure secrets vault** — encrypted credentials, OS-keychain-protected, no plaintext ever.
- 🤖 **Agentic AI** — natural-language intent → a reviewable plan → confirmed execution.
- ⚡ **Async & responsive** — the UI never blocks on slow networks or huge listings.
- 🧩 **Extensible** — sandboxed WASM plugins for custom backends, viewers, and actions.
- 💻 **Truly cross-platform** — Linux, macOS, Windows, and headless/remote terminals.

## Documentation

| Doc | Purpose |
|-----|---------|
| [PRD](docs/PRD.md) | Product requirements — *what* and *why* (high-level) |
| [LLD](docs/LLD.md) | Low-Level Design — architecture & technical design |
| [Implementation Plan](docs/IMPLEMENTATION_PLAN.md) | Milestones, sequencing, and the living progress tracker |
| [ADRs](docs/adr/) | Architecture Decision Records |
| [RFCs](docs/rfcs/) | Design proposals for non-trivial work |
| [Reviews](docs/reviews/) | Dated quality reviews — findings, what was fixed, what is still open |
| [Contributing](CONTRIBUTING.md) | How to build, branch, and submit changes |

## Building

> Requires the Rust toolchain (see [`rust-toolchain.toml`](rust-toolchain.toml)).

```sh
cargo build --workspace
cargo run -p cairn
```

## Configuration

Cairn reads an optional TOML config from the platform config directory (e.g.
`~/.config/cairn/config.toml` on Linux); a missing or unreadable file falls back to defaults.

Keys can be remapped under `[ui.keybindings]` — a map of key-chord → action, layered over the
built-in scheme. Chords combine optional `ctrl+`/`alt+`/`shift+` modifiers with a key (a single
character, a named key like `enter`/`space`/`esc`/`tab`/arrows, or `f1`–`f24`); actions are
snake_case (`cursor_down`, `copy`, `move`, `delete`, `ai_propose`, `cycle_sort`, `toggle_hidden`,
`filter`, `make_dir`, `rename`, `calculate_size`, `view`, `edit`, `open_queue`, `toggle_pause`,
`background`, `quit`, …). Unrecognized entries are ignored with a warning, and `Ctrl-C` always quits.

Each entry shows its Unix permissions (`drwxr-xr-x`) and last-modified date next to the name,
right-aligned MC-style; on a narrow pane the columns drop out responsively — permissions first,
keeping the date as long as it fits — so the name keeps priority, and are blank for backends that
don't expose that metadata. Dates are shown in UTC (`YYYY-MM-DD`). By default `s` cycles the active pane's sort order (name → size → modified →
type) and `.` toggles whether hidden entries (dotfiles) are listed; the current sort mode and hidden
state show in each pane's bottom-right corner, and the volume's **free disk space** (e.g.
`137.0 GiB free`) shows in the bottom-left for backends that report it (local now; SSH is a
follow-up). `Ctrl-R` reloads the active pane. `F7` creates a new
directory and `r` renames the entry under the cursor (`F2` is an alias; both
open a text prompt; `Enter` confirms, `Esc` cancels). `Ctrl-S` recursively calculates the size of
the folder under the cursor and shows it in a stats popup (total size, file and subfolder counts);
the walk runs in the background with live totals, and `Esc` cancels it and closes the popup. `v` opens a read-only pager on the entry
under the cursor (`F3` alias), auto-detecting text vs. binary content and switching to a hex view for the
latter. `e` opens the entry under the cursor in an external editor (`F4` alias) — `$VISUAL`, then `$EDITOR`,
then `vi` (Unix only; on Windows, set one of the two first); `Enter` on a text file opens the same
editor, while `Enter` on a binary file still opens the read-only hex pager. Editing works on
**every backend**, local or remote: a remote file is downloaded to a private temp copy, edited
there, and written back after a conflict check (has the remote file changed since you started
editing?) — with size limits and a confirm prompt if the remote drifted or the local edit came
back empty (see [RFC-0012](docs/rfcs/0012-file-open-view-edit.md)).
`Space` (or `Insert`) marks the entry under the cursor, and copy/move/delete act on every marked
entry rather than just the cursor. Marks are **positions in the current listing**, so they are
cleared whenever the listing is replaced — a refresh, a completed operation, or a change to the
filter. That is deliberate: after a refresh the old positions point at whatever now occupies them,
and acting on them would touch files you never selected.
`/` filters the listing as you type (`Enter` keeps the filter, `Esc` clears it). Copying or moving
files auto-opens an MC-style transfer dialog — a progress bar, byte count, rate, and ETA per active
transfer, plus the pending queue. `↑`/`↓` select a row (active transfer or pending item); `p`
pauses/resumes the **selected** active transfer and `d` cancels it (or drops it if it's a pending
item); `Esc` aborts **all** active transfers (the panic-stop); `b` sends the dialog to the
background (transfers keep running, the status line keeps its compact summary) and `Ctrl-T` brings
it back to the foreground. For the pending queue, `K`/`J` reorder and `x` clears it. The dialog
dismisses itself once the last transfer finishes and nothing remains queued. Up to two transfers
run at once by default — set `[transfers] concurrency = N` in config to change it.

**Quitting while a transfer is running asks first** — `q` sits one key from `p` and `b` on that
dialog, and quitting tears the copy down mid-write. A second `q` confirms. A running *delete* has
no half-written state to leave behind, so it does not prompt.

What a copy does and does not carry:

- **Only files and directories.** A symlink, socket, device or FIFO inside a copied tree is
  **skipped** and counted as skipped, never opened — reading a FIFO blocks until something writes to
  it, and a symlink pointing at a directory used to fail the whole transfer part-way. Recreating
  links needs a VFS operation Cairn does not have yet.
- **A move never deletes what it did not copy.** If any file under the source was skipped (a
  conflict policy that keeps the destination, say), the source is left alone. Losing a move is
  recoverable; losing the data is not.
- **The destination is replaced atomically.** Local and SFTP writes go to a hidden
  `.<name>.cairn-….part` sibling and are renamed into place on completion, so cancelling or losing
  a copy leaves the original file untouched rather than truncated. If Cairn is killed outright
  mid-copy you may find one of those `.part` files left behind.
- **Both panes on the same directory** is refused rather than executed — copying a file onto itself
  would empty it. Use `r`/`F6` to rename instead.

**Operations a backend cannot do are refused before you commit to them.** Each backend declares
what it supports for the directory you are in, so Delete, MakeDir, Rename and a copy into a
read-only destination fail immediately with a message naming the limitation, rather than after the
operation has started.

```toml
[ui.keybindings]
"ctrl+a" = "ai_propose"   # ask the AI assistant for a plan
"G"      = "cursor_bottom"
"f5"     = "copy"
"s"      = "cycle_sort"   # name → size → modified → type
"."      = "toggle_hidden"
```

Entries are colored by **type** so folders, files, and archives read at a glance: blue directories,
amber archives, green executables, cyan symlinks, purple streams (logs), red special nodes — and a
hidden (`.`-prefixed) directory or file uses the dimmed variant of its color, so `.git/` is clearly
a folder but recedes next to `src/`.

Pick a **theme** with `[ui] theme = "..."`:

| Preset | Look |
|--------|------|
| `dark` (default) | modern truecolor Tokyo-Night; leaves your terminal's own background |
| `mc` | classic **Midnight Commander** — the iconic saturated VGA-blue panels, white folders, black-on-cyan selection |
| `nord` | the arctic Nord palette |
| `gruvbox` | warm retro Gruvbox (dark) |
| `light` | a clean light scheme (forces its own light background) |

```toml
[ui]
theme = "mc"
```

Or switch **live** in the TUI with **Shift-T**, which cycles through the presets
(`dark → mc → nord → gruvbox → light → …`) — the status line shows the new theme. (The live choice is
session-only; set `[ui] theme` to make it the default.)

All presets use truecolor RGB, so they're best-effort on terminals without 24-bit color. `dark`
leaves the terminal background untouched, while `mc`/`nord`/`gruvbox`/`light` paint their own.

Every color role is themeable under `[ui.colors]` on top of the chosen preset — override individual
roles (`background`, `foreground`, `focused_border`, `unfocused_border`, `dir`, `hidden_dir`,
`file`, `hidden_file`, `archive`, `executable`, `symlink`, `stream`, `special`, `error`, `status`,
`remote`, `selection_bg`, `selection_fg`) using color names or `#rrggbb`. `background`/`foreground`
also accept `none` to clear a preset's forced value back to the terminal default:

```toml
[ui]
theme = "mc"

[ui.colors]
background     = "none"      # keep MC's colors but drop its blue panel background
dir            = "#7aa2f7"
hidden_dir     = "#4c5a8c"   # dimmed dir tone for .folders
archive        = "#e0af68"   # .zip / .tar.gz / …
executable     = "#9ece6a"
remote         = "yellow"    # accent for a pane on a remote backend (SSH/S3/…)
```

Each pane's top border shows which backend it is browsing: a local pane shows just its path,
while a remote pane shows a full `scheme://user@host:path` locator (e.g.
`ssh://root@dietpi6:/home`) in the `remote` accent color, so the two are easy to tell apart.

### AI assistant

`Ctrl-A` asks the assistant for a plan. Nothing runs until you approve it: the overlay lists each
step with its verb, whether it is reversible, and — beneath each one — **the actual call that will
be made**, operands included.

That second line matters more than it looks. A step's description is prose the model wrote about
itself; the operands are what the executor consumes. Reviewing only the description means approving
a *claim* rather than an *action*, so both are shown. `Enter` approves the highlighted step, `x`
rejects it, `Esc` abandons the plan; a plan containing an irreversible step cannot be bulk-approved.

The assistant never sees your credentials — it depends only on a secret-free view of the vault and
can name a credential but never read one.

### Connections

`Ctrl-O` opens the connection switcher — pick a backend to open in the active pane. Docker and
Kubernetes connections appear automatically (auto-discovery; opt out per-source with
`[discovery] docker = false` / `kubernetes = false`), alongside your saved profiles and the local
filesystem roots. Inside the switcher:

| Key | Action |
|-----|--------|
| `Enter` | Open the highlighted connection in the active pane |
| `Ctrl-N` | Add a new connection (scheme picker → fields → credential) |
| `e` | Edit the highlighted saved profile |
| `d` | Delete the highlighted saved profile (asks to confirm; cleans up its vault credential) |
| `t` | Test the highlighted connection's reachability — **without** opening it into a pane |
| `P` | Pin/unpin the highlighted entry to the top of the list |
| `H` | Hide/un-hide the highlighted entry from the default view |
| `S` | Show hidden entries for this session (so a hidden one can be found again and un-hidden) |

Pin/hide apply to any entry (built-in, saved, or auto-discovered) and persist to
`[discovery].pinned` / `[discovery].hidden` in `config.toml`, keyed by a stable identifier — not
the display name, so renaming a profile never orphans its pin/hide state. Testing reuses the same
connection logic as a real open (so a real SSH/S3/GCS/Azure test performs genuine credential
resolution) but never mounts the result or switches any pane; a connection that needs the secrets
vault unlocked reports that directly rather than popping the vault-unlock prompt.

### SSH host keys

An SSH profile takes `host`, `user`, `port` (default `22`), `known_hosts` (default
`~/.ssh/known_hosts`) and `host_key` — the policy Cairn applies to the server's key:

| `host_key` | Behaviour |
|-----------|-----------|
| `accept-new` (default) | Trust-on-first-use: a host you hold **no** key for is recorded and accepted. A host you already have a key for must present a key that matches one of them. |
| `strict` | Only a recorded, matching key is accepted. An unknown host is refused. |

"A host you already have a key for" means exactly that, and it is worth spelling out because the
naive reading is a real vulnerability: a server answering for a known host with a **different key
algorithm** (ECDSA where you pinned Ed25519) is *not* a new host, and is refused rather than
learned. Cairn reads `known_hosts` itself for this, so `@revoked` and `@cert-authority` markers,
`host1,host2` lists, `*.example.com` patterns and mixed-case hostnames are all understood — a
revoked key is refused under **both** policies. Hashed (`ssh-keygen -H`) entries work too.

Cairn also asks for the algorithms a host is already pinned under **first** during key exchange, the
way OpenSSH's `order_hostkeyalgs()` does, so a host pinned under an older algorithm still connects
when its server has since added a newer one.

`~/.ssh` and `known_hosts` are created `0700`/`0600` when Cairn creates them; existing permissions
are left alone. A `known_hosts` that exists but cannot be read is treated as "this host may be
pinned" — Cairn refuses to learn rather than gamble that you have no pins. A read-only shared file
(`/etc/ssh/ssh_known_hosts`) works normally.

> A rejected host key currently surfaces as a generic `connection failed`, the same message as an
> unreachable host. That is a known gap — if a host you have connected to before suddenly fails,
> check `known_hosts` before assuming the network.

### Shell-command actions

Bind a key to run a local program against the entry under the cursor. Each `[[shell_actions]]` entry
has a `name`, a `key` (same chord syntax as keybindings), a `command`, and `args` with the
placeholders `{path}` (the file's real path), `{dir}` (its directory), and `{name}` (its file name):

```toml
[[shell_actions]]
name    = "Checksum"
key     = "ctrl+h"
command = "/usr/bin/sha256sum"
args    = ["--", "{path}"]
# confirm = false   # skip the confirm prompt for a trusted action (default: true)
```

**Security:** actions run only on **local** panes, with **no shell** (so filenames can't inject
commands — prefer `--` before a `{path}`/`{name}` argument), in a scrubbed environment (no secrets are
passed to the program), with a confirm prompt and a timeout. For this reason the `[[shell_actions]]`
section is ignored if `config.toml` is writable by other users or not owned by you. Interactive
programs (editors) are not yet supported. See `docs/adr/0005-shell-command-actions.md`.

## Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) and our
[Code of Conduct](CODE_OF_CONDUCT.md). All changes land via pull request — `main` is protected.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this project by you, as defined in the Apache-2.0 license, shall be dual-licensed as above,
without any additional terms or conditions.
