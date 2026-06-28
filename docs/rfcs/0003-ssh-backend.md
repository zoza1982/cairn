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
- **`SftpVfs<O: SftpOps>`** implements `Vfs`: lists (streamed page), stats, ranged reads, buffered
  writes, `create_dir`, `rename`, and **recursive remove** (post-order subtree walk — files first,
  then directories deepest-first). Capabilities: `LIST|READ|WRITE|CREATE_DIR|DELETE|RENAME|
  RENAME_ATOMIC|RANDOM_READ|SYMLINK`.
- **`RealSftp`** implements `SftpOps` over a `russh_sftp::client::SftpSession` (any stream). It is
  compiled and type-checked against the real client API; errors are mapped to `VfsError`
  (not-found vs backend).
- **Transport (deferred to integration):** establishing the SSH connection — a `russh` client
  channel whose stream feeds `SftpSession::new`, with auth via the broker (key/agent/password) — is
  the remaining wiring, validated by a live-server integration test in a dedicated CI job (an sshd
  service container), kept out of the default offline build.

## Drawbacks / deferred

- `open_read` reads the whole object into memory for now (streaming refinement later).
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

- Streaming reads/writes (vs buffer) over SFTP.
- The exact russh auth/host-key flow and bastion chaining (transport integration).
