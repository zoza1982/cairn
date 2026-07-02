# ADR-0010: Ephemeral containers for Docker image content browsing

- **Status:** Accepted
- **Date:** 2026-07-02
- **Deciders:** container-backend-engineer (design; cross-checked by rust-staff-engineer,
  security-engineer per CLAUDE.md §2)

## Context

RFC-0004 shipped `cairn-backend-docker` with container filesystem browsing over the Docker
archive (tar) API, but deferred image content browsing: entering `/images/<tag>` returned an
empty listing regardless of the image's actual contents (`crates/cairn-backend-docker/src/lib.rs`,
the old `["images", tag] => if exists { Vec::new() }` arm). That's a silent lie to the user — the
image is not empty, browsing it is just unimplemented.

The Docker Engine API has no "browse an image's rootfs" endpoint. The two realistic approaches:

- **A — ephemeral container.** `docker create` (never `start`) a container from the image, then
  reuse the existing container-filesystem archive/tar path (`ContainerOps::list_dir`/`stat`/
  `read`) against it, exactly as if it were a container.
- **B — OCI layer walk.** Pull/inspect the image's layer blobs directly (registry or local
  content store) and present a per-layer or merged view without touching the container API.

Forces:

- The container-fs path (list_dir/stat/read over `GET /containers/{id}/archive`) is already
  implemented, tested against a mock, and validated against a live daemon (RFC-0004, M6-2). Any
  approach that produces a container id gets all of that for free.
- A `docker create`d-but-never-started container's rootfs is exactly the image's rootfs (no
  entrypoint/cmd executes, nothing writes to it) — semantically it *is* image content browsing,
  not container browsing.
- Leaving containers behind on the daemon is a real risk: crashes, panics, or a user closing Cairn
  mid-browse must not leak `docker create`d containers indefinitely.
- Multiple Cairn instances (or multiple panes) may browse the same image concurrently, or the same
  image by different tag/digest aliases — creation should not race or duplicate.

## Decision

We implement **Approach A (ephemeral container)** for this phase. Approach B (OCI per-layer view)
is deferred to a later phase — see Consequences.

### Lifecycle

1. `DockerVfs` resolves an `/images/<tag>` path to a canonical image id via
   `ContainerOps::resolve_image_id(tag)`, matching `tag` against either a repo:tag or the raw
   image id. This is a **separate, cheap method from `list_images()`** — it runs on every
   `list_dir`/`stat`/`read` inside an image, so it must stay a single lookup rather than pay
   `list_images()`'s per-image `inspect_image` cost (see step 5's sibling note and Consequences).
2. It calls `ContainerOps::ephemeral_for_image(image_id)` — **keyed by the canonical image id,
   never the tag** — so `nginx:latest` and the raw id `nginx` resolves to (e.g. `img1` in the
   mock, `sha256:…` for a real daemon) share one ephemeral container instead of each getting their
   own. **Caveat:** `resolve_image_id` currently matches only `repo_tags` and the raw id, **not**
   `repo_digests` — a digest reference (`nginx@sha256:…`) is not yet resolved to the same
   container as its tag. Matching `repo_digests` too is a cheap, tracked follow-up (see
   Consequences); until then, browsing by digest reference isn't supported at all (`resolve_image_id`
   returns `NotFound`), which is honest rather than silently wrong.
3. `BollardDocker::ephemeral_for_image` single-flights creation per image id via
   `tokio::sync::OnceCell` in a `HashMap<image_id, Arc<OnceCell<Arc<EphemeralEntry>>>>`, guarded by
   a `std::sync::Mutex` around the map lookup/insert (not held across the `.await`). Concurrent
   callers for the same image id converge on one `docker create` call.
4. On creation failure, `OnceCell::get_or_try_init` leaves the cell **uninitialized** and returns
   the error — it does not permanently poison the cache. The next call for that image id retries
   `create_ephemeral_container` from scratch. This matters because create failures can be
   transient (daemon under load, momentary image-store contention).
