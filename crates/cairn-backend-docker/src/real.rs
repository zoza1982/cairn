//! The real Docker engine adapter over `bollard`.
//!
//! Container and image **listing** are implemented against the Docker Engine API. Container
//! filesystem browsing (`list_dir`/`stat`/`read`) is implemented over the Docker archive endpoint
//! (`GET /containers/{id}/archive?path=…`), which returns a tar stream. The tar is collected into
//! memory and then parsed synchronously with the `tar` crate — an adequate approach for M6
//! (streaming-extract and zero-copy are follow-ups). The full path-routing/mapping is verified via
//! the mock; the live adapter is validated against a real daemon in the dind integration job.
//!
//! Image browsing reuses that same archive-based `list_dir`/`stat`/`read` path against an
//! **ephemeral container** created (never started) from the image on first access — see
//! [`ContainerOps::ephemeral_for_image`] and ADR-0010 for the full lifecycle/cleanup design.

use crate::ops::{ContainerInfo, ContainerOps, ImageInfo, RemoteEntry, RemoteMeta};
use async_trait::async_trait;
use bollard::Docker;
use bytes::Bytes;
use cairn_types::{Caps, ContainerState, EntryKind, VfsPath};
use cairn_vfs::{SessionHandle, VfsError};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;

/// Docker label applied to every ephemeral image-browse container, so a crash-recovery sweep can
/// find and reap them without touching user containers. See ADR-0010.
const EPHEMERAL_LABEL_KEY: &str = "cairn.role";
const EPHEMERAL_LABEL_VALUE: &str = "image-browse-ephemeral";
/// Idle-TTL reaper tick interval and idle threshold (tier 1, ADR-0010).
const IDLE_REAP_TICK: Duration = Duration::from_secs(60);
const IDLE_REAP_TTL: Duration = Duration::from_secs(5 * 60);
/// Crash-safety label+age sweep tick interval and max age (tier 2, ADR-0010). The age threshold
/// (not unconditional removal) is what lets two concurrent Cairn instances share a daemon without
/// reaping each other's live ephemeral containers.
const SWEEP_TICK: Duration = Duration::from_secs(10 * 60);
const SWEEP_MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// One live ephemeral image-browse container tracked by [`EphemeralRegistry`].
struct EphemeralEntry {
    /// The container id returned by `docker create`.
    cid: String,
    /// Updated on every `list_dir`/`stat`/`read` hit that resolves through this entry; read by
    /// the idle-TTL reaper.
    last_access: Mutex<Instant>,
}

/// Per-`BollardDocker` state for ephemeral image-browse containers: a single-flight cache from
/// image id to its live ephemeral container, keyed by the **canonical image id** (never the tag)
/// so every tag/digest alias of an image shares one container. See ADR-0010.
#[derive(Default)]
struct EphemeralRegistry {
    /// image id -> single-flight cell yielding the ephemeral container for that image.
    ///
    /// Using `OnceCell::get_or_try_init` here is what gives "don't permanently cache a hard
    /// failure": on an `Err`, tokio's `OnceCell` leaves the cell uninitialized and releases its
    /// internal semaphore permit, so the *next* `ephemeral_for_image` call for the same image id
    /// retries `create_ephemeral_container` from scratch rather than replaying a stale error.
    cells: Mutex<HashMap<String, Arc<OnceCell<Arc<EphemeralEntry>>>>>,
}

/// A [`ContainerOps`] implementation backed by a live Docker engine via `bollard`.
pub struct BollardDocker {
    docker: Docker,
    /// Ephemeral image-browse container bookkeeping (ADR-0010).
    ephemeral: Arc<EphemeralRegistry>,
    /// Guards one-time spawn of the background reaper/sweep tasks (lazily started on first
    /// `ephemeral_for_image` call, not at connect time — `discovery::probe_one` constructs and
    /// immediately drops a `BollardDocker` per socket probe, and eagerly spawning here would
    /// spin up and abort two tasks per probe for no benefit).
    reaper_started: OnceCell<()>,
    /// Handles for the spawned background tasks, aborted on `Drop` so a dropped `BollardDocker`
    /// never leaves orphaned tasks polling the daemon.
    task_handles: Mutex<Vec<JoinHandle<()>>>,
}

impl BollardDocker {
    fn from_docker(docker: Docker) -> Self {
        Self {
            docker,
            ephemeral: Arc::new(EphemeralRegistry::default()),
            reaper_started: OnceCell::new(),
            task_handles: Mutex::new(Vec::new()),
        }
    }
}

impl Drop for BollardDocker {
    fn drop(&mut self) {
        if let Ok(handles) = self.task_handles.lock() {
            for h in handles.iter() {
                h.abort();
            }
        }
    }
}

impl BollardDocker {
    /// Connect using the platform's default Docker endpoint (socket / named pipe / env).
    ///
    /// Bollard's `connect_with_local_defaults` checks `DOCKER_HOST`, the standard system socket
    /// path, and (on Windows) the named pipe — in that order.
    ///
    /// # Errors
    /// [`VfsError::Connection`] if the client cannot be constructed (malformed `DOCKER_HOST`, etc.).
    /// Note: this does **not** prove the daemon is reachable; call [`ping`](Self::ping) to verify.
    pub fn connect_local() -> Result<Self, VfsError> {
        Docker::connect_with_local_defaults()
            .map(Self::from_docker)
            .map_err(|e| VfsError::Connection(Box::new(e)))
    }

