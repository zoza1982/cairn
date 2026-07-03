# ADR-0013: Compressed-tar decoder selection — pure-Rust across all four codecs (RFC-0013 P5)

- **Status:** Accepted
- **Date:** 2026-07-03
- **Deciders:** storage-engineer (design; flagged for `security-review` sign-off on the trade-offs
  below, per CLAUDE.md §2/§11 — any change to how this backend parses untrusted bytes is in scope)

## Context

RFC-0013 P5 adds compressed-tar browsing (`.tgz`/`.tbz2`/`.txz`/`.tzst`). Each container wraps a
plain tar in an outer compression that Cairn must decode before `tar_backend::TarOps` can index it
(see RFC-0013 P5 §"Indexing" for the decompress-to-temp design this ADR assumes). For each of the
four codecs, at least one pure-Rust and one C-FFI-bindings crate exists on crates.io; P4's own
`zip`/`tar` dependency comments already established the project's preference (no cross-platform
build risk, no C parsing untrusted bytes — see ADR-0006) but P5 is the first place all four
concrete choices had to be made and weighed individually, since maintenance/adoption varies a lot
more between them than between `tar`/`zip`'s alternatives did.

## Decision

**Pure-Rust decoder for every one of the four codecs — no C/FFI anywhere in this backend's parse of
untrusted compressed bytes.** Concretely (all MIT or MIT/Apache-2.0, `cargo-deny`-clean with no
`deny.toml` changes needed; versions Context7/crates.io-verified 2026-07-03):

| Codec | Chosen | Alternative considered (rejected) |
|---|---|---|
| gzip | `flate2 = "1"`, `rust_backend` (miniz_oxide) feature | `flate2` with a C zlib backend feature |
| zstd | `ruzstd = "0.8"` | `zstd` (C-FFI bindings to libzstd) |
| xz/lzma | `lzma-rs = "0.3"` | `xz2`/`liblzma` (C-FFI bindings) |
| bzip2 | `bzip2-rs = "0.1"` | `bzip2` (C-FFI bindings to libbzip2) |

The common thread, restated from RFC-0013's own framing: **this backend's entire job is parsing
bytes the user did not necessarily create** (a download, an attachment, a file handed over by
someone else). A memory-safety bug in a C decompressor parsing adversarial input is a fundamentally
worse failure mode (memory corruption, potential RCE) than an equivalent logic bug in a
`forbid(unsafe_code)` pure-Rust one (a wrong byte, a panic-turned-typed-error, at worst a DoS bounded
by our own caps regardless). Gzip and zstd were easy calls — `flate2`'s pure-Rust backend and
`ruzstd` are both mature, actively maintained (verified: `ruzstd` 0.8.3 published 2026, tens of
millions of recent downloads), and already either in the tree (`flate2`, transitively via `zip`) or
clearly the stronger choice by every measure. xz and bzip2 needed a real trade-off, recorded below
for `security-review`.

### xz/lzma: `lzma-rs`, with a known gap

`lzma-rs`'s last crates.io release (0.3.0) is from January 2023; its upstream repo has commits into
May 2024 (MSRV bump, CI maintenance, dependency updates — no functional gap found for our read-only
xz-container use). The alternative, `xz2` (C bindings to `liblzma`), is *also* stale (last published
June 2022) and additionally reintroduces exactly the C-parsing-untrusted-bytes and
cross-platform-build-risk profile ADR-0006 already steered away from for the cloud SDKs. Pure-Rust
wins on the security axis even though neither crate is under active, frequent release.

