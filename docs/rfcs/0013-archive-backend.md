# RFC-0013: Archive Backend — Read-Only Tar/Zip Browsing

- **Status:** Draft (P4 implemented; P5 designed, not yet implemented)
- **Author(s):** storage-engineer, software-architect (synthesized)
- **Date:** 2026-07-03
- **Tracking item:** M4-8 (see `docs/IMPLEMENTATION_PLAN.md`)

## Summary

Cairn can browse every backend (local, SSH, S3/GCS/Azure, Docker, Kubernetes) but treats a `.tar` or
`.zip` file as an opaque blob. This RFC adds a **read-only archive backend** that lets a pane
descend into a local, uncompressed tar or zip file exactly like a directory:

1. **P4 (this PR): uncompressed tar and zip, local only.** `Enter` on a file whose *magic bytes*
   (never its extension) identify it as a recognized tar or zip mounts it as a fresh, ephemeral
   connection; the pane browses it like any other directory tree. `..` at the archive's root leaves
   back to the connection/directory the pane came from. Copying files *out* of the mounted archive
   works for free through the existing transfer engine (stream-through copy via `open_read`, since
   `copy_within` stays `Unsupported`). No writing back into the archive; no auto-descent into a
   nested archive member.
2. **P5 (designed, not implemented): compressed tar** (`.tgz`/`.txz`/`.tbz2`/`.tzst`). Same mount
   model and security posture, decoding through a streaming decompressor ahead of the same `tar`
   crate scan. Deferred because it changes the tar indexing strategy (see "Unresolved questions").

Also deferred, beyond P5: staging a *remote* archive to a local temp file so it can be mounted
(v1 requires the archive to already be on a local pane), nested-archive browsing (opening an
archive found inside a mounted archive), and mount refcounting/explicit unmount.

## Motivation

Users routinely need to peek inside a downloaded `.tar.gz` or a `.zip` attachment — check what's in
it, pull one file out — without a shell. Today that means leaving Cairn. This is the natural
extension of RFC-0012 (file open/view/edit): where RFC-0012 taught Cairn to look *at* a file's
bytes, this RFC teaches it to look *inside* a file that is itself a filesystem.

Two things make this a genuinely new backend rather than a viewer feature:

- **It's a directory tree, not a byte stream.** The pager/editor model (open one file, show its
  bytes) doesn't fit "browse hundreds of entries, `Enter` into subdirectories, copy some of them
  out." The existing `Vfs` trait already models exactly this shape.
- **The bytes are untrusted.** Unlike every other backend, the "server" here is a file the user
  didn't necessarily create and may not fully trust (a download, an email attachment, a file
  handed over by someone else). Parsing it is attacker-facing parsing, not "our own filesystem,"
  and has to be threat-modeled as such (see "Security and privacy considerations").

## Guide-level explanation

Press `Enter` on `notes.zip`. Cairn reads its first bytes, recognizes the zip local-file-header
magic (`PK\x03\x04`), and — instead of opening the pager or an error — mounts the archive and
navigates the pane into it. The pane now shows the archive's top-level entries exactly like a
directory listing: subdirectories, files, and (grayed-out, inert) symlinks. `Enter` on a
subdirectory descends normally. Copy the marked files to the other pane and they stream out to
wherever that pane points, same as any other cross-backend copy.

At the archive's root, pressing `Leave`/`..`/`Backspace` doesn't error or wrap around — it takes you
back to wherever you were (the directory that contained `notes.zip`, on whichever backend that was).

What you *can't* do (v1): edit a file inside the archive and have it write back, delete/rename/create
inside it, or `Enter` into a `.tar.gz` found inside a mounted archive (it shows as a plain file —
open it as a separate, explicit action once nested-archive browsing lands).

## Reference-level explanation

### Crate shape

A new crate, `cairn-backend-archive`, sibling to `cairn-backend-local`/`cairn-backend-object`:

```
cairn-backend-archive/
├── src/lib.rs          — module docs + the ArchiveOps trait
├── src/vfs.rs           — ArchiveVfs (impl Vfs), magic-byte format sniff, ArchiveVfs::open
├── src/tar_backend.rs   — TarOps: sequential-scan indexing + seek-based reads
├── src/zip_backend.rs   — ZipOps: central-directory indexing + by_index reads
├── src/index.rs         — ArchiveIndex/IndexBuilder shared by both formats
└── src/security.rs      — caps, path validation, the zip ratio guard
```

