//! Live Docker integration tests against a real daemon (dind or local socket).
//!
//! **Env-guarded:** these tests are a no-op unless `CAIRN_IT_DOCKER` is set, so the default
//! `cargo test` (and the hermetic CI in `ci.yml`) never touch the daemon. The dedicated
//! integration job spins up a dind sidecar, sets `CAIRN_IT_DOCKER=1`, and runs these tests
//! with `--features docker`. See ADR-0006.
//!
//! Tests included:
//! - **`docker_container_fs_round_trip`**: Pulls `busybox:latest`, creates a container, drives the
//!   `Vfs` surface (`list`/`stat`/`open_read`) against the container's filesystem, asserts that a
//!   missing path is `NotFound`, then force-removes the container.
//! - **`docker_container_log_streaming`**: Pulls `busybox:latest`, creates a container that emits
//!   a known marker line on stdout, drives `DockerVfs::invoke("logs")` (non-follow, bounded tail),
//!   and asserts the collected stream contains the marker. Force-removes the container.
#![cfg(feature = "docker")]

use bollard::models::ContainerCreateBody;
use bollard::Docker;
use bytes::Bytes;
use cairn_backend_docker::{BollardDocker, DockerVfs};
use cairn_types::{ConnectionId, VfsPath};
use cairn_vfs::{action_ids, ActionCtx, ActionId, ActionOutcome, ListOpts, Vfs, VfsError};
use futures::StreamExt;
use tokio::io::AsyncReadExt;

const TEST_IMAGE: &str = "busybox:latest";
/// Container name is stable so a prior failed run can be cleaned up at the start.
const CONTAINER_NAME: &str = "cairn-it-docker-fs-test";
/// Separate stable name for the log streaming test container.
const LOG_CONTAINER_NAME: &str = "cairn-it-docker-logs-test";
/// Stable name for the exec integration test container.
const EXEC_CONTAINER_NAME: &str = "cairn-it-docker-exec-test";
/// Marker printed by the log test container — chosen to be unique and easily grepped.
const LOG_MARKER: &str = "CAIRN_LOG_STREAM_MARKER";
/// Marker used by the exec integration test — distinct from the log marker.
const EXEC_MARKER: &str = "CAIRN_EXEC_MARKER";