5. The created container: `network_disabled: true`, `host_config.readonly_rootfs: true`, and
   labeled `cairn.role=image-browse-ephemeral`. **It is never started, and its `entrypoint`/`cmd`
   are deliberately left unset** (inheriting the image's own config) — the container never runs
   regardless, but Docker validates the merged command is non-empty at `create` time, and forcing
   empty vectors would turn a working `create` into a spurious failure for any image whose own
   config has no `CMD`/`ENTRYPOINT` (some minimal/distroless-style images). `list_dir`/`stat`/
   `read` then run against the created container through the existing container-fs path,
   unchanged. **An image with neither `CMD` nor `ENTRYPOINT` still cannot be browsed this way** —
   Docker's `create` validation rejects it regardless of what we send — but `create_ephemeral_container`
   detects the daemon's specific "No command specified" error (matched on message content, since
   the Engine reports this as a 500 rather than a 4xx) and maps it to a clear
   `VfsError::Backend { code: "image_no_command", .. }` instead of an opaque generic `Backend`
   error, so the failure is at least legible. This is exactly the case a future OCI-layer view
   (Approach B) would sidestep entirely, since it would never need a container at all.
6. Every hit refreshes an in-memory `last_access: Instant` on the cache entry, read by the idle
   reaper (below).
7. **Image directory-entry naming.** The `/images` listing names each entry by its first tag —
   except when that tag contains `/` (namespaced/registry images: `grafana/grafana`,
   `myorg/app:v1`, `registry.example.com/team/app` — the common case for anything outside the
   Docker Hub official library), which `VfsPath` cannot represent as a single path segment. Those
   images are listed and browsed by their (always segment-safe) id instead; the human tag is still
   carried in `EntryExt::Image.tags` for display.

### Cleanup — two tiers, no graceful-shutdown hook (yet)

A clean "close this pane, tear down its ephemeral containers" hook would need a
`ContainerOps`/`Vfs`-level close/teardown API that doesn't exist today and touches every backend,
not just Docker — that's a `software-architect` conversation, tracked as a follow-up (see
Consequences). Until it lands, two time-based safety nets do the job:

- **Tier 1 — idle-TTL reaper.** A background task per `BollardDocker`, ticking every 60 s,
  force-removes and evicts any ephemeral container whose `last_access` is older than 5 minutes.
  Handles the common case (pane closed, image browse abandoned) without any explicit signal.
- **Tier 2 — label+age startup sweep.** A second background task, ticking every 10 minutes (and
  once immediately on activation), lists containers by the `cairn.role=image-browse-ephemeral`
  label and force-removes any whose daemon-reported `Created` timestamp is older than 30 minutes
  **and that this process's own `EphemeralRegistry` isn't currently tracking as live.** That
  registry check matters as much as the age threshold: a continuously-used browse session can
  exceed 30 minutes without ever going idle long enough for tier 1 to reap it, and an age-only
  sweep would kill it out from under an active `list_dir`/`stat`/`read` call. Tier 2's job is
  catching orphans this process doesn't know about — a prior crashed run, or another instance that
  has since exited — not second-guessing its own live cache. This is the crash-safety net: if
  Cairn is killed (not a clean exit), tier 1 never runs again, but the *next* Cairn process to
  talk to that daemon reaps the orphan (which, being a different process, is by definition not in
  *its* registry) on its own sweep.
  **The 30-minute age threshold is deliberate, not "remove everything with this label"** — a
  second, independent Cairn instance may be legitimately browsing the same image against the same
  daemon right now, and an unconditional sweep would rip its live container out from under it.

Both tasks start via `BollardDocker::ensure_background_tasks` — an idempotent, `OnceCell`-guarded
method safe to call from multiple sites — rather than automatically inside
`connect_local`/`connect_with_socket` themselves. Two things call it:

1. **Real connection-open call sites** (`cairn::app::open_docker_socket`, used for RFC-0011 P3
   auto-discovered targets, and `cairn::connect::ConnectionOpener::open_docker`, used for saved/
   pinned profiles) call it explicitly, immediately after constructing the `BollardDocker` they're
   about to hand to a `DockerVfs` and keep. This means the crash-safety sweep (tier 2) starts
   reaping orphans from the moment the user actually opens a Docker connection — not only once
   they happen to browse an image on it, closing the gap an earlier version of this ADR left
   between "on connect" (the claim) and "on first image browse" (the actual old behavior).
2. `ephemeral_for_image_impl` also calls it defensively, so a caller that constructs a
   `BollardDocker` and goes straight to browsing without an explicit start call (the dind
   integration tests do this) still gets both reapers.