`ArchiveVfs` implements `Vfs`, advertising `Caps::LIST | Caps::READ | Caps::RANDOM_READ` only —
every mutating trait method (`open_write`, `remove`, `rename`, `create_dir`, `copy_within`, `invoke`)
keeps the trait's default `Unsupported` response, and `local_path` keeps the default `None` (an
archive member is not a real OS path a local process can act on — v1 has no unpack-to-temp path).

`ArchiveOps` is the small trait implemented once for tar, once for zip, so `impl Vfs for ArchiveVfs`
is written exactly once (mirroring how `cairn-backend-ssh` factors `SftpOps` out of `SftpVfs`):

```rust
pub(crate) trait ArchiveOps: Send + Sync + 'static {
    fn list_children(&self, dir: &VfsPath) -> Result<Vec<Entry>, VfsError>;
    fn entry_meta(&self, path: &VfsPath) -> Result<Entry, VfsError>;
    fn read_member(&self, path: &VfsPath, cap: u64) -> Result<Vec<u8>, VfsError>;
}
```

**One deliberate deviation from the `SftpOps` precedent:** `ArchiveVfs` holds `Arc<dyn ArchiveOps>`
rather than being generic (`ArchiveVfs<O: ArchiveOps>`). Which concrete backend to build is decided
at *runtime*, from the file's magic bytes, inside `ArchiveVfs::open` — there is no call site that
ever names a concrete `TarOps`/`ZipOps` type, so a trait object is the natural fit; a generic
parameter would just push the same runtime branch one level up with no benefit.

Every `ArchiveOps` method is **synchronous** — `tar` and `zip` are sync-only crates (there is no
maintained async fork; `tokio-tar` is abandoned and is deliberately not used here). `ArchiveVfs`'s
async `Vfs` methods invoke them inside `tokio::task::spawn_blocking` rather than making the trait
itself `async`, keeping the render path non-blocking (CLAUDE.md §9).

### Format detection: magic bytes, never an extension

```
zip:  b"PK\x03\x04" at byte offset 0    (local file header signature)
tar:  b"ustar"       at byte offset 257  (POSIX ustar magic)
```

This check exists in two independent places, deliberately not shared as a dependency:

1. **`cairn-core::detect_file_kind`** (pure, dependency-free — `cairn-core` has no backend
   dependencies; every backend is wired at the binary edge) runs this check against the same ~8 KiB
   prefix `AppEffect::SniffFile` already reads for the text/binary heuristic, classifying the result
   as `FileKind::Archive(ArchiveFormat)` *before* the NUL-byte check (an archive's bytes are
   thoroughly binary, so the archive check must win the race).
2. **`ArchiveVfs::open`** (in `cairn-backend-archive`) re-derives the same two constants
   authoritatively against the real file, independent of whatever the sniff decided — the backend
   must be correct on its own even if it were ever invoked some other way.

A `.zip` that fails both checks (corrupt, or a false-positive extension) falls through to the normal
text/binary classification, exactly like any other file — extension is never consulted anywhere in
this path.

### Indexing: tar and zip differ fundamentally

**zip** has a central directory: `zip::ZipArchive::new(File)` parses it in one pass, and the whole
member list (names, compressed/uncompressed sizes, unix mode) is available without touching any
member's content. `ZipOps::build` walks it once, validating and classifying each entry; reads later
go through `archive.by_index(i)` (guarded by a `Mutex`, since `Vfs::open_read` takes `&self`) to
decode lazily.

**Plain tar** has no such index — it's a sequential stream of `header, data, header, data, …`. One
initial scan via `tar::Archive::entries_with_seek()` (uses `Seek` to skip over each member's
*content* rather than reading it) records each kept member's `raw_file_position()` (a byte offset
into the file) and declared size. Reads later `seek` directly to that offset and read the capped
byte count. No re-scan, no temporary extraction to disk, any archive size.

Both indexers feed a **shared** `IndexBuilder`/`ArchiveIndex<L>` (`index.rs`, generic over the
locator type — a `u64` byte offset for tar, a `usize` central-directory index for zip). Neither tar
nor zip guarantees an explicit header for every intermediate directory (tar's `Directory` entry type
is conventional, not required; zip directory entries are a convention too), so the builder
synthesizes any missing ancestor directory as members are inserted — building the tree once, up
front, rather than re-deriving it from common prefixes on every `list_children` call (the
object-store backend's approach, which makes sense there because it re-queries per page; here the
whole member list is already in hand).

