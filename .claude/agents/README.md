# Cairn project agents

These are the specialist subagents the Cairn team uses. They are vendored here (project-level,
under `.claude/agents/`) so **every contributor gets the same team** — Claude Code discovers
project agents automatically, and project definitions take precedence over any global ones.

This implements the team-of-agents working model in [`../../CLAUDE.md`](../../CLAUDE.md) §2: every
feature and significant decision is run past the relevant specialist(s) before being finalized.

## Domain → agent

| Domain | Agent |
|--------|-------|
| System architecture, cross-cutting design | `software-architect`, `systems-design-engineer` |
| Rust implementation, `unsafe`, perf-critical code | `rust-staff-engineer` |
| General implementation | `software-engineer` |
| Object-store backends (S3/GCS/Azure) + transfer engine | `storage-engineer` |
| Kubernetes backend (client, pods, exec, logs, port-forward) | `kube-staff-engineer` |
| Docker/OCI container & image backend | `container-backend-engineer` |
| Connectivity/transport: SSH/SFTP, pooling, resilience | `network-engineer` |
| CI/CD, packaging, releases, container dev workflows | `devops-engineer` |
| Secrets vault, crypto, auth, exec safety | `security-engineer` |
| AI / agentic assistant (LLM providers, tools, plan→confirm) | `ai-integration-engineer` |
| TUI rendering, layout, input, theming, keybinds | `tui-engineer` |
| Interaction/UX design | `ux-engineer`, `ui-engineer` |
| WASM plugin system & extensibility | `plugin-systems-engineer` |
| Performance analysis & optimization | `performance-tuning-engineer` |
| Test strategy & quality | `qa-engineer` |
| Documentation, ADRs/RFCs, rustdoc, changelog | `technical-writer` |
| Naming, branding, messaging | `product-branding-expert` |
| Planning, sequencing, coordination | `project-manager`, `workflow-orchestrator` |
| Bug/edge-case/security analysis of a diff (gate) | `bug-bot` |
| Correctness & simplification review of a diff (gate) | `code-reviewer` |

## Notes

- The following agents were authored specifically for Cairn — focused on building **client-side
  backends** and the app itself, not administering cloud infrastructure: `kube-staff-engineer`,
  `container-backend-engineer`, `network-engineer`, `storage-engineer`, `ai-integration-engineer`,
  `tui-engineer`, `plugin-systems-engineer`, and `technical-writer`. The `code-reviewer` was also
  refocused on Rust/Cairn.
- The remaining agents are general-purpose role definitions.
- To propose changes to an agent, open a PR like any other change (see `CONTRIBUTING.md`).