Deliberately **not** called from `connect_local`/`connect_with_socket` themselves, because
`discovery::probe_one` (RFC-0011 P3) calls exactly those two constructors, pings, and drops the
`BollardDocker` immediately per candidate socket — spawning two background tasks per probe (which
never browses an image) would be pure waste, spun up and aborted milliseconds later on every
discovery pass. Making the constructors themselves "know" whether they're being used for a
probe or a real connection would require a second constructor or a flag; calling
`ensure_background_tasks()` explicitly from the real-connect call sites achieves the same
separation without that extra API surface.

### Why not a graceful-shutdown hook now

Rejected for *this* PR, not forever: it requires deciding where in the `Vfs`/`ContainerOps` trait
surface a "the user is done with this connection" signal lives, and whether it's Docker-specific
or a general pattern other backends (S3 multipart uploads left open, SSH connection pools) would
also want. That's cross-cutting enough to need its own design pass rather than being bolted on
here. Tiers 1+2 are the interim safety net; the hook is tracked as a follow-up issue.

## Consequences

### Positive

- Reuses the proven, tested `list_dir`/`stat`/`read` archive/tar path verbatim — no new
  filesystem-access code, no new class of bugs in that path.
- `stat`/`list_dir` on an image now give an honest answer (real rootfs, or a real error) instead
  of a silent empty list.
