//! Live Azure Blob Storage round-trip against Azurite (or a real Azure endpoint).
//!
//! **Env-guarded:** this test is a no-op unless `CAIRN_IT_AZURE` is set, so the default
//! `cargo test` (and the hermetic CI in `ci.yml`) never touch the network. A dedicated integration
//! job spins up Azurite, creates the container, and sets the `CAIRN_IT_AZURE_*` variables before
//! running this. See ADR-0006 and the depth-vs-breadth contract.
//!
//! It exercises the full `ObjectStore` surface through the `Vfs` mapping: PUT (top-level and
//! nested key) → LIST (file + synthesised directory) → STAT (object + prefix) → GET (full + ranged)
//! → server COPY (Copy Blob From URL, synchronous) → DELETE → not-found-after-delete.
#![cfg(feature = "azure")]

use cairn_backend_object::{azure_connect, AzureConnectParams};
use cairn_types::{ConnectionId, VfsPath};
use cairn_vault::{AzureCredential, CredentialSecret, SecretString};
use cairn_vfs::{ByteRange, ListOpts, Recurse, Vfs, VfsError, WriteOpts};
use futures::StreamExt;
use tokio::io::AsyncReadExt;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

fn p(s: &str) -> VfsPath {
    VfsPath::parse(s).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_round_trip_against_azurite() {
    if env("CAIRN_IT_AZURE").is_none() {
        eprintln!("CAIRN_IT_AZURE unset — skipping live Azure integration test");
        return;
    }

    let account = env("CAIRN_IT_AZURE_ACCOUNT")
        .expect("CAIRN_IT_AZURE_ACCOUNT must be set when CAIRN_IT_AZURE is set");
    let key = env("CAIRN_IT_AZURE_KEY")
        .expect("CAIRN_IT_AZURE_KEY must be set when CAIRN_IT_AZURE is set");
    let container = env("CAIRN_IT_AZURE_CONTAINER")
        .expect("CAIRN_IT_AZURE_CONTAINER must be set when CAIRN_IT_AZURE is set");

    // `for_emulator` sets the custom endpoint; fall back to `new` for real Azure (no endpoint).
    let params = match env("CAIRN_IT_AZURE_ENDPOINT") {
        Some(ep) => AzureConnectParams::for_emulator(&account, &container, ep),
        None => AzureConnectParams::new(&account, &container),
    };

    let cred = CredentialSecret::Azure(AzureCredential::SharedKey {
        account: account.clone(),
        key: SecretString::from(key),
    });

    // Unique root prefix per run so concurrent/leftover runs don't collide.
    let root = format!("it/{}", std::process::id());
    let vfs = azure_connect(ConnectionId(1), &params, &cred, &root)
        .await
        .expect("azure_connect should succeed against Azurite");

    // PUT: a top-level object and a nested one (the nested key synthesises a directory on LIST).
    for (path, body) in [
        ("/a.txt", b"hello".as_slice()),
        ("/d/b.txt", b"world!!".as_slice()),
    ] {
        let mut wh = vfs
            .open_write(&p(path), WriteOpts::default())
            .await
            .unwrap_or_else(|e| panic!("open_write {path}: {e}"));
        wh.write_chunk(bytes::Bytes::copy_from_slice(body))
            .await
            .unwrap_or_else(|e| panic!("write_chunk {path}: {e}"));
        let e = wh
            .finish()
            .await
            .unwrap_or_else(|e| panic!("PUT {path}: {e}"));
        assert_eq!(e.size, Some(body.len() as u64), "PUT {path} size mismatch");
    }

    // LIST root: `a.txt` (file) and `d` (directory synthesised from the common prefix).
    let mut names = Vec::new();
    let mut s = vfs.list(&p("/"), ListOpts::default());
    while let Some(page) = s.next().await {
        for e in page.unwrap().entries {
            names.push((e.name.to_string(), e.is_dir()));
        }
    }
    names.sort();
    assert!(
        names.iter().any(|(n, dir)| n == "a.txt" && !dir),
        "expected file a.txt in listing {names:?}"
    );
    assert!(
        names.iter().any(|(n, dir)| n == "d" && *dir),
        "expected synthesised dir d in listing {names:?}"
    );

    // STAT: an object (head) and a prefix (directory probe).
    assert_eq!(
        vfs.stat(&p("/a.txt")).await.unwrap().size,
        Some(5),
        "a.txt must be 5 bytes"
    );
    assert!(
        vfs.stat(&p("/d")).await.unwrap().is_dir(),
        "d must stat as a directory"
    );

    // GET full content.
    let mut rh = vfs.open_read(&p("/a.txt"), None).await.unwrap();
    let mut buf = String::new();
    rh.read_to_string(&mut buf).await.unwrap();
    assert_eq!(buf, "hello", "full GET must return exact content");

    // GET with a byte range (bytes 1–3 of "hello" → "ell").
    let mut rh = vfs
        .open_read(
            &p("/a.txt"),
            Some(ByteRange {
                offset: 1,
                len: Some(3),
            }),
        )
        .await
        .unwrap();
    let mut buf = String::new();
    rh.read_to_string(&mut buf).await.unwrap();
    assert_eq!(buf, "ell", "ranged GET bytes=1+3 must return 'ell'");

    // Zero-byte blob round-trip: the SDK always sends a range header, so a full read of an empty
    // blob would 416 unless the adapter treats that as empty. Empty files are common.
    {
        let wh = vfs
            .open_write(&p("/empty.txt"), WriteOpts::default())
            .await
            .unwrap_or_else(|e| panic!("open_write empty.txt: {e}"));
        let e = wh
            .finish()
            .await
            .unwrap_or_else(|e| panic!("PUT empty.txt: {e}"));
        assert_eq!(e.size, Some(0), "empty PUT size");
        let mut rh = vfs
            .open_read(&p("/empty.txt"), None)
            .await
            .unwrap_or_else(|e| panic!("GET empty.txt: {e}"));
        let mut empty = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut rh, &mut empty)
            .await
            .unwrap();
        assert!(
            empty.is_empty(),
            "full GET of a zero-byte blob must return []"
        );
        let _ = vfs.remove(&p("/empty.txt"), Recurse::No).await;
    }

    // Server-side COPY, then confirm the copy is readable and has the same size.
    vfs.copy_within(&p("/a.txt"), &p("/copy.txt"))
        .await
        .unwrap_or_else(|e| panic!("server copy a.txt → copy.txt: {e}"));
    assert_eq!(
        vfs.stat(&p("/copy.txt")).await.unwrap().size,
        Some(5),
        "copied blob must be 5 bytes"
    );

    // DELETE then confirm the object is gone (NotFound).
    vfs.remove(&p("/a.txt"), Recurse::No)
        .await
        .unwrap_or_else(|e| panic!("delete a.txt: {e}"));
    assert!(
        matches!(vfs.stat(&p("/a.txt")).await, Err(VfsError::NotFound(_))),
        "deleted object must read as NotFound"
    );

    // Best-effort cleanup of the run's objects — ignore errors so a partial cleanup doesn't
    // mask earlier assertion failures.
    let _ = vfs.remove(&p("/copy.txt"), Recurse::No).await;
    let _ = vfs.remove(&p("/d/b.txt"), Recurse::No).await;
}
