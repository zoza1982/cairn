# ADR-0013: Compressed-tar decoder selection — pure-Rust, xz deferred (RFC-0013 P5)

- **Status:** Accepted
- **Date:** 2026-07-03
- **Deciders:** storage-engineer (design; revised after `bug-bot`/`security-review` gate findings on
  the initial P5 commit — see "Revision" below); flagged for `security-review` sign-off on the
  trade-offs below, per CLAUDE.md §2/§11 — any change to how this backend parses untrusted bytes is
  in scope.

## Context

RFC-0013 P5 adds compressed-tar browsing. Each container wraps a plain tar in an outer compression
that Cairn must decode before `tar_backend::TarOps` can index it (see RFC-0013 P5 §"Compressed tar"
for the decompress-to-temp design this ADR assumes). For gzip, zstd, xz, and bzip2, at least one
pure-Rust and one C-FFI-bindings crate exists on crates.io; P4's own `zip`/`tar` dependency comments
already established the project's preference (no cross-platform build risk, no C parsing untrusted
bytes — see ADR-0006), but P5 is the first place each concrete choice had to be made and weighed
individually, since maintenance/adoption varies a lot more between these crates than between
`tar`/`zip`'s alternatives did.

## Revision note

This ADR originally covered four codecs, including xz via `lzma-rs`. The gate review on the initial
P5 commit (`bug-bot` + `security-review`) traced `lzma_rs::xz_decompress`'s LZMA2 path and found it
is **not memory-bounded against a decompression bomb**: its internal dictionary buffer is only
flushed to the output sink on specific internal resets, which a crafted stream can defer
indefinitely — so a tiny, crafted `.txz` can accumulate multiple GiB in **process RAM** before
`compressed_tar::CappedWriter`'s output-byte guard ever sees a byte to count. The guard bounds the
*file* this backend writes to disk, not the decoder's own internal memory, so this was a real,
unconditional OOM vector reachable simply by opening a file — not the "bounded, disclosed residual
gap" the original version of this ADR characterized it as. **xz decoding was dropped entirely**
rather than shipped with a known bomb; see "Decision" below for what shipped instead. The same
review also found that this ADR had understated `ruzstd`'s safety profile (see "Decision," zstd
row) and that a naive completeness check for bzip2/zstd (comparing bytes consumed from the input
reader) is unreliable — see RFC-0013 P5 §"Multi-stream" for that design.

## Decision

**Pure-Rust decoder for each codec that is actually decoded — no C/FFI anywhere in this backend's
parse of untrusted compressed bytes. xz is recognized (sniffed) but never decoded, pending a
memory-bounded decoder.** Concretely (all MIT or MIT/Apache-2.0, `cargo-deny`-clean with no
`deny.toml` changes needed; versions Context7/crates.io-verified 2026-07-03):

| Codec | Chosen | Alternative considered (rejected) |
|---|---|---|
| gzip | `flate2 = "1"`, `rust_backend` (miniz_oxide) feature, decoded via `MultiGzDecoder` | `flate2` with a C zlib backend feature |
| zstd | `ruzstd = "0.8"` | `zstd` (C-FFI bindings to libzstd) |
| bzip2 | `bzip2-rs = "0.1"` | `bzip2` (C-FFI bindings to libbzip2) |
| xz/lzma | **not decoded** — `.txz`/`.tar.xz` is sniffed and refused with a typed, friendly error | `lzma-rs` (dropped, see below); `xz2`/`liblzma` (C-FFI, also rejected) |

The common thread, restated from RFC-0013's own framing: **this backend's entire job is parsing
bytes the user did not necessarily create** (a download, an attachment, a file handed over by
someone else). A memory-safety bug in a C decompressor parsing adversarial input is a fundamentally
worse failure mode (memory corruption, potential RCE) than an equivalent logic bug in a pure-Rust
one (a wrong byte, a panic-turned-typed-error, at worst a DoS bounded by our own caps regardless).
Gzip was an easy call — `flate2`'s pure-Rust backend is mature, already in the tree (transitively via
`zip`), and `MultiGzDecoder` (not `GzDecoder`) is required regardless of the C-FFI-vs-pure-Rust axis
to correctly reconstruct concatenated gzip members (see RFC-0013 P5 §"Multi-stream"). zstd and
bzip2 needed a real trade-off, recorded below; xz needed a design reversal, recorded above and in
its own subsection.

### zstd: `ruzstd` — pure-Rust means no C/FFI, not "no `unsafe`"

