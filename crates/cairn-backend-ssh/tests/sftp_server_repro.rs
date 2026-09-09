#![cfg(unix)]
//! Reproduction harness that drives the **real** OpenSSH `sftp-server` over a pipe (no SSH
//! transport / auth needed — `sftp-server` speaks the SFTP protocol directly on stdin/stdout).
//!
//! Unix-only: it spawns the `sftp-server` binary, wires it over Unix stdio pipes, and creates
//! symlinks with `std::os::unix::fs` — none of which exist on Windows — so the whole file is
//! `#![cfg(unix)]` to keep `cargo test` compiling there (the suite is env-guarded at runtime too).
//!
//! This lets us exercise `SftpVfs` + `RealSftp` against a genuine server implementation, which is
//! where readdir-attribute quirks live that mocks can't capture. Env-guarded like the other
//! `CAIRN_IT_*` integration tests so the default `cargo test` stays hermetic and offline.
//!
//! Run with:
//!   CAIRN_IT_SFTP=1 cargo test -p cairn-backend-ssh --test sftp_server_repro -- --nocapture

use cairn_backend_ssh::{RealSftp, SftpVfs};
use cairn_types::{ConnectionId, EntryKind, VfsPath};
use cairn_vfs::{ListOpts, Recurse, Vfs, WriteOpts};
use futures::StreamExt;
use russh_sftp::client::SftpSession;
use tokio::io::AsyncReadExt;

const CONN: ConnectionId = ConnectionId(1);

fn sftp_server_bin() -> Option<&'static str> {
    // Debian/Ubuntu, macOS/BSD, Arch/Fedora respectively — a missing entry makes the whole suite
    // silently skip on that distro, so add rather than replace.
    [
        "/usr/lib/openssh/sftp-server",
        "/usr/libexec/sftp-server",
        "/usr/lib/ssh/sftp-server",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).exists())
}

