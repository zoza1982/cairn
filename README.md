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
> Argon2id) behind a capability **broker**, TOML **config**, and an **AI plan → confirm** overlay.
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
`filter`, `make_dir`, `rename`, `open_queue`, `toggle_pause`, `quit`, …). Unrecognized entries are ignored with a warning, and `Ctrl-C` always quits.

By default `s` cycles the active pane's sort order (name → size → modified → type) and `.` toggles whether
hidden entries (dotfiles) are listed; the current sort mode and hidden state show in each pane's
bottom-right corner. `F7` creates a new directory and `F2` renames the entry under the cursor (both
open a text prompt; `Enter` confirms, `Esc` cancels). `/` filters the listing as you type
(`Enter` keeps the filter, `Esc` clears it). During a transfer, `p` pauses/resumes it and `Esc`
cancels it; `Ctrl-T` opens the transfer queue.

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

## Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) and our
[Code of Conduct](CODE_OF_CONDUCT.md). All changes land via pull request — `main` is protected.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this project by you, as defined in the Apache-2.0 license, shall be dual-licensed as above,
without any additional terms or conditions.
