# RFC-0003: SSH/SFTP backend

- **Status:** Accepted
- **Author(s):** network-engineer, rust-staff-engineer (synthesized)
- **Date:** 2026-06-28
- **Tracking item:** M4-1 (and M4-2/M4-3)

## Summary

`cairn-backend-ssh` browses and operates on remote hosts over SFTP. The product logic (mapping SFTP
operations to the `Vfs` trait) is isolated behind a small transport trait so it is fully unit-tested
offline; the real network transport is a thin `russh`/`russh-sftp` adapter.

## Design

- **`SftpOps` transport trait** — `read_dir`/`stat`/`read`/`write`/`remove_file`/`remove_dir`/
  `create_dir`/`rename`, all returning `Result<_, VfsError>`. This is the seam: the bug-prone mapping
  logic depends only on this trait, so it is tested against an **in-memory `MockSftp`** with no
  network.
- **`SftpVfs<O: SftpOps>`** implements `Vfs`: lists (streamed page), stats, **streaming** ranged reads and
  writes (see "Streaming I/O" below), `create_dir`, `rename`, and **recursive remove** (post-order subtree walk — files first,
  then directories deepest-first). Capabilities: `LIST|READ|WRITE|CREATE_DIR|DELETE|RENAME|
  RANDOM_READ|SYMLINK` (not `RENAME_ATOMIC`: SFTP has no portable atomic overwrite — see
  `SftpVfs::caps`).
- **`RealSftp`** implements `SftpOps` over a `russh_sftp::client::SftpSession` (any stream). It is
  compiled and type-checked against the real client API; errors are mapped to `VfsError`
  (not-found vs backend).
- **Transport (deferred to integration):** establishing the SSH connection — a `russh` client
  channel whose stream feeds `SftpSession::new`, with auth via the broker (key/agent/password) — is
  the remaining wiring, validated by a live-server integration test in a dedicated CI job (an sshd
  service container), kept out of the default offline build.

## Drawbacks / deferred

- ~~`open_read` reads the whole object into memory for now (streaming refinement later).~~
  Resolved — see "Streaming I/O".
- `exec` actions (remote `grep` → `SEARCH_CONTENT`), bastion/jump-host chains, keepalive/retry
  resilience, and the live transport are deferred to the integration step.

## Rationale & alternatives

- *Implement directly on `russh-sftp` with no trait* — would make the mapping logic untestable
  without a live/in-process server; the `SftpOps` seam buys hermetic tests for the part that has
  bugs.
- *Hand-write an in-process SFTP server for tests* — large and protocol-fiddly; the mock trait gives
  the same coverage of the mapping logic far more cheaply.

## Security & privacy

Credentials are resolved by the broker at connect time (never stored by the backend); host-key
verification and auth live in the transport layer. Errors avoid leaking secrets.

## Unresolved questions

- ~~Streaming reads/writes (vs buffer) over SFTP.~~ Resolved — see "Streaming I/O".
- The exact russh auth/host-key flow and bastion chaining (transport integration).

## Streaming I/O (resolved)

The first cut buffered: `open_read` fetched the whole file into a `Vec` and replayed it from a
`Cursor`, and the write sink accumulated every chunk in memory and uploaded it in `finish()`. That
made the transfer engine's per-chunk progress a memcpy — a copy to SFTP raced to 100% and then sat
under "Finalizing…" for the entire real upload — and held a whole file in RAM per transfer.

`SftpOps` now hands out handles instead of buffers:

- `open_read(path, range) -> Box<dyn AsyncRead + Send + Unpin>`: the `russh-sftp` `File`
  (seeked, and wrapped in `Take` for a bounded range). `File::poll_read` issues one `SSH_FXP_READ`
  per poll, so nothing is buffered beyond a packet. Only the open+seek is retried (idempotent);
  a mid-stream failure fails that file — resuming is a ranged re-open, an engine-level concern.
- `open_write(path) -> Box<dyn SftpWriteStream>`: a small purpose-built trait (`write_all` /
  `finish` / `abort`) over the `File`'s `AsyncWrite`. `File::poll_write` pipelines `WRITE`s in a
  bounded window (`max_concurrent_writes`, 8 × ≤ `max_packet_len`), so awaiting `write_all` *is* the
  backpressure and the bytes the engine counts are bytes the server has been handed. `finish` runs
  `flush` then `shutdown` and **surfaces the `CLOSE` status** (previously discarded — the last chance
  to learn a commit failed). `abort` drops the handle and removes the file the open created, which
  exists from the moment `open_write` returned (CREATE|TRUNCATE).
- Not `AsyncWrite` directly: the mock stays a plain `async fn` impl, `finish` maps the close status
  through the crate's error classifier, and `abort` owns the cleanup policy with the path it opened.
- **Writes go to a hidden sibling temp** (`.<name>.cairn-<pid>-<seq>.part`) and `SftpVfs`'s write
  sink renames it onto the target in `finish`, reusing the overwrite emulation `Vfs::rename` already
  has (move aside → rename → restore on failure). Opening the *target* with CREATE|TRUNCATE would
  destroy the user's existing file the instant an overwrite copy started, so a cancel or a mid-file
  error — both of which abort the sink — would leave them with nothing. With the temp, the original
  is untouched until the new content is fully committed; `abort`, a failed flush/`CLOSE`, and a
  failed rename all remove only the temp. `WriteOpts::overwrite == false` is honored (`AlreadyExists`
  at open, and again at commit if a file appeared in between).
- `open_read` pays one extra `stat` round trip for `ReadHandle::len_hint` (the reader streams, so
  nothing else knows the size). It is best-effort: a failed stat degrades the hint to `None`, never
  the read. The transfer engine uses its own `stat`, not the hint.

Throughput is unchanged or better: the old path serialized read-all then write-all; the new path
overlaps the next source read with the in-flight `WRITE` window.

`MockSftp` models the same shape (create-on-open, per-call log, reads served in 4 KiB pieces) so
the unit tests assert *how* the mapping drives the transport, and the env-guarded
`sftp_server_repro` suite round-trips 32 MiB against a real OpenSSH `sftp-server`.
