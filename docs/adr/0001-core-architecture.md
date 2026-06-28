# ADR-0001: Core architecture — crate layout, async VFS trait, and TEA app loop

- **Status:** Accepted
- **Date:** 2026-06-27
- **Deciders:** Maintainer, with `software-architect`, `rust-staff-engineer`, `tui-engineer`

## Context

Cairn must present many heterogeneous backends (local, SSH, object stores, Docker, Kubernetes — and
runtime-registered plugin backends) behind one interface, run all I/O asynchronously, and never block
the terminal UI (CLAUDE.md §9). We need a structure that is testable, keeps heavy backend SDKs out of
the default build, and makes "the UI never blocks" a structural property rather than a discipline.

## Decision

1. **Cargo workspace of small crates** split on stable seams. Backends are leaf siblings that never
   depend on each other; `cairn-core` holds `Arc<dyn Vfs>` from a registry, not concrete backends;
   `cairn-ai`/`cairn-plugin` depend only on `cairn-broker`. Heavy SDKs are quarantined behind crates
   + Cargo features (default = local + SSH). See LLD §2.
2. **VFS dispatch via `#[async_trait]` + `Arc<dyn Vfs>`.** Object-safety is required (heterogeneous +
   plugin-registered backends); native `async fn` in traits is not object-safe on stable without
   experimental shims. `list` is the exception: a non-async method returning `BoxStream` (an `async fn`
   returning `BoxStream<'_>` is an unsatisfiable lifetime). `ReadHandle: AsyncRead`; `WriteHandle` is a
   chunk API whose `finish()` returns metadata.
3. **Elm/TEA application loop.** `update(&mut AppState, Msg) -> Vec<AppEffect>` is pure (no I/O, no
   `.await`); a synchronous render reads an immutable `&AppState`; an effect runner executes effects on
   a tokio multi-thread runtime and feeds results back as `AppEvent`. Bounded event channel with
   coalesced progress; per-task `CancellationToken`s.

## Consequences

### Positive
- "UI never blocks" is structural (render path has no `.await`/I/O).
- Cross-backend transfer composes backends only in `cairn-transfer`.
- Pure reducer is trivially unit-testable; AI/secret isolation is a compile-time graph property.

### Negative / trade-offs
- `#[async_trait]` adds a boxed-future allocation per call (negligible vs network/disk).
- More crates = more `Cargo.toml` ceremony and longer cold builds.
- TEA requires routing async results back as events rather than calling inline.

## Alternatives considered
- **Native AFIT + `dynosaur`/manual vtables** — experimental/again object-safety friction; rejected for the public trait.
- **One fat crate** — every build pays for aws+kube+wasmtime; harms test hermeticity; rejected.
- **Retained-widget model with internal state** (tui-realm-style) — complicates async result routing and testing; used only for render composition, not as the architecture.
- **`Arc<Mutex<AppState>>` shared with render** — needless contention; the sequential loop borrows `&AppState` safely.

## References
- [LLD](../LLD.md) §1–§5; supersedes nothing.
