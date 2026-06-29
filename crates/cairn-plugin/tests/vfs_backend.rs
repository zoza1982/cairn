//! Drive a plugin component through the `Vfs` trait via [`PluginVfsBackend`], against the committed
//! guest fixture (no WASM toolchain needed in CI). Covers the full contract — metadata, listing,
//! streaming reads and writes, and mutations — plus the host's defenses against a misbehaving guest.

use cairn_plugin::{engine_config, Limits, PluginComponent, PluginVfsBackend};
use cairn_types::{Caps, ConnectionId, EntryKind, Scheme, VfsPath};
use cairn_vfs::{ByteRange, CapabilityProvider, ListOpts, Recurse, Vfs, VfsError};
use futures::StreamExt;
use tokio::io::AsyncReadExt;
use wasmtime::Engine;

const FIXTURE: &[u8] = include_bytes!("fixtures/backend.wasm");

fn backend() -> PluginVfsBackend {
    backend_with(Limits::default())
}

fn backend_with(limits: Limits) -> PluginVfsBackend {
    let engine = Engine::new(&engine_config()).unwrap();
    let comp = PluginComponent::instantiate(&engine, FIXTURE, limits).unwrap();
    PluginVfsBackend::new(comp, limits, ConnectionId(1)).unwrap()
}

fn p(s: &str) -> VfsPath {
    VfsPath::parse(s).unwrap()
}

#[tokio::test]
async fn scheme_connection_and_caps_are_reported() {
    let b = backend();
    assert_eq!(b.scheme(), Scheme::Plugin("fixture".into()));
    assert_eq!(b.connection(), ConnectionId(1));
    assert!(b.caps().contains(Caps::LIST) && b.caps().contains(Caps::READ));
    assert!(b.caps().contains(Caps::WRITE) && b.caps().contains(Caps::DELETE));
}

#[tokio::test]
async fn stat_via_vfs() {
    let b = backend();
    let entry = b.stat(&p("/dir/a.txt")).await.unwrap();
    assert_eq!(entry.name, "a.txt");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, Some(5));
}

#[tokio::test]
async fn stat_missing_maps_to_not_found() {
    let b = backend();
    assert!(matches!(
        b.stat(&p("/nope")).await,
        Err(VfsError::NotFound(_))
    ));
}

#[tokio::test]
async fn list_via_vfs_streams_entries() {
    let b = backend();
    let mut pages = b.list(&p("/dir"), ListOpts::default());
    let page = pages.next().await.expect("a page").expect("ok");
    assert!(page.done);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].name, "a.txt");
    assert!(pages.next().await.is_none(), "single page, then end");
}

#[tokio::test]
async fn list_missing_dir_yields_an_error_then_ends() {
    let b = backend();
    let mut pages = b.list(&p("/missing"), ListOpts::default());
    assert!(matches!(
        pages.next().await,
        Some(Err(VfsError::NotFound(_)))
    ));
    assert!(pages.next().await.is_none(), "error terminates the stream");
}

#[tokio::test]
async fn open_read_streams_file_contents() {
    let b = backend();
    let mut h = b.open_read(&p("/dir/a.txt"), None).await.expect("open");
    let mut out = Vec::new();
    h.read_to_end(&mut out).await.expect("read");
    assert_eq!(out, b"hello");
}

#[tokio::test]
async fn open_read_honors_a_byte_range() {
    let b = backend();
    let range = ByteRange {
        offset: 1,
        len: Some(3),
    };
    let mut h = b
        .open_read(&p("/dir/a.txt"), Some(range))
        .await
        .expect("open");
    let mut out = Vec::new();
    h.read_to_end(&mut out).await.expect("read");
    assert_eq!(out, b"ell");
}

#[tokio::test]
async fn open_read_missing_maps_to_not_found() {
    let b = backend();
    assert!(matches!(
        b.open_read(&p("/nope"), None).await,
        Err(VfsError::NotFound(_))
    ));
}

#[tokio::test]
async fn read_stream_over_the_byte_cap_is_cut_off() {
    // A guest whose stream never reports EOF must not drive the consumer to exhaust memory: the host
    // errors once `max_stream_bytes` is exceeded. Use a tiny cap so the test stays cheap.
    let limits = Limits {
        max_stream_bytes: 4096,
        ..Limits::default()
    };
    let b = backend_with(limits);
    let mut h = b.open_read(&p("/dir/infinite"), None).await.expect("open");
    let mut out = Vec::new();
    let err = h.read_to_end(&mut out).await.expect_err("must be cut off");
    assert_eq!(err.kind(), std::io::ErrorKind::Other);
    assert!(err.to_string().contains("stream exceeded"), "got: {err}");
}

#[tokio::test]
async fn read_chunk_larger_than_requested_is_rejected() {
    // A guest that returns more bytes than the host asked for violates the `<= max_bytes` contract
    // and is rejected rather than buffered (host-allocation defense).
    let b = backend();
    let mut h = b.open_read(&p("/dir/oversized"), None).await.expect("open");
    let mut out = Vec::new();
    assert!(
        h.read_to_end(&mut out).await.is_err(),
        "oversized chunk rejected"
    );
}

