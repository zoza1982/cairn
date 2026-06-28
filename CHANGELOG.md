# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `cairn-core`: the Elm/TEA app model and pure `update` reducer (panes, cursor/marks, navigate,
  list events) — no I/O, 10 tests (M1-4).
- `cairn-backend-local`: the local filesystem backend over `tokio::fs` (list, stat, ranged
  read/write, mkdir/remove/rename, Unix perms), with RFC-0001 (M1-2/M1-3).
- Engineering rule: use the Context7 MCP for current library/API docs (CLAUDE.md §10, §13).
- `cairn-vfs`: the `Vfs` trait (object-safe, `#[async_trait]`, streaming `list`), capability
  provider, `ReadHandle`/`WriteHandle`, `VfsRegistry`, the action interface, and `MockVfs` (M1).
- `cairn-types` crate: `VfsPath` (normalized, traversal-safe), `Entry`/`EntryKind`/`EntryExt`,
  the `Caps` capability model, and `ConnectionId`/`Scheme` — the shared leaf vocabulary (M0).
- Binary edge: tracing init (`CAIRN_LOG`), a panic hook, and `--version`/`--help` (M0).
- Project scaffolding: engineering rules (`CLAUDE.md`), contribution and governance docs,
  GitHub issue/PR templates, CI workflow, and a minimal Cargo workspace.

### Changed
- Tuned workspace clippy lints for velocity under CI's `-D warnings`: deny `clippy::all` + forbid
  unsafe + require rustdoc, but drop the over-broad `pedantic`/`unwrap_used`/`expect_used` lints
  (advisory via review per CLAUDE.md §9, not a hard test-breaking gate).
- Product Requirements Document (`docs/PRD.md`).
- Team-of-agents working model in `CLAUDE.md` §2: every feature and significant decision is
  run past the relevant specialist agent(s), with a domain→agent mapping.
- Vendored specialist agents under `.claude/agents/` so every contributor shares the same team.
  Includes Cairn-specific agents authored for this project: `tui-engineer`,
  `ai-integration-engineer`, `plugin-systems-engineer`, `container-backend-engineer`,
  `technical-writer`, plus client-backend-focused `kube-staff-engineer`, `network-engineer`,
  `storage-engineer`, and a Rust-focused `code-reviewer`.
- Low-Level Design (`docs/LLD.md`): architecture, the core async VFS abstraction + capability
  model, tokio/TEA app model, transfer engine, object-store backends, secrets vault + AI/plugin
  broker boundary, AI agentic layer, and WASM plugin system.
- ADRs recording the load-bearing decisions: core architecture (ADR-0001), vault crypto + broker
  boundary (ADR-0002), object-store SDKs (ADR-0003), WASM plugin runtime (ADR-0004).
- Implementation Plan & living progress tracker (`docs/IMPLEMENTATION_PLAN.md`): milestones M0–M8 +
  v0.1, work breakdown with status, critical path & dependency DAG, parallelization lanes, RFC
  sequencing, risk register, and a same-PR status-update rule (CLAUDE.md §5).
- GitHub Milestones M0–M8 + v0.1 backing the tracker.

### Changed
- Renumbered `CLAUDE.md` sections to accommodate the new team-of-agents model (§2).

[Unreleased]: https://github.com/zoza1982/cairn/commits/main
