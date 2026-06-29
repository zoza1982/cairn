//! Live Google Cloud Storage adapter.
//!
//! Gated behind the `gcs` cargo feature. This module integrates two separate GCS clients:
//!
//! - [`Storage`] — the JSON/HTTP **data plane** (reads and writes). Used for
//!   [`GcsObjectStore::get`] and [`GcsObjectStore::put`].
//! - [`StorageControl`] — the gRPC **control plane** (metadata and lifecycle). Used for
//!   [`GcsObjectStore::list_page`], [`GcsObjectStore::head`], [`GcsObjectStore::delete`], and
//!   the server-side copy in [`GcsObjectStore::copy`] (via the GCS `rewrite` API).
//!
//! Both clients share the same credentials obtained from the vault. Credential material is
//! never embedded in any [`VfsError`] message; service-account JSON key material is consumed
//! in-place and immediately dropped — the key never surfaces in any error path.
//!
//! The public entry point is [`gcs_connect`]; [`GcsConnectParams`] configures the bucket and
//! optional emulator endpoint; [`GcsObjectStore`] is exported so the connection registry can
//! name the concrete type — construct it exclusively via [`gcs_connect`].

use crate::{ListChunk, ObjectMeta, ObjectStore, ObjectStoreVfs};
use async_trait::async_trait;
use cairn_types::{Caps, ConnectionId, Scheme, VfsPath};
use cairn_vault::{CredentialSecret, ExposeSecret, GcpCredential};
use cairn_vfs::VfsError;
// RewriteObjectExt adds `rewrite_until_done()` onto the `RewriteObject` builder; imported
// anonymously for the same reason.
use google_cloud_storage::builder_ext::RewriteObjectExt as _;
use google_cloud_storage::client::{Storage, StorageControl};
use google_cloud_storage::model_ext::ReadRange;
use smol_str::SmolStr;
use std::sync::Arc;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// GcsObjectStore
// ---------------------------------------------------------------------------

/// Live GCS object store backed by the official `google-cloud-storage` client.
///
/// The data plane ([`Storage`]) and the control plane ([`StorageControl`]) are kept as
/// separate clients because the GCS SDK exposes them that way: reads and writes go over
/// JSON/HTTP while listing, metadata, delete, and rewrite go over gRPC.
///
/// Construct via [`gcs_connect`] rather than directly; the clients already hold credentials
/// obtained from the vault and have the correct endpoint configured.
pub struct GcsObjectStore {
    /// Data-plane client (JSON/HTTP): object reads and writes.
    storage: Storage,
    /// Control-plane client (gRPC): list, head, delete, and server-side copy.
    control: StorageControl,
    /// The full GCS resource name (`projects/_/buckets/{bucket}`), pre-computed once at connect so
    /// every call clones a ready string rather than re-running `format!`.
    bucket_resource: String,
}

#[async_trait]
impl ObjectStore for GcsObjectStore {
    fn capabilities(&self) -> Caps {
        // GCS supports random-access reads (range-get), server-side copy (rewrite), and the
        // full CRUD set.
        Caps::LIST | Caps::READ | Caps::WRITE | Caps::DELETE | Caps::COPY_SERVER | Caps::RANDOM_READ
    }