/// The spawned server handle is returned so the caller keeps it alive for the test's duration and
/// drops it at the end. `kill_on_drop` reaps the `sftp-server` child when the handle drops, so no
/// zombie lingers.
async fn connect(root: &std::path::Path) -> (SftpVfs<RealSftp>, tokio::process::Child) {
    let bin = sftp_server_bin().expect("sftp-server binary");
    let mut child = tokio::process::Command::new(bin)
        .current_dir(root)
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
    (SftpVfs::new(CONN, RealSftp::new(session)), child)
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

    let (vfs, _server) = connect(tmp.path()).await;
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

    let (vfs, _server) = connect(tmp.path()).await;
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

#[tokio::test]
async fn real_sftp_recursive_remove_spares_a_symlink_target() {
    if std::env::var("CAIRN_IT_SFTP").is_err() {
        eprintln!("skipping: set CAIRN_IT_SFTP=1 to run");
        return;
    }
    if sftp_server_bin().is_none() {
        eprintln!("skipping: no sftp-server binary found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // `d/` contains a symlink to an external directory; deleting `d/` must unlink the symlink and
    // leave the target (and its file) intact — never follow the link and delete outside the tree.
    std::fs::create_dir_all(tmp.path().join("d")).unwrap();
    std::fs::write(tmp.path().join("d/own.txt"), b"o").unwrap();
    std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
    std::fs::write(tmp.path().join("outside/precious.txt"), b"p").unwrap();
    std::os::unix::fs::symlink(tmp.path().join("outside"), tmp.path().join("d/link")).unwrap();

    let (vfs, _server) = connect(tmp.path()).await;
    let root = VfsPath::parse(&format!("{}/d", tmp.path().to_str().unwrap())).unwrap();
    vfs.remove(&root, Recurse::Yes)
        .await
        .expect("recursive remove");
    assert!(!tmp.path().join("d").exists(), "the deleted tree is gone");
    assert!(
        tmp.path().join("outside/precious.txt").exists(),
        "a recursive delete followed a symlink and destroyed data outside the tree"
    );
}

/// A large file must round-trip through the *streaming* read/write paths against a real server:
/// many `write_chunk`s over `russh-sftp`'s pipelined `WRITE` window (32 MiB is well past several
/// `max_concurrent_writes × max_packet_len` windows), a `CLOSE` whose status is now surfaced, and a
/// read that is served packet by packet. The mock proves the *shape* of the calls; this proves the
/// real transport agrees on the bytes.
#[tokio::test]
async fn real_sftp_streams_a_large_file_both_ways() {
    if std::env::var("CAIRN_IT_SFTP").is_err() {
        eprintln!("skipping: set CAIRN_IT_SFTP=1 to run");
        return;
    }
    if sftp_server_bin().is_none() {
        eprintln!("skipping: no sftp-server binary found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (vfs, _server) = connect(tmp.path()).await;
    let remote = VfsPath::parse(&format!("{}/big.bin", tmp.path().to_str().unwrap())).unwrap();

    // 32 MiB of a cheap non-repeating pattern (so a misordered chunk would change the digest).
    const CHUNK: usize = 1 << 20;
    const CHUNKS: usize = 32;
    let mut expect = std::collections::hash_map::DefaultHasher::new();
    let mut wh = vfs.open_write(&remote, WriteOpts::default()).await.unwrap();
    for i in 0..CHUNKS {
        let chunk: Vec<u8> = (0..CHUNK).map(|j| ((i * 31 + j) % 253) as u8).collect();
        std::hash::Hasher::write(&mut expect, &chunk);
        wh.write_chunk(bytes::Bytes::from(chunk)).await.unwrap();
    }
    let entry = wh.finish().await.expect("finish surfaces the CLOSE status");
    assert_eq!(entry.size, Some((CHUNK * CHUNKS) as u64));
    assert_eq!(
        std::fs::metadata(tmp.path().join("big.bin")).unwrap().len(),
        (CHUNK * CHUNKS) as u64,
        "the server-side file is complete once finish() returns"
    );

    let mut rh = vfs.open_read(&remote, None).await.unwrap();
    assert_eq!(rh.len_hint(), Some((CHUNK * CHUNKS) as u64));
    let mut got = std::collections::hash_map::DefaultHasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0usize;
    loop {
        let n = rh.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        std::hash::Hasher::write(&mut got, &buf[..n]);
        total += n;
    }
    assert_eq!(total, CHUNK * CHUNKS);
    assert_eq!(
        std::hash::Hasher::finish(&got),
        std::hash::Hasher::finish(&expect),
        "bytes read back differ from bytes written"
    );
}

/// Aborting a write mid-stream must leave nothing on a real server: the hidden `.part` temp is
/// created at open, the handle is dropped (best-effort CLOSE) and the temp removed — confirms
/// `sftp-server` accepts the REMOVE for a path whose handle was just released. The target itself is
/// never created.
#[tokio::test]
async fn real_sftp_abort_mid_write_leaves_no_file() {
    if std::env::var("CAIRN_IT_SFTP").is_err() {
        eprintln!("skipping: set CAIRN_IT_SFTP=1 to run");
        return;
    }
    if sftp_server_bin().is_none() {
        eprintln!("skipping: no sftp-server binary found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (vfs, _server) = connect(tmp.path()).await;
    let remote = VfsPath::parse(&format!("{}/partial.bin", tmp.path().to_str().unwrap())).unwrap();

    let names = |dir: &std::path::Path| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };

    let mut wh = vfs
        .open_write(
            &remote,
            WriteOpts {
                overwrite: true,
                size_hint: None,
            },
        )
        .await
        .unwrap();
    let during = names(tmp.path());
    assert_eq!(
        during.len(),
        1,
        "exactly one temp during the write: {during:?}"
    );
    assert!(
        during[0].starts_with(".partial.bin.cairn-") && during[0].ends_with(".part"),
        "the real server creates the temp at open: {during:?}"
    );
    wh.write_chunk(bytes::Bytes::from(vec![9u8; 3 << 20]))
        .await
        .unwrap();
    wh.abort().await;
    assert!(
        names(tmp.path()).is_empty(),
        "abort left something behind: {:?}",
        names(tmp.path())
    );
}

/// Overwriting an existing remote file: the original must survive a cancel (abort) untouched, and a
/// completed write must replace it — against the real server's rename-refuses-existing semantics.
#[tokio::test]
async fn real_sftp_overwrite_keeps_the_original_until_commit() {
    if std::env::var("CAIRN_IT_SFTP").is_err() {
        eprintln!("skipping: set CAIRN_IT_SFTP=1 to run");
        return;
    }
    if sftp_server_bin().is_none() {
        eprintln!("skipping: no sftp-server binary found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("keep.txt"), b"original").unwrap();
    let (vfs, _server) = connect(tmp.path()).await;
    let remote = VfsPath::parse(&format!("{}/keep.txt", tmp.path().to_str().unwrap())).unwrap();
    let opts = WriteOpts {
        overwrite: true,
        size_hint: None,
    };

    // Cancelled overwrite → original intact, no temp.
    let mut wh = vfs.open_write(&remote, opts.clone()).await.unwrap();
    wh.write_chunk(bytes::Bytes::from_static(b"new-"))
        .await
        .unwrap();
    wh.abort().await;
    assert_eq!(
        std::fs::read(tmp.path().join("keep.txt")).unwrap(),
        b"original"
    );
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);

    // Completed overwrite → replaced, no temp, no rename backup.
    let mut wh = vfs.open_write(&remote, opts).await.unwrap();
    wh.write_chunk(bytes::Bytes::from_static(b"replaced"))
        .await
        .unwrap();
    assert_eq!(wh.finish().await.unwrap().size, Some(8));
    assert_eq!(
        std::fs::read(tmp.path().join("keep.txt")).unwrap(),
        b"replaced"
    );
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
}