`ruzstd` is actively maintained (0.8.3, published 2026) and widely used (tens of millions of recent
downloads) — a clear win over the C-FFI `zstd` crate on every axis. **Correction from this ADR's
first version:** `ruzstd` is not `unsafe`-free — it contains internal `unsafe` (a hand-written ring
buffer used for its decode window). The correct claim is narrower than "no unsafe / no memory
corruption possible": pure-Rust means **no C/FFI parsing untrusted bytes**, which still meaningfully
narrows the attack surface relative to a C decoder (no cross-language ABI boundary, no
manually-managed C allocations, a smaller and single-language codebase to audit), but a
memory-safety bug inside `ruzstd`'s own `unsafe` block is not ruled out by this choice alone. This
is mitigated by `ruzstd`'s own fuzzing and, regardless of `ruzstd`'s internal soundness, by this
backend's own bomb/ratio caps bounding the blast radius of any output-level misbehavior.

### bzip2: `bzip2-rs`, deliberately over the far more popular `bzip2` crate

This is the sharpest adoption trade-off. `bzip2-rs` is "100% safe" (no `unsafe` at all — a genuinely
correct claim for this crate, unlike `ruzstd` above), ships its own fuzz corpus, and has commits into
December 2024 — but its last crates.io release (0.1.2) is from February 2021, and its download count
(~300K total) is roughly **three orders of magnitude** below the C-FFI `bzip2` crate's (~135M total,
actively maintained by the Rust Foundation-adjacent `trifectatechfoundation`, last published October
2025). Less adoption means less real-world adversarial mileage exercising its decoder.

**Decided in favor of `bzip2-rs` anyway**, on the reasoning that the *failure mode* matters more
than the *frequency* of failures for a decoder whose whole purpose is parsing attacker-influenceable
bytes: an undiscovered bug in a safe-Rust decoder degrades to a wrong byte or a clean panic-turned-
error (bounded by our own caps regardless of the codec's own robustness), where the equivalent bug in
the C decoder risks memory corruption. This trade-off is exactly the kind CLAUDE.md §11 asks to route
through `security-review` before shipping — flagged here explicitly rather than assumed. Separately,
`bzip2-rs`'s `DecoderReader` was found (empirically, during gate remediation) to read its *entire*
input into an internal buffer regardless of how many logical bzip2 streams it contains — a
correctness quirk, not a safety one, but it is why the multi-stream guard (RFC-0013 P5) cannot rely
on "bytes consumed from the input reader" and instead scans the file's own bytes for a second magic
occurrence.

### xz/lzma: not decoded — `lzma-rs`'s LZMA2 path is not memory-bounded

`lzma-rs` (pure-Rust LZMA/LZMA2/XZ) was the initial choice for this codec and was dropped after gate
review. `lzma_rs::xz_decompress` has no public `memlimit`/dictionary-size option for the
xz-container path (unlike `lzma_decompress_with_options`, which does, for the raw-LZMA path this
backend never used), and — the disqualifying finding — its dictionary buffer is not guaranteed to
flush to the output sink until an internal reset point the compressed stream itself controls. A
crafted stream can defer that reset arbitrarily, so the *output*-side `CappedWriter` guard this
backend relies on for every other codec never gets a chance to fire: memory accumulates inside the
decoder, unbounded, before a single byte reaches our counter. This is a straightforward OOM DoS
reachable by opening one small, adversarial file — unacceptable regardless of how the rest of this
backend's caps are designed.

**Decision: drop xz decoding entirely for now.** `Compression::Xz` and its magic-byte sniff are kept
(so `.txz`/`.tar.xz` is still *recognized* and refused with a clear, typed
`compressed_tar_xz_unsupported` error rather than falling through to "not a recognized archive"),
but `compressed_tar::decompress_to_temp` refuses to decode it. Follow-up options, none implemented
yet: (a) a pure-Rust LZMA2/xz decoder with a genuine bounded-memory guarantee (revisit `lzma-rs` if
it ever exposes one, or evaluate alternatives as the ecosystem matures); (b) a C `liblzma` binding
used specifically for its `memlimit` support, explicitly gated through `security-review` given it
would be this backend's first C-FFI parse of untrusted bytes. Tracked in RFC-0013 and
`docs/IMPLEMENTATION_PLAN.md`.

## Consequences

### Positive

- No C/FFI anywhere in the parse of untrusted compressed bytes for any codec this backend actually
  decodes — a memory-safety bug in `ruzstd`'s or `bzip2-rs`'s Rust code degrades to a bounded,
  typed-error failure (or, for `ruzstd`'s internal `unsafe`, whatever that block's own soundness
  guarantees are — see the zstd correction above), never a C-level memory-corruption/RCE class bug.
- No unconditional OOM vector shipped: dropping xz removed the one codec whose decoder could not be
  bounded by this backend's own output-side guard.
- Consistent with ADR-0006's existing rationale for `tar`/`zip`: no cross-platform build risk (no C
  toolchain/linking step) added to `cairn-backend-archive` for gzip/zstd/bzip2.
