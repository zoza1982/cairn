//! The real Docker engine adapter over `bollard`.
//!
//! Container and image **listing** are implemented against the Docker Engine API. Container
//! filesystem browsing (`list_dir`/`stat`/`read`) is implemented over the Docker archive endpoint
//! (`GET /containers/{id}/archive?path=…`), which returns a tar stream. The tar is collected into
//! memory and then parsed synchronously with the `tar` crate — an adequate approach for M6
//! (streaming-extract and zero-copy are follow-ups). The full path-routing/mapping is verified via
//! the mock; the live adapter is validated against a real daemon in the dind integration job.

use crate::ops::{ContainerInfo, ContainerOps, ImageInfo, RemoteEntry, RemoteMeta};
use async_trait::async_trait;
use bollard::Docker;
use bytes::Bytes;
use cairn_types::{Caps, ContainerState, EntryKind, VfsPath};
use cairn_vfs::VfsError;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::BTreeSet;
use std::io::Read;

/// A [`ContainerOps`] implementation backed by a live Docker engine via `bollard`.
pub struct BollardDocker {
    docker: Docker,
}

impl BollardDocker {
    /// Connect using the platform's default Docker endpoint (socket / named pipe / env).
    ///
    /// # Errors
    /// [`VfsError::Connection`] if the engine cannot be reached.
    pub fn connect_local() -> Result<Self, VfsError> {
        Docker::connect_with_local_defaults()
            .map(|docker| Self { docker })
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

    async fn list_images(&self) -> Result<Vec<ImageInfo>, VfsError> {
        let list = self
            .docker
            .list_images(None::<bollard::query_parameters::ListImagesOptions>)
            .await
            .map_err(backend_err)?;
        Ok(list
            .into_iter()
            .map(|i| ImageInfo {
                id: i.id,
                tags: i.repo_tags,
            })
            .collect())
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
    /// Bollard's [`LogOutput`] decoder already demultiplexes Docker's 8-byte per-frame stream
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
}

#[cfg(test)]
mod tests {
    use super::{strip_tar_prefix, tar_prefix};

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
