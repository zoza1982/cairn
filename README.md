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
> browsing** — `Enter` on a `.tar`/`.zip`/`.tgz`/`.tbz2`/`.txz`/`.tzst` mounts it like a directory
> (RFC-0013, behind the `archive` feature).
> **Backend mapping cores** for **SSH/SFTP**, **object stores** (S3/GCS/Azure-shaped), **Docker**, and
> **Kubernetes** are implemented against a transport seam and fully unit-tested with in-memory mocks,
> and the **WASM plugin host** (wasmtime, resource-limited, default-deny) runs sandboxed modules.
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
`filter`, `make_dir`, `rename`, `view`, `edit`, `open_queue`, `toggle_pause`, `quit`, …). Unrecognized entries are ignored with a warning, and `Ctrl-C` always quits.

By default `s` cycles the active pane's sort order (name → size → modified → type) and `.` toggles whether
hidden entries (dotfiles) are listed; the current sort mode and hidden state show in each pane's
bottom-right corner. `F7` creates a new directory and `F2` renames the entry under the cursor (both
open a text prompt; `Enter` confirms, `Esc` cancels). `F3` opens a read-only pager on the entry
under the cursor, auto-detecting text vs. binary content and switching to a hex view for the
latter. `F4` opens the entry under the cursor in an external editor — `$VISUAL`, then `$EDITOR`,
then `vi` (Unix only; on Windows, set one of the two first); `Enter` on a text file opens the same
editor, while `Enter` on a binary file still opens the read-only hex pager. Editing works on
**local files only** for now — a file on a remote connection shows a message pointing at the
still-designed remote-writeback phase (see [RFC-0012](docs/rfcs/0012-file-open-view-edit.md)).
`/` filters the listing as you type (`Enter` keeps the filter, `Esc` clears it). During a transfer,
`p` pauses/resumes and `Esc` cancels (all active transfers); `Ctrl-T` opens the transfer queue. Up
to two transfers run at once by default — set `[transfers] concurrency = N` in config to change it.

```toml
[ui.keybindings]
"ctrl+a" = "ai_propose"   # ask the AI assistant for a plan
"G"      = "cursor_bottom"
"f5"     = "copy"
"s"      = "cycle_sort"   # name → size → modified → type
"."      = "toggle_hidden"
```

Colors can be themed under `[ui.colors]` — override individual roles
(`focused_border`, `unfocused_border`, `dir`, `error`, `status`, `selection_bg`, `selection_fg`) over
the built-in `dark` preset, using color names or `#rrggbb`:

```toml
[ui.colors]
focused_border = "magenta"
dir            = "#5fafff"
```

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