fn p(s: &str) -> VfsPath {
    VfsPath::parse(s).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_container_fs_round_trip() {
    if std::env::var("CAIRN_IT_DOCKER").is_err() {
        eprintln!("CAIRN_IT_DOCKER unset — skipping live Docker integration test");
        return;
    }

    // Raw Docker handle used only for test scaffolding (pull / create / remove).
    let raw =
        Docker::connect_with_local_defaults().expect("connect to Docker daemon for scaffolding");

    // Pull the image, draining the progress stream.  This is a no-op when the dind sidecar has
    // already pre-pulled the image; we drain the stream so errors surface immediately.
    {
        let pull_opts = bollard::query_parameters::CreateImageOptionsBuilder::default()
            .from_image(TEST_IMAGE)
            .build();
        let mut pull = raw.create_image(Some(pull_opts), None, None);
        while let Some(item) = pull.next().await {
            item.expect("image pull progress should not error");
        }
    }

    // Remove any leftover container from a previous failed run so the name is free.
    let _ = raw
        .remove_container(
            CONTAINER_NAME,
            Some(
                bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                    .force(true)
                    .build(),
            ),
        )
        .await;

    // Create a long-running container so its filesystem is fully accessible.
    raw.create_container(
        Some(
            bollard::query_parameters::CreateContainerOptionsBuilder::default()
                .name(CONTAINER_NAME)
                .build(),
        ),
        ContainerCreateBody {
            image: Some(TEST_IMAGE.to_owned()),
            cmd: Some(vec!["sleep".to_owned(), "3600".to_owned()]),
            ..Default::default()
        },
    )
    .await
    .expect("create container");

    raw.start_container(
        CONTAINER_NAME,
        None::<bollard::query_parameters::StartContainerOptions>,
    )
    .await
    .expect("start container");

    // Build the Vfs under test using the real BollardDocker adapter.
    let ops = BollardDocker::connect_local().expect("BollardDocker::connect_local");
    let vfs = DockerVfs::new(ConnectionId(1), ops);

    // /containers should list our container.
    {
        let mut stream = vfs.list(&p("/containers"), ListOpts::default());
        let page = stream
            .next()
            .await
            .expect("stream has one page")
            .expect("list /containers");
        let names: Vec<_> = page.entries.iter().map(|e| e.name.to_string()).collect();
        assert!(
            names.contains(&CONTAINER_NAME.to_owned()),
            "expected {CONTAINER_NAME} in /containers listing, got {names:?}"
        );
    }

    // Container root must be non-empty (busybox always has /bin, /etc, …).
    {
        let root_path = format!("/containers/{CONTAINER_NAME}");
        let mut stream = vfs.list(&p(&root_path), ListOpts::default());
        let page = stream
            .next()
            .await
            .expect("stream has one page")
            .expect("list container root");
        assert!(!page.entries.is_empty(), "container root must be non-empty");
        // Confirm at least one directory entry (e.g. "bin") appears.
        let has_dir = page.entries.iter().any(|e| e.is_dir());
        assert!(
            has_dir,
            "container root should contain at least one directory"
        );
    }

    // /bin must stat as a directory.
    {
        let bin_path = format!("/containers/{CONTAINER_NAME}/bin");
        let meta = vfs.stat(&p(&bin_path)).await.expect("stat /bin");
        assert!(meta.is_dir(), "/bin should be a directory");

        // List /bin — busybox populates it with executables.
        let mut stream = vfs.list(&p(&bin_path), ListOpts::default());
        let page = stream
            .next()
            .await
            .expect("stream has one page")
            .expect("list /bin");
        assert!(!page.entries.is_empty(), "/bin must contain files");
    }

    // /bin/busybox must stat as a file with a known size and start with ELF magic bytes.
    {
        let busybox_path = format!("/containers/{CONTAINER_NAME}/bin/busybox");
        let meta = vfs
            .stat(&p(&busybox_path))
            .await
            .expect("stat /bin/busybox");
        assert!(!meta.is_dir(), "/bin/busybox should be a regular file");
        assert!(
            meta.size.is_some(),
            "/bin/busybox should report a non-None size"
        );

        let mut rh = vfs
            .open_read(&p(&busybox_path), None)
            .await
            .expect("open_read /bin/busybox");
        let mut data = Vec::new();
        rh.read_to_end(&mut data).await.expect("read_to_end");
        assert!(!data.is_empty(), "/bin/busybox must not be empty");
        // ELF magic: 0x7F 'E' 'L' 'F'
        assert!(
            data.starts_with(b"\x7fELF"),
            "/bin/busybox should be an ELF binary (got first 4 bytes: {:?})",
            &data[..data.len().min(4)]
        );
    }

    // A path that does not exist must return VfsError::NotFound.
    {
        let missing = format!("/containers/{CONTAINER_NAME}/no-such-path-cairn-it");
        assert!(
            matches!(vfs.stat(&p(&missing)).await, Err(VfsError::NotFound(_))),
            "a missing path must be NotFound"
        );
    }

    // Cleanup — force-remove so the name is free for the next run.
    raw.remove_container(
        CONTAINER_NAME,
        Some(
            bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                .force(true)
                .build(),
        ),
    )
    .await
    .expect("force-remove container");
}

/// Live log-streaming round-trip: invoke `DockerVfs::invoke("logs")` against a container that
/// emits a known marker on stdout, and assert the stream contains it.
///
/// The container runs `sh -c 'echo CAIRN_LOG_STREAM_MARKER; sleep 3600'`. After the echo exits,
/// the marker is in the daemon's log buffer. We invoke logs with `follow: false` and `tail: "100"`
/// so the stream is bounded (the test terminates without killing the container mid-stream).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_container_log_streaming() {
    if std::env::var("CAIRN_IT_DOCKER").is_err() {
        eprintln!("CAIRN_IT_DOCKER unset — skipping live Docker log streaming test");
        return;
    }

    let raw =
        Docker::connect_with_local_defaults().expect("connect to Docker daemon for scaffolding");

    // Pull the image (no-op when the dind sidecar has already cached it).
    {
        let pull_opts = bollard::query_parameters::CreateImageOptionsBuilder::default()
            .from_image(TEST_IMAGE)
            .build();
        let mut pull = raw.create_image(Some(pull_opts), None, None);
        while let Some(item) = pull.next().await {
            item.expect("image pull progress should not error");
        }
    }

    // Clean up any leftover container from a previous failed run.
    let _ = raw
        .remove_container(
            LOG_CONTAINER_NAME,
            Some(
                bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                    .force(true)
                    .build(),
            ),
        )
        .await;

    // Create and start a container that prints the marker then sleeps.
    raw.create_container(
        Some(
            bollard::query_parameters::CreateContainerOptionsBuilder::default()
                .name(LOG_CONTAINER_NAME)
                .build(),
        ),
        ContainerCreateBody {
            image: Some(TEST_IMAGE.to_owned()),
            // sh is available in busybox; 'echo …; sleep 3600' keeps the container alive so
            // the VFS can list it, while putting the marker into the log buffer immediately.
            cmd: Some(vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("echo {LOG_MARKER}; sleep 3600"),
            ]),
            ..Default::default()
        },
    )
    .await
    .expect("create log-test container");

    raw.start_container(
        LOG_CONTAINER_NAME,
        None::<bollard::query_parameters::StartContainerOptions>,
    )
    .await
    .expect("start log-test container");

    // Give the daemon a moment to flush the echo output into the log buffer.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Build the Vfs under test.
    let ops = BollardDocker::connect_local().expect("BollardDocker::connect_local");
    let vfs = DockerVfs::new(ConnectionId(42), ops);

    // Drive invoke("logs") — non-follow / tail=100 so the stream terminates.
    let log_path = format!("/containers/{LOG_CONTAINER_NAME}");
    let outcome = vfs
        .invoke(
            &p(&log_path),
            ActionId::new(action_ids::LOGS),
            ActionCtx::Logs {
                follow: false,
                since: None,
                container: None,
            },
        )
        .await
        .expect("invoke logs must succeed on a running container");

    let stream = match outcome {
        ActionOutcome::Stream(s) => s,
        _ => panic!("expected ActionOutcome::Stream from invoke logs"),
    };

    // Collect all frames — the stream ends when non-follow mode exhausts buffered history.
    let chunks: Vec<bytes::Bytes> = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|r| r.expect("log stream item must be Ok"))
        .collect();

    let combined = chunks
        .iter()
        .flat_map(|b| b.iter().copied())
        .collect::<Vec<u8>>();
    let text = String::from_utf8_lossy(&combined);
    assert!(
        text.contains(LOG_MARKER),
        "log stream must contain the marker '{LOG_MARKER}'; got: {text:?}"
    );

    // Cleanup.
    raw.remove_container(
        LOG_CONTAINER_NAME,
        Some(
            bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                .force(true)
                .build(),
        ),
    )
    .await
    .expect("force-remove log-test container");
}

