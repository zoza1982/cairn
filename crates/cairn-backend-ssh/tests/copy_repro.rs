#![cfg(unix)]
//! Reproduction for the "Copy failed: not found" bug when copying a directory onto a remote SFTP
//! backend where the destination directory already exists (an overwrite copy).
//!
//! Drives the real OpenSSH `sftp-server` over a pipe (same harness as `sftp_server_repro.rs`) as the
//! transfer *destination*, with a `LocalVfs` as the source, and runs the actual
//! `cairn_transfer::run_transfer` engine — exactly the code the app runs behind Copy (F5).
//!
//!   CAIRN_IT_SFTP=1 cargo test -p cairn-backend-ssh --test copy_repro -- --nocapture

use cairn_backend_local::LocalVfs;
use cairn_backend_ssh::{RealSftp, SftpVfs};
use cairn_transfer::{run_transfer, ConflictPolicy, TransferOp, TransferSpec, VerifyPolicy};
use cairn_types::{ConnectionId, VfsPath};
use cairn_vfs::Vfs;
use russh_sftp::client::SftpSession;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

fn sftp_server_bin() -> Option<&'static str> {
    ["/usr/lib/openssh/sftp-server", "/usr/libexec/sftp-server"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

async fn connect(conn: ConnectionId) -> (SftpVfs<RealSftp>, tokio::process::Child) {
    let bin = sftp_server_bin().expect("sftp-server binary");
    let mut child = tokio::process::Command::new(bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn sftp-server");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stream = tokio::io::join(stdout, stdin);
    let session = SftpSession::new(stream).await.expect("sftp init");
    (SftpVfs::new(conn, RealSftp::new(session)), child)
}

/// Copy a directory tree whose destination dir already exists (overwrite). This is the screenshot's
/// scenario: the remote `pentesting/` already exists.
#[tokio::test]
async fn copy_dir_onto_existing_remote_dir() {
    if std::env::var("CAIRN_IT_SFTP").is_err() {
        eprintln!("skipping: set CAIRN_IT_SFTP=1 to run");
        return;
    }
    if sftp_server_bin().is_none() {
        eprintln!("skipping: no sftp-server binary found");
        return;
    }

    // Source tree: <src>/pentesting/{a.txt, sub/b.txt}
    let src_tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src_tmp.path().join("pentesting/sub")).unwrap();
    std::fs::write(src_tmp.path().join("pentesting/a.txt"), b"hello").unwrap();
    std::fs::write(src_tmp.path().join("pentesting/sub/b.txt"), b"world").unwrap();

    // Destination: <dst>/pentesting already exists (overwrite case). sftp-server does not chroot, so
    // its paths are real absolute paths.
    let dst_tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dst_tmp.path().join("pentesting")).unwrap();

    let src: Arc<dyn Vfs> = Arc::new(LocalVfs::new(ConnectionId(1), src_tmp.path()));
    let (dst_vfs, _server) = connect(ConnectionId(2)).await;
    let dst: Arc<dyn Vfs> = Arc::new(dst_vfs);

    let from = VfsPath::parse("/pentesting").unwrap();
    let to = VfsPath::parse(&format!("{}/pentesting", dst_tmp.path().to_str().unwrap())).unwrap();
    let items = vec![(from, to)];

    let spec = TransferSpec {
        op: TransferOp::Copy,
        conflict: ConflictPolicy::Overwrite,
        verify: VerifyPolicy::Size,
    };
    let cancel = CancellationToken::new();
    let (_tx, paused) = watch::channel(false);
    let mut progress = |_e: cairn_transfer::ProgressEvent| {};

    let result = run_transfer(&src, &dst, &items, spec, &cancel, &paused, &mut progress).await;
    eprintln!("run_transfer result: {result:?}");

    assert!(
        result.is_ok(),
        "copy onto an existing remote dir failed: {:?}",
        result.err()
    );
    assert!(dst_tmp.path().join("pentesting/a.txt").exists());
    assert!(dst_tmp.path().join("pentesting/sub/b.txt").exists());
}

/// Isolate the suspected culprit: what does `create_dir` return when the directory already exists?
/// The transfer engine only tolerates `VfsError::AlreadyExists`; anything else aborts the copy.
#[tokio::test]
async fn create_dir_on_existing_returns_already_exists() {
    if std::env::var("CAIRN_IT_SFTP").is_err() {
        eprintln!("skipping: set CAIRN_IT_SFTP=1 to run");
        return;
    }
    if sftp_server_bin().is_none() {
        eprintln!("skipping: no sftp-server binary found");
        return;
    }
    let dst_tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dst_tmp.path().join("exists")).unwrap();
    let (dst, _server) = connect(ConnectionId(3)).await;
    let p = VfsPath::parse(&format!("{}/exists", dst_tmp.path().to_str().unwrap())).unwrap();
    let err = dst.create_dir(&p).await;
    eprintln!("create_dir on existing dir returned: {err:?}");
    assert!(
        matches!(err, Err(cairn_vfs::VfsError::AlreadyExists(_))),
        "expected AlreadyExists, got {err:?}"
    );
}
