//! Docker/OCI container backend.
//!
//! Presents containers and images as a navigable tree: `/containers/<name>/…` browses a container's
//! filesystem and `/images/<tag>` lists images. The path-routing and entry-mapping logic lives in
//! [`DockerVfs`] over a [`ContainerOps`] seam and is fully unit-tested against an in-memory mock; the
//! real engine access is the `BollardDocker` adapter, compiled only under the `docker` feature
//! (off by default — it pulls bollard's hyper stack). See `docs/LLD.md` §3.6, RFC-0004, ADR-0006.

mod ops;
#[cfg(feature = "docker")]
mod real;

pub use ops::{ContainerInfo, ContainerOps, ImageInfo, RemoteEntry, RemoteMeta};
#[cfg(feature = "docker")]
pub use real::BollardDocker;

use async_trait::async_trait;
use cairn_types::{Caps, ConnectionId, Entry, EntryExt, EntryKind, Scheme, VfsPath};
use cairn_vfs::{
    action_ids, apply_byte_range, join_abs_path, ActionCtx, ActionDescriptor, ActionId, ActionKind,
    ActionOutcome, ByteRange, CapabilityProvider, ListOpts, ListPage, ReadHandle, Recurse, Vfs,
    VfsError, WriteHandle, WriteOpts,
};
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use smol_str::SmolStr;
use std::sync::Arc;

/// A [`Vfs`] over a Docker engine. Read-only browse of containers, images, and container filesystems.
pub struct DockerVfs<O: ContainerOps> {
    conn: ConnectionId,
    ops: Arc<O>,
}

impl<O: ContainerOps> DockerVfs<O> {
    /// Create a backend over the given engine.
    pub fn new(conn: ConnectionId, ops: O) -> Self {
        Self {
            conn,
            ops: Arc::new(ops),
        }
    }

    async fn list_dir(&self, dir: VfsPath) -> Result<ListPage, VfsError> {
        let segs: Vec<&str> = dir.segments().iter().map(SmolStr::as_str).collect();
        let entries = match segs.as_slice() {
            [] => vec![
                Entry::new("containers", EntryKind::Dir),
                Entry::new("images", EntryKind::Dir),
            ],
            ["containers"] => self
                .ops
                .list_containers()
                .await?
                .into_iter()
                .map(|c| {
                    let mut e = Entry::new(c.name, EntryKind::Dir);
                    e.ext = EntryExt::Container {
                        id: SmolStr::new(c.id),
                        state: c.state,
                        image: SmolStr::new(c.image),
                    };
                    e
                })
                .collect(),
            ["images"] => self
                .ops
                .list_images()
                .await?
                .into_iter()
                .map(|img| {
                    let name = img.tags.first().cloned().unwrap_or_else(|| img.id.clone());
                    let mut e = Entry::new(name, EntryKind::Dir);
                    e.ext = EntryExt::Image {
                        id: SmolStr::new(img.id),
                        layers: 0,
                        tags: img.tags.into_iter().map(SmolStr::new).collect(),
                    };
                    e
                })
                .collect(),
            ["images", tag] => {
                // The image must exist; its layer browse is deferred (RFC-0004).
                let exists = self
                    .ops
                    .list_images()
                    .await?
                    .iter()
                    .any(|img| img.tags.iter().any(|t| t == tag) || img.id == *tag);
                if exists {
                    Vec::new()
                } else {
                    return Err(VfsError::NotFound(dir));
                }
            }
            ["containers", name, rest @ ..] => {
                let in_path = join_abs_path(rest);
                self.ops
                    .list_dir(name, &in_path)
                    .await?
                    .into_iter()
                    .map(|r| {
                        let mut e = Entry::new(r.name, r.kind);
                        if r.kind == EntryKind::File {
                            e.size = r.size;
                        }
                        e
                    })
                    .collect()
            }
            _ => return Err(VfsError::NotFound(dir)),
        };
        Ok(ListPage {
            entries,
            cursor: None,
            done: true,
        })
    }
}

impl<O: ContainerOps> CapabilityProvider for DockerVfs<O> {
    fn caps(&self) -> Caps {
        // The Vfs mapping honors ranged reads (in-memory clamp). The real adapter's in-container
        // fs ops (list/read) are the deferred integration step; until then they return Unsupported.
        Caps::LIST | Caps::READ | Caps::RANDOM_READ
    }
}

#[async_trait]
impl<O: ContainerOps> Vfs for DockerVfs<O> {
    fn scheme(&self) -> Scheme {
        Scheme::Docker
    }