- The bomb caps (`ARCHIVE_MAX_DECOMPRESSED_BYTES`, the shared `compression_ratio_is_bomb` guard) and
  the multi-stream guard are enforced identically across the three decoded codecs via the shared
  `CappedWriter`/`contains_magic_after_start`, so codec-specific robustness differences don't change
  the worst-case blast radius of a malicious archive.

### Negative / trade-offs

- `.txz`/`.tar.xz` is not browsable in P5 at all — a real feature gap relative to the original RFC
  scope, accepted because shipping a decode path with a known unconditional OOM vector is worse than
  not shipping it.
- `bzip2-rs`'s low adoption (per above) is a real, disclosed gap in real-world battle-testing
  relative to the alternative — mitigated, not eliminated, by our own caps and by the crate's own
  fuzz corpus.
- `bzip2-rs` has gone multiple years without a crates.io release, so a future upstream bug fix
  landing only in its unreleased git history wouldn't reach us via a normal `cargo update` — worth
  revisiting if this decode path becomes a maintenance concern.
- `ruzstd` contains internal `unsafe`; this ADR no longer overclaims otherwise (see "Decision").

### Neutral

- `tempfile` moves from a dev-only to a real dependency of `cairn-backend-archive` (P4 used it only
  in tests; P5's `compressed_tar::decompress_to_temp` uses it in production code to create the
  private, RAII-deleted decode destination, preferring `$XDG_RUNTIME_DIR` when set).
- `lzma-rs` was added and then removed from `Cargo.toml` within the same PR (added in the initial P5
  commit, dropped in the gate-remediation commit) — no released version of this crate ever shipped.

## Alternatives considered

- **C-FFI bindings for zstd/bzip2** for maximum real-world adoption and release cadence. Rejected:
  reintroduces the exact C-parses-untrusted-bytes and cross-platform build risk ADR-0006 steered
  away from, for a backend whose entire threat model is untrusted input.
- **Mixed strategy: pure-Rust where clearly mature (gzip, zstd), C-FFI for bzip2.** Considered
  seriously given the adoption gap. Rejected in favor of a consistent policy across the backend —
  still flagged explicitly for `security-review` rather than treated as an obviously-correct call.
- **Ship xz with the known dictionary-flush gap, documented as a disclosed residual risk** (the
  original version of this ADR's position). Rejected on gate review: further investigation showed
  the gap is not a bounded "up to 1.5 GiB" ceiling as first believed, but an *unbounded* accumulation
  gated only by when the compressed stream itself chooses to trigger a dictionary reset — a
  materially different and unacceptable risk profile once understood correctly.
- **Streaming re-decompression per read instead of decompress-to-temp-once** (avoiding a full-file
  decode pass entirely). Rejected in RFC-0013 P5 itself (this ADR only covers *which* decoder per
  codec, not the decode strategy) — see RFC-0013 P5 §"Compressed tar".

## References

- RFC-0013 (`docs/rfcs/0013-archive-backend.md`) — P5's decompress-to-temp design, the bomb-cap
  mechanism, and the multi-stream guard this ADR's decoder choices feed into.
- ADR-0006 (`docs/adr/0006-feature-gated-backends-and-ci.md`) — the pure-Rust/no-cross-platform-
  build-risk precedent this ADR extends to the new codecs.
- ADR-0012 (`docs/adr/0012-archive-mount-model.md`) — the archive-mount model P5 is additive to.
- `crates/cairn-backend-archive/Cargo.toml` — the per-dependency comments recording the same
  rationale next to each `flate2`/`ruzstd`/`bzip2-rs` entry (and the note on why `lzma-rs` isn't
  there).
- `crates/cairn-backend-archive/src/compressed_tar.rs` — `CappedWriter`, `decompress_to_temp`,
  `contains_magic_after_start`, `xz_unsupported_err` — the mechanisms referenced throughout this ADR.