/// Live exec round-trip: invoke `DockerVfs::invoke("exec")` with `echo CAIRN_EXEC_MARKER`,
/// collect all stdout bytes, assert the marker is present, and assert exit code 0.
///
/// Uses `tty: false` and a short-lived command so the output stream closes naturally,
/// letting `inspect_exec` return the real exit code without any manual cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_container_exec_round_trip() {
    if std::env::var("CAIRN_IT_DOCKER").is_err() {
        eprintln!("CAIRN_IT_DOCKER unset — skipping live Docker exec integration test");
        return;
    }

    let raw =
        Docker::connect_with_local_defaults().expect("connect to Docker daemon for scaffolding");

    // Pull the image (no-op when the dind sidecar has already cached it).
    {
        let pull_opts = bollard::query_parameters::CreateImageOptionsBuilder::default()
            .from_image(TEST_IMAGE)
            .build();
        let mut pull = raw.create_image(Some(pull_opts), None, None);
        while let Some(item) = pull.next().await {
            item.expect("image pull progress should not error");
        }
    }

    // Remove any leftover container from a previous failed run.
    let _ = raw
        .remove_container(
            EXEC_CONTAINER_NAME,
            Some(
                bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                    .force(true)
                    .build(),
            ),
        )
        .await;

    // Create and start a sleeping container — exec requires the container to be running.
    raw.create_container(
        Some(
            bollard::query_parameters::CreateContainerOptionsBuilder::default()
                .name(EXEC_CONTAINER_NAME)
                .build(),
        ),
        ContainerCreateBody {
            image: Some(TEST_IMAGE.to_owned()),
            cmd: Some(vec!["sleep".to_owned(), "3600".to_owned()]),
            ..Default::default()
        },
    )
    .await
    .expect("create exec-test container");

    raw.start_container(
        EXEC_CONTAINER_NAME,
        None::<bollard::query_parameters::StartContainerOptions>,
    )
    .await
    .expect("start exec-test container");

    // Build the Vfs under test using the real BollardDocker adapter.
    let ops = BollardDocker::connect_local().expect("BollardDocker::connect_local");
    let vfs = DockerVfs::new(ConnectionId(99), ops);

    // Invoke exec — echo the marker and exit immediately. non-TTY so the output stream
    // terminates and inspect_exec can retrieve the exit code.
    let exec_path = format!("/containers/{EXEC_CONTAINER_NAME}");
    let outcome = vfs
        .invoke(
            &p(&exec_path),
            ActionId::new(action_ids::EXEC),
            ActionCtx::Exec {
                argv: vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    format!("echo {EXEC_MARKER}"),
                ],
                tty: false,
            },
        )
        .await
        .expect("invoke exec must succeed on a running container");

    let mut handle = match outcome {
        ActionOutcome::Session(h) => h,
        _ => panic!("expected ActionOutcome::Session from invoke exec"),
    };

    // Non-TTY exec: no resize, no local_port.
    assert!(
        handle.resize.is_none(),
        "non-tty exec must have no resize channel"
    );
    assert!(
        handle.local_port.is_none(),
        "exec sessions never bind a port"
    );

    // Collect all stdout bytes until the stream closes (the echo process has exited).
    let mut collected: Vec<Bytes> = Vec::new();
    if let Some(stdout_rx) = handle.stdout.as_mut() {
        while let Some(chunk) = stdout_rx.recv().await {
            collected.push(chunk);
        }
    }

    let combined: Vec<u8> = collected.iter().flat_map(|b| b.iter().copied()).collect();
    let text = String::from_utf8_lossy(&combined);
    assert!(
        text.contains(EXEC_MARKER),
        "exec output must contain the marker '{EXEC_MARKER}'; got: {text:?}"
    );

    // done must resolve with exit code 0 (echo always exits cleanly).
    let exit = handle
        .done
        .await
        .expect("done channel must resolve")
        .expect("exit result must be Ok");
    assert_eq!(exit, 0, "echo command must exit with code 0; got {exit}");

    // Cleanup.
    raw.remove_container(
        EXEC_CONTAINER_NAME,
        Some(
            bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                .force(true)
                .build(),
        ),
    )
    .await
    .expect("force-remove exec-test container");
}