    fn connection(&self) -> ConnectionId {
        self.conn
    }

    fn list<'a>(
        &'a self,
        dir: &VfsPath,
        _opts: ListOpts,
    ) -> BoxStream<'a, Result<ListPage, VfsError>> {
        let dir = dir.clone();
        stream::once(async move { self.list_dir(dir).await }).boxed()
    }

    async fn stat(&self, path: &VfsPath) -> Result<Entry, VfsError> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();
        match segs.as_slice() {
            [] | ["containers"] | ["images"] => {
                Ok(Entry::new(path.file_name().unwrap_or(""), EntryKind::Dir))
            }
            ["images", tag] => {
                let exists = self
                    .ops
                    .list_images()
                    .await?
                    .iter()
                    .any(|img| img.tags.iter().any(|t| t == tag) || img.id == *tag);
                if exists {
                    Ok(Entry::new(*tag, EntryKind::Dir))
                } else {
                    Err(VfsError::NotFound(path.clone()))
                }
            }
            ["containers", name] => {
                let exists = self
                    .ops
                    .list_containers()
                    .await?
                    .iter()
                    .any(|c| c.name == *name);
                if exists {
                    Ok(Entry::new(*name, EntryKind::Dir))
                } else {
                    Err(VfsError::NotFound(path.clone()))
                }
            }
            ["containers", name, rest @ ..] => {
                let m = self.ops.stat(name, &join_abs_path(rest)).await?;
                let mut e = Entry::new(path.file_name().unwrap_or(""), m.kind);
                if m.kind == EntryKind::File {
                    e.size = m.size;
                }
                Ok(e)
            }
            _ => Err(VfsError::NotFound(path.clone())),
        }
    }

    async fn open_read(
        &self,
        path: &VfsPath,
        range: Option<ByteRange>,
    ) -> Result<ReadHandle, VfsError> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();
        let data = match segs.as_slice() {
            ["containers", name, rest @ ..] if !rest.is_empty() => {
                self.ops.read(name, &join_abs_path(rest)).await?
            }
            _ => return Err(VfsError::Unsupported(Caps::READ)),
        };
        let sliced = match range {
            None => data,
            // Clamp in memory (no transport-level seek yet); saturating, so a pathological
            // caller-controlled range can never overflow or panic the slice.
            Some(r) => apply_byte_range(&data, r).to_vec(),
        };
        let len = sliced.len() as u64;
        Ok(ReadHandle::new(
            Box::new(std::io::Cursor::new(sliced)),
            Some(len),
        ))
    }

    async fn open_write(&self, _path: &VfsPath, _opts: WriteOpts) -> Result<WriteHandle, VfsError> {
        Err(VfsError::Unsupported(Caps::WRITE))
    }

    async fn remove(&self, _path: &VfsPath, _recurse: Recurse) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::DELETE))
    }

    /// Advertise the per-container actions (`logs`, `exec`) anywhere within a container's subtree.
    /// This reflects path *shape*, not existence (it does no I/O, mirroring how the UI calls it on an
    /// already-navigated node); existence is enforced by `stat`/`invoke`. Both actions are live in
    /// [`Vfs::invoke`]: `logs` returns `ActionOutcome::Stream`; `exec` returns `ActionOutcome::Session`.
    fn actions_at(&self, path: &VfsPath) -> Vec<ActionDescriptor> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();
        match segs.as_slice() {
            ["containers", _name, ..] => vec![
                ActionDescriptor {
                    id: ActionId::new(action_ids::LOGS),
                    label: SmolStr::new("Stream logs"),
                    kind: ActionKind::Stream,
                    destructive: false,
                },
                ActionDescriptor {
                    id: ActionId::new(action_ids::EXEC),
                    label: SmolStr::new("Exec"),
                    kind: ActionKind::Interactive,
                    destructive: false,
                },
            ],
            _ => Vec::new(),
        }
    }

    /// Invoke a per-container action.
    ///
    /// **Implemented:**
    /// - `logs` — streams container log output as a `BoxStream<'static, Result<Bytes, VfsError>>`
    ///   (`ActionOutcome::Stream`). Stdout and stderr are interleaved in arrival order; bollard
    ///   demultiplexes the Docker 8-byte stream header so callers receive plain payload bytes.
    ///   `follow` is taken from [`ActionCtx::Logs`] when present; the default is `false` (bounded
    ///   history, so a hermetic/dind test terminates). `tail` defaults to `"100"` for the same reason.
    /// - `exec` — opens an interactive exec session via the Docker Engine API (`create_exec` →
    ///   `start_exec` → relay tasks → `inspect_exec`). Returns `ActionOutcome::Session` with a
    ///   [`cairn_vfs::SessionHandle`] whose `stdin`/`stdout` channels carry bidirectional I/O;
    ///   `resize` is populated when `tty: true`. Requires [`ActionCtx::Exec`]; any other ctx
    ///   variant returns `VfsError::Backend { code: "invalid_ctx" }`.
    async fn invoke(
        &self,
        path: &VfsPath,
        action: ActionId,
        ctx: ActionCtx,
    ) -> Result<ActionOutcome, VfsError> {
        let segs: Vec<&str> = path.segments().iter().map(SmolStr::as_str).collect();

        // Only container paths expose actions; anything else (including unknown paths under
        // /images or /containers that are not the per-container row) is an unsupported action.
        let container_name = match segs.as_slice() {
            ["containers", name, ..] => *name,
            _ => {
                return Err(VfsError::Backend {
                    code: "not_implemented".to_owned(),
                    msg: format!("action '{}' is not available at this path", action.as_str()),
                    retryable: false,
                })
            }
        };

        match action.as_str() {
            action_ids::LOGS => {
                let (follow, tail) = match &ctx {
                    ActionCtx::Logs { follow, .. } => (*follow, "100"),
                    _ => (false, "100"),
                };
                let stream = self.ops.logs(container_name, follow, tail);
                Ok(ActionOutcome::Stream(stream))
            }
            action_ids::EXEC => {
                // ActionCtx::Exec is required; any other variant is a caller bug, not a
                // retryable engine error.
                let (argv, tty) = match &ctx {
                    ActionCtx::Exec { argv, tty } => (argv.clone(), *tty),
                    _ => {
                        return Err(VfsError::Backend {
                            code: "invalid_ctx".to_owned(),
                            msg: "exec action requires ActionCtx::Exec".to_owned(),
                            retryable: false,
                        })
                    }
                };
                // Validate before the API call so the caller gets a clear error rather than an
                // opaque daemon 400.
                if argv.is_empty() {
                    return Err(VfsError::Backend {
                        code: "empty_argv".to_owned(),
                        msg: "exec: argv must be non-empty".to_owned(),
                        retryable: false,
                    });
                }
                let handle = self.ops.exec(container_name, argv, tty).await?;
                Ok(ActionOutcome::Session(handle))
            }
            other => Err(VfsError::Backend {
                code: "not_implemented".to_owned(),
                msg: format!("action '{other}' is not available"),
                retryable: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ops::mock::MockDocker;
    use tokio::io::AsyncReadExt;

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    fn backend() -> DockerVfs<MockDocker> {
        DockerVfs::new(ConnectionId(1), MockDocker::new())
    }

    async fn names(vfs: &DockerVfs<MockDocker>, path: &str) -> Vec<String> {
        let mut s = vfs.list(&p(path), ListOpts::default());
        let page = s.next().await.unwrap().unwrap();
        let mut n: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        n.sort();
        n
    }

    #[tokio::test]
    async fn root_lists_containers_and_images() {
        assert_eq!(names(&backend(), "/").await, vec!["containers", "images"]);
    }

    #[tokio::test]
    async fn lists_containers_and_images() {
        assert_eq!(names(&backend(), "/containers").await, vec!["web"]);
        assert_eq!(names(&backend(), "/images").await, vec!["nginx:latest"]);
    }

    #[tokio::test]
    async fn navigates_container_filesystem() {
        let vfs = backend();
        assert_eq!(names(&vfs, "/containers/web").await, vec!["app", "etc"]);
        assert_eq!(
            names(&vfs, "/containers/web/etc").await,
            vec!["hostname", "hosts"]
        );
    }

    #[tokio::test]
    async fn reads_a_container_file() {
        let vfs = backend();
        let mut rh = vfs
            .open_read(&p("/containers/web/etc/hostname"), None)
            .await
            .unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "web-1\n");
    }

    #[tokio::test]
    async fn stat_distinguishes_dirs_and_files() {
        let vfs = backend();
        assert!(vfs.stat(&p("/containers")).await.unwrap().is_dir());
        assert!(vfs.stat(&p("/containers/web")).await.unwrap().is_dir());
        assert!(vfs.stat(&p("/containers/web/etc")).await.unwrap().is_dir());
        assert_eq!(
            vfs.stat(&p("/containers/web/etc/hostname"))
                .await
                .unwrap()
                .size,
            Some(6)
        );
        assert!(matches!(
            vfs.stat(&p("/containers/nope")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn ranged_read_clamps_and_never_panics() {
        let vfs = backend();
        let path = p("/containers/web/etc/hostname"); // "web-1\n", 6 bytes
                                                      // A pathological range must clamp to empty, not overflow/panic.
        let mut rh = vfs
            .open_read(
                &path,
                Some(ByteRange {
                    offset: u64::MAX,
                    len: Some(1),
                }),
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        rh.read_to_end(&mut out).await.unwrap();
        assert!(out.is_empty());
        // A normal sub-range still works.
        let mut rh = vfs
            .open_read(
                &path,
                Some(ByteRange {
                    offset: 0,
                    len: Some(3),
                }),
            )
            .await
            .unwrap();
        let mut out = String::new();
        rh.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "web");
    }

    #[tokio::test]
    async fn stat_rejects_unknown_image_and_routes() {
        let vfs = backend();
        assert!(vfs.stat(&p("/images/nginx:latest")).await.unwrap().is_dir());
        assert!(matches!(
            vfs.stat(&p("/images/nope")).await,
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            vfs.stat(&p("/bogus")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn listing_a_file_or_missing_dir_is_not_found() {
        let vfs = backend();
        assert!(matches!(
            vfs.list(&p("/containers/web/etc/hostname"), ListOpts::default())
                .next()
                .await
                .unwrap(),
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            vfs.list(&p("/containers/web/missing"), ListOpts::default())
                .next()
                .await
                .unwrap(),
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn writes_are_unsupported() {
        let vfs = backend();
        assert!(matches!(
            vfs.open_write(&p("/containers/web/x"), WriteOpts::default())
                .await,
            Err(VfsError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn containers_advertise_exec_and_logs_actions() {
        let vfs = backend();
        // Containers (and their subtree) expose exec + logs; the top-level tree does not.
        let ids: Vec<String> = vfs
            .actions_at(&p("/containers/web"))
            .iter()
            .map(|a| a.id.as_str().to_owned())
            .collect();
        assert_eq!(ids, vec!["logs", "exec"]);
        assert!(!vfs.actions_at(&p("/containers/web/etc")).is_empty());
        assert!(vfs.actions_at(&p("/containers")).is_empty());
        assert!(vfs.actions_at(&p("/images/nginx:latest")).is_empty());
        // Reflects path shape, not existence (a phantom container still lists actions).
        assert!(!vfs.actions_at(&p("/containers/ghost")).is_empty());
    }

    #[tokio::test]
    async fn invoke_logs_returns_stream_with_canned_output() {
        let vfs = backend();
        // invoke("logs") on a known mock container yields ActionOutcome::Stream.
        let outcome = vfs
            .invoke(
                &p("/containers/web"),
                ActionId::new(action_ids::LOGS),
                ActionCtx::None,
            )
            .await
            .expect("invoke logs on a known container must succeed");

        let stream = match outcome {
            ActionOutcome::Stream(s) => s,
            _ => panic!("expected ActionOutcome::Stream"),
        };

        let chunks: Vec<Vec<u8>> = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.expect("stream item must be Ok").to_vec())
            .collect();
        let combined = chunks.concat();
        let text = String::from_utf8(combined).expect("mock log output must be valid UTF-8");
        assert!(text.contains("[mock] log line 1\n"), "got: {text:?}");
        assert!(text.contains("[mock] log line 2\n"), "got: {text:?}");
    }

    #[tokio::test]
    async fn invoke_logs_on_non_container_path_errors() {
        let vfs = backend();
        // invoke("logs") on /images/… is not a container path — must be a backend error.
        assert!(matches!(
            vfs.invoke(
                &p("/images/nginx:latest"),
                ActionId::new(action_ids::LOGS),
                ActionCtx::None
            )
            .await,
            Err(VfsError::Backend { code, .. }) if code == "not_implemented"
        ));
    }

    /// Passing the wrong `ActionCtx` variant must be a typed caller error, not a daemon error.
    #[tokio::test]
    async fn invoke_exec_with_wrong_ctx_returns_invalid_ctx() {
        let vfs = backend();
        assert!(matches!(
            vfs.invoke(
                &p("/containers/web"),
                ActionId::new(action_ids::EXEC),
                // ActionCtx::None instead of ActionCtx::Exec → invalid_ctx
                ActionCtx::None,
            )
            .await,
            Err(VfsError::Backend { code, .. }) if code == "invalid_ctx"
        ));
    }

    /// Non-TTY exec: stdin bytes must be echoed back to stdout; resize is absent; local_port is None.
    #[tokio::test]
    async fn invoke_exec_non_tty_echoes_stdin_to_stdout() {
        use bytes::Bytes;

        let vfs = backend();
        let outcome = vfs
            .invoke(
                &p("/containers/web"),
                ActionId::new(action_ids::EXEC),
                ActionCtx::Exec {
                    argv: vec!["sh".to_owned()],
                    tty: false,
                },
            )
            .await
            .expect("exec on a running mock container must succeed");

        let mut handle = match outcome {
            ActionOutcome::Session(h) => h,
            _ => panic!("expected ActionOutcome::Session"),
        };

        // Non-TTY exec must not provide a resize or local_port.
        assert!(
            handle.resize.is_none(),
            "non-tty exec must have no resize channel"
        );
        assert!(
            handle.local_port.is_none(),
            "exec sessions never bind a port"
        );

        // Send a chunk via stdin and expect it echoed on stdout.
        let stdin_tx = handle.stdin.take().expect("stdin must be Some for exec");
        stdin_tx
            .send(Bytes::from_static(b"hello"))
            .await
            .expect("stdin send");
        drop(stdin_tx); // Close stdin → mock exits with code 0.

        let stdout_rx = handle
            .stdout
            .as_mut()
            .expect("stdout must be Some for exec");
        let chunk = stdout_rx
            .recv()
            .await
            .expect("stdout must yield the echoed chunk");
        assert_eq!(chunk, Bytes::from_static(b"hello"));

        // done must resolve with Ok(0) after stdin closes.
        let exit = handle
            .done
            .await
            .expect("done channel must resolve")
            .expect("exit must be Ok");
        assert_eq!(exit, 0);
    }

    /// TTY exec: a resize channel must be present; cancellation resolves done with Ok(-1).
    #[tokio::test]
    async fn invoke_exec_tty_has_resize_channel_and_cancel_works() {
        let vfs = backend();
        let outcome = vfs
            .invoke(
                &p("/containers/web"),
                ActionId::new(action_ids::EXEC),
                ActionCtx::Exec {
                    argv: vec!["sh".to_owned()],
                    tty: true,
                },
            )
            .await
            .expect("tty exec on mock container must succeed");

        let handle = match outcome {
            ActionOutcome::Session(h) => h,
            _ => panic!("expected ActionOutcome::Session"),
        };

        // TTY exec must provide a resize channel.
        let resize = handle.resize.expect("tty exec must have a resize channel");

        // Send a resize event; mock accepts and discards it.
        resize
            .send((24, 80))
            .await
            .expect("resize send must succeed");

        // Cancel the session and verify done resolves with Ok(-1).
        handle.cancel.send(()).expect("cancel send must succeed");
        let exit = handle
            .done
            .await
            .expect("done channel must resolve")
            .expect("exit must be Ok");
        assert_eq!(exit, -1, "cancelled session must report exit code -1");
    }

    /// An empty argv must be rejected before any API call with a typed `empty_argv` error.
    #[tokio::test]
    async fn invoke_exec_empty_argv_returns_error() {
        let vfs = backend();
        assert!(matches!(
            vfs.invoke(
                &p("/containers/web"),
                ActionId::new(action_ids::EXEC),
                ActionCtx::Exec {
                    argv: vec![], // deliberately empty
                    tty: false,
                },
            )
            .await,
            Err(VfsError::Backend { code, .. }) if code == "empty_argv"
        ));
    }

    /// Invoking exec on an unknown container must surface VfsError::NotFound from the mock.
    #[tokio::test]
    async fn invoke_exec_unknown_container_returns_not_found() {
        let vfs = backend();
        // "ghost" is not in MockDocker's container list.
        assert!(matches!(
            vfs.invoke(
                &p("/containers/ghost"),
                ActionId::new(action_ids::EXEC),
                ActionCtx::Exec {
                    argv: vec!["sh".to_owned()],
                    tty: false,
                },
            )
            .await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn invoke_logs_ctx_follow_is_forwarded() {
        let vfs = backend();
        // ActionCtx::Logs{follow: true} must also return a stream (the mock ignores follow).
        let outcome = vfs
            .invoke(
                &p("/containers/web"),
                ActionId::new(action_ids::LOGS),
                ActionCtx::Logs {
                    follow: true,
                    since: None,
                    container: None,
                },
            )
            .await
            .expect("invoke logs with follow=true must succeed");
        assert!(matches!(outcome, ActionOutcome::Stream(_)));
    }
}
