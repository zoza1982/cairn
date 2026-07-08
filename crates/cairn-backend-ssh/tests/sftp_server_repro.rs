//! Reproduction harness that drives the **real** OpenSSH `sftp-server` over a pipe (no SSH
//! transport / auth needed — `sftp-server` speaks the SFTP protocol directly on stdin/stdout).
//!
//! This lets us exercise `SftpVfs` + `RealSftp` against a genuine server implementation, which is
//! where readdir-attribute quirks live that mocks can't capture. Env-guarded like the other
//! `CAIRN_IT_*` integration tests so the default `cargo test` stays hermetic and offline.
//!
//! Run with:
//!   CAIRN_IT_SFTP=1 cargo test -p cairn-backend-ssh --test sftp_server_repro -- --nocapture

use cairn_backend_ssh::{RealSftp, SftpVfs};
use cairn_types::{ConnectionId, EntryKind, VfsPath};
use cairn_vfs::{ListOpts, Recurse, Vfs};
use futures::StreamExt;
use russh_sftp::client::SftpSession;

const CONN: ConnectionId = ConnectionId(1);

fn sftp_server_bin() -> Option<&'static str> {
    ["/usr/lib/openssh/sftp-server", "/usr/libexec/sftp-server"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

async fn connect(root: &std::path::Path) -> SftpVfs<RealSftp> {
    let bin = sftp_server_bin().expect("sftp-server binary");
    let mut child = tokio::process::Command::new(bin)
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sftp-server");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    // Leak the child so it lives for the test; killed on process exit.
    std::mem::forget(child);
    let stream = tokio::io::join(stdout, stdin);
    let session = SftpSession::new(stream).await.expect("sftp init");
    SftpVfs::new(CONN, RealSftp::new(session))
}

/// Replicates the app-edge delete walk in `cairn::app::run_delete_effect` (DFS, files removed on
/// pop, dirs removed deepest-first) so we exercise the exact logic that ships — against a real
/// server. Returns the number of failures.
async fn walk_delete(vfs: &SftpVfs<RealSftp>, root: &VfsPath) -> u64 {
    let mut failures = 0u64;
    let mut stack: Vec<(VfsPath, bool)> = Vec::new();
    match vfs.stat(root).await {
        Ok(meta) => stack.push((root.clone(), meta.is_dir())),
        Err(e) => panic!("stat root: {e:?}"),
    }
    let mut dirs_post: Vec<VfsPath> = Vec::new();
    while let Some((p, is_dir)) = stack.pop() {
        if is_dir {
            dirs_post.push(p.clone());
            let mut stream = vfs.list(&p, ListOpts { all: true });
            while let Some(page) = stream.next().await {
                let pg = page.expect("list page");
                for e in pg.entries {
                    let child = p.join(&e.name).unwrap();
                    stack.push((child, e.is_dir()));
                }
            }
        } else {
            match vfs.remove(&p, Recurse::No).await {
                Ok(()) | Err(cairn_vfs::VfsError::NotFound(_)) => {}
                Err(e) => {
                    eprintln!("remove file {p:?}: {e:?}");
                    failures += 1;
                }
            }
        }
    }
    for d in dirs_post.iter().rev() {
        match vfs.remove(d, Recurse::No).await {
            Ok(()) | Err(cairn_vfs::VfsError::NotFound(_)) => {}
            Err(e) => {
                eprintln!("remove dir {d:?}: {e:?}");
                failures += 1;
            }
        }
    }
    failures
}

#[tokio::test]
async fn real_sftp_lists_hidden_and_reports_kind() {
    if std::env::var("CAIRN_IT_SFTP").is_err() {
        eprintln!("skipping: set CAIRN_IT_SFTP=1 to run");
        return;
    }
    if sftp_server_bin().is_none() {
        eprintln!("skipping: no sftp-server binary found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("d/.hidden/deep")).unwrap();
    std::fs::write(tmp.path().join("d/.hidden/deep/.secret"), b"s").unwrap();
    std::fs::write(tmp.path().join("d/.dotfile"), b"d").unwrap();
    std::fs::write(tmp.path().join("d/visible.txt"), b"v").unwrap();

    let vfs = connect(tmp.path()).await;
    let root = VfsPath::parse(&format!("{}/d", tmp.path().to_str().unwrap())).unwrap();

    // 1) Does the real server's readdir surface the hidden dir, and with the right kind?
    let mut stream = vfs.list(&root, ListOpts { all: true });
    let mut hidden_kind = None;
    while let Some(page) = stream.next().await {
        for e in page.unwrap().entries {
            eprintln!("entry {:?} kind={:?}", e.name, e.kind);
            if e.name == ".hidden" {
                hidden_kind = Some(e.kind);
            }
        }
    }
    assert_eq!(
        hidden_kind,
        Some(EntryKind::Dir),
        "real sftp-server misreports the hidden dir's kind in readdir"
    );

    // 2) The full app-edge walk deletes everything, including the hidden subtree.
    let failures = walk_delete(&vfs, &root).await;
    assert_eq!(failures, 0, "walk delete had failures");
    assert!(
        !tmp.path().join("d").exists(),
        "hidden subtree left the dir behind"
    );
}

#[tokio::test]
async fn real_sftp_recursive_remove_of_hidden_only_dir() {
    if std::env::var("CAIRN_IT_SFTP").is_err() {
        eprintln!("skipping: set CAIRN_IT_SFTP=1 to run");
        return;
    }
    if sftp_server_bin().is_none() {
        eprintln!("skipping: no sftp-server binary found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // A directory whose *only* content is a hidden subtree — the case most likely to strand a
    // parent dir if hidden entries are skipped.
    std::fs::create_dir_all(tmp.path().join("d/.git/objects")).unwrap();
    std::fs::write(tmp.path().join("d/.git/config"), b"c").unwrap();
    std::fs::write(tmp.path().join("d/.git/objects/x"), b"x").unwrap();

    let vfs = connect(tmp.path()).await;
    let root = VfsPath::parse(&format!("{}/d", tmp.path().to_str().unwrap())).unwrap();

    // The backend's own recursive remove (Recurse::Yes) — used by move/overwrite paths.
    vfs.remove(&root, Recurse::Yes)
        .await
        .expect("recursive remove");
    assert!(
        !tmp.path().join("d").exists(),
        "recursive remove stranded the hidden-only subtree"
    );
}