    async fn list_page(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
        max: usize,
    ) -> Result<ListChunk, VfsError> {
        // GCS caps page size at 1000; clamp so a large `usize` can't overflow i32.
        let page_size = max.min(1000) as i32;
        let mut req = self
            .control
            .list_objects()
            // The control plane requires the full bucket resource name.
            .set_parent(self.bucket_resource.clone())
            // Always send prefix even when empty — avoids an inadvertent "no filter" fallback
            // on some emulators that treat a missing prefix differently from "".
            .set_prefix(prefix)
            .set_page_size(page_size);
        if let Some(d) = delimiter {
            req = req.set_delimiter(d);
        }
        if let Some(t) = token {
            req = req.set_page_token(t);
        }
        let out = req.send().await.map_err(map_gcs_err)?;

        let common_prefixes = out.prefixes;

        let objects = out
            .objects
            .into_iter()
            .map(|obj| {
                // GCS Object.name is the object key within the bucket (not the full resource name).
                let key = obj.name;
                // GCS Object.size is i64; .max(0) guards against malformed negative values from
                // non-conforming implementations.
                let size = obj.size.max(0) as u64;
                let etag = opt_smol(&obj.etag);
                let modified = obj.update_time.and_then(|ts| SystemTime::try_from(ts).ok());
                let storage_class = opt_smol(&obj.storage_class);
                ObjectMeta {
                    key,
                    size,
                    etag,
                    modified,
                    storage_class,
                }
            })
            .collect();

        Ok(ListChunk {
            common_prefixes,
            objects,
            // An empty continuation token means "no more pages"; treating it as `Some` would
            // cause the caller to re-list from the start on the next call.
            next_token: Some(out.next_page_token).filter(|t| !t.is_empty()),
        })
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, VfsError> {
        let obj = self
            .control
            .get_object()
            .set_bucket(self.bucket_resource.clone())
            .set_object(key)
            .send()
            .await
            .map_err(map_gcs_err)?;

        let size = obj.size.max(0) as u64;
        let etag = opt_smol(&obj.etag);
        let modified = obj.update_time.and_then(|ts| SystemTime::try_from(ts).ok());
        let storage_class = opt_smol(&obj.storage_class);

        Ok(ObjectMeta {
            key: key.to_owned(),
            size,
            etag,
            modified,
            storage_class,
        })
    }

    async fn get(&self, key: &str, range: Option<(u64, Option<u64>)>) -> Result<Vec<u8>, VfsError> {
        let mut req = self.storage.read_object(self.bucket_resource.clone(), key);
        if let Some((offset, len)) = range {
            req = req.set_read_range(to_read_range(offset, len));
        }
        let mut resp = req.send().await.map_err(map_gcs_err)?;
        let mut data = Vec::new();
        while let Some(chunk) = resp.next().await.transpose().map_err(map_gcs_err)? {
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> Result<ObjectMeta, VfsError> {
        let obj = self
            .storage
            .write_object(self.bucket_resource.clone(), key, bytes::Bytes::from(data))
            .send_buffered()
            .await
            .map_err(map_gcs_err)?;

        let etag = opt_smol(&obj.etag);
        Ok(ObjectMeta {
            key: key.to_owned(),
            // Server-confirmed object size (consistent with head()/copy()).
            size: obj.size.max(0) as u64,
            etag,
            // write_object does not return a timestamp; callers that need the write time must
            // follow with a head() call.
            modified: None,
            storage_class: None,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), VfsError> {
        self.control
            .delete_object()
            .set_bucket(self.bucket_resource.clone())
            .set_object(key)
            .send()
            .await
            .map_err(map_gcs_err)?;
        Ok(())
    }

    async fn copy(&self, from: &str, to: &str) -> Result<ObjectMeta, VfsError> {
        // GCS `rewrite` is the server-side copy primitive; it handles objects of any size and
        // preserves metadata. `rewrite_until_done()` (from the `RewriteObjectExt` extension trait)
        // polls the rewrite until it completes, resuming with the continuation token on each cycle.
        let bucket_res = self.bucket_resource.clone();
        let obj = self
            .control
            .rewrite_object()
            .set_source_bucket(bucket_res.clone())
            .set_source_object(from)
            .set_destination_bucket(bucket_res)
            .set_destination_name(to)
            .rewrite_until_done()
            .await
            .map_err(map_gcs_err)?;

        let etag = opt_smol(&obj.etag);
        let modified = obj.update_time.and_then(|ts| SystemTime::try_from(ts).ok());
        Ok(ObjectMeta {
            key: to.to_owned(),
            // rewrite returns the destination object in full, including its size.
            size: obj.size.max(0) as u64,
            etag,
            modified,
            storage_class: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Connection parameters
// ---------------------------------------------------------------------------

/// Connection parameters for a Google Cloud Storage bucket.
///
/// Pass to [`gcs_connect`] alongside a [`CredentialSecret::Gcp`] from the vault.
/// For the `fake-gcs-server` emulator set `endpoint` to the server URL (e.g.
/// `"http://localhost:4443"`) — anonymous credentials are used automatically when an
/// endpoint is present. For production GCS leave `endpoint` as `None`.
#[derive(Debug, Clone)]
pub struct GcsConnectParams {
    /// The GCS bucket name in short form (e.g. `"my-bucket"`).
    pub bucket: String,
    /// Custom endpoint URL for GCS-compatible emulators (`fake-gcs-server`). `None` = production.
    pub endpoint: Option<String>,
}

impl GcsConnectParams {
    /// Create params for a production GCS bucket.
    ///
    /// Credentials are resolved from the [`CredentialSecret::Gcp`] passed to [`gcs_connect`].
    /// No custom endpoint or emulator overrides are applied.
    #[must_use]
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            endpoint: None,
        }
    }

    /// Create params for a GCS-compatible emulator (e.g. `fake-gcs-server`).
    ///
    /// Sets the custom endpoint URL and switches the credential path to anonymous — no
    /// `Authorization` header is sent, matching the emulator's default behaviour.
    #[must_use]
    pub fn for_emulator(bucket: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            endpoint: Some(endpoint.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Connector
// ---------------------------------------------------------------------------

/// Connect to a GCS bucket and return an [`ObjectStoreVfs`] rooted at `root` (a key prefix;
/// `""` = bucket root).
///
/// # Credential dispatch
///
/// Credentials must arrive as [`CredentialSecret::Gcp`]; any other variant returns
/// [`VfsError::Auth`] immediately. Within the GCP variant:
///
/// - [`GcpCredential::ServiceAccountKey`] — parses the JSON key file held in the vault and
///   configures a service-account credential provider. The raw JSON never appears in any error
///   message; the `map_err` discards the parse/build error before it can be formatted.
/// - [`GcpCredential::ApplicationDefault`] — delegates to Application Default Credentials
///   (`GOOGLE_APPLICATION_CREDENTIALS`, `gcloud auth`, or the GCE metadata server).
/// - Any future unknown variant (the enum is `#[non_exhaustive]`) → [`VfsError::Auth`].
///
/// When `params.endpoint` is set (emulator mode), anonymous credentials are used regardless of
/// the credential variant — the vault credential is ignored for that path.
///
/// # Security
///
/// Service-account JSON key material is exposed once to build the credential and then dropped;
/// it is never embedded in a [`VfsError`] message.
#[must_use = "the established ObjectStoreVfs must be used or the connection is dropped"]
pub async fn gcs_connect(
    conn: ConnectionId,
    params: &GcsConnectParams,
    cred: &CredentialSecret,
    root: &str,
) -> Result<ObjectStoreVfs, VfsError> {
    // Reject non-GCP credentials before doing any async work.
    let gcp_cred = match cred {
        CredentialSecret::Gcp(g) => g,
        _ => return Err(VfsError::Auth),
    };

    // Emulator mode always uses anonymous credentials; production mode resolves from the vault.
    let auth_creds = if params.endpoint.is_some() {
        google_cloud_auth::credentials::anonymous::Builder::new().build()
    } else {
        build_production_creds(gcp_cred)?
    };

    // Build both the data-plane and the control-plane clients from the same credentials.
    // `Credentials` is `Clone`, so we can pass the same value to both builders.
    let mut storage_builder = Storage::builder().with_credentials(auth_creds.clone());
    let mut control_builder = StorageControl::builder().with_credentials(auth_creds);

    if let Some(ep) = &params.endpoint {
        storage_builder = storage_builder.with_endpoint(ep.as_str());
        control_builder = control_builder.with_endpoint(ep.as_str());
    }

    let storage = storage_builder
        .build()
        .await
        .map_err(|e| VfsError::Connection(Box::new(e)))?;
    let control = control_builder
        .build()
        .await
        .map_err(|e| VfsError::Connection(Box::new(e)))?;

    Ok(ObjectStoreVfs::new(
        conn,
        Scheme::Gcs,
        Arc::new(GcsObjectStore {
            storage,
            control,
            bucket_resource: bucket_resource(&params.bucket),
        }),
        root,
    ))
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Build production [`google_cloud_auth::credentials::Credentials`] from the vault's GCP secret.
///
/// Returns [`VfsError::Auth`] for invalid or unrecognised credential material; errors from the
/// credential builders are mapped with `map_err(|_| VfsError::Auth)` so the raw JSON or error
/// message never surfaces. The ADC path wraps its error as [`VfsError::Connection`] because
/// an ADC failure typically indicates a missing environment or network issue, not bad key material.
fn build_production_creds(
    gcp_cred: &GcpCredential,
) -> Result<google_cloud_auth::credentials::Credentials, VfsError> {
    match gcp_cred {
        GcpCredential::ServiceAccountKey(json_secret) => {
            // Parse the JSON key held in the vault's SecretString into a `serde_json::Value`.
            // The JSON is consumed in-place; `map_err(|_| …)` discards the parse error so the
            // raw JSON never appears in a `VfsError` message.
            let val: serde_json::Value =
                serde_json::from_str(json_secret.expose_secret()).map_err(|_| VfsError::Auth)?;
            google_cloud_auth::credentials::service_account::Builder::new(val)
                .build()
                .map_err(|_| VfsError::Auth)
        }
        GcpCredential::ApplicationDefault => {
            // Delegate to ADC: GOOGLE_APPLICATION_CREDENTIALS → gcloud → GCE metadata.
            // A build failure here is an environment or network problem, not bad credentials.
            google_cloud_auth::credentials::Builder::default()
                .build()
                .map_err(|e| VfsError::Connection(Box::new(e)))
        }
        // `GcpCredential` is `#[non_exhaustive]`; reject any future unknown variant.
        _ => Err(VfsError::Auth),
    }
}

/// Convert a GCS string field to `Option<SmolStr>`, mapping an empty string to `None`.
///
/// GCS returns several fields (etag, storage_class) as empty `String` when not available,
/// rather than using `Option`. This helper applies the `is_empty() → None` convention
/// consistently across all object-metadata extraction sites.
fn opt_smol(s: &str) -> Option<SmolStr> {
    if s.is_empty() {
        None
    } else {
        Some(SmolStr::new(s))
    }
}

/// Build the GCS bucket resource name.
///
/// All GCS Storage Control API calls (gRPC) require the full resource name of the bucket,
/// `projects/_/buckets/{bucket_id}`, where `_` is the wildcard project identifier. The data-plane
/// [`Storage`] client uses the same format for consistency.
fn bucket_resource(bucket: &str) -> String {
    format!("projects/_/buckets/{bucket}")
}

/// Translate a `(offset, len)` range pair into a [`ReadRange`].
///
/// - `len = None` → open-ended range from `offset` to EOF.
/// - `len = Some(0)` → single-byte read at `offset` (a zero-length request is invalid; this
///   substitution keeps the GCS request syntactically well-formed).
/// - `len = Some(n)` → exactly `n` bytes starting at `offset`.
fn to_read_range(offset: u64, len: Option<u64>) -> ReadRange {
    match len {
        None => ReadRange::offset(offset),
        Some(0) => ReadRange::segment(offset, 1),
        Some(n) => ReadRange::segment(offset, n),
    }
}

/// Translate a GCS SDK error into a [`VfsError`].
///
/// Classification precedence:
/// 1. gRPC status code name (control-plane errors) → semantic variant or `Backend`.
/// 2. HTTP status code (data-plane errors) → semantic variant or `Backend`.
/// 3. Neither → transport or client-construction failure → [`VfsError::Connection`].
///
/// Credential material is never embedded in any variant; the `msg` fields contain only the
/// SDK's own error messages, which never include credential material.
fn map_gcs_err(e: google_cloud_storage::Error) -> VfsError {
    // Prefer the gRPC status, but ONLY when its code is meaningful. Data-plane (HTTP) Service errors
    // also carry a `Status`, but with `Code::Unknown` — the SDK parses the gRPC `status` *string*
    // from the JSON error body and ignores the numeric HTTP code — so trusting it blindly would
    // collapse every data-plane 401/403/404/5xx into `Backend("UNKNOWN")`. When the gRPC code is
    // UNKNOWN, fall through to the HTTP status. Control-plane gRPC errors have no `http_status_code`,
    // so they always take the gRPC branch. (Compared by name to avoid importing the gax `Code` type.)
    if let Some(status) = e.status() {
        if status.code.name() != "UNKNOWN" {
            return classify_grpc_code(status.code.name(), &status.message);
        }
    }
    // HTTP status (Storage / data-plane errors, or an HTTP Service error with an UNKNOWN gRPC code).
    if let Some(code) = e.http_status_code() {
        return classify_http_code(code, &e.to_string());
    }
    // Transport or client-construction error — wrap as a connection failure.
    VfsError::Connection(Box::new(e))
}

/// Map a gRPC status code name (e.g. `"NOT_FOUND"`) to a [`VfsError`].
///
/// The `VfsPath::root()` sentinel is used for path-typed errors because the [`ObjectStore`]
/// trait only sees an opaque key, not the user's [`VfsPath`]; the wrapping
/// [`ObjectStoreVfs`](crate::ObjectStoreVfs) re-wraps `NotFound` with the real path where it
/// matters.
///
/// `UNAUTHENTICATED` (gRPC code 16) means the token has expired, the service-account key has
/// been revoked, or the credential is invalid at request time; it maps to [`VfsError::Auth`]
/// so the UI can prompt for re-authentication rather than showing a generic backend error.
///
/// Retryable codes follow the gRPC retryability convention: `UNAVAILABLE`,
/// `RESOURCE_EXHAUSTED`, `INTERNAL`, and `DEADLINE_EXCEEDED` are transient and safe to retry.
fn classify_grpc_code(code: &str, msg: &str) -> VfsError {
    match code {
        "NOT_FOUND" => VfsError::NotFound(VfsPath::root()),
        "PERMISSION_DENIED" => VfsError::Forbidden(VfsPath::root()),
        // Token expired, service-account key revoked, or credential invalid at request time.
        "UNAUTHENTICATED" => VfsError::Auth,
        // ABORTED is returned when a conflicting concurrent operation prevented completion; it
        // is safe to retry (typically after the conflict resolves).
        "UNAVAILABLE" | "RESOURCE_EXHAUSTED" | "INTERNAL" | "DEADLINE_EXCEEDED" | "ABORTED" => {
            VfsError::Backend {
                code: code.to_owned(),
                msg: msg.to_owned(),
                retryable: true,
            }
        }
        _ => VfsError::Backend {
            code: code.to_owned(),
            msg: msg.to_owned(),
            retryable: false,
        },
    }
}

/// Map an HTTP status code to a [`VfsError`].
///
/// `401` (Unauthorized) maps to [`VfsError::Auth`] — the token is missing, expired, or invalid
/// and the app should prompt for re-authentication. `429` (Too Many Requests), `408` (Request
/// Timeout), and `5xx` server errors are transient and safe to retry.
fn classify_http_code(code: u16, msg: &str) -> VfsError {
    match code {
        401 => VfsError::Auth,
        403 => VfsError::Forbidden(VfsPath::root()),
        404 => VfsError::NotFound(VfsPath::root()),
        408 | 429 | 500..=599 => VfsError::Backend {
            code: code.to_string(),
            msg: msg.to_owned(),
            retryable: true,
        },
        _ => VfsError::Backend {
            code: code.to_string(),
            msg: msg.to_owned(),
            retryable: false,
        },
    }
}

// ---------------------------------------------------------------------------
// Unit tests — no cloud or local emulator required
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // bucket_resource ---------------------------------------------------------

    #[test]
    fn bucket_resource_plain_name() {
        assert_eq!(bucket_resource("my-bucket"), "projects/_/buckets/my-bucket");
    }

    #[test]
    fn bucket_resource_with_dots_and_dashes() {
        assert_eq!(
            bucket_resource("org.example.data-lake"),
            "projects/_/buckets/org.example.data-lake"
        );
    }

    // to_read_range -----------------------------------------------------------

    #[test]
    fn to_read_range_unbounded_does_not_panic() {
        // Verify construction succeeds; we can't easily inspect the range value.
        let _r = to_read_range(5, None);
    }

    #[test]
    fn to_read_range_bounded_does_not_panic() {
        let _r = to_read_range(10, Some(512));
    }

    #[test]
    fn to_read_range_zero_len_becomes_single_byte() {
        // Some(0) must not produce a zero-length (invalid) range request.
        let _r = to_read_range(7, Some(0));
    }

    // classify_grpc_code -------------------------------------------------------

    #[test]
    fn grpc_not_found_maps_to_not_found() {
        assert!(matches!(
            classify_grpc_code("NOT_FOUND", "object not found"),
            VfsError::NotFound(_)
        ));
    }

    #[test]
    fn grpc_permission_denied_maps_to_forbidden() {
        assert!(matches!(
            classify_grpc_code("PERMISSION_DENIED", "access denied"),
            VfsError::Forbidden(_)
        ));
    }

    #[test]
    fn grpc_unavailable_is_retryable_backend() {
        assert!(matches!(
            classify_grpc_code("UNAVAILABLE", "server unavailable"),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn grpc_resource_exhausted_is_retryable_backend() {
        assert!(matches!(
            classify_grpc_code("RESOURCE_EXHAUSTED", "quota exceeded"),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn grpc_internal_is_retryable_backend() {
        assert!(matches!(
            classify_grpc_code("INTERNAL", "internal error"),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn grpc_deadline_exceeded_is_retryable_backend() {
        assert!(matches!(
            classify_grpc_code("DEADLINE_EXCEEDED", "deadline exceeded"),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn grpc_unauthenticated_maps_to_auth() {
        // Expired/revoked token must surface as Auth so the UI can prompt re-authentication.
        assert!(matches!(
            classify_grpc_code("UNAUTHENTICATED", "token expired"),
            VfsError::Auth
        ));
    }

    #[test]
    fn grpc_aborted_is_retryable_backend() {
        assert!(matches!(
            classify_grpc_code("ABORTED", "concurrent write conflict"),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn grpc_invalid_argument_is_non_retryable_backend() {
        assert!(matches!(
            classify_grpc_code("INVALID_ARGUMENT", "bad field"),
            VfsError::Backend {
                retryable: false,
                ..
            }
        ));
    }

    // classify_http_code -------------------------------------------------------

    #[test]
    fn http_404_maps_to_not_found() {
        assert!(matches!(
            classify_http_code(404, "not found"),
            VfsError::NotFound(_)
        ));
    }

    #[test]
    fn http_403_maps_to_forbidden() {
        assert!(matches!(
            classify_http_code(403, "forbidden"),
            VfsError::Forbidden(_)
        ));
    }

    #[test]
    fn http_429_is_retryable_backend() {
        assert!(matches!(
            classify_http_code(429, "rate limit"),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn http_500_is_retryable_backend() {
        assert!(matches!(
            classify_http_code(500, "internal server error"),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn http_503_is_retryable_backend() {
        assert!(matches!(
            classify_http_code(503, "service unavailable"),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn http_400_is_non_retryable_backend() {
        assert!(matches!(
            classify_http_code(400, "bad request"),
            VfsError::Backend {
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn http_401_maps_to_auth() {
        // Missing/expired Bearer token must surface as Auth so the UI can prompt re-authentication.
        assert!(matches!(
            classify_http_code(401, "unauthorized"),
            VfsError::Auth
        ));
    }

    #[test]
    fn http_408_is_retryable_backend() {
        assert!(matches!(
            classify_http_code(408, "request timeout"),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }

    // GcsConnectParams --------------------------------------------------------

    #[test]
    fn connect_params_new_has_no_endpoint() {
        let p = GcsConnectParams::new("my-bucket");
        assert_eq!(p.bucket, "my-bucket");
        assert!(p.endpoint.is_none());
    }

    #[test]
    fn connect_params_for_emulator_sets_endpoint() {
        let p = GcsConnectParams::for_emulator("test-bucket", "http://localhost:4443");
        assert_eq!(p.bucket, "test-bucket");
        assert_eq!(p.endpoint.as_deref(), Some("http://localhost:4443"));
    }

    // Guards the `map_gcs_err` dispatch for data-plane (HTTP) errors — an HTTP-status error must
    // reach `classify_http_code` and map semantically, not collapse to a generic backend error.
    #[test]
    fn map_gcs_err_http_status_maps_semantically() {
        use google_cloud_storage::http::HeaderMap;
        let mk =
            |code| google_cloud_storage::Error::http(code, HeaderMap::new(), bytes::Bytes::new());
        assert!(matches!(map_gcs_err(mk(404)), VfsError::NotFound(_)));
        assert!(matches!(map_gcs_err(mk(403)), VfsError::Forbidden(_)));
        assert!(matches!(map_gcs_err(mk(401)), VfsError::Auth));
        assert!(matches!(
            map_gcs_err(mk(503)),
            VfsError::Backend {
                retryable: true,
                ..
            }
        ));
    }
}
