# ADR-0006: Feature-gated network backends and the lean/full CI split

- **Status:** Accepted
- **Date:** 2026-06-29
- **Deciders:** maintainer (with devops-engineer design; to be cross-checked by rust-staff-engineer,
  security-engineer, storage-engineer, network-engineer per CLAUDE.md §2)

## Context

Cairn ships seven VFS backends. `local` is always built in; the rest — SSH/SFTP, S3, GCS, Azure
Blob, Docker, Kubernetes — pull heavy, often TLS-bearing SDKs (`aws-sdk-s3`/`aws-lc-rs`, `kube`,
`bollard`'s hyper stack, `russh`). CI builds and tests on **Linux + macOS + Windows**.

Forces:

- `--all-features` on a workspace activates the **union** of every member crate's features; a feature
  cannot be subtracted from `--all-features`. So as soon as one backend wires a real SDK, every
  `--all-features` job on **every** OS compiles that SDK.
- `aws-lc-sys` (the default crypto provider for `rustls`/`aws-sdk-s3`) vendors BoringSSL and needs
  NASM + a C toolchain on Windows; enabling it on the Windows runner fails at **build** time.
- The default `cargo test` must stay **hermetic and offline** (CLAUDE.md §8) — no live services, no
  cloud credentials.
- Today only `hyper`/`hyper-util` (via the then-unconditional `bollard`) are in the tree; no
  `rustls`/`aws-lc-rs`/`ring` yet. The TLS trap is **pre-armed but not yet sprung** — a clean window
  to fix the structure before the first real SDK lands.

## Decision

1. **Per-backend, capability-named Cargo features, owned by each backend crate.** Each backend crate's
   default build is the transport-seam + mock core (hermetic, always compiled and tested). The real
   SDK adapter (`real.rs`/`connect.rs`) and its `optional` SDK deps sit behind a feature named for the
   capability: `docker`, `ssh`, `s3`, `gcs`, `azure`, `k8s`. The binary crate (`cairn`) **will**
   re-export these as user-facing features (`cairn-backend-docker/docker`, …) plus umbrellas (`cloud`,
   `containers`, `all-backends`), becoming the single place an end user selects backends — added per
   backend as each lands (see Rollout). PR-0 establishes only the crate-level pattern (docker).

2. **The cross-platform build is LEAN; the full build is Linux-only.** No `macos-latest` or
   `windows-latest` CI job may pass `--all-features` or any backend `--features`. The 3-OS `test` and
   the `clippy-lean` jobs build default features (seam + mock); Linux-only `test-full`/`clippy-full`
   (and `docs`) build `--all-features`. This keeps the heavy/aws-lc stack off Windows/macOS forever
   while still type-checking and testing every real adapter on Linux every PR.

3. **`cargo-deny` runs twice** — once on the default graph, once `--all-features` — because
   feature-gated SDKs are invisible to the default run; license/advisory drift in the TLS stack must
   not slip in behind a non-default feature.

4. **Integration tests are double-gated.** A backend's emulator test is `#![cfg(feature = "<x>")]`
   (compile gate) **and** early-returns unless its env var (`CAIRN_IT_<X>`) is set (run gate), so the
   default `cargo test` never compiles or runs them. A dedicated, Linux-only `integration.yml`
   workflow (nightly + `workflow_dispatch` + path-filtered, initially non-required) starts the
   emulators — sshd container, MinIO, Azurite, fake-gcs-server, kind, host Docker daemon — and sets
   the env guards.

5. **Runtime "not built in" vs "unknown" (future, lands with the first disableable backend).** A
   recognized scheme whose feature is off will resolve to a `NotCompiled { feature }` state, yielding
   *"backend `ssh` is recognized but not built into this binary — rebuild with `--features ssh`"*
   rather than "unknown scheme", and `cairn --version` will list the compiled-in backends. Not wired
   in PR-0 (the binary does not yet select any optional backend); it arrives with M4 SSH.

## Consequences

- **Positive:** Windows/macOS never compile aws-lc/TLS; the default build/test is genuinely lean and
  hermetic; each real adapter is still type-checked + tested on Linux every PR; the structure is in
  place *before* the first heavy SDK, so adding S3/GCS/Azure/k8s is additive and cannot redden
  Windows. Per-backend granularity lets users build a minimal binary.
- **Negative / accepted:** backends are not verified on Windows/macOS in CI (acceptable — they are
  Linux-oriented; a Windows user gets the lean default + any feature they opt into locally). More
  transitive crates mean more RUSTSEC churn (triaged as today). The S3 PR must ship `aws-lc-sys`/`ring`
  license exceptions in `deny.toml` in the same PR (OpenSSL-family SPDX) or the `deny --all-features`
  job goes red — evaluate `rustls`+`ring` vs `aws-lc-rs` with security-engineer to minimize the
  exception surface and record it then.

## Rollout

This ADR is realized incrementally; `main` stays green at each step.

- **PR-0 (this change):** split the CI matrix (lean cross-OS + Linux-only full + `deny --all-features`)
  and make the already-unconditional `bollard` optional behind the `docker` feature (removes hyper
  from the lean build; establishes the pattern). The SSH crate keeps `russh-sftp` unconditional (its
  `real.rs` is the type-checked seam adapter); the `ssh` feature + `russh` connection layer arrive
  with M4.
- **Per backend (M4 SSH → M5 object-store → M6 containers):** each adds one feature, includes its
  adapter in the Linux full build, and grows `integration.yml` by one isolated emulator job. The S3
  PR carries the `deny.toml` license exceptions. No backend waits on another's emulator.

## Alternatives considered

- **Keep `--all-features` on all OSes, absorb the Windows TLS build.** Rejected: `aws-lc-sys` fails to
  build on Windows without extra toolchain, and it destroys hermeticity.
- **Drop Windows/macOS from the matrix entirely.** Rejected: the lean core (local backend, TUI,
  transfer engine, vault) genuinely must work cross-platform; only the heavy backends are Linux-bound.
- **One umbrella `backends` feature.** Rejected: forces all-or-nothing SDK builds and couples
  unrelated backends' license/advisory surfaces.
