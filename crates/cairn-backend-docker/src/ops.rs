//! The [`ContainerOps`] transport seam for the Docker backend, plus an in-memory mock.

use async_trait::async_trait;
use bytes::Bytes;
use cairn_types::{ContainerState, EntryKind};
use cairn_vfs::{SessionHandle, VfsError};
use futures::stream::BoxStream;

/// Summary of a container.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    /// Container id.
    pub id: String,
    /// Primary name.
    pub name: String,
    /// Image reference.
    pub image: String,
    /// Runtime state.
    pub state: ContainerState,
}

/// Summary of an image.
#[derive(Debug, Clone)]
pub struct ImageInfo {
    /// Image id.
    pub id: String,
    /// Tags pointing at the image.
    pub tags: Vec<String>,
}

/// One entry inside a container's filesystem.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    /// Leaf name.
    pub name: String,
    /// Kind.
    pub kind: EntryKind,
    /// Size (files).
    pub size: Option<u64>,
}

/// Metadata for a path inside a container.
#[derive(Debug, Clone)]
pub struct RemoteMeta {
    /// Kind.
    pub kind: EntryKind,
    /// Size (files).
    pub size: Option<u64>,
}

/// The Docker engine surface the backend needs. Implemented by the bollard adapter and the mock.
#[async_trait]
pub trait ContainerOps: Send + Sync + 'static {
    /// List containers.
    async fn list_containers(&self) -> Result<Vec<ContainerInfo>, VfsError>;
    /// List images.
    async fn list_images(&self) -> Result<Vec<ImageInfo>, VfsError>;
    /// List a directory inside a container's filesystem.
    async fn list_dir(&self, container: &str, path: &str) -> Result<Vec<RemoteEntry>, VfsError>;
    /// Stat a path inside a container.
    async fn stat(&self, container: &str, path: &str) -> Result<RemoteMeta, VfsError>;
    /// Read a file inside a container.
    async fn read(&self, container: &str, path: &str) -> Result<Vec<u8>, VfsError>;
    /// Stream log output from a container.
    ///
    /// Each item in the returned stream carries one log frame as raw [`Bytes`]. Bollard's
    /// `LogOutput` type already demultiplexes Docker's 8-byte multiplexed stream header (used
    /// when no TTY is allocated), so callers receive plain payload bytes — stdout and stderr
    /// interleaved in arrival order — not raw wire frames.
    ///
    /// `follow` controls whether the stream tails live output (`true`) or returns only buffered
    /// history and then ends (`false`). `tail` is passed verbatim as the Docker `tail` query
    /// parameter (`"all"` for all history, a decimal number for the last N lines).
    ///
    /// Error mapping: a 404 from the daemon (container not found) surfaces as
    /// [`VfsError::NotFound`] in the stream; any other engine error becomes [`VfsError::Backend`].
    /// The stream never panics; it may be empty for containers with no log output.
    fn logs(
        &self,
        container: &str,
        follow: bool,
        tail: &str,
    ) -> BoxStream<'static, Result<Bytes, VfsError>>;

    /// Open an interactive exec session in a running container.
    ///
    /// Returns a [`SessionHandle`] immediately; the remote process starts concurrently in a
    /// spawned task. The caller drives the session via the handle's channels:
    ///
    /// - Write to `stdin` to send bytes to the process's stdin.
    /// - Read from `stdout` to receive combined stdout (and, when `tty: false`, stderr) output.
    /// - Send `(rows, cols)` to `resize` (present only when `tty: true`) to propagate terminal
    ///   resize events to the running process via `POST /exec/{id}/resize`.
    /// - Drop or send on `cancel` to request teardown; `done` resolves with the exit code.
    ///
    /// # Parameters
    ///
    /// - `container`: container name or ID. The container must be running; a stopped or missing
    ///   container returns [`VfsError::NotFound`] or [`VfsError::Backend`] from the daemon.
    /// - `argv`: argument vector passed directly to the container runtime (not a shell; use
    ///   `["sh", "-c", "…"]` for shell commands). Must be non-empty.
    /// - `tty`: allocate a pseudo-TTY. When `true`: stderr is merged into stdout (Docker
    ///   convention), the `resize` channel is populated, and the process sees a PTY. When `false`:
    ///   stderr is interleaved into `stdout`; `resize` is `None`.
    ///
    /// # Exit code and `done`
    ///
    /// When the output stream closes naturally (the remote process exits), the backend calls
    /// `GET /exec/{id}/json` (`inspect_exec`) to retrieve the numeric exit code and resolves
    /// `done` with `Ok(exit_code)`. On cancel before exit: `Ok(-1)`. On transport error: `Err`.
    /// Credential material is never included in error messages.
    ///
    /// # Error mapping
    ///
    /// 404 → [`VfsError::NotFound`]; 401 → [`VfsError::Auth`]; 403 → [`VfsError::Forbidden`];
    /// other engine errors → [`VfsError::Backend`].
    async fn exec(
        &self,
        container: &str,
        argv: Vec<String>,
        tty: bool,
    ) -> Result<SessionHandle, VfsError>;
}

