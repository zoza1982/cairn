//! M8-3b PR1: drive a plugin component through the `Vfs` trait via [`PluginVfsBackend`], against the
//! committed guest fixture (no WASM toolchain needed in CI).

use cairn_plugin::{engine_config, Limits, PluginComponent, PluginVfsBackend};
use cairn_types::{Caps, ConnectionId, EntryKind, Scheme, VfsPath};
use cairn_vfs::{CapabilityProvider, ListOpts, Vfs, VfsError};
use futures::StreamExt;
use wasmtime::Engine;

const FIXTURE: &[u8] = include_bytes!("fixtures/backend.wasm");

fn backend() -> PluginVfsBackend {
    let engine = Engine::new(&engine_config()).unwrap();
    let comp = PluginComponent::instantiate(&engine, FIXTURE, Limits::default()).unwrap();
    PluginVfsBackend::new(comp, Limits::default(), ConnectionId(1)).unwrap()
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
    assert!(!b.caps().contains(Caps::WRITE));
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
async fn open_read_and_write_are_unsupported_for_now() {
    // Deferred to M8-3b PR2/PR3; must report Unsupported, not panic.
    let b = backend();
    assert!(matches!(
        b.open_read(&p("/dir/a.txt"), None).await,
        Err(VfsError::Unsupported(_))
    ));
}

#[tokio::test]
async fn usable_as_arc_dyn_vfs() {
    // The whole point: a plugin backend is indistinguishable from a built-in one.
    let b: std::sync::Arc<dyn Vfs> = std::sync::Arc::new(backend());
    assert_eq!(b.stat(&p("/dir/a.txt")).await.unwrap().name, "a.txt");
}
