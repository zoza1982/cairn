//! Live AWS S3 adapter and S3-compatible store connector (MinIO, Cloudflare R2, Backblaze B2).
//!
//! Gated behind the `s3` cargo feature. Credentials always flow through [`cairn_vault`] — never
//! plaintext on disk or in logs. Signed URLs, session tokens, and the secret access key are never
//! included in any [`VfsError`](cairn_vfs::VfsError) message; AWS service error messages are safe
//! because S3/compatible stores never echo credential material back.
//!
//! The public entry point is [`s3_connect`]; [`S3ConnectParams`] configures the target bucket and
//! any S3-compatible overrides; [`S3ObjectStore`] is exported only so the connection registry can
//! name the concrete type — construct it exclusively via [`s3_connect`].

use crate::{ListChunk, ObjectMeta, ObjectStore, ObjectStoreVfs};
use async_trait::async_trait;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use cairn_types::{Caps, ConnectionId, Scheme, VfsPath};
use cairn_vault::{AwsCredential, CredentialSecret, ExposeSecret};
use cairn_vfs::VfsError;
use smol_str::SmolStr;
use std::sync::Arc;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// S3ObjectStore
// ---------------------------------------------------------------------------

/// Live S3 (and S3-compatible) object store backed by the official `aws-sdk-s3` client.
///
/// Construct via [`s3_connect`] rather than directly; the client already holds credentials
/// obtained from the vault and has the correct region/endpoint configured.
pub struct S3ObjectStore {
    client: aws_sdk_s3::Client,
    /// The bucket (or container) name this store operates on.
    bucket: String,
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    fn capabilities(&self) -> Caps {
        // S3 supports random-access reads (Range), server-side copy, and the full CRUD set.
        Caps::LIST | Caps::READ | Caps::WRITE | Caps::DELETE | Caps::COPY_SERVER | Caps::RANDOM_READ
    }

