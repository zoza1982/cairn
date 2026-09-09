# Cairn Documentation

| Document | Status | Purpose |
|----------|--------|---------|
| [PRD.md](PRD.md) | Draft | Product requirements — *what* we build and *why* (high-level) |
| [LLD.md](LLD.md) | Draft | Low-Level Design — architecture, VFS abstraction, async/TEA model, transfer engine, vault crypto, AI layer, plugin sandbox |
| [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) | Draft | Milestones, sequencing, dependency graph, RFC ordering — **and the living progress tracker** |

## Subdirectories

- [`adr/`](adr/) — **Architecture Decision Records.** One decision per file, numbered, immutable
  once accepted. Start from [`0000-template.md`](adr/0000-template.md).
- [`rfcs/`](rfcs/) — **Design proposals** for non-trivial work (new backends, vault crypto, AI
  layer). Write and review an RFC *before* large implementation.
- [`reviews/`](reviews/) — **Dated quality reviews.** A record of what was found and what was done
  about it, so findings nobody acted on stay visible instead of evaporating. Each report states its
  method and marks which findings were adversarially verified and which were not.

## Documentation discipline

Documentation is part of the definition of done. See [`../CLAUDE.md`](../CLAUDE.md) §5 for the
per-change documentation requirements, and §2 for the team-of-agents working model.