- Tag and raw-id aliases of the same image share one ephemeral container (id-keyed cache), so
  browsing `nginx:latest` and then its raw image id doesn't double the daemon-side footprint.
  (Digest references don't share yet — see the Lifecycle caveat and Negatives.)
- The tier-2 sweep's age threshold (not label-only) means a Cairn instance's own sweep never reaps
  its own still-live container, and it starts reaping crash orphans as soon as a Docker connection
  is actually opened (see Lifecycle), not only once an image happens to be browsed on it. (This
  does **not** fully protect against a *different* instance's browse session — see Negatives.)

### Negative / trade-offs

- A `docker create`d container exists on the daemon for the life of a browse session (up to 5
  minutes idle, or up to 30 minutes in the crash-recovery case) — visible in `docker ps -a`,
  consuming a container slot and a thin read-only layer's worth of daemon bookkeeping. Acceptable:
  it never runs, costs no CPU, and is clearly labeled.
- No graceful-shutdown hook yet: a container from an abandoned browse lives for up to the idle TTL
  (typically much less than the worst case) rather than being removed the instant the pane closes.
- `list_images()` now does one `inspect_image` call per image to populate `EntryExt::Image.layers`
  from `RootFS.Layers` (previously hardcoded `0`) — N+1 API calls. This is paid once per `/images`
  directory render, **not** per navigation step inside an image — the image-browse hot path
  (`list_dir`/`stat`/`read` inside `/images/<tag>/…`) resolves tag→id via the cheaper
  `ContainerOps::resolve_image_id` instead, specifically to avoid multiplying this cost. Acceptable
  for typical local image counts; flagged in the code as a target for `join_all` parallelization if
  it proves slow on hosts with very large local image caches.
- **Known limitation — residual idle-reaper TOCTOU.** The reaper (`idle_reaper_loop` /
  `reap_idle_pass`) snapshots idle-looking `(image_id, cid)` candidates, then — for each — re-locks
  the registry and only commits to evicting-and-removing if the entry's cid still matches the
  snapshot *and* it is still idle right now (`evict_if_still_idle`); eviction happens **before**
  the daemon-side `remove_container` call, in the same critical section as the re-check. This
  closes the wide version of the race (a resumed browse's `last_access` refresh, which happens
  synchronously before the caller uses the returned cid, is always visible to a re-check that
  starts after it) and a unit test (`evict_if_still_idle_skips_when_last_access_was_refreshed_after_snapshot`)
  pins it. A narrower residual window remains: the hand-off between `ephemeral_for_image_impl`
  releasing the registry lock (after cloning the cell) and it re-acquiring a *different* lock to
  write `last_access` is not itself atomic, so a reaper re-check landing in that exact multi-
  instruction gap could still observe stale data. Additionally, `last_access` is stamped once when
  `ephemeral_for_image` resolves the container id, not continuously refreshed while a single
  `list_dir`/`stat`/`read` call is in flight — a single operation exceeding the idle TTL (5
  minutes; e.g. `list_dir` on a very large image over a slow daemon, given the archive endpoint
  streams the whole recursive subtree into memory, see the `list_dir` M6 memory note) could still
  be reaped mid-fetch. Closing both fully needs an in-flight "in-use" guard/refcount the reaper
  honors, which is more machinery than this phase warrants. Tracked as a follow-up alongside the
  graceful-shutdown hook.
- **Known limitation — cross-instance sweep can reap another instance's long-running browse.** The
  tier-2 sweep's registry check only protects *this* process's own live containers; a second Cairn
  instance's sweep sees only the 30-minute age threshold for a container it doesn't have in its own
  registry. A continuously-used browse session in instance A that runs longer than 30 minutes can
  be force-removed by instance B's sweep, even though it's still live. The 30-minute age threshold
  makes this unlikely for a typical browse (open, look around, close), but it is a real gap for a
  pane left open on an image for a long time. **Not fixed here** — the earlier draft of this ADR
  claimed instances "do not reap each other's live containers," which was true only within the
  30-minute window; that claim is corrected above (Positive). The proper fix is to have
  `ephemeral_for_image` periodically re-stamp a `cairn.last-access` label (or similar) on the
  daemon-side container itself, so a *different* instance's sweep can see recent activity even for
  containers outside its own local registry — deferred pending that follow-up (daemon chatter on
  every access, plus scope) rather than implemented speculatively now.
- `resolve_image_id` does not match `repo_digests` (see the Lifecycle caveat) — browsing by a
  digest reference (`nginx@sha256:…`) is not currently supported; it resolves to `NotFound` rather
  than silently matching the wrong image or duplicating a container. Matching `repo_digests` too
  is a cheap follow-up.

### Neutral

- Approach B (OCI per-layer view: `/images/<tag>/@layers/<n>/…` or similar) remains a live option
  for a later phase — it would let a user inspect an individual layer's diff rather than the
  merged rootfs, and would avoid touching the container API at all. It is complementary to, not a
  replacement for, this ADR's merged-rootfs view.

## Alternatives considered

- **Option B (OCI layer walk)** — more "correct" in that it never touches the container API, and
  enables true per-layer inspection. Rejected *for this phase* because it requires new code to
  read OCI layout / registry blobs and merge/present layers, none of which is proven yet, versus
  Approach A's reuse of an already-shipped, already-tested path. Revisit as a follow-up phase.
- **Keying the ephemeral cache by tag instead of image id** — simpler cache key, but breaks the
  moment a user browses the same image by two different tags/digests: two ephemeral containers for
  one image, doubling daemon-side footprint and idle-reaper bookkeeping for no benefit. Rejected.
- **Permanently caching a creation failure** — simpler `HashMap<String, Result<String, VfsError>>`
  cache, but a transient daemon error (momentary overload, image-store lock contention) would
  wedge that image's browsing until process restart. `OnceCell::get_or_try_init`'s built-in
  "uninitialized on error" behavior gives free retry-ability with no extra bookkeeping. Chosen.
- **Unconditional label-based sweep (no age threshold)** — simpler, but two concurrent Cairn
  instances against one daemon would reap each other's live containers on every sweep tick.
  Rejected; the 30-minute age threshold is the fix.
- **Spawning inside `connect_local`/`connect_with_socket` themselves** — matches the literal "on
  connect" phrasing most directly, but those two constructors are shared by every call site,
  including the short-lived probe-only instances `discovery::probe_one` creates on every socket
  discovery pass; spawning there would spin up and abort two tasks per probe for no benefit.
  Rejected in favor of an explicit, idempotent `ensure_background_tasks()` the real connection-open
  call sites invoke instead (see Lifecycle) — same "starts on real connect" behavior, without
  forcing the constructors to know who's calling them.
- **Deferring both tasks to the first `ephemeral_for_image` call only** (this ADR's original
  design) — simpler (one trigger point), but meant the crash-safety sweep didn't start until the
  user happened to browse an image, so a process that opened a Docker connection and only ever
  browsed containers never reaped that daemon's crash orphans. Superseded by also calling
  `ensure_background_tasks()` from the real connection-open call sites (see Lifecycle); the
  first-browse trigger remains as a defensive fallback.

## References

- RFC-0004 (Docker/OCI container backend) — the original deferral this ADR resolves.
- `docs/IMPLEMENTATION_PLAN.md` M6-2.
- `crates/cairn-backend-docker/src/{lib.rs,ops.rs,real.rs}`.
