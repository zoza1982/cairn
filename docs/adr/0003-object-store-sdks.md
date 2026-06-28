# ADR-0003: Object-store backends on official SDKs, not a unifying abstraction

- **Status:** Accepted
- **Date:** 2026-06-27
- **Deciders:** Maintainer, with `storage-engineer`, `software-architect`

## Context

The S3, GCS, and Azure Blob backends must support browsing at scale and a transfer engine that does
resumable multipart uploads, ranged parallel downloads, server-side copy, integrity checksums, and
conflict-safe conditional writes. A unifying crate (`opendal`) would reduce provider code but
abstracts away the very primitives the transfer engine drives directly.

## Decision

Implement three provider modules on the **official SDKs** — `aws-sdk-s3`, `google-cloud-storage`,
`azure_storage_blobs` — behind a shared in-crate `ObjectStore` trait that the `Vfs` impl wraps.
S3-compatible endpoints (MinIO/R2/B2) are handled by configuring `endpoint_url` + `path_style` on the
S3 provider. Credentials arrive resolved from the broker and are held in `ArcSwap` for refresh
without rebuilding the SDK client.

## Consequences

### Positive
- Full part-level multipart control (independent retry, per-part CRC32C, upload-id persistence for
  cross-restart resume), provider CAS primitives (S3 conditional writes, GCS generation
  preconditions), and presign — none of which a unifying abstraction exposes.
- Stable 1.x SDK floor (AWS) instead of a 0.x meta-library at a correctness-critical layer.
- Each provider's canonical checksum is used correctly (S3 CRC32C, GCS crc32c, Azure MD5), avoiding
  the multipart-ETag content-integrity trap.

### Negative / trade-offs
- ~3× the provider code; three SDK upgrade streams to track.
- Behavioral divergence between providers must be guarded — mitigated by a contract test suite run
  against all three (LLD §14).

## Alternatives considered
- **`opendal`** — clean unification, but hides multipart/resume/CAS/presign and was API-unstable
  through 2024–25; rejected for this layer.
- **Hand-rolled HTTP (no SDK)** — re-implements signing/retry/streaming; far more risk; rejected.

## References
- [LLD](../LLD.md) §8.
