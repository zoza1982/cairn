# RFC-0001: Local filesystem backend

- **Status:** Accepted
- **Author(s):** rust-staff-engineer, software-engineer (synthesized)
- **Date:** 2026-06-27
- **Tracking item:** M1-2 / M1-3

## Summary

`cairn-backend-local` implements the [`Vfs`](../LLD.md) trait over `tokio::fs`, providing the first
end-to-end backend and validating the abstraction against a real filesystem.

## Motivation

The local filesystem is the baseline backend and the foundation of the M1 vertical slice (browse →
operate → transfer). It must be correct and non-blocking on Linux, macOS, and Windows.

## Design

- **Rooting.** `LocalVfs::new(conn, root)` holds a base directory; a `VfsPath` is resolved relative
  to it. Because `VfsPath` rejects `..` at parse time, requests cannot escape the root. The base is
  the OS filesystem root in normal use; rooting at an arbitrary directory keeps the backend testable
  and contained (and sidesteps Windows drive-letter handling for now — see Unresolved).
- **Async.** All I/O uses `tokio::fs`; nothing blocks. `list` returns a single `ListPage` per
  directory for M1 (local reads are fast; the TUI virtualizes — streaming/chunked listing is a later
  optimization). `open_read` returns a `tokio::fs::File` (seeked + `take`-bounded for ranged reads).
  `open_write` writes directly to the target file, `sync_all` on `finish`, and removes the partial
  file on `abort`.
- **Capabilities.** `LIST|READ|WRITE|CREATE_DIR|DELETE|RENAME|RENAME_ATOMIC|RANDOM_READ|APPEND`
  everywhere; `CHMOD|SYMLINK` added on Unix. `set_perms` is real on Unix and returns
  `Unsupported(CHMOD)` elsewhere.
- **Errors.** `io::Error` is mapped to `VfsError` by kind (`NotFound`/`PermissionDenied`→`Forbidden`/
  `AlreadyExists`), else `Io`.
- **Hidden files.** Excluded unless `ListOpts.all`.

## Drawbacks

Direct (non-atomic) writes leave a partial file if the process is killed mid-write; acceptable for
M1 (the transfer engine adds resumability/atomicity later).

## Rationale & alternatives

- *Map `VfsPath` straight to absolute OS paths* — cleaner for "browse the whole FS" but needs
  Windows drive handling and risks accidental escapes; deferred.
- *Atomic temp-write + rename for `open_write`* — better durability; deferred to the transfer-engine
  work to keep this backend minimal.

## Security & privacy

No credentials. The traversal-safe `VfsPath` is the security boundary; the root contains requests.

## Unresolved questions

- Windows: how to present drives (`C:`, `D:`) — a synthetic root listing drives, or per-drive
  connections. To be settled before a Windows release.
- `notify`-based `WATCH` support (deferred; `Caps::WATCH` currently off).
- Symlink targets are not yet surfaced in `Entry.symlink_target` (set to `None`).