#[tokio::test]
async fn a_spinning_guest_call_hits_the_wall_clock_deadline() {
    // End-to-end: a guest that spins inside a call is trapped by the epoch deadline (not fuel) and
    // surfaces as a `plugin_timeout` backend error. Huge fuel so the *epoch* wins the race; a tiny
    // tick budget so it fires fast. `tokio::time::timeout` is a safety net: if the deadline somehow
    // never fires the test fails instead of hanging CI.
    let limits = Limits {
        fuel: u64::MAX,
        max_call_ticks: 1,
        ..Limits::default()
    };
    let b = backend_with(limits);
    let mut h = b.open_read(&p("/dir/spin"), None).await.expect("open");
    let mut out = Vec::new();
    let read = tokio::time::timeout(std::time::Duration::from_secs(30), h.read_to_end(&mut out));
    let err = read
        .await
        .expect("epoch deadline must fire well within 30s")
        .expect_err("a spinning guest must error, not return data");
    assert!(
        err.to_string().contains("plugin_timeout"),
        "expected a plugin_timeout error, got: {err}"
    );
}

#[tokio::test]
async fn dropping_a_handle_mid_stream_keeps_the_instance_usable() {
    // Abandoning a read without reaching EOF must free the guest resource (via the handle's Drop)
    // and leave the thread alive for further operations — i.e. no leak, no deadlock.
    let b = backend();
    {
        let mut h = b.open_read(&p("/dir/a.txt"), None).await.expect("open");
        let mut one = [0u8; 1];
        let n = h.read(&mut one).await.expect("read one byte");
        assert_eq!(n, 1);
        // `h` dropped here mid-stream → CloseRead.
    }
    // The instance is still serving: a follow-up op succeeds.
    assert_eq!(b.stat(&p("/dir/a.txt")).await.unwrap().name, "a.txt");
}

#[tokio::test]
async fn open_write_streams_and_finish_reports_size() {
    let b = backend();
    let mut h = b
        .open_write(&p("/dir/new.txt"), cairn_vfs::WriteOpts::default())
        .await
        .expect("open_write");
    h.write_chunk(bytes::Bytes::from_static(b"abc"))
        .await
        .unwrap();
    h.write_chunk(bytes::Bytes::from_static(b"defg"))
        .await
        .unwrap();
    let entry = h.finish().await.expect("finish");
    assert_eq!(entry.name, "new.txt");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, Some(7));
}

#[tokio::test]
async fn finish_with_a_traversal_name_is_rejected() {
    // A malicious guest whose `finish` returns an entry name like "../evil" must not pass through —
    // the host validates the leaf name (traversal / terminal-injection defense), as `list` does.
    let b = backend();
    let mut h = b
        .open_write(&p("/dir/evilname"), cairn_vfs::WriteOpts::default())
        .await
        .expect("open_write");
    h.write_chunk(bytes::Bytes::from_static(b"x"))
        .await
        .unwrap();
    assert!(matches!(h.finish().await, Err(VfsError::Backend { .. })));
}

#[tokio::test]
async fn open_write_missing_parent_maps_to_not_found() {
    let b = backend();
    assert!(matches!(
        b.open_write(&p("/nope/x"), cairn_vfs::WriteOpts::default())
            .await,
        Err(VfsError::NotFound(_))
    ));
}

#[tokio::test]
async fn aborting_a_write_keeps_the_instance_usable() {
    // Drop a write handle without finishing → the sink is aborted, not committed; the thread lives.
    let b = backend();
    {
        let mut h = b
            .open_write(&p("/dir/tmp"), cairn_vfs::WriteOpts::default())
            .await
            .expect("open_write");
        h.write_chunk(bytes::Bytes::from_static(b"partial"))
            .await
            .unwrap();
        // `h` dropped here without finish → AbortWrite.
    }
    assert_eq!(b.stat(&p("/dir/a.txt")).await.unwrap().name, "a.txt");
}

#[tokio::test]
async fn create_dir_maps_ok_and_already_exists() {
    let b = backend();
    assert!(b.create_dir(&p("/dir/sub")).await.is_ok());
    assert!(matches!(
        b.create_dir(&p("/dir")).await,
        Err(VfsError::AlreadyExists(_))
    ));
}

#[tokio::test]
async fn remove_threads_the_recursive_flag() {
    let b = backend();
    // A non-empty dir is a Conflict without recursion, Ok with it.
    assert!(matches!(
        b.remove(&p("/dir"), Recurse::No).await,
        Err(VfsError::Conflict)
    ));
    assert!(b.remove(&p("/dir"), Recurse::Yes).await.is_ok());
    assert!(b.remove(&p("/dir/a.txt"), Recurse::No).await.is_ok());
}

#[tokio::test]
async fn rename_maps_ok_and_not_found() {
    let b = backend();
    assert!(b.rename(&p("/dir/a.txt"), &p("/dir/b.txt")).await.is_ok());
    assert!(matches!(
        b.rename(&p("/dir/missing"), &p("/dir/x")).await,
        Err(VfsError::NotFound(_))
    ));
}

#[tokio::test]
async fn usable_as_arc_dyn_vfs() {
    // The whole point: a plugin backend is indistinguishable from a built-in one.
    let b: std::sync::Arc<dyn Vfs> = std::sync::Arc::new(backend());
    assert_eq!(b.stat(&p("/dir/a.txt")).await.unwrap().name, "a.txt");
}
