//! A minimal in-memory backend-plugin component fixture for the host's plugin-VFS tests.
//!
//! It serves a single directory `/dir` containing one file `/dir/a.txt` (contents `b"hello"`).
//! `open-read` streams that file's bytes (honoring an optional byte range); `open-write` accepts a
//! streamed write under `/dir/` and `finish` reports the accumulated size; `create-dir`/`remove`/
//! `rename` exercise the mutation plumbing (including the `recursive` flag and distinct error
//! mappings) without a real filesystem.
//!
//! Two extra read paths exercise the host's defenses against a *misbehaving* guest (the host treats
//! every plugin as untrusted): `/dir/infinite` is a read stream that never reports EOF, and
//! `/dir/oversized` returns more bytes than the host requested. Built locally
//! (`cargo component build`) and committed as `../fixtures/backend.wasm` so CI needs no WASM
//! toolchain.

#[allow(warnings)]
mod bindings;

use std::cell::{Cell, RefCell};

use bindings::cairn::plugin::types::{ByteRange, Caps, Entry, EntryKind, VfsError};
use bindings::exports::cairn::plugin::backend::{
    Guest, GuestReadStream, GuestWriteSink, ListPageResult, ReadStream, WriteSink,
};

const FILE_BODY: &[u8] = b"hello";

struct Fixture;

/// A write sink that accumulates streamed bytes in memory; `finish` reports the byte count.
struct MemWriteSink {
    name: String,
    buf: RefCell<Vec<u8>>,
}

/// What a [`MemReadStream`] yields. `Bytes` is the well-behaved case; the others are deliberately
/// hostile, to exercise the host's stream defenses.
enum Mode {
    /// A finite buffer, served in `max_bytes`-sized chunks.
    Bytes(Vec<u8>),
    /// Never reports EOF — returns `max_bytes` filler bytes on every call.
    Infinite,
    /// Returns one byte *more* than the host requested (a `<= max_bytes` contract violation).
    Oversized,
}

/// A read stream. `Cell` gives interior mutability for the cursor (the WIT resource methods take
/// `&self`); the component model runs the guest single-threaded, so this is sound.
struct MemReadStream {
    mode: Mode,
    pos: Cell<usize>,
}

impl GuestReadStream for MemReadStream {
    fn read_chunk(&self, max_bytes: u32) -> Result<Vec<u8>, VfsError> {
        match &self.mode {
            Mode::Bytes(bytes) => {
                let pos = self.pos.get();
                let end = pos.saturating_add(max_bytes as usize).min(bytes.len());
                let chunk = bytes[pos..end].to_vec();
                self.pos.set(end);
                Ok(chunk)
            }
            Mode::Infinite => Ok(vec![b'x'; max_bytes as usize]),
            Mode::Oversized => Ok(vec![b'x'; max_bytes as usize + 1]),
        }
    }
    fn close(&self) {}
}

impl GuestWriteSink for MemWriteSink {
    fn write_chunk(&self, chunk: Vec<u8>) -> Result<(), VfsError> {
        self.buf.borrow_mut().extend_from_slice(&chunk);
        Ok(())
    }
    fn finish(&self) -> Result<Entry, VfsError> {
        Ok(Entry {
            name: self.name.clone(),
            kind: EntryKind::File,
            size: Some(self.buf.borrow().len() as u64),
            modified_secs: None,
            etag: None,
            symlink_target: None,
        })
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
    type ReadStream = MemReadStream;
    type WriteSink = MemWriteSink;

    fn scheme() -> String {
        "fixture".to_string()
    }

    fn backend_caps() -> Caps {
        Caps::LIST_DIR | Caps::READ | Caps::WRITE | Caps::CREATE_DIR | Caps::DELETE | Caps::RENAME
    }

    fn caps_at(_path: String) -> Caps {
        Caps::LIST_DIR | Caps::READ | Caps::WRITE | Caps::CREATE_DIR | Caps::DELETE | Caps::RENAME
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

    fn open_read(path: String, range: Option<ByteRange>) -> Result<ReadStream, VfsError> {
        let mode = match path.as_str() {
            "/dir/a.txt" => {
                // Apply an optional byte range, clamping to the file's bounds.
                let bytes = match range {
                    None => FILE_BODY.to_vec(),
                    Some(ByteRange { offset, len }) => {
                        let start = (offset as usize).min(FILE_BODY.len());
                        let end = match len {
                            None => FILE_BODY.len(),
                            Some(n) => start.saturating_add(n as usize).min(FILE_BODY.len()),
                        };
                        FILE_BODY[start..end].to_vec()
                    }
                };
                Mode::Bytes(bytes)
            }
            "/dir/infinite" => Mode::Infinite,
            "/dir/oversized" => Mode::Oversized,
            _ => return Err(VfsError::NotFound(path)),
        };
        Ok(ReadStream::new(MemReadStream {
            mode,
            pos: Cell::new(0),
        }))
    }

    fn open_write(
        path: String,
        _overwrite: bool,
        _size_hint: Option<u64>,
    ) -> Result<WriteSink, VfsError> {
        // Accept a write to a leaf directly under `/dir`.
        match path.strip_prefix("/dir/") {
            // Hostile case: `finish` reports a traversal name, to prove the host rejects it.
            Some("evilname") => Ok(WriteSink::new(MemWriteSink {
                name: "../evil".to_string(),
                buf: RefCell::new(Vec::new()),
            })),
            Some(leaf) if !leaf.is_empty() && !leaf.contains('/') => Ok(WriteSink::new(MemWriteSink {
                name: leaf.to_string(),
                buf: RefCell::new(Vec::new()),
            })),
            _ => Err(VfsError::NotFound(path)),
        }
    }

    fn create_dir(path: String) -> Result<(), VfsError> {
        match path.as_str() {
            "/dir" => Err(VfsError::AlreadyExists(path)), // distinct error mapping
            p if p.starts_with("/dir/") => Ok(()),
            _ => Err(VfsError::NotFound(path)),
        }
    }

    fn remove(path: String, recursive: bool) -> Result<(), VfsError> {
        match path.as_str() {
            "/dir/a.txt" => Ok(()),
            // A non-empty dir needs `recursive` — proves the flag is threaded through.
            "/dir" if recursive => Ok(()),
            "/dir" => Err(VfsError::Conflict),
            _ => Err(VfsError::NotFound(path)),
        }
    }

    fn rename(src: String, _dst: String) -> Result<(), VfsError> {
        match src.as_str() {
            "/dir/a.txt" => Ok(()),
            _ => Err(VfsError::NotFound(src)),
        }
    }
}

bindings::export!(Fixture with_types_in bindings);
