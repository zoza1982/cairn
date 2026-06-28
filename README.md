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

> **Status:** 🚧 Pre-alpha — design phase. The product is being specified before implementation.
> See [`docs/PRD.md`](docs/PRD.md) for the product requirements.

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
| Implementation Plan | Milestones & sequencing *(planned)* |
| [ADRs](docs/adr/) | Architecture Decision Records |
| [RFCs](docs/rfcs/) | Design proposals for non-trivial work |
| [Contributing](CONTRIBUTING.md) | How to build, branch, and submit changes |

## Building

> Requires the Rust toolchain (see [`rust-toolchain.toml`](rust-toolchain.toml)).

```sh
cargo build --workspace
cargo run -p cairn
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
