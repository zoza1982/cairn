//! Live S3 round-trip against MinIO (or a real S3-compatible endpoint).
//!
//! **Env-guarded:** this test is a no-op unless `CAIRN_IT_S3` is set, so the default `cargo test`
//! (and the hermetic CI in `ci.yml`) never touch the network. The dedicated integration job
//! (`.github/workflows/integration.yml`) spins up MinIO, creates the bucket, and sets the
//! `CAIRN_IT_S3_*` variables before running this. See ADR-0006 and the depth-vs-breadth contract.
//!
//! It exercises the full `ObjectStore` surface through the `Vfs` mapping: PUT (incl. nested key) →
//! LIST (file + synthesized directory) → STAT (object + prefix) → GET (full + ranged) → server COPY
//! → DELETE → not-found-after-delete.
#![cfg(feature = "s3")]

use cairn_backend_object::{s3_connect, S3ConnectParams};
use cairn_types::{ConnectionId, VfsPath};
use cairn_vault::{AwsCredential, CredentialSecret, SecretString};
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
async fn s3_round_trip_against_minio() {
    if env("CAIRN_IT_S3").is_none() {
        eprintln!("CAIRN_IT_S3 unset — skipping live S3 integration test");
        return;
    }

    let bucket = env("CAIRN_IT_S3_BUCKET").expect("CAIRN_IT_S3_BUCKET must be set");
    // `for_compat` sets the endpoint + path-style addressing MinIO needs; fall back to plain `new`
    // if no endpoint is given (e.g. pointing the test at real S3).
    let mut params = match env("CAIRN_IT_S3_ENDPOINT") {
        Some(ep) => S3ConnectParams::for_compat(bucket, ep),
        None => S3ConnectParams::new(bucket),
    };
    params.region = env("CAIRN_IT_S3_REGION");

    let cred = CredentialSecret::Aws(AwsCredential::Static {
        access_key_id: env("CAIRN_IT_S3_ACCESS_KEY").expect("CAIRN_IT_S3_ACCESS_KEY must be set"),
        secret_access_key: SecretString::from(
            env("CAIRN_IT_S3_SECRET_KEY").expect("CAIRN_IT_S3_SECRET_KEY must be set"),
        ),
        session_token: None,
    });

    // Unique root prefix per run so concurrent/leftover runs don't collide.
    let root = format!("it/{}", std::process::id());
    let vfs = s3_connect(ConnectionId(1), &params, &cred, &root)
        .await
        .expect("s3_connect should succeed against MinIO");

    // PUT: a top-level object and a nested one (the nested key synthesizes a directory on LIST).
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
        assert_eq!(e.size, Some(body.len() as u64), "PUT {path} size");
    }

    // LIST root: `a.txt` (file) and `d` (directory synthesized from the common prefix).
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
        "expected file a.txt in {names:?}"
    );
    assert!(
        names.iter().any(|(n, dir)| n == "d" && *dir),
        "expected dir d in {names:?}"
    );

    // STAT: an object (head) and a prefix (directory probe).
    assert_eq!(vfs.stat(&p("/a.txt")).await.unwrap().size, Some(5));
    assert!(vfs.stat(&p("/d")).await.unwrap().is_dir());

    // GET full, then a ranged GET.
    let mut rh = vfs.open_read(&p("/a.txt"), None).await.unwrap();
    let mut buf = String::new();
    rh.read_to_string(&mut buf).await.unwrap();
    assert_eq!(buf, "hello");

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
    assert_eq!(buf, "ell", "ranged GET bytes=1-3");

    // Server-side COPY, then confirm the copy is readable.
    vfs.copy_within(&p("/a.txt"), &p("/copy.txt"))
        .await
        .unwrap();
    assert_eq!(vfs.stat(&p("/copy.txt")).await.unwrap().size, Some(5));

    // DELETE then confirm the object is gone.
    vfs.remove(&p("/a.txt"), Recurse::No).await.unwrap();
    assert!(
        matches!(vfs.stat(&p("/a.txt")).await, Err(VfsError::NotFound(_))),
        "deleted object must read as NotFound"
    );

    // Best-effort cleanup of the run's objects.
    let _ = vfs.remove(&p("/copy.txt"), Recurse::No).await;
    let _ = vfs.remove(&p("/d/b.txt"), Recurse::No).await;
}