    async fn list_page(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
        max: usize,
    ) -> Result<ListChunk, VfsError> {
        let mut req = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            // Always send prefix even when empty — avoids an inadvertent "no filter" fallback on
            // some S3-compatible stores that treat a missing Prefix differently from "".
            .prefix(prefix)
            // S3 caps MaxKeys at 1000; clamp so a large `usize` can't wrap when narrowed to i32.
            .max_keys(max.min(1000) as i32);
        if let Some(d) = delimiter {
            req = req.delimiter(d);
        }
        if let Some(t) = token {
            req = req.continuation_token(t);
        }
        let out = req.send().await.map_err(map_sdk_err)?;

        let common_prefixes = out
            .common_prefixes()
            .iter()
            .filter_map(|cp| cp.prefix())
            .map(ToOwned::to_owned)
            .collect();

        let objects = out
            .contents()
            .iter()
            .filter_map(|obj| {
                // An object without a key is malformed; skip it rather than panicking.
                let key = obj.key()?.to_owned();
                // size() is Option<i64>. S3 never returns negative sizes; .max(0) guards against
                // malformed responses from some S3-compatible stores.
                let size = obj.size().unwrap_or(0).max(0) as u64;
                let etag = obj.e_tag().map(SmolStr::new);
                let modified = obj
                    .last_modified()
                    .and_then(|dt| SystemTime::try_from(*dt).ok());
                let storage_class = obj.storage_class().map(|sc| SmolStr::new(sc.as_str()));
                Some(ObjectMeta {
                    key,
                    size,
                    etag,
                    modified,
                    storage_class,
                })
            })
            .collect();

        Ok(ListChunk {
            common_prefixes,
            objects,
            // An empty token (some S3-compatible stores emit one when done) means "no more pages";
            // treating it as `Some` would re-list from the start in a loop.
            next_token: out
                .next_continuation_token()
                .filter(|t| !t.is_empty())
                .map(ToOwned::to_owned),
        })
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, VfsError> {
        let out = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(map_sdk_err)?;

        // HeadObject returns content_length as Option<i64>; guard against negatives defensively.
        let size = out.content_length().unwrap_or(0).max(0) as u64;
        let etag = out.e_tag().map(SmolStr::new);
        let modified = out
            .last_modified()
            .and_then(|dt| SystemTime::try_from(*dt).ok());
        let storage_class = out.storage_class().map(|sc| SmolStr::new(sc.as_str()));

        Ok(ObjectMeta {
            key: key.to_owned(),
            size,
            etag,
            modified,
            storage_class,
        })
    }

    async fn get(&self, key: &str, range: Option<(u64, Option<u64>)>) -> Result<Vec<u8>, VfsError> {
        let mut req = self.client.get_object().bucket(&self.bucket).key(key);
        if let Some((offset, len)) = range {
            req = req.range(range_header(offset, len));
        }
        let out = req.send().await.map_err(map_sdk_err)?;
        // `body` is a public field on GetObjectOutput; `.collect()` buffers all streaming chunks
        // into AggregatedBytes and `.to_vec()` copies them into a contiguous Vec<u8>.
        let bytes = out
            .body
            .collect()
            .await
            .map_err(|e| VfsError::Connection(Box::new(e)))?;
        Ok(bytes.to_vec())
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> Result<ObjectMeta, VfsError> {
        // Capture the length before the Vec is moved into ByteStream.
        let size = data.len() as u64;
        let out = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(map_sdk_err)?;

        Ok(ObjectMeta {
            key: key.to_owned(),
            size,
            etag: out.e_tag().map(SmolStr::new),
            // PutObject does not return a timestamp; callers that need the write time must
            // follow with a head() call.
            modified: None,
            storage_class: None,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), VfsError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }

    async fn copy(&self, from: &str, to: &str) -> Result<ObjectMeta, VfsError> {
        // CopyObject returns a CopyObjectResult with only e_tag and last_modified.
        // The destination size is unknowable without a subsequent head_object; the caller
        // (ObjectStoreVfs::copy_within) discards the returned ObjectMeta entirely, so 0 is safe.
        let out = self
            .client
            .copy_object()
            .copy_source(encode_copy_source(&self.bucket, from))
            .bucket(&self.bucket)
            .key(to)
            .send()
            .await
            .map_err(map_sdk_err)?;

        let (etag, modified) = out
            .copy_object_result()
            .map(|r| {
                let etag = r.e_tag().map(SmolStr::new);
                let modified = r
                    .last_modified()
                    .and_then(|dt| SystemTime::try_from(*dt).ok());
                (etag, modified)
            })
            .unwrap_or((None, None));

        Ok(ObjectMeta {
            key: to.to_owned(),
            size: 0, // see note above: size is not returned by CopyObject
            etag,
            modified,
            storage_class: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Connection parameters
// ---------------------------------------------------------------------------

/// Connection parameters for an S3 or S3-compatible bucket.
///
/// Pass to [`s3_connect`] alongside a [`CredentialSecret::Aws`] from the vault. For MinIO set
/// `endpoint` to the MinIO server URL and `force_path_style` to `true`. For genuine AWS S3 leave
/// both at their defaults.
#[derive(Debug, Clone)]
pub struct S3ConnectParams {
    /// The S3 bucket (or container) name.
    pub bucket: String,
    /// AWS region (e.g. `"us-east-1"`). For [`AwsCredential::Static`] defaults to `"us-east-1"`.
    /// For profile/default-chain the SDK resolves it from the environment when `None`.
    pub region: Option<String>,
    /// Custom endpoint URL for S3-compatible stores (MinIO, Cloudflare R2, Backblaze B2, …).
    /// Leave `None` for genuine AWS S3.
    pub endpoint: Option<String>,
    /// Use path-style requests (`http://endpoint/bucket/key`) instead of virtual-hosted-style
    /// (`http://bucket.endpoint/key`). Required for MinIO and most self-hosted S3-compatible stores.
    pub force_path_style: bool,
}

impl S3ConnectParams {
    /// Create params for a genuine AWS S3 bucket.
    ///
    /// Region is resolved by the SDK from the credential chain when `None`. No custom endpoint
    /// or path-style override is applied.
    #[must_use]
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: None,
            endpoint: None,
            force_path_style: false,
        }
    }

    /// Create params for an S3-compatible store (MinIO, Cloudflare R2, Backblaze B2, …): sets the
    /// custom `endpoint` and turns on `force_path_style` (required by most self-hosted stores).
    #[must_use]
    pub fn for_compat(bucket: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: None,
            endpoint: Some(endpoint.into()),
            force_path_style: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Connector
// ---------------------------------------------------------------------------

/// Connect to an S3 bucket (or S3-compatible store) and return an [`ObjectStoreVfs`] rooted at
/// `root` (a key prefix; `""` = bucket root).
///
/// # Credential dispatch
///
/// Credentials must arrive as [`CredentialSecret::Aws`]; any other variant returns
/// [`VfsError::Auth`] immediately. Within the AWS variant:
///
/// - [`AwsCredential::Static`] — builds the client from the provided key pair; never touches
///   environment variables, `~/.aws`, or instance metadata.
/// - [`AwsCredential::Profile`] — delegates to `aws-config` scoped to the named profile (full chain,
///   so SSO and `credential_process` profiles resolve).
/// - [`AwsCredential::DefaultChain`] — delegates to the SDK default chain (env vars → shared
///   profile → container/IMDS).
/// - Any future unknown variant (the enum is `#[non_exhaustive]`) → [`VfsError::Auth`].
///
/// # Security
///
/// Signed URLs, session tokens, and secret access keys are never embedded in any [`VfsError`]
/// message. AWS service error messages are safe to surface; they never contain credential material.
#[must_use = "the established ObjectStoreVfs must be used or the connection is dropped"]
pub async fn s3_connect(
    conn: ConnectionId,
    params: &S3ConnectParams,
    cred: &CredentialSecret,
    root: &str,
) -> Result<ObjectStoreVfs, VfsError> {
    let aws_cred = match cred {
        CredentialSecret::Aws(a) => a,
        _ => return Err(VfsError::Auth),
    };

    let client = match aws_cred {
        AwsCredential::Static {
            access_key_id,
            secret_access_key,
            session_token,
        } => {
            // Build the client entirely from explicit values — no environment or file lookups.
            let creds = Credentials::new(
                access_key_id.as_str(),
                secret_access_key.expose_secret(),
                session_token.as_ref().map(|t| t.expose_secret().to_owned()),
                None,
                "cairn-static",
            );
            let region = Region::new(
                params
                    .region
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_owned()),
            );
            let builder = aws_sdk_s3::config::Builder::new()
                .behavior_version(BehaviorVersion::latest())
                .region(region)
                .credentials_provider(creds);
            aws_sdk_s3::Client::from_conf(apply_endpoint(builder, params).build())
        }
        AwsCredential::Profile(profile_name) => {
            // Delegate credential and region resolution to aws-config, scoped to the named profile.
            let loader =
                aws_config::defaults(BehaviorVersion::latest()).profile_name(profile_name.as_str());
            client_from_loader(loader, params).await
        }
        AwsCredential::DefaultChain => {
            // SDK default chain: env vars → shared profile → container/IMDS credential providers.
            let loader = aws_config::defaults(BehaviorVersion::latest());
            client_from_loader(loader, params).await
        }
        // AwsCredential is #[non_exhaustive]; gracefully reject any future unknown variant.
        _ => return Err(VfsError::Auth),
    };

    Ok(ObjectStoreVfs::new(
        conn,
        Scheme::S3,
        Arc::new(S3ObjectStore {
            client,
            bucket: params.bucket.clone(),
        }),
        root,
    ))
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Build an S3 client from an `aws-config` loader (the `Profile` / `DefaultChain` paths), applying
/// the optional region override and any S3-compatible endpoint/path-style settings. Factored out so
/// the two delegating arms don't duplicate the load → builder → endpoint dance.
async fn client_from_loader(
    mut loader: aws_config::ConfigLoader,
    params: &S3ConnectParams,
) -> aws_sdk_s3::Client {
    if let Some(r) = &params.region {
        loader = loader.region(Region::new(r.clone()));
    }
    let shared = loader.load().await;
    let builder = aws_sdk_s3::config::Builder::from(&shared);
    aws_sdk_s3::Client::from_conf(apply_endpoint(builder, params).build())
}

/// Apply the optional custom endpoint URL and path-style flag to a config builder.
///
/// Called after the per-credential builder setup so the overrides are always the last layer.
fn apply_endpoint(
    mut b: aws_sdk_s3::config::Builder,
    params: &S3ConnectParams,
) -> aws_sdk_s3::config::Builder {
    if let Some(ep) = &params.endpoint {
        b = b.endpoint_url(ep.as_str());
    }
    b.force_path_style(params.force_path_style)
}

/// Build an HTTP `Range` header value for a byte range.
///
/// S3 uses inclusive byte ranges: `bytes=first-last`. When `len` is `None` the range is
/// open-ended (from `offset` to EOF). Callers should pass `len >= 1`; `Some(0)` is clamped to a
/// single byte at `offset` to keep the header syntactically valid.
fn range_header(offset: u64, len: Option<u64>) -> String {
    match len {
        Some(n) => {
            // Inclusive end; clamp Some(0) to `offset` rather than producing bytes=N-(N-1).
            let last = if n == 0 { offset } else { offset + n - 1 };
            format!("bytes={offset}-{last}")
        }
        None => format!("bytes={offset}-"),
    }
}

/// Build the `copy_source` header value: `"{bucket}/{percent-encoded-key}"`.
///
/// S3 requires the copy-source header to be URL-encoded. Unreserved characters per RFC 3986 §2.3
/// (`A-Z a-z 0-9 - _ . ~`) and `/` (kept literal as the key-segment separator) are left as-is;
/// every other byte is percent-encoded with uppercase hex. We write a tiny inline encoder rather
/// than adding a URL-encoding crate dependency.
fn encode_copy_source(bucket: &str, key: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bucket.len() + 1 + key.len() * 3 / 2);
    // Bucket names are restricted to [a-z0-9.-] by S3, so they never need percent-encoding; only the
    // key bytes can contain characters that do.
    out.push_str(bucket);
    out.push('/');
    for b in key.bytes() {
        match b {
            // Unreserved per RFC 3986 §2.3 plus `/` as the key-segment separator.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            other => {
                out.push('%');
                out.push(HEX[(other >> 4) as usize] as char);
                out.push(HEX[(other & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Map an AWS SDK error code to a semantic [`VfsError`] variant when the code is well-known.
///
/// Returns `None` for codes that should fall through to the generic `Backend` variant.
///
/// The path is the `VfsPath::root()` sentinel: the `ObjectStore` trait only sees an opaque key, not
/// the user's `VfsPath`, so — like [`MockObjectStore`](crate::MockObjectStore) — we don't fabricate a
/// path here. [`ObjectStoreVfs::stat`](crate::ObjectStoreVfs) re-wraps `NotFound` with the real path
/// where it matters; carrying the internal key would also leak the bucket-root prefix.
fn classify_code(code: &str) -> Option<VfsError> {
    match code {
        // S3 uses "NoSuchKey" for GetObject/DeleteObject; HeadObject maps 404 to "NotFound";
        // literal "404" covers some S3-compatible stores that surface numeric codes.
        "NoSuchKey" | "NotFound" | "404" => Some(VfsError::NotFound(VfsPath::root())),
        // "AccessDenied" from real S3; "Forbidden"/"403" from compatible stores.
        "AccessDenied" | "Forbidden" | "403" => Some(VfsError::Forbidden(VfsPath::root())),
        _ => None,
    }
}

/// Whether the given error code suggests a transient failure that is safe to retry.
fn retryable_code(code: &str) -> bool {
    matches!(
        code,
        "SlowDown"
            | "RequestTimeout"
            | "ServiceUnavailable"
            | "InternalError"
            | "ThrottlingException"
            | "503"
            | "500"
    )
}

/// Translate any `SdkError<E, R>` into a [`VfsError`].
///
/// Classification precedence:
/// 1. Well-known error codes → semantic variant ([`VfsError::NotFound`], [`VfsError::Forbidden`]).
/// 2. Other SDK error codes → [`VfsError::Backend`] with the code, a safe message, and retryable
///    flag.
/// 3. No code (dispatch/networking/construction failure) → [`VfsError::Connection`].
///
/// Secret material — access key, session token, signature — is never embedded in any variant.
/// AWS/S3 service error messages are safe to surface because S3 never echoes credentials.
fn map_sdk_err<E, R>(e: SdkError<E, R>) -> VfsError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    match e.code() {
        Some(code) => {
            if let Some(v) = classify_code(code) {
                return v;
            }
            let msg = e.message().unwrap_or("s3 error").to_owned();
            let retryable = retryable_code(code);
            VfsError::Backend {
                code: code.to_owned(),
                msg,
                retryable,
            }
        }
        // No code: the failure occurred before or outside the service call (network, signing,
        // or SDK construction) — wrap as a connection error.
        None => VfsError::Connection(Box::new(e)),
    }
}

// ---------------------------------------------------------------------------
// Unit tests — no cloud or local emulator required
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // range_header -----------------------------------------------------------

    #[test]
    fn range_header_bounded_three_bytes() {
        assert_eq!(range_header(2, Some(3)), "bytes=2-4");
    }

    #[test]
    fn range_header_unbounded() {
        assert_eq!(range_header(5, None), "bytes=5-");
    }

    #[test]
    fn range_header_zero_len_clamps_to_single_byte() {
        // Some(0) is ill-formed from the caller's perspective, but we emit a valid range.
        assert_eq!(range_header(7, Some(0)), "bytes=7-7");
    }

    #[test]
    fn range_header_single_byte() {
        assert_eq!(range_header(0, Some(1)), "bytes=0-0");
    }

    // encode_copy_source -----------------------------------------------------

    #[test]
    fn encode_copy_source_plain_key() {
        assert_eq!(
            encode_copy_source("my-bucket", "path/to/object.txt"),
            "my-bucket/path/to/object.txt"
        );
    }

    #[test]
    fn encode_copy_source_space_becomes_pct20() {
        assert_eq!(encode_copy_source("b", "hello world"), "b/hello%20world");
    }

    #[test]
    fn encode_copy_source_slash_is_preserved() {
        assert_eq!(encode_copy_source("b", "a/b/c"), "b/a/b/c");
    }

    #[test]
    fn encode_copy_source_special_chars() {
        // `+` and `?` must be percent-encoded; `.` and `_` and `-` are unreserved and must not be.
        let result = encode_copy_source("bkt", "file+name?.bin");
        assert_eq!(result, "bkt/file%2Bname%3F.bin");
    }

    // classify_code ----------------------------------------------------------

    #[test]
    fn classify_not_found_codes() {
        for code in &["NoSuchKey", "NotFound", "404"] {
            assert!(
                matches!(classify_code(code), Some(VfsError::NotFound(_))),
                "expected NotFound for code {code}"
            );
        }
    }

    #[test]
    fn classify_forbidden_codes() {
        for code in &["AccessDenied", "Forbidden", "403"] {
            assert!(
                matches!(classify_code(code), Some(VfsError::Forbidden(_))),
                "expected Forbidden for code {code}"
            );
        }
    }

    #[test]
    fn classify_unknown_code_returns_none() {
        assert!(classify_code("NoSuchBucket").is_none());
        assert!(classify_code("InternalError").is_none());
    }

    // retryable_code ---------------------------------------------------------

    #[test]
    fn retryable_codes_are_recognised() {
        for code in &[
            "SlowDown",
            "RequestTimeout",
            "ServiceUnavailable",
            "InternalError",
            "ThrottlingException",
            "503",
            "500",
        ] {
            assert!(retryable_code(code), "expected retryable for {code}");
        }
    }

    #[test]
    fn non_retryable_codes_are_not_retryable() {
        assert!(!retryable_code("NoSuchKey"));
        assert!(!retryable_code("AccessDenied"));
        assert!(!retryable_code("404"));
    }
}