#[cfg(test)]
pub(crate) mod mock {
    use super::{ContainerInfo, ContainerOps, ImageInfo, RemoteEntry, RemoteMeta};
    use async_trait::async_trait;
    use bytes::Bytes;
    use cairn_types::{ContainerState, EntryKind, VfsPath};
    use cairn_vfs::{SessionHandle, VfsError};
    use futures::stream::{self, BoxStream};
    use futures::StreamExt as _;
    use std::collections::BTreeMap;

    /// In-memory Docker engine for tests: a few containers, each with a file tree, and images.
    pub(crate) struct MockDocker {
        containers: Vec<ContainerInfo>,
        images: Vec<ImageInfo>,
        /// container name -> (path -> file bytes; directories are implied by path prefixes).
        files: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    }

    impl MockDocker {
        pub(crate) fn new() -> Self {
            let mut files = BTreeMap::new();
            let mut web: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            web.insert("/etc/hostname".to_owned(), b"web-1\n".to_vec());
            web.insert("/etc/hosts".to_owned(), b"127.0.0.1 localhost\n".to_vec());
            web.insert("/app/main.rs".to_owned(), b"fn main() {}\n".to_vec());
            files.insert("web".to_owned(), web);
            Self {
                containers: vec![ContainerInfo {
                    id: "abc123".to_owned(),
                    name: "web".to_owned(),
                    image: "nginx:latest".to_owned(),
                    state: ContainerState::Running,
                }],
                images: vec![ImageInfo {
                    id: "img1".to_owned(),
                    tags: vec!["nginx:latest".to_owned()],
                }],
                files,
            }
        }

        fn nf(path: &str) -> VfsError {
            VfsError::NotFound(VfsPath::parse(path).unwrap_or_else(|_| VfsPath::root()))
        }
    }

    #[async_trait]
    impl ContainerOps for MockDocker {
        async fn list_containers(&self) -> Result<Vec<ContainerInfo>, VfsError> {
            Ok(self.containers.clone())
        }

        async fn list_images(&self) -> Result<Vec<ImageInfo>, VfsError> {
            Ok(self.images.clone())
        }

        async fn list_dir(
            &self,
            container: &str,
            path: &str,
        ) -> Result<Vec<RemoteEntry>, VfsError> {
            let tree = self
                .files
                .get(container)
                .ok_or_else(|| Self::nf(container))?;
            // A path that is itself a file is not a directory.
            if tree.contains_key(path) {
                return Err(Self::nf(path));
            }
            let prefix = if path == "/" {
                "/".to_owned()
            } else {
                format!("{path}/")
            };
            // A non-root directory must have at least one child to exist.
            if path != "/" && !tree.keys().any(|k| k.starts_with(&prefix)) {
                return Err(Self::nf(path));
            }
            let mut dirs = std::collections::BTreeSet::new();
            let mut files = Vec::new();
            for (key, data) in tree {
                let Some(rest) = key.strip_prefix(&prefix) else {
                    continue;
                };
                match rest.split_once('/') {
                    Some((dir, _)) => {
                        dirs.insert(dir.to_owned());
                    }
                    None => files.push(RemoteEntry {
                        name: rest.to_owned(),
                        kind: EntryKind::File,
                        size: Some(data.len() as u64),
                    }),
                }
            }
            let mut out: Vec<RemoteEntry> = dirs
                .into_iter()
                .map(|name| RemoteEntry {
                    name,
                    kind: EntryKind::Dir,
                    size: None,
                })
                .collect();
            out.extend(files);
            Ok(out)
        }