    /// Connect to a Docker daemon at an explicit Unix socket path.
    ///
    /// Use this for rootless-Docker or Podman sockets discovered via `$XDG_RUNTIME_DIR`.
    /// The timeout passed to bollard is 120 seconds (the same default bollard uses internally);
    /// individual operation timeouts are the caller's responsibility.
    ///
    /// # Errors
    /// [`VfsError::Connection`] if `path` is not valid UTF-8 (bollard requires a string address)
    /// or if bollard cannot construct the client.
    pub fn connect_with_socket(path: &std::path::Path) -> Result<Self, VfsError> {
        let addr = path.to_str().ok_or_else(|| {
            VfsError::Connection(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Docker socket path is not valid UTF-8",
            )))
        })?;
        Docker::connect_with_unix(addr, 120, bollard::API_DEFAULT_VERSION)
            .map(Self::from_docker)
            .map_err(|e| VfsError::Connection(Box::new(e)))
    }

    /// Probe daemon reachability by sending `GET /_ping`.
    ///
    /// Returns `Ok(())` if the daemon is up and responds. Used by the `DockerProvider` during
    /// auto-discovery with a short external `tokio::time::timeout` wrapper.
    ///
    /// # Errors
    /// [`VfsError::Connection`] on any failure (socket not found, refused, protocol error, etc.).
    pub async fn ping(&self) -> Result<(), VfsError> {
        self.docker
            .ping()
            .await
            .map(|_| ())
            .map_err(|e| VfsError::Connection(Box::new(e)))
    }

    /// Fetch a tar archive for the given path inside the container, collecting all stream chunks.
    ///
    /// Maps an HTTP 404 from the Docker daemon to [`VfsError::NotFound`]; any other engine error
    /// becomes [`VfsError::Backend`].
    async fn fetch_archive(&self, container: &str, path: &str) -> Result<Vec<u8>, VfsError> {
        let opts = bollard::query_parameters::DownloadFromContainerOptionsBuilder::default()
            .path(path)
            .build();
        let mut stream = self.docker.download_from_container(container, Some(opts));
        let mut buf = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => buf.extend_from_slice(&chunk),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Err(not_found(path)),
                Err(e) => return Err(backend_err(e)),
            }
        }
        Ok(buf)
    }

    /// Resolve `image_id` to a live ephemeral browse container, creating one on first call for
    /// that image id (single-flight via [`EphemeralRegistry`]) and refreshing its idle-TTL clock
    /// on every call. See [`ContainerOps::ephemeral_for_image`] and ADR-0010.
    async fn ephemeral_for_image_impl(&self, image_id: &str) -> Result<String, VfsError> {
        self.ensure_background_tasks().await;

        let cell = {
            // A poisoned lock here would mean a prior holder panicked mid-critical-section; the
            // critical section is a single infallible `HashMap` insert, so that can't happen in
            // practice, but we still fail soft (`Backend` error) rather than propagate a panic —
            // no `unwrap`/`expect` on this reachable path, per CLAUDE.md §9.
            let Ok(mut cells) = self.ephemeral.cells.lock() else {
                return Err(backend_err("ephemeral registry mutex poisoned"));
            };
            cells
                .entry(image_id.to_owned())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        let entry = cell
            .get_or_try_init(|| self.create_ephemeral_container(image_id))
            .await?;
        if let Ok(mut last_access) = entry.last_access.lock() {
            *last_access = Instant::now();
        }
        Ok(entry.cid.clone())
    }

    /// `docker create` an ephemeral, never-started container from `image_id`: networking
    /// disabled, a read-only rootfs, and the `cairn.role=image-browse-ephemeral` label that the
    /// crash-safety sweep uses to find it. See ADR-0010.
    ///
    /// **Deliberately does not override `entrypoint`/`cmd`.** The container is never started, so
    /// nothing ever runs regardless — but Docker validates that the *merged* command is
    /// non-empty at `create` time, not just `start` time. Explicitly forcing `entrypoint: []` /
    /// `cmd: []` would make that validation fail for any image whose own config has no
    /// CMD/ENTRYPOINT (some minimal/distroless-style images), turning a working create into a
    /// spurious failure. Leaving both `None` lets the image's own config (which was already
    /// proven valid when the image was built) stand.
    async fn create_ephemeral_container(
        &self,
        image_id: &str,
    ) -> Result<Arc<EphemeralEntry>, VfsError> {
        let mut labels = HashMap::new();
        labels.insert(
            EPHEMERAL_LABEL_KEY.to_owned(),
            EPHEMERAL_LABEL_VALUE.to_owned(),
        );
        let body = bollard::models::ContainerCreateBody {
            image: Some(image_id.to_owned()),
            network_disabled: Some(true),
            labels: Some(labels),
            host_config: Some(bollard::models::HostConfig {
                readonly_rootfs: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resp = self
            .docker
            .create_container(
                None::<bollard::query_parameters::CreateContainerOptions>,
                body,
            )
            .await
            .map_err(|e| map_status_error(e, image_id))?;
        Ok(Arc::new(EphemeralEntry {
            cid: resp.id,
            last_access: Mutex::new(Instant::now()),
        }))
    }

    /// Lazily spawn the two background cleanup tasks (idle-TTL reaper + crash-safety sweep) at
    /// most once per `BollardDocker`. Deferred to first `ephemeral_for_image` call rather than
    /// construction time — see the `reaper_started` field doc for why.
    async fn ensure_background_tasks(&self) {
        self.reaper_started
            .get_or_init(|| async {
                let idle = tokio::spawn(idle_reaper_loop(
                    self.docker.clone(),
                    self.ephemeral.clone(),
                ));
                let sweep = tokio::spawn(startup_sweep_loop(
                    self.docker.clone(),
                    self.ephemeral.clone(),
                ));
                if let Ok(mut handles) = self.task_handles.lock() {
                    handles.push(idle);
                    handles.push(sweep);
                }
            })
            .await;
    }
}

/// Tier 1 (ADR-0010): every [`IDLE_REAP_TICK`], force-remove and evict any ephemeral container
/// that has been idle (no `list_dir`/`stat`/`read` hit) for longer than [`IDLE_REAP_TTL`].
async fn idle_reaper_loop(docker: Docker, registry: Arc<EphemeralRegistry>) {
    let mut ticker = tokio::time::interval(IDLE_REAP_TICK);
    loop {
        ticker.tick().await;
        let stale: Vec<(String, String)> = {
            let cells = match registry.cells.lock() {
                Ok(c) => c,
                Err(_) => continue, // poisoned: skip this tick rather than panic the task
            };
            cells
                .iter()
                .filter_map(|(image_id, cell)| {
                    let entry = cell.get()?;
                    let idle = is_idle(entry.last_access.lock().ok()?.elapsed(), IDLE_REAP_TTL);
                    idle.then(|| (image_id.clone(), entry.cid.clone()))
                })
                .collect()
        };
        for (image_id, cid) in stale {
            remove_ephemeral_container(&docker, &cid).await;
            if let Ok(mut cells) = registry.cells.lock() {
                cells.remove(&image_id);
            }
        }
    }
}

/// Tier 2 (ADR-0010, crash safety): every [`SWEEP_TICK`] (and once immediately — the first
/// `interval` tick fires without delay), list containers carrying the
/// `cairn.role=image-browse-ephemeral` label and force-remove any older than [`SWEEP_MAX_AGE`]
/// that this process's own [`EphemeralRegistry`] isn't actively tracking. The age threshold (not
/// unconditional removal) is what lets a second, independent Cairn instance browse images
/// against the same daemon without the two reaping each other's live ephemeral containers; the
/// registry check on top of that is what stops this process's *own* sweep from reaping its own
/// still-live, long-running browse session (tier 1's idle-TTL reaper owns that container's
/// lifecycle instead — see `sweep_stale_labeled_containers`).
async fn startup_sweep_loop(docker: Docker, registry: Arc<EphemeralRegistry>) {
    let mut ticker = tokio::time::interval(SWEEP_TICK);
    loop {
        ticker.tick().await;
        sweep_stale_labeled_containers(&docker, &registry).await;
    }
}

/// One pass of the label+age sweep — factored out so it runs identically on the immediate first
/// tick and every subsequent one.
async fn sweep_stale_labeled_containers(docker: &Docker, registry: &EphemeralRegistry) {
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_owned(),
        vec![format!("{EPHEMERAL_LABEL_KEY}={EPHEMERAL_LABEL_VALUE}")],
    );
    let opts = bollard::query_parameters::ListContainersOptions {
        all: true,
        filters: Some(filters),
        ..Default::default()
    };
    // Best-effort: a transient daemon error just skips this sweep pass, it retries next tick.
    let Ok(list) = docker.list_containers(Some(opts)).await else {
        return;
    };

    // Containers this process currently tracks as live — never age-sweep these. A long-running,
    // continuously-used browse session can easily exceed SWEEP_MAX_AGE without ever going idle
    // long enough for tier 1 to reap it; sweeping by age alone (ignoring our own liveness
    // tracking) would kill it out from under an active `list_dir`/`stat`/`read`. Tier 2 exists to
    // catch orphans this process doesn't know about (a prior crashed run, or another instance
    // that has since exited) — not to second-guess its own live cache.
    let live_cids: std::collections::HashSet<String> = match registry.cells.lock() {
        Ok(cells) => cells
            .values()
            .filter_map(|cell| cell.get().map(|e| e.cid.clone()))
            .collect(),
        Err(_) => std::collections::HashSet::new(),
    };

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for c in list {
        let (Some(id), Some(created)) = (c.id, c.created) else {
            continue;
        };
        if live_cids.contains(&id) {
            continue;
        }
        if is_stale_by_age(now_secs, created, SWEEP_MAX_AGE) {
            remove_ephemeral_container(docker, &id).await;
        }
    }
}

/// Pure staleness check for the tier-2 sweep, factored out for hermetic unit testing of the
/// age-threshold arithmetic (clock-skew fallback, saturating subtraction) without a live daemon.
fn is_stale_by_age(now_secs: i64, created_secs: i64, max_age: Duration) -> bool {
    now_secs.saturating_sub(created_secs) > max_age.as_secs() as i64
}

/// Pure idle check for the tier-1 reaper, factored out for hermetic unit testing.
fn is_idle(elapsed_since_access: Duration, ttl: Duration) -> bool {
    elapsed_since_access >= ttl
}

/// Force-remove a container, idempotently: an already-gone container (e.g. removed by a
/// concurrent reaper pass, or by the user) is not an error worth surfacing anywhere.
async fn remove_ephemeral_container(docker: &Docker, cid: &str) {
    let opts = bollard::query_parameters::RemoveContainerOptionsBuilder::new()
        .force(true)
        .build();
    let _ = docker.remove_container(cid, Some(opts)).await;
}

fn backend_err(e: impl std::fmt::Display) -> VfsError {
    VfsError::Backend {
        code: "docker".to_owned(),
        msg: e.to_string(),
        retryable: false,
    }
}

/// Build a `NotFound` error for the given container-internal path.  Falls back to the VFS root
/// if the path is somehow unparseable (which cannot happen for paths we control, but keeps the
/// error site infallible).
fn not_found(path: &str) -> VfsError {
    VfsError::NotFound(VfsPath::parse(path).unwrap_or_else(|_| VfsPath::root()))
}

/// Map a bollard error from an initiation call (`create_exec`/`start_exec`, or `create_container`
/// for an ephemeral image-browse container) to a [`VfsError`], carrying `context` (a container
/// name or an image id) for `NotFound`/`Forbidden`.
///
/// HTTP 404 → [`VfsError::NotFound`]; 401 → [`VfsError::Auth`]; 403 → [`VfsError::Forbidden`];
/// all other engine errors → [`VfsError::Backend`]. No credential material appears in any
/// error message; bollard's API-response messages contain only daemon-provided text.
fn map_status_error(e: bollard::errors::Error, context: &str) -> VfsError {
    let p = VfsPath::parse(context).unwrap_or_else(|_| VfsPath::root());
    match e {
        bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        } => VfsError::NotFound(p),
        bollard::errors::Error::DockerResponseServerError {
            status_code: 401, ..
        } => VfsError::Auth,
        bollard::errors::Error::DockerResponseServerError {
            status_code: 403, ..
        } => VfsError::Forbidden(p),
        other => backend_err(other),
    }
}

/// Map bollard's container-state enum to the cairn-types state.
fn map_state(s: Option<bollard::models::ContainerSummaryStateEnum>) -> ContainerState {
    use bollard::models::ContainerSummaryStateEnum as B;
    match s {
        Some(B::CREATED) => ContainerState::Created,
        Some(B::RUNNING) => ContainerState::Running,
        Some(B::PAUSED) => ContainerState::Paused,
        Some(B::RESTARTING) => ContainerState::Restarting,
        Some(B::EXITED) => ContainerState::Exited,
        Some(B::DEAD) => ContainerState::Dead,
        // REMOVING / STOPPING / EMPTY / None have no precise cairn equivalent.
        _ => ContainerState::Unknown,
    }
}

/// Return the tar-entry prefix Docker uses when archiving `path`.
///
/// The Docker archive API names entries relative to the *parent* of the requested path, using the
/// basename as the leading component — EXCEPT the root, whose entries Docker names with a leading
/// `/` (verified against a live daemon). Examples:
/// - `path = "/"` → entries: ``, `/.dockerenv`, `/bin`, `/bin/ls`, …  → prefix `""` (empty)
/// - `path = "/etc"` → entries: `etc/`, `etc/hostname`, …             → prefix `"etc/"`
/// - `path = "/etc/hostname"` → single entry `hostname`               → prefix `"hostname/"` (file)
///
/// Entry names are additionally normalized by [`strip_tar_prefix`] to absorb the Docker root `/`
/// and the Podman/older-Moby `./` variants before this prefix is stripped.
fn tar_prefix(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        // "/" trims to "" → rsplit_once returns None → root entries are direct children (no prefix).
        None => String::new(),
        // "/etc" → ("", "etc") or "/foo/bar" → ("/foo", "bar")
        Some((_, basename)) => format!("{basename}/"),
    }
}

