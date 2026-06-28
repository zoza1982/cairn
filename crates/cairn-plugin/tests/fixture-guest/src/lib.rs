//! A minimal in-memory backend-plugin component fixture for the host's M8-3a tests.
//!
//! It serves a single directory `/dir` containing one file `/dir/a.txt`. It exports only the
//! non-streaming `backend` calls meaningfully; `open-read`/`open-write`/mutations return
//! `unsupported`. Built locally (`cargo component build`) and committed as `../fixtures/backend.wasm`
//! so CI needs no WASM toolchain.

#[allow(warnings)]
mod bindings;

use bindings::cairn::plugin::types::{ByteRange, Caps, Entry, EntryKind, VfsError};
use bindings::exports::cairn::plugin::backend::{
    Guest, GuestReadStream, GuestWriteSink, ListPageResult, ReadStream, WriteSink,
};

struct Fixture;

// The WIT declares `read-stream`/`write-sink` resources; the guest must define their types even
// though `open-read`/`open-write` never construct one (they return `unsupported`).
struct StubReadStream;
struct StubWriteSink;

impl GuestReadStream for StubReadStream {
    fn read_chunk(&self, _max_bytes: u32) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }
    fn close(&self) {}
}

impl GuestWriteSink for StubWriteSink {
    fn write_chunk(&self, _chunk: Vec<u8>) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }
    fn finish(&self) -> Result<Entry, VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }
    fn abort(&self) {}
}

fn file_entry() -> Entry {
    Entry {
        name: "a.txt".to_string(),
        kind: EntryKind::File,
        size: Some(5),
        modified_secs: None,
        etag: None,
        symlink_target: None,
    }
}

impl Guest for Fixture {
    type ReadStream = StubReadStream;
    type WriteSink = StubWriteSink;

    fn scheme() -> String {
        "fixture".to_string()
    }

    fn backend_caps() -> Caps {
        Caps::LIST_DIR | Caps::READ
    }

    fn caps_at(_path: String) -> Caps {
        Caps::LIST_DIR | Caps::READ
    }

    fn list_page(
        dir: String,
        _cursor: Option<String>,
        _include_hidden: bool,
    ) -> Result<ListPageResult, VfsError> {
        match dir.as_str() {
            "/dir" => Ok(ListPageResult {
                entries: vec![file_entry()],
                cursor: None,
                done: true,
            }),
            other => Err(VfsError::NotFound(other.to_string())),
        }
    }

    fn stat(path: String) -> Result<Entry, VfsError> {
        match path.as_str() {
            "/dir" => Ok(Entry {
                name: "dir".to_string(),
                kind: EntryKind::Dir,
                size: None,
                modified_secs: None,
                etag: None,
                symlink_target: None,
            }),
            "/dir/a.txt" => Ok(file_entry()),
            other => Err(VfsError::NotFound(other.to_string())),
        }
    }

    fn open_read(_path: String, _range: Option<ByteRange>) -> Result<ReadStream, VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }

    fn open_write(
        _path: String,
        _overwrite: bool,
        _size_hint: Option<u64>,
    ) -> Result<WriteSink, VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }

    fn create_dir(_path: String) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }

    fn remove(_path: String, _recursive: bool) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }

    fn rename(_src: String, _dst: String) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(Caps::empty()))
    }
}

bindings::export!(Fixture with_types_in bindings);