        async fn stat(&self, container: &str, path: &str) -> Result<RemoteMeta, VfsError> {
            let tree = self
                .files
                .get(container)
                .ok_or_else(|| Self::nf(container))?;
            // The container root is always a directory (even when empty).
            if path == "/" {
                return Ok(RemoteMeta {
                    kind: EntryKind::Dir,
                    size: None,
                });
            }
            if let Some(data) = tree.get(path) {
                return Ok(RemoteMeta {
                    kind: EntryKind::File,
                    size: Some(data.len() as u64),
                });
            }
            // A directory if any key is under it.
            let prefix = format!("{path}/");
            if tree.keys().any(|k| k.starts_with(&prefix)) {
                Ok(RemoteMeta {
                    kind: EntryKind::Dir,
                    size: None,
                })
            } else {
                Err(Self::nf(path))
            }
        }

        async fn read(&self, container: &str, path: &str) -> Result<Vec<u8>, VfsError> {
            let tree = self
                .files
                .get(container)
                .ok_or_else(|| Self::nf(container))?;
            tree.get(path).cloned().ok_or_else(|| Self::nf(path))
        }

        fn logs(
            &self,
            container: &str,
            _follow: bool,
            _tail: &str,
        ) -> BoxStream<'static, Result<Bytes, VfsError>> {
            // Return a canned two-line log for the known "web" container; error for unknown ones.
            if self.containers.iter().any(|c| c.name == container) {
                let lines: Vec<Result<Bytes, VfsError>> = vec![
                    Ok(Bytes::from_static(b"[mock] log line 1\n")),
                    Ok(Bytes::from_static(b"[mock] log line 2\n")),
                ];
                stream::iter(lines).boxed()
            } else {
                let err = VfsError::NotFound(
                    VfsPath::parse(container).unwrap_or_else(|_| VfsPath::root()),
                );
                stream::iter(vec![Err(err)]).boxed()
            }
        }

        /// Echo-style exec mock: relays each stdin chunk back to stdout, then exits with code 0.
        ///
        /// Matches the shape of [`super::super::real::BollardDocker::exec`]:
        /// - An unknown container returns [`VfsError::NotFound`].
        /// - When `tty: true`, the `resize` channel is present and accepted (events are discarded).
        /// - The `cancel` signal (drop or explicit send) is honoured cooperatively: the relay
        ///   loop selects between `cancel` and `stdin.recv()`. On cancel, `done` resolves with
        ///   `Ok(-1)`. On stdin-close (sender dropped), `done` resolves with `Ok(0)`.
        async fn exec(
            &self,
            container: &str,
            _argv: Vec<String>,
            tty: bool,
        ) -> Result<SessionHandle, VfsError> {
            if !self.containers.iter().any(|c| c.name == container) {
                return Err(VfsError::NotFound(
                    VfsPath::parse(container).unwrap_or_else(|_| VfsPath::root()),
                ));
            }

            let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Bytes>(16);
            let (stdout_tx, stdout_rx) = tokio::sync::mpsc::channel::<Bytes>(16);
            let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
            let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Result<i32, VfsError>>();

            // TTY-only resize channel — accepted and drained; the mock ignores resize geometry.
            let (resize_tx, resize_rx) = if tty {
                let (t, r) = tokio::sync::mpsc::channel::<(u16, u16)>(4);
                (Some(t), Some(r))
            } else {
                (None, None)
            };

            tokio::spawn(async move {
                // Drain the resize channel in a background task so backpressure never stalls
                // the main echo loop (the TTY resize sender would block otherwise).
                if let Some(mut rr) = resize_rx {
                    tokio::spawn(async move { while rr.recv().await.is_some() {} });
                }

                // Echo loop: relay stdin → stdout until stdin closes or cancel fires.
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut cancel_rx => {
                            let _ = done_tx.send(Ok(-1));
                            return;
                        }
                        chunk = stdin_rx.recv() => {
                            match chunk {
                                Some(c) => {
                                    // Best-effort echo; stop if the stdout receiver was dropped.
                                    if stdout_tx.send(c).await.is_err() {
                                        break;
                                    }
                                }
                                None => break, // stdin sender dropped → EOF
                            }
                        }
                    }
                }

                // Exit code 0 regardless of how the loop ended.
                let _ = done_tx.send(Ok(0));
            });

            Ok(SessionHandle::new(
                cancel_tx,
                done_rx,
                None, // local_port: exec sessions never bind a port
                Some(stdin_tx),
                Some(stdout_rx),
                resize_tx,
            ))
        }
    }
}