/// Normalize a raw tar entry name to a parent-relative form: strip the Docker root `/` prefix and
/// the Podman/older-Moby `./` prefix, then strip the directory `prefix` for the listed path. Returns
/// `None` when the entry is not under `prefix` (e.g. the archive's self-entry).
fn strip_tar_prefix<'a>(raw: &'a str, prefix: &str) -> Option<&'a str> {
    let normalized = raw.trim_start_matches("./").trim_start_matches('/');
    normalized.strip_prefix(prefix)
}

#[async_trait]
impl ContainerOps for BollardDocker {
    async fn list_containers(&self) -> Result<Vec<ContainerInfo>, VfsError> {
        // `all: true` so stopped/exited containers are listed too (the Engine default is
        // running-only), keeping `list` and `stat` consistent across container states.
        let opts = bollard::query_parameters::ListContainersOptions {
            all: true,
            ..Default::default()
        };
        let list = self
            .docker
            .list_containers(Some(opts))
            .await
            .map_err(backend_err)?;
        Ok(list
            .into_iter()
            .map(|c| {
                let id = c.id.unwrap_or_default();
                // Fall back to the id when a container has no names, so the entry stays
                // navigable rather than collapsing to an empty path segment.
                let name = c
                    .names
                    .unwrap_or_default()
                    .first()
                    .map(|n| n.trim_start_matches('/').to_owned())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| id.clone());
                ContainerInfo {
                    id,
                    name,
                    image: c.image.unwrap_or_default(),
                    state: map_state(c.state),
                }
            })
            .collect())
    }

    /// List images, including each image's layer count.
    ///
    /// **N+1 note:** `ListImagesOptions` doesn't return `RootFS`, so the layer count is fetched
    /// with one `inspect_image` call per image after the initial list. This method is used only
    /// for the `/images` directory listing itself (paid once per render) — the image-browse hot
    /// path (`list_dir`/`stat`/`read` inside an image) resolves tag→id via the much cheaper
    /// [`Self::resolve_image_id`] instead, so the N+1 cost does **not** multiply per navigation
    /// step. Still acceptable to parallelize further with `join_all` as a follow-up if the modest
    /// image counts typical of a dev/ops workstation ever stop being modest.
    async fn list_images(&self) -> Result<Vec<ImageInfo>, VfsError> {
        let list = self
            .docker
            .list_images(None::<bollard::query_parameters::ListImagesOptions>)
            .await
            .map_err(backend_err)?;
        let mut out = Vec::with_capacity(list.len());
        for i in list {
            // Best-effort: an inspect failure (e.g. the image was removed mid-listing) just
            // yields an unknown (0) layer count rather than failing the whole listing.
            let layers = self
                .docker
                .inspect_image(&i.id)
                .await
                .ok()
                .and_then(|insp| insp.root_fs)
                .and_then(|rf| rf.layers)
                .map(|l| l.len() as u32)
                .unwrap_or(0);
            out.push(ImageInfo {
                id: i.id,
                tags: i.repo_tags,
                layers,
            });
        }
        Ok(out)
    }

    /// Cheap tag/id → canonical id resolution: a single `list_images` call over the wire
    /// (`GET /images/json`, no `inspect_image` follow-ups). Deliberately does not reuse
    /// `Self::list_images` above, which pays the N+1 `inspect_image` cost for `layers` — this is
    /// the method the image-browse hot path calls on every `list_dir`/`stat`/`read`.
    async fn resolve_image_id(&self, tag: &str) -> Result<String, VfsError> {
        let list = self
            .docker
            .list_images(None::<bollard::query_parameters::ListImagesOptions>)
            .await
            .map_err(backend_err)?;
        list.into_iter()
            .find(|i| i.repo_tags.iter().any(|t| t == tag) || i.id == tag)
            .map(|i| i.id)
            .ok_or_else(|| not_found(tag))
    }

    async fn ephemeral_for_image(&self, image_id: &str) -> Result<String, VfsError> {
        self.ephemeral_for_image_impl(image_id).await
    }

    /// List the immediate children of `path` inside the container's filesystem.
    ///
    /// Uses `GET /containers/{container}/archive?path={path}`. The Docker daemon returns a tar
    /// whose entry names are rooted at the basename of the requested path (e.g. requesting
    /// `/etc` yields entries `etc/`, `etc/hostname`, `etc/subdir/file`). This method strips that
    /// prefix, deduplicates intermediate directory names, and returns only the immediate children.
    ///
    /// **M6 memory note:** The archive endpoint is recursive — the full subtree (all descendant
    /// files and their bytes) is streamed and buffered in memory just to enumerate immediate
    /// children. This is acceptable for M6 but will OOM on large containers. A follow-up should
    /// use `HEAD /containers/{id}/archive` (archive info endpoint) or a streaming tar walk that
    /// stops at depth > 1 without buffering file contents.
    async fn list_dir(&self, container: &str, path: &str) -> Result<Vec<RemoteEntry>, VfsError> {
        let buf = self.fetch_archive(container, path).await?;
        let prefix = tar_prefix(path);

        let mut archive = tar::Archive::new(buf.as_slice());
        let mut seen_dirs = BTreeSet::<String>::new();
        let mut files: Vec<RemoteEntry> = Vec::new();

        for entry_result in archive.entries().map_err(backend_err)? {
            let entry = entry_result.map_err(backend_err)?;
            let entry_path = entry.path().map_err(backend_err)?;
            let entry_str_raw = entry_path.to_string_lossy();

            // Normalize away the Docker root `/` and Podman/older-Moby `./` prefixes, then strip the
            // directory prefix to get the path relative to the requested directory.
            let relative = match strip_tar_prefix(&entry_str_raw, &prefix) {
                Some(r) => r,
                None => continue, // entry outside our subtree (e.g. the archive self-entry)
            };

            // Empty relative path is the self/root entry — skip it.
            if relative.is_empty() || relative == "." {
                continue;
            }

            // Remove trailing slash so we can analyse the components uniformly.
            let name_part = relative.trim_end_matches('/');

            if let Some(slash_pos) = name_part.find('/') {
                // This entry is a deeper descendant — the first component is a directory.
                let dir_name = &name_part[..slash_pos];
                if !dir_name.is_empty() {
                    seen_dirs.insert(dir_name.to_owned());
                }
            } else if entry.header().entry_type().is_dir() {
                // Immediate directory child (e.g. `etc/subdir/` stripped to `subdir`).
                seen_dirs.insert(name_part.to_owned());
            } else if entry.header().entry_type().is_symlink()
                || entry.header().entry_type().is_hard_link()
            {
                // Symlinks have no own size; hardlink entries in a recursive archive carry header
                // size 0 (only the first occurrence of the inode is a regular file), so reporting
                // their `size()` would be a misleading 0. Surface unknown size instead.
                // TODO(symlinks): present as EntryKind::Symlink once that is added to the VFS surface.
                files.push(RemoteEntry {
                    name: name_part.to_owned(),
                    kind: EntryKind::File,
                    size: None,
                });
            } else {
                // Immediate file child (regular file, hard link, char/block device, FIFO, etc.).
                let size = entry.header().size().map_err(backend_err)?;
                files.push(RemoteEntry {
                    name: name_part.to_owned(),
                    kind: EntryKind::File,
                    size: Some(size),
                });
            }
        }

        // Dirs first (sorted), then files — consistent ordering the UI can rely on.
        let mut entries: Vec<RemoteEntry> = seen_dirs
            .into_iter()
            .map(|name| RemoteEntry {
                name,
                kind: EntryKind::Dir,
                size: None,
            })
            .collect();
        entries.extend(files);
        Ok(entries)
    }

    /// Stat `path` inside the container by examining the first tar entry returned by the archive
    /// endpoint. A 404 from the daemon maps to [`VfsError::NotFound`].
    async fn stat(&self, container: &str, path: &str) -> Result<RemoteMeta, VfsError> {
        let buf = self.fetch_archive(container, path).await?;
        let mut archive = tar::Archive::new(buf.as_slice());
        let entry = archive
            .entries()
            .map_err(backend_err)?
            .next()
            .ok_or_else(|| not_found(path))?
            .map_err(backend_err)?;
        let header = entry.header();
        if header.entry_type().is_dir() {
            Ok(RemoteMeta {
                kind: EntryKind::Dir,
                size: None,
            })
        } else {
            // Symlinks: report as File for now; see TODO in list_dir.
            Ok(RemoteMeta {
                kind: EntryKind::File,
                size: Some(header.size().map_err(backend_err)?),
            })
        }
    }

    /// Read the full contents of `path` inside the container.
    ///
    /// Downloads the path via the archive endpoint (a tar with a single entry) and reads that
    /// entry's bytes. A 404 from the daemon maps to [`VfsError::NotFound`]. Reading a directory
    /// path is rejected with [`VfsError::Unsupported`].
    async fn read(&self, container: &str, path: &str) -> Result<Vec<u8>, VfsError> {
        let buf = self.fetch_archive(container, path).await?;
        let mut archive = tar::Archive::new(buf.as_slice());
        let mut entry = archive
            .entries()
            .map_err(backend_err)?
            .next()
            .ok_or_else(|| not_found(path))?
            .map_err(backend_err)?;
        if entry.header().entry_type().is_dir() {
            return Err(VfsError::Unsupported(Caps::READ));
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(backend_err)?;
        Ok(data)
    }

    /// Stream log output from a container via the Docker `GET /containers/{id}/logs` endpoint.
    ///
    /// Bollard's `LogOutput` decoder already demultiplexes Docker's 8-byte per-frame stream
    /// header (present when no TTY is allocated), so callers receive plain payload [`Bytes`] —
    /// stdout and stderr interleaved in arrival order — without hand-parsing the wire format.
    ///
    /// Bollard's `Docker::logs()` return type is lifetime-tied to `&self` (Rust's `impl Trait`
    /// elision), even though the implementation clones the internal `Arc<Transport>`. To produce
    /// a `BoxStream<'static, …>` we spawn a Tokio task that owns both the `Docker` clone and the
    /// bollard stream, forwarding frames over a bounded `mpsc` channel. The receiver side is
    /// wrapped in `stream::unfold` and boxed. The task exits when either the bollard stream ends
    /// (non-follow mode or daemon EOF) or the receiver is dropped (caller cancelled).
    ///
    /// Error mapping: 404 → [`VfsError::NotFound`]; any other bollard error →
    /// [`VfsError::Backend`]. Errors appear as `Err` items in the stream, not panics.
    fn logs(
        &self,
        container: &str,
        follow: bool,
        tail: &str,
    ) -> BoxStream<'static, Result<Bytes, VfsError>> {
        let docker = self.docker.clone();
        let container = container.to_owned();
        let tail_owned = tail.to_owned();

        // 64-frame buffer: ample back-pressure at typical log line sizes without retaining much
        // memory. Adjust upward if the consumer is materially slower than the daemon.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, VfsError>>(64);

        tokio::spawn(async move {
            let opts = bollard::query_parameters::LogsOptionsBuilder::default()
                .follow(follow)
                .stdout(true)
                .stderr(true)
                .tail(&tail_owned)
                .build();
            let mut stream = docker.logs(&container, Some(opts));
            while let Some(item) = stream.next().await {
                let mapped = match item {
                    Ok(log) => Ok(log.into_bytes()),
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => Err(not_found(&container)),
                    Err(e) => Err(backend_err(e)),
                };
                // If the receiver was dropped (caller cancelled), stop producing.
                if tx.send(mapped).await.is_err() {
                    break;
                }
            }
            // Task ends here; dropping `tx` closes the channel, ending the consumer stream.
        });

        futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed()
    }

    /// Open an interactive exec session in a running container via the Docker Engine API.
    ///
    /// # API flow (bollard 0.21)
    ///
    /// 1. `POST /containers/{container}/exec` ([`Docker::create_exec`]) — creates the exec
    ///    instance and returns an `exec_id`.
    /// 2. `POST /exec/{id}/start` ([`Docker::start_exec`]) with no detach options — upgrades
    ///    the connection and returns `StartExecResults::Attached { output, input }`, where
    ///    `output` is a `Stream<Item = Result<LogOutput, Error>>` and `input` is an
    ///    `AsyncWrite` handle for stdin.
    ///
    /// # Task model
    ///
    /// A single Tokio task is spawned that owns `output` and `input`. It runs a `select!` loop:
    ///
    /// - **stdout relay**: on each `output.next()` item, forwards the payload bytes (via
    ///   `LogOutput::into_bytes`) to the `stdout` mpsc sender. `StdOut`, `StdErr`, `StdIn`,
    ///   and `Console` variants are all forwarded without discrimination — Docker merges stderr
    ///   into stdout when `tty: true` (producing `Console` frames).
    /// - **cancel arm**: signals stdout EOF (`drop(stdout_tx)`), then breaks with `Ok(-1)`.
    /// - **stream end** (`output.next()` returns `None`): signals stdout EOF, then calls
    ///   `GET /exec/{id}/json` ([`Docker::inspect_exec`]) to retrieve the exit code. The daemon
    ///   can race (`exit_code: null` briefly after the stream closes); this method polls with
    ///   bounded back-off (up to 5 attempts, 50 ms doubling) before falling back to `Ok(0)`.
    ///   On a transport error from `inspect_exec`: resolves `done` with `Err(backend_err(…))`.
    ///
    /// `stdout_tx` is always dropped **before** `done_tx.send(…)` on all exit paths so that
    /// consumers that drain stdout before awaiting `done` always see stdout close first.
    ///
    /// The **stdin relay** sub-task drains the `stdin` mpsc receiver and writes each chunk to
    /// `input` via `AsyncWriteExt::write_all`. After the loop exits (sender dropped = EOF, or
    /// write error), it calls `AsyncWriteExt::shutdown` explicitly on the writer. Bollard's
    /// `input` is the `WriteHalf` of a split `AsyncUpgraded` connection: dropping `WriteHalf`
    /// does **not** send a TCP half-close (the `ReadHalf` keeps the connection alive), so the
    /// remote process would never see stdin EOF without an explicit `shutdown().await`.
    ///
    /// The **resize relay** sub-task (TTY only) reads `(rows, cols)` from the resize channel
    /// and calls `POST /exec/{id}/resize` ([`Docker::resize_exec`]) as a best-effort
    /// fire-and-forget. Errors are silently discarded (the session may have already ended).
    ///
    /// # Process lifecycle on cancel
    ///
    /// Docker has **no kill-exec API**. Cancelling (sending on `cancel`, or dropping it) detaches
    /// Cairn's relay task from the exec stream but the exec'd process continues running orphaned
    /// inside the container. This is a Docker Engine limitation. `done` resolves with `Ok(-1)` to
    /// signal the cancel, but the process is not guaranteed to be gone. Future enhancement: on TTY
    /// execs, attempt a best-effort Ctrl-C sequence before cancelling — tracked as a follow-up.
    ///
    /// # Credential safety
    ///
    /// Container IDs and exec IDs are never included in `VfsError` messages. Bollard's
    /// API-response messages contain only daemon-provided text.
    async fn exec(
        &self,
        container: &str,
        argv: Vec<String>,
        tty: bool,
    ) -> Result<SessionHandle, VfsError> {
        use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecResults};
        use tokio::io::AsyncWriteExt as _;

        if argv.is_empty() {
            return Err(VfsError::Backend {
                code: "empty_argv".to_owned(),
                msg: "exec: argv must be non-empty".to_owned(),
                retryable: false,
            });
        }

        let docker = self.docker.clone();
        let container = container.to_owned();

        // Step 1: create the exec instance.
        let exec_result = docker
            .create_exec(
                &container,
                CreateExecOptions {
                    cmd: Some(argv),
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    // When tty=true Docker merges stderr into the stdout Console stream.
                    // Attaching stderr separately would deliver it twice: once in the Console
                    // stream, once as a StdErr frame. For non-TTY execs, attach stderr so it
                    // is interleaved in the output stream alongside stdout.
                    attach_stderr: Some(!tty),
                    tty: Some(tty),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| map_status_error(e, &container))?;

        let exec_id = exec_result.id;

        // Step 2: start the exec and obtain attached I/O streams.
        let (output, input) = match docker
            .start_exec(&exec_id, None)
            .await
            .map_err(|e| map_status_error(e, &container))?
        {
            StartExecResults::Attached { output, input } => (output, input),
            // We never set detach=true, so Detached is a daemon protocol violation rather
            // than a panic-worthy unreachable — surface it as a typed error.
            StartExecResults::Detached => {
                return Err(VfsError::Backend {
                    code: "exec-io".to_owned(),
                    msg: "exec: daemon returned Detached unexpectedly".to_owned(),
                    retryable: false,
                });
            }
        };

        // Session channels.
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Bytes>(32);
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::channel::<Bytes>(64);
        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Result<i32, VfsError>>();

        // Resize channel: present iff tty=true.
        let (resize_tx, resize_rx) = if tty {
            let (t, r) = tokio::sync::mpsc::channel::<(u16, u16)>(8);
            (Some(t), Some(r))
        } else {
            (None, None)
        };

        tokio::spawn(async move {
            // --- stdin relay ---
            // A sub-task drains the stdin mpsc receiver and writes each chunk to the exec's
            // AsyncWrite input handle. After the loop exits (stdin sender dropped → recv returns
            // None, or write error), it calls `shutdown()` explicitly. Bollard's `input` writer
            // is the WriteHalf of a split connection: dropping WriteHalf does NOT half-close the
            // underlying TCP connection (ReadHalf keeps it alive), so the remote process would
            // never see stdin EOF without an explicit shutdown().
            let stdin_task = {
                let mut writer = input;
                tokio::spawn(async move {
                    // Drain stdin chunks until the sender is dropped or a write fails.
                    while let Some(chunk) = stdin_rx.recv().await {
                        if writer.write_all(&chunk).await.is_err() {
                            break;
                        }
                    }
                    // Explicit half-close: signals stdin EOF to the remote process.
                    // Bollard's `input` writer is the WriteHalf of a split connection;
                    // dropping WriteHalf does NOT half-close the TCP connection (the
                    // ReadHalf keeps it alive), so shutdown() is mandatory here.
                    let _ = writer.shutdown().await;
                })
            };

            // --- resize relay (TTY only) ---
            // A sub-task reads (rows, cols) and calls resize_exec as best-effort fire-and-forget.
            let resize_task = if let Some(mut rr) = resize_rx {
                let docker_r = docker.clone();
                let exec_id_r = exec_id.clone();
                Some(tokio::spawn(async move {
                    while let Some((rows, cols)) = rr.recv().await {
                        // Errors are silently ignored — the session may have already ended.
                        let _ = docker_r
                            .resize_exec(
                                &exec_id_r,
                                ResizeExecOptions {
                                    height: rows,
                                    width: cols,
                                },
                            )
                            .await;
                    }
                }))
            } else {
                None
            };

            // --- stdout relay + main exit-code logic ---
            // Process the output stream inline, selecting against cancel on every item.
            // All LogOutput variants (StdOut, StdErr, StdIn, Console) carry user-visible bytes;
            // forward them all without discrimination (Docker merges streams on tty=true).
            //
            // INVARIANT: `stdout_tx` is dropped BEFORE `done_tx.send(…)` on ALL exit paths so
            // that consumers see stdout EOF before `done` resolves.
            let mut output = output;
            let exit_result: Result<i32, VfsError> = loop {
                tokio::select! {
                    biased;
                    // Cancel path: signal stdout EOF, then break with sentinel -1.
                    // The exec'd process continues running orphaned in the container (Docker has
                    // no kill-exec API); this is documented in the method-level rustdoc.
                    _ = &mut cancel_rx => {
                        drop(stdout_tx);
                        break Ok(-1);
                    }
                    item = output.next() => {
                        match item {
                            Some(Ok(log)) => {
                                // Best-effort: continue draining even if the consumer dropped
                                // stdout_rx (send error is silently ignored).
                                let _ = stdout_tx.send(log.into_bytes()).await;
                            }
                            Some(Err(e)) => {
                                // Transport error while reading output: signal stdout EOF before
                                // resolving done so consumers see the stream close first.
                                drop(stdout_tx);
                                break Err(backend_err(e));
                            }
                            None => {
                                // Output stream closed → the remote process has exited.
                                // Signal stdout EOF before inspect_exec so the consumer can drain
                                // buffered output while we poll for the exit code.
                                drop(stdout_tx);
                                // Docker races: immediately after the stream closes, inspect_exec
                                // may return exit_code: null (running still true). Poll with a
                                // bounded back-off. Cap iterations so done always resolves.
                                const MAX_POLLS: u8 = 5;
                                let mut inspect_result: Result<i32, VfsError> = Ok(0);
                                for attempt in 0..MAX_POLLS {
                                    match docker.inspect_exec(&exec_id).await {
                                        Ok(info) if info.exit_code.is_some() => {
                                            // Safety: checked is_some() above.
                                            inspect_result =
                                                Ok(info.exit_code.unwrap() as i32);
                                            break;
                                        }
                                        Ok(_) => {
                                            // exit_code still null — daemon is transitioning.
                                            // Back off before retrying (50 ms, 100 ms, …).
                                            if attempt < MAX_POLLS - 1 {
                                                tokio::time::sleep(
                                                    std::time::Duration::from_millis(
                                                        50u64 << attempt,
                                                    ),
                                                )
                                                .await;
                                            }
                                            // else: last attempt, fall through to Ok(0)
                                        }
                                        Err(e) => {
                                            inspect_result = Err(backend_err(e));
                                            break;
                                        }
                                    }
                                }
                                break inspect_result;
                            }
                        }
                    }
                }
            };

            // Tear down relay sub-tasks.
            stdin_task.abort();
            if let Some(t) = resize_task {
                t.abort();
            }

            // Resolve done. stdout_tx was already dropped above on all paths.
            // If the receiver was dropped (session pane torn down), discard the send error.
            let _ = done_tx.send(exit_result);
        });

        Ok(SessionHandle::new(
            cancel_tx,
            done_rx,
            None, // local_port: exec never binds a port
            Some(stdin_tx),
            Some(stdout_rx),
            resize_tx,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{is_idle, is_stale_by_age, strip_tar_prefix, tar_prefix};
    use std::time::Duration;

    #[test]
    fn is_idle_thresholds_correctly() {
        let ttl = Duration::from_secs(300);
        assert!(!is_idle(Duration::from_secs(299), ttl));
        // >= threshold counts as idle (matches the reaper's intent: 5 min exactly is reapable).
        assert!(is_idle(Duration::from_secs(300), ttl));
        assert!(is_idle(Duration::from_secs(301), ttl));
    }

    #[test]
    fn is_stale_by_age_thresholds_correctly() {
        let max_age = Duration::from_secs(1800);
        let now = 10_000_i64;
        // Exactly at the threshold is NOT stale (strictly greater-than, mirroring the sweep's
        // "older than" wording) — only crossing it triggers removal.
        assert!(!is_stale_by_age(now, now - 1800, max_age));
        assert!(is_stale_by_age(now, now - 1801, max_age));
    }

    #[test]
    fn is_stale_by_age_never_panics_on_clock_skew() {
        // `created` in the future (clock skew / daemon vs. local clock drift) must saturate to
        // "not stale", never underflow/panic via the subtraction.
        let max_age = Duration::from_secs(1800);
        assert!(!is_stale_by_age(1_000, 5_000, max_age));
    }

    #[test]
    fn tar_prefix_root_is_empty() {
        // Docker names root archive entries with a leading `/` (not `./`), so the root prefix must
        // be empty — a non-empty prefix here silently dropped every root child (the M6-2 root bug).
        assert_eq!(tar_prefix("/"), "");
        assert_eq!(tar_prefix(""), "");
    }

    #[test]
    fn tar_prefix_nested_uses_basename() {
        assert_eq!(tar_prefix("/etc"), "etc/");
        assert_eq!(tar_prefix("/etc/ssl"), "ssl/");
        assert_eq!(tar_prefix("/a/b/c"), "c/");
    }

    #[test]
    fn strip_root_entries_docker_style() {
        // Real Docker root archive: ``, `/.dockerenv`, `/bin`, `/bin/ls`.
        assert_eq!(strip_tar_prefix("/.dockerenv", ""), Some(".dockerenv"));
        assert_eq!(strip_tar_prefix("/bin", ""), Some("bin"));
        assert_eq!(strip_tar_prefix("/bin/ls", ""), Some("bin/ls"));
        // The archive self-entry (root) collapses to empty and is skipped by the caller.
        assert_eq!(strip_tar_prefix("/", ""), Some(""));
        assert_eq!(strip_tar_prefix("", ""), Some(""));
    }

    #[test]
    fn strip_nested_entries_docker_and_podman() {
        // Docker basename-rooted, and the Podman/older-Moby `./` variant.
        assert_eq!(strip_tar_prefix("etc/hostname", "etc/"), Some("hostname"));
        assert_eq!(strip_tar_prefix("./etc/hostname", "etc/"), Some("hostname"));
        assert_eq!(strip_tar_prefix("etc/", "etc/"), Some(""));
        // An entry outside the requested subtree is rejected.
        assert_eq!(strip_tar_prefix("var/log", "etc/"), None);
    }
}