**Known caveat, explicitly called out for `security-review`:** the public `lzma_rs::xz_decompress`
function has no `memlimit`/dictionary-size option for the xz-container path (unlike
`lzma_decompress_with_options`, which does, for the raw-LZMA path we don't use). A crafted xz header
can declare a dictionary up to the LZMA2/xz spec's own ceiling (1.5 GiB) before our incremental
output-byte cap (`compressed_tar::CappedWriter`, `security::ARCHIVE_MAX_DECOMPRESSED_BYTES`) gets a
chance to abort the decode — verified from `lzma-rs`'s source (`LzCircularBuffer`) that decoded
bytes *are* flushed to our capped `Write` sink incrementally as the dictionary-sized circular buffer
fills, not held entirely in memory until the end, so the cap does still bound the total decode; the
gap is a one-time allocation up to the spec ceiling that can happen *before* the first output chunk
reaches our guard, not an unbounded decode. Accepted as a bounded, disclosed residual risk rather
than a blocking issue — 1.5 GiB is a real but finite ceiling, not the "1 KiB → 10 GiB" unbounded-bomb
shape the caps exist to stop.

### bzip2: `bzip2-rs`, deliberately over the far more popular `bzip2` crate

This is the sharpest trade-off of the four. `bzip2-rs` is "100% safe" (no `unsafe`, matching this
crate's `forbid(unsafe_code)`), ships its own fuzz corpus, and has commits into December 2024 — but
its last crates.io release (0.1.2) is from February 2021, and its download count (~300K total) is
roughly **three orders of magnitude** below the C-FFI `bzip2` crate's (~135M total, actively
maintained by the Rust Foundation-adjacent `trifectatechfoundation`, last published October 2025).
Less adoption means less real-world adversarial mileage exercising its decoder.

**Decided in favor of `bzip2-rs` anyway**, on the reasoning that the *failure mode* matters more
than the *frequency* of failures for a decoder whose whole purpose is parsing attacker-influenceable
bytes: an undiscovered bug in a safe-Rust decoder degrades to a wrong byte or a clean panic-turned-
error (bounded by our own caps regardless of the codec's own robustness), where the equivalent bug
in the C decoder risks memory corruption. This trade-off is exactly the kind CLAUDE.md §11 asks to
route through `security-review` before shipping — flagged here explicitly rather than assumed.

## Consequences

### Positive

- No C/FFI anywhere in the parse of untrusted compressed bytes for any of the four formats — a
  memory-safety bug in any decoder degrades to a bounded, typed-error failure, never memory
  corruption.
- Consistent with ADR-0006's existing rationale for `tar`/`zip`: no cross-platform build risk (no C
  toolchain/linking step) added to `cairn-backend-archive` for any of the four new codecs.
- The bomb caps (`ARCHIVE_MAX_DECOMPRESSED_BYTES`, the shared `compression_ratio_is_bomb` guard) are
  enforced identically regardless of codec via the shared `CappedWriter`, so codec-specific
  robustness differences don't change the worst-case blast radius of a malicious archive.

### Negative / trade-offs

- `bzip2-rs`'s low adoption (per above) is a real, disclosed gap in real-world battle-testing
  relative to the alternative — mitigated, not eliminated, by our own caps and by the crate's own
  fuzz corpus.
- `lzma-rs` has no public memlimit control for the xz-container decode path, so a crafted header's
  declared dictionary size is bounded only by the format spec's own ceiling (1.5 GiB), not by us,
  before our per-chunk output cap can intervene — see "Decision" above.
- Both `lzma-rs` and `bzip2-rs` have gone multiple years without a crates.io release, so a future
  upstream bug fix landing only in their unreleased git history wouldn't reach us via a normal
  `cargo update` — worth revisiting if either format's decode path becomes a maintenance concern.

### Neutral

- `tempfile` moves from a dev-only to a real dependency of `cairn-backend-archive` (P4 used it only
  in tests; P5's `compressed_tar::decompress_to_temp` uses it in production code to create the
  private, RAII-deleted decode destination).

## Alternatives considered

- **C-FFI bindings for all four codecs** (`zstd`, `xz2`, `bzip2`) for maximum real-world adoption and
  release cadence. Rejected: reintroduces the exact C-parses-untrusted-bytes and cross-platform
  build risk ADR-0006 steered away from, for a backend whose entire threat model is untrusted input.
- **Mixed strategy: pure-Rust where clearly mature (gzip, zstd), C-FFI for the two harder calls (xz,
  bzip2).** Considered seriously for bzip2 specifically, given the adoption gap. Rejected in favor of
  a consistent policy across the backend — see "Decision" above; still flagged explicitly for
  `security-review` rather than treated as an obviously-correct call.
- **Streaming re-decompression per read instead of decompress-to-temp-once** (avoiding a full-file
  decode pass entirely). Rejected in RFC-0013 P5 itself (this ADR only covers *which* decoder per
  codec, not the decode strategy) — see RFC-0013 P5 §"Why decompress-to-temp".

## References

- RFC-0013 (`docs/rfcs/0013-archive-backend.md`) — P5's decompress-to-temp design and the bomb-cap
  mechanism this ADR's decoder choices feed into.
- ADR-0006 (`docs/adr/0006-feature-gated-backends-and-ci.md`) — the pure-Rust/no-cross-platform-
  build-risk precedent this ADR extends to the four new codecs.
- ADR-0012 (`docs/adr/0012-archive-mount-model.md`) — the archive-mount model P5 is additive to.
- `crates/cairn-backend-archive/Cargo.toml` — the per-dependency comments recording the same
  rationale next to each `flate2`/`ruzstd`/`lzma-rs`/`bzip2-rs` entry.
- `crates/cairn-backend-archive/src/compressed_tar.rs` — `CappedWriter`, `decompress_to_temp`, the
  shared bomb-guard mechanism referenced throughout this ADR.
