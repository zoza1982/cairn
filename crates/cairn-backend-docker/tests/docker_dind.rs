//! Live Docker filesystem round-trip against a real daemon (dind or local socket).
//!
//! **Env-guarded:** this test is a no-op unless `CAIRN_IT_DOCKER` is set, so the default
//! `cargo test` (and the hermetic CI in `ci.yml`) never touch the daemon. The dedicated
//! integration job spins up a dind sidecar, sets `CAIRN_IT_DOCKER=1`, and runs this test
//! with `--features docker`. See ADR-0006.
//!
//! The test:
//! 1. Pulls `busybox:latest` (no-op if already cached by the dind sidecar).
//! 2. Creates and starts a long-running container.
//! 3. Drives the **`Vfs`** surface (`list`, `stat`, `open_read`) against the container's
//!    filesystem, asserting plausible results.
//! 4. Verifies that a missing path returns `VfsError::NotFound`.
//! 5. Force-removes the container.
#![cfg(feature = "docker")]

use bollard::models::ContainerCreateBody;
use bollard::Docker;
use cairn_backend_docker::{BollardDocker, DockerVfs};
use cairn_types::{ConnectionId, VfsPath};
use cairn_vfs::{ListOpts, Vfs, VfsError};
use futures::StreamExt;
use tokio::io::AsyncReadExt;

const TEST_IMAGE: &str = "busybox:latest";
/// Container name is stable so a prior failed run can be cleaned up at the start.
const CONTAINER_NAME: &str = "cairn-it-docker-fs-test";

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