**A deliberate simplification vs. true streaming:** `ArchiveOps::read_member` returns a `Vec<u8>` (a
capped, in-memory buffer), the same shape `SftpVfs`/`ObjectStoreVfs` already use — `Vfs::open_read`
wraps it in `std::io::Cursor`. This is not "stream a member's bytes lazily to the caller"; it reads
up to the per-member cap (see below) and returns. This keeps the implementation aligned with the
existing backend conventions and is safe *because* the security caps already bound how much a
single read can decode — a genuinely zero-copy streaming reader for archive members is future work
if a workload needs it (e.g. copying many-GiB members out at scale).

### Mount model: an ephemeral connection, not an overlay

An archive mount is a **real entry in the `VfsRegistry`**, addressed by a normal `ConnectionId` —
not a transient view bolted onto the pane. See ADR-0012 for the full rationale; in short:

- `Scheme::Archive` (new, `cairn-types`) names the backend family.
- `AppEvent::FileSniffed`'s `FileKind::Archive(_)` arm emits `AppEffect::MountArchive { pane, conn,
  path }` (`cairn-core`, pure — no I/O in the reducer).
- The runtime (`crates/cairn/src/app.rs`) resolves `Vfs::local_path(&path)` **first**, off the
  render path. `None` (the source entry lives on a remote backend, e.g. a `.zip` on S3) refuses
  cleanly with `AppEvent::ArchiveMountFailed { message: "Copy the archive to a local pane to browse
  it" }` and touches nothing else — v1 requires the archive to already be local; auto-staging a
  remote archive to a temp file is deferred (see "Unresolved questions").
- On `Some(local_path)`, `ArchiveVfs::open(new_conn, local_path)` builds the index (inside its own
  `spawn_blocking`), the runtime mints a fresh `ConnectionId` from a disjoint, monotonically
  increasing range (`ARCHIVE_CONN_ID_BASE = 1_000_000_000`, far above anything the connection
  coordinator could plausibly assign), inserts the result into the `VfsRegistry`, and reports
  `AppEvent::ArchiveMounted { pane, conn: new_id, root }`.
- The reducer's `AppEvent::ArchiveMounted` handler pushes the pane's **pre-mount** `(conn, cwd)`
  onto a new `PaneState::mount_stack: Vec<MountFrame>`, then re-points the pane at the new
  connection and navigates to `root` — the same shape as the existing `ConnectionOpened` success
  path.
- `leave_dir` (the `Action::Leave`/`..`/Backspace handler) already no-ops when `cwd.parent()` is
  `None` (at the VFS root). It now checks `mount_stack` first: if non-empty, it pops the top frame
  and restores that `(conn, cwd)` instead. A `Vec` (not a single slot) so mounting an archive found
  *inside* another mounted archive would nest correctly, once nested-archive browsing lands.
- The mounted connection stays in the registry for the rest of the session (v1) — no refcounting,
  no explicit unmount. Revisiting the same archive path re-mounts it (a second entry, a second
  connection); this is a known, accepted v1 limitation (see ADR-0012's consequences).

### Security guards (untrusted-input parsing)

An archive's *structure itself* is attacker-influenceable data — unlike browsing the local
filesystem (whose contents the OS already mediates), parsing tar/zip headers is genuine untrusted
bytes handling. Every guard below has a dedicated hermetic test constructing the adversarial case
with the same `tar`/`zip` writer APIs (never a checked-in binary fixture):

| Guard | Where | Test |
|---|---|---|
| Per-member read cap (64 MiB) | `ArchiveVfs::open_read` computes `cap`; `ArchiveOps::read_member` never reads more | `tar_backend`/`zip_backend`: `per_member_cap_bounds_the_read` |
| Cumulative per-session decoded-byte cap (512 MiB) | `ArchiveVfs`'s `AtomicU64` counter, checked before every read | exercised via the cap constant; a session-scoped integration test is a natural QA follow-up once transfer-engine wiring lands |
| Zip compression-ratio guard (>100:1 above a 1 MiB floor) | `zip_ratio_is_bomb`, checked from central-directory metadata *before* any decompression | `zip_backend::absurd_ratio_member_is_rejected_before_decompression` (a **real** deflated 2 MiB run of zeros) |
| Entry-count cap (100k), checked against every raw header/central-directory record *seen* | `security::check_entry_count`, called by both `TarOps::build` and `ZipOps::build` | `security::entry_count_cap_boundary` (boundary) + `zip_backend::entry_count_cap_rejects_an_oversized_archive` (a real 100 001-entry zip) |
| Path traversal / absolute paths / UNC / drive letters / NUL | `validate_member_name` (normalizes `\`→`/`, rejects a leading `/`, rejects `C:`-style prefixes, delegates `..`/control-char rejection to `VfsPath::parse`) | `security::rejects_absolute_and_traversal_and_unc_and_drive`, `tar_backend`/`zip_backend`: `traversal_member_is_skipped_not_fatal` |
| Symlink/hardlink members: inert, never followed | `StoredEntry::Symlink`; `ArchiveVfs::open_read`/`ArchiveOps::read_member` both return `Unsupported` for one | `tar_backend::symlink_read_is_unsupported`, `zip_backend::symlink_member_is_inert_with_target_shown` |
| Special files (device/FIFO/socket) and setuid/setgid/sticky members: skipped, never materialized | tar: `EntryType` match + `header.mode() & 0o7000`; zip: `unix_mode()` file-type bits + the same mask | `tar_backend::special_and_setuid_members_are_skipped` |
| Checked arithmetic on size/offset/count fields | `take = declared.min(cap)` (no addition that could overflow); `check_entry_count` is a plain comparison | `tar_backend::huge_declared_size_does_not_overflow_or_over_allocate` (a genuinely truncated tar with a `u64::MAX/2`-declared size) |
| No auto-descent into a nested archive member | Not implemented at all in P4 — a nested archive is just a `File` entry | N/A (absence of a feature) |
| Display-name sanitization (length cap; control chars rejected) | `validate_member_name`'s `ARCHIVE_MAX_NAME_LEN` cap; `VfsPath::parse` rejects control chars outright | `security::rejects_overlong_names`, `rejects_control_chars_and_nul` |

Additional hardening: `#![forbid(unsafe_code)]` (workspace default); no `unwrap`/`expect`/`panic!` on
any backend-reachable path — a malformed header, an unreadable central-directory record, or a
poisoned internal `Mutex` (recovered via `PoisonError::into_inner`, never `.unwrap()`) all become a
typed `VfsError`, never a crash; no member bytes are ever passed to the AI layer (this backend has
no `invoke`/action surface at all); errors are redacted the same way every other backend's are
(`VfsError::redacted()`).

### What P4 explicitly does not do

- Compressed tar (`.tgz`/`.txz`/`.tbz2`/`.tzst`) — P5.
- Mount a remote archive without first copying it to a local pane.
- Write, rename, delete, or create anything inside a mounted archive.
- Auto-descend into an archive found inside another archive.
- Refcount or explicitly unmount a mounted archive connection.

## Drawbacks

- **A second, independent implementation of the magic-byte check** (`cairn-core` and
  `cairn-backend-archive` each have their own tiny copy) rather than a shared dependency — accepted
  because `cairn-core` has no backend dependencies by design (RFC-0007/RFC-0011's isolation
  invariant) and the check itself is two constants and two comparisons; a shared crate would be
  disproportionate ceremony for this little logic.
- **In-memory buffered reads, not true zero-copy streaming** — bounded and safe, but a very large
  member (say, a multi-GiB file inside a zip) pays the cost of a capped, buffered copy rather than a
  genuinely lazy stream. Acceptable for v1's target use case (peek/extract a handful of files); the
  security caps mean this is a correctness trade-off, not a safety one.
- **No unmount** — a long Cairn session that mounts many archives accumulates registry entries and
  open file handles for the process lifetime. Tracked as a known v1 limitation (ADR-0012).

## Rationale and alternatives

- **Mount-as-connection vs. a transient in-pane overlay.** Considered rendering archive contents as
  a special overlay without registering a real `Vfs`/`ConnectionId`. Rejected: every existing
  feature (copy/move, marks, filtering, sort) is already built against `Vfs`/`ConnectionId`/
  `VfsPath` — reusing that machinery is what makes "copy files out" free. See ADR-0012 for the full
  comparison.
- **Buffered reads vs. a true streaming `ArchiveOps`.** Considered a `Read`-returning API per member.
  Rejected for v1: it would need per-format `Send`-safe streaming decoders threaded through
  `spawn_blocking` in a way that doesn't fit `Vfs::open_read`'s `ReadHandle` cleanly for a first cut,
  and the buffered-plus-cap approach is both simpler and already the pattern two other backends use.
- **Generic `ArchiveVfs<O: ArchiveOps>` vs. `Arc<dyn ArchiveOps>`.** See "Crate shape" above —
  runtime format selection makes the trait-object form the natural fit here, unlike `SftpVfs<O>`
  where the transport is fixed at the call site.

## Security and privacy considerations

Covered in depth under "Security guards" above. Two points worth calling out explicitly for the
security reviewer:

- This backend introduces the project's **first genuinely untrusted-bytes parser** reachable from a
  normal user action (`Enter` on a file) — every other backend either talks to infrastructure the
  user configured (SSH/S3/Docker/K8s) or is the local OS filesystem. The threat model here is
  "a file handed to the user by someone/something else," which is why the guard table above is this
  extensive relative to the size of the feature.
- No credential material is involved anywhere in this backend — no vault interaction, no secrets in
  scope, no `invoke`/action surface for the AI layer to reach.

## Unresolved questions

- **Remote-archive staging (P5+):** should Cairn auto-copy a remote archive to a temp file and mount
  that, or require an explicit user action? Auto-staging risks silently pulling a large file over
  the network the user didn't ask to download in full; an explicit "stage & mount" action is safer
  but adds UI surface. Deferred to when a concrete use case pushes on it.
- **Compressed tar indexing strategy (P5):** `entries_with_seek`'s "skip via `Seek`" trick doesn't
  work through a decompressor (there is nothing meaningful to seek *to* in the compressed stream for
  an arbitrary member) — P5 will likely need either a full sequential decode-and-index pass (paying
  the whole file's decompression cost up front, still bounded by the same caps) or accept that only
  gzip's trailing size field (unreliable beyond 4 GiB) offers any shortcut. Needs its own design pass
  before implementation.
- **Unmount / refcounting:** when should a mounted archive's registry entry and file handle be
  freed? Candidates: on pane navigation away and no longer referenced by any `mount_stack`, or an
  explicit close action. Deferred; v1 accepts the leak for the process lifetime as documented.

## Crate, dependency, and feature impact

New crate `cairn-backend-archive`, wired into the `cairn` binary behind a new `archive` feature
(included in `all-backends`), consistent with the `ssh`/`s3`/`gcs`/`azure`/`docker`/`k8s` pattern —
except this backend needs no vault/credential plumbing at all (it is local-file-only), so there is
no inner "transport" feature to forward, unlike the credential-bearing backends.

New external dependencies (both Context7-verified against their current published versions):

- **`tar = "0.4.46"`** (MIT/Apache-2.0) — the same crate Cargo itself uses to unpack crate sources.
  Small, pure-Rust, no TLS/FFI — unlike the S3/GCS/Azure SDKs (ADR-0006), it carries no
  cross-platform build risk. `default-features = false` (no `xattr`/`ownership` need for read-only
  browsing).
- **`zip = "8.6"`** (the maintained `zip2` fork published as `zip`, MIT). `default-features = false,
  features = ["deflate"]` only — the vast majority of real-world zips are Stored or Deflated;
  dropping `bzip2`/`lzma`/`zstd`/`xz`/`ppmd`/`aes-crypto` keeps the dependency tree lean (CLAUDE.md
  §10). An entry using an unsupported compression method surfaces as a clear, typed
  `VfsError::Backend` rather than a panic or a build-time requirement.

Both licenses pass `cargo-deny`'s existing allow-list (`MIT`, already permitted) with no `deny.toml`
changes needed.

## References

- ADR-0012 (`docs/adr/0012-archive-mount-model.md`) — the mount-as-connection + `mount_stack`
  decision and the `Scheme::Archive` addition.
- RFC-0012 (`docs/rfcs/0012-file-open-view-edit.md`) — the sniff-then-route pattern this RFC extends
  (`AppEffect::SniffFile` → `AppEvent::FileSniffed` → per-kind routing).
- ADR-0006 (`docs/adr/0006-feature-gated-backends-and-ci.md`) — referenced above for the "why a lean
  pure-Rust codec carries no equivalent cross-platform risk" comparison.
- `crates/cairn-backend-archive/`: `ArchiveVfs`, `ArchiveOps`, `TarOps`, `ZipOps`,
  `IndexBuilder`/`ArchiveIndex`, `security` module.
- `crates/cairn-core/src/state.rs`: `FileKind::Archive`, `ArchiveFormat`, `PaneState::mount_stack`,
  `MountFrame`.
- `crates/cairn-core/src/msg.rs`: `AppEffect::MountArchive`, `AppEvent::ArchiveMounted`,
  `AppEvent::ArchiveMountFailed`.
- `crates/cairn/src/app.rs`: `run_mount_archive_effect`, `open_archive`.
