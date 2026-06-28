# RFC-0002: Transfer engine

- **Status:** Accepted
- **Author(s):** storage-engineer, rust-staff-engineer (synthesized)
- **Date:** 2026-06-28
- **Tracking item:** M2-1 (and M2-2/3/4)

## Summary

`cairn-transfer` moves bytes within and across backends by composing two `Arc<dyn Vfs>`. It is the
only place cross-backend logic lives, so "pod → S3" is the same code path as "local → local".

## Design

- **Position.** Above the `Vfs` trait; takes a source and destination `Arc<dyn Vfs>` plus a list of
  `(from, to)` path pairs and a [`TransferSpec`].
- **Copy paths.** A same-connection **server-side copy** fast path (`copy_within`, when
  `Caps::COPY_SERVER`); otherwise a **stream-through** loop: `open_read` → fixed 1 MiB buffer →
  `write_chunk` → `finish`. Backpressure is implicit (the write awaits); a bounded reader/writer
  pipeline is a later optimization.
- **Directory trees** are walked iteratively (an explicit work stack) to avoid async recursion,
  creating destination directories and enqueuing children.
- **Move** = an atomic `rename` when source and destination share a connection with `Caps::RENAME`;
  otherwise copy-tree then `remove(.., Recurse::Yes)` — the source is deleted only after the copy
  succeeds.
- **Conflict policy** (`Skip | Overwrite | Rename | NewerWins | Prompt`) is resolved by `stat`-ing
  the destination before writing; `Prompt` returns `TransferError::Conflict` for the UI to handle;
  `Rename` finds a non-colliding `name (n)`.
- **Verification** (`VerifyPolicy::Size`) compares bytes written to the committed entry size.
- **Cancellation** is cooperative: the `CancellationToken` is checked between items, between tree
  nodes, and between chunks; an in-flight write is `abort`ed.

## Drawbacks / deferred

- No pause/resume, retry-with-backoff, per-backend concurrency semaphores, or a persistent queue yet
  — these matter most with multiple concurrent jobs and network backends, and arrive with the
  object-store work (M5) and the queue UI.
- Checksum verification (beyond size) is deferred to the object-store milestone where provider
  checksums exist.

## Security & privacy

No credentials. Errors propagate as `VfsError` (already secret-redactable).

## Unresolved questions

- The exact resumable-upload state format (RFC to follow with S3 multipart, M5).
- Bounded reader/writer pipelining for higher throughput on high-latency links.
