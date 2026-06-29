//! Azure Blob Storage adapter for Cairn.
//!
//! Gated behind the `azure` cargo feature. Credentials always flow through [`cairn_vault`] — never
//! plaintext on disk or in logs. SAS tokens and account keys are **never** included in any
//! [`VfsError`](cairn_vfs::VfsError) message — only safe, non-credential information is surfaced.
//! Azure service error messages are considered safe to propagate because the service never echoes
//! credential material back.
//!
//! The public entry point is [`azure_connect`]; [`AzureConnectParams`] configures the target
//! container and an optional endpoint override for Azurite or custom deployments;
//! [`AzureObjectStore`] is exported only so the connection registry can name the concrete type —
//! construct it exclusively via [`azure_connect`].
//!
//! [`AzureCredential::AzureAd`] is not wired in this build (requires `azure_identity` 0.21, which
//! has not yet been added). Any attempt using that variant returns [`VfsError::Auth`] immediately.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use azure_storage::{CloudLocation, Error as AzureError, StorageCredentials};
use azure_storage_blobs::prelude::{ClientBuilder, ContainerClient};
use cairn_types::{Caps, ConnectionId, Scheme, VfsPath};
use cairn_vault::{AzureCredential, CredentialSecret, ExposeSecret};
use cairn_vfs::VfsError;
use futures::StreamExt;
use smol_str::SmolStr;

use crate::{ListChunk, ObjectMeta, ObjectStore, ObjectStoreVfs};

// ---------------------------------------------------------------------------
// AzureObjectStore
// ---------------------------------------------------------------------------

/// Live Azure Blob Storage object store backed by the official `azure_storage_blobs` client.
///
/// Construct via [`azure_connect`] rather than directly; the client already holds credentials
/// obtained from the vault and has the correct endpoint configured.
pub struct AzureObjectStore {
    container: ContainerClient,
}

#[async_trait]
impl ObjectStore for AzureObjectStore {
    fn capabilities(&self) -> Caps {
        // Azure Blob supports random-access reads (Range), server-side copy, and the full CRUD set.
        Caps::LIST | Caps::READ | Caps::WRITE | Caps::DELETE | Caps::COPY_SERVER | Caps::RANDOM_READ
    }

    async fn list_page(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
        max: usize,
    ) -> Result<ListChunk, VfsError> {
        // Azure caps list results at 5000; clamp to [1, 5000] before narrowing to NonZeroU32.
        let n = max.clamp(1, 5000) as u32;
        debug_assert!(n >= 1, "clamp(1, 5000) guarantees n is non-zero");
        let nz_max = NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN);

        let mut builder = self
            .container
            .list_blobs()
            // Always send prefix even when empty — some implementations treat a missing Prefix
            // differently from an empty one.
            .prefix(prefix.to_owned())
            .max_results(nz_max);

        if let Some(d) = delimiter {
            // String implements Into<Delimiter> in azure_core, so no azure_core import needed.
            builder = builder.delimiter(d.to_owned());
        }
        if let Some(t) = token {
            // String implements Into<NextMarker> in azure_core.
            builder = builder.marker(t.to_owned());
        }

        // Pull exactly ONE page; the returned `next_marker` becomes our continuation token.
        let mut stream = builder.into_stream();
        let maybe_page = stream.next().await.transpose().map_err(map_az_err)?;

        let page = match maybe_page {
            Some(p) => p,
            // An empty initial stream means the prefix has no content at all — valid, empty chunk.
            None => return Ok(ListChunk::default()),
        };

        let common_prefixes = page.blobs.prefixes().map(|bp| bp.name.clone()).collect();

        let objects = page
            .blobs
            .blobs()
            .map(|blob| {
                let key = blob.name.clone();
                let size = blob.properties.content_length;
                let etag = Some(SmolStr::new(blob.properties.etag.as_ref()));
                let modified = Some(odt_to_system_time(
                    blob.properties.last_modified.unix_timestamp(),
                    blob.properties.last_modified.nanosecond(),
                ));
                let storage_class = blob.properties.access_tier.map(|tier| {
                    // create_enum! generates From<AccessTier> for &'static str.
                    let s: &str = tier.into();
                    SmolStr::new(s)
                });
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
            // An empty string marker (should not occur but guard anyway) means "no more pages".
            next_token: page
                .next_marker
                .map(|nm| nm.as_str().to_owned())
                .filter(|t| !t.is_empty()),
        })
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, VfsError> {
        let resp = self
            .container
            .blob_client(key)
            .get_properties()
            .await
            .map_err(map_az_err)?;

        let blob = resp.blob;
        Ok(ObjectMeta {
            key: key.to_owned(),
            size: blob.properties.content_length,
            etag: Some(SmolStr::new(blob.properties.etag.as_ref())),
            modified: Some(odt_to_system_time(
                blob.properties.last_modified.unix_timestamp(),
                blob.properties.last_modified.nanosecond(),
            )),
            storage_class: blob.properties.access_tier.map(|tier| {
                let s: &str = tier.into();
                SmolStr::new(s)
            }),
        })
    }

    async fn get(&self, key: &str, range: Option<(u64, Option<u64>)>) -> Result<Vec<u8>, VfsError> {
        let mut req = self.container.blob_client(key).get();

        if let Some((offset, len)) = range {
            // std::ops::Range<u64> and RangeFrom<u64> both implement Into<azure_core::Range>.
            // Use saturating_add to prevent u64 overflow for adversarial/extreme inputs.
            match len {
                None => req = req.range(offset..),
                // Some(0) is ill-formed from the caller's perspective; clamp to a single byte.
                Some(0) => req = req.range(offset..offset.saturating_add(1)),
                Some(n) => req = req.range(offset..offset.saturating_add(n)),
            }
        }

        // azure_core streams GetBlobResponse in 16 MiB chunks; collect them all.
        let mut stream = req.into_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let mapped = map_az_err(e);
                    // The SDK always sends an `x-ms-range` header, even for a full read, so a
                    // zero-length blob comes back as 416 (Range Not Satisfiable). For an *unranged*
                    // read that means "empty object", not an error — return the empty buffer. A 416
                    // for an explicit caller range is still a real error.
                    if range.is_none()
                        && matches!(&mapped, VfsError::Backend { code, .. } if code == "416")
                    {
                        break;
                    }
                    return Err(mapped);
                }
            };
            let data = chunk.data.collect().await.map_err(map_az_err)?;
            buf.extend_from_slice(data.as_ref());
        }
        Ok(buf)
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> Result<ObjectMeta, VfsError> {
        // Capture size before moving data into the SDK call (Vec<u8>: Into<Body>).
        let size = data.len() as u64;
        let resp = self
            .container
            .blob_client(key)
            .put_block_blob(data)
            .await
            .map_err(map_az_err)?;

        Ok(ObjectMeta {
            key: key.to_owned(),
            size,
            etag: Some(SmolStr::new(&resp.etag)),
            modified: Some(odt_to_system_time(
                resp.last_modified.unix_timestamp(),
                resp.last_modified.nanosecond(),
            )),
            storage_class: None,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), VfsError> {
        // Azure returns 404 for a non-existent blob. Treat that as success to match the
        // idempotent-delete contract established by MockObjectStore and the S3 backend (AWS
        // DeleteObject returns 204 regardless of whether the key existed). This also makes
        // retried deletes safe after a transient response-loss.
        match self
            .container
            .blob_client(key)
            .delete()
            .await
            .map_err(map_az_err)
        {
            Ok(_) | Err(VfsError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn copy(&self, from: &str, to: &str) -> Result<ObjectMeta, VfsError> {
        // The source URL is a plain structural URL (no SAS material) — safe to use as copy source.
        let source_url = self.container.blob_client(from).url().map_err(map_az_err)?;

        // is_synchronous(true) sets x-ms-requires-sync: true so the copy completes before
        // returning rather than starting a background async copy job.
        //
        // NOTE: the synchronous Copy Blob From URL API is limited to blobs ≤ 256 MiB by Azure.
        // Blobs above this limit return a non-retryable VfsError::Backend (HTTP 400). A
        // polling-based async fallback for large blobs is tracked as a follow-up.
        let resp = self
            .container
            .blob_client(to)
            .copy_from_url(source_url)
            .is_synchronous(true)
            .await
            .map_err(map_az_err)?;

        Ok(ObjectMeta {
            key: to.to_owned(),
            // copy_from_url does not return the object size; callers that need it must head().
            size: 0,
            etag: Some(SmolStr::new(&resp.etag)),
            modified: Some(odt_to_system_time(
                resp.last_modified.unix_timestamp(),
                resp.last_modified.nanosecond(),
            )),
            storage_class: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Connection parameters
// ---------------------------------------------------------------------------

/// Connection parameters for an Azure Blob Storage container.
///
/// Pass to [`azure_connect`] alongside a [`CredentialSecret::Azure`] from the vault. For the
/// public Azure cloud use [`AzureConnectParams::new`]. For Azurite or a custom endpoint use
/// [`AzureConnectParams::for_emulator`].
#[derive(Debug, Clone)]
pub struct AzureConnectParams {
    /// Storage account name (e.g. `"mystorageaccount"`).
    pub account: String,
    /// Container name within the storage account.
    pub container: String,
    /// Optional custom blob-service endpoint URL.
    ///
    /// For Azurite this is typically `"http://127.0.0.1:10000/devstoreaccount1"`. For the public
    /// Azure cloud leave this `None`.
    pub endpoint: Option<String>,
}

impl AzureConnectParams {
    /// Create params for an Azure Blob Storage container in the public Azure cloud.
    ///
    /// Leave `endpoint` unset; the SDK will construct the standard
    /// `https://{account}.blob.core.windows.net` URL automatically.
    #[must_use]
    pub fn new(account: impl Into<String>, container: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            container: container.into(),
            endpoint: None,
        }
    }

    /// Create params for an Azurite emulator or any custom blob-service endpoint.
    ///
    /// Set `endpoint` to the full blob-service URL including the account path segment, e.g.
    /// `"http://127.0.0.1:10000/devstoreaccount1"` for a local Azurite instance.
    #[must_use]
    pub fn for_emulator(
        account: impl Into<String>,
        container: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            account: account.into(),
            container: container.into(),
            endpoint: Some(endpoint.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Connector
// ---------------------------------------------------------------------------

/// Connect to an Azure Blob Storage container and return an [`ObjectStoreVfs`] rooted at `root`
/// (a key prefix; `""` = container root).
///
/// # Credential dispatch
///
/// Credentials must arrive as [`CredentialSecret::Azure`]; any other variant returns
/// [`VfsError::Auth`] immediately. Within the Azure variant:
///
/// - [`AzureCredential::SharedKey`] — builds the client from the provided account name and access
///   key; never touches environment variables or instance metadata.
/// - [`AzureCredential::SasToken`] — builds the client from the provided SAS token (the token is
///   never embedded in any error message even if parsing fails).
/// - [`AzureCredential::AzureAd`] — **not supported in this build** (requires `azure_identity`
///   0.21, which has not yet been wired in). Returns [`VfsError::Auth`].
/// - Any future unknown variant (the enum is `#[non_exhaustive]`) → [`VfsError::Auth`].
///
/// # Security
///
/// SAS tokens and account access keys are **never** embedded in any [`VfsError`] message. If SAS
/// token parsing fails, the azure error (which may contain the token) is discarded and only
/// [`VfsError::Auth`] is returned.
#[must_use = "the established ObjectStoreVfs must be used or the connection is dropped"]
pub async fn azure_connect(
    conn: ConnectionId,
    params: &AzureConnectParams,
    cred: &CredentialSecret,
    root: &str,
) -> Result<ObjectStoreVfs, VfsError> {
    let az_cred = match cred {
        CredentialSecret::Azure(a) => a,
        _ => return Err(VfsError::Auth),
    };

    let storage_creds = match az_cred {
        AzureCredential::SharedKey { account, key } => {
            // account is an identifier (not secret); key is secret — expose only inside the SDK.
            StorageCredentials::access_key(account.as_str(), key.expose_secret().to_owned())
        }
        AzureCredential::SasToken(token) => {
            // Discard the azure error to avoid any risk of leaking the SAS token into VfsError.
            StorageCredentials::sas_token(token.expose_secret()).map_err(|_| VfsError::Auth)?
        }
        AzureCredential::AzureAd => {
            // AAD / managed-identity delegation is not wired in this build.
            // Requires azure_identity 0.21 — track in a follow-up issue.
            return Err(VfsError::Auth);
        }
        // AzureCredential is #[non_exhaustive]; reject any future unknown variant gracefully.
        _ => return Err(VfsError::Auth),
    };

    let location = match &params.endpoint {
        Some(uri) => CloudLocation::Custom {
            account: params.account.clone(),
            uri: uri.clone(),
        },
        None => CloudLocation::Public {
            account: params.account.clone(),
        },
    };

    let container =
        ClientBuilder::with_location(location, storage_creds).container_client(&params.container);

    Ok(ObjectStoreVfs::new(
        conn,
        Scheme::Azure,
        Arc::new(AzureObjectStore { container }),
        root,
    ))
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Classify an HTTP status code into a semantic [`VfsError`] variant.
///
/// The `path` inside `NotFound`/`Forbidden` is the `VfsPath::root()` sentinel — the `ObjectStore`
/// trait only sees opaque keys, not user-visible `VfsPath`s. [`ObjectStoreVfs`] re-wraps `NotFound`
/// with the real path where it matters.
fn classify_http_status(status: u16, msg: &str) -> VfsError {
    match status {
        404 => VfsError::NotFound(VfsPath::root()),
        403 => VfsError::Forbidden(VfsPath::root()),
        401 => VfsError::Auth,
        // Timeouts (408), rate-limits (429), and 5xx server errors are transient and safe to retry.
        408 | 429 | 500..=599 => VfsError::Backend {
            code: status.to_string(),
            msg: msg.to_owned(),
            retryable: true,
        },
        _ => VfsError::Backend {
            code: status.to_string(),
            msg: msg.to_owned(),
            retryable: false,
        },
    }
}

/// Translate an [`AzureError`] into a [`VfsError`].
///
/// HTTP errors are classified by status code (see [`classify_http_status`]). Connection, signing,
/// and construction failures — which produce no HTTP status — become [`VfsError::Connection`].
///
/// # Security
///
/// Azure service error messages are safe to surface; the service never echoes credential material.
/// We propagate only the service message, never the request URL, SAS token, or signing material.
///
/// The non-HTTP branch explicitly does **not** forward the raw `AzureError`: transport-layer
/// errors from hyper/reqwest embed the full request URL in their source chain. For
/// `SasToken`-authenticated stores this URL contains the SAS query parameters (a bearer
/// credential). Instead we surface only the error `kind` (a safe, non-secret enum variant name)
/// with no source chain, satisfying the invariant in `VfsError::Connection`'s doc comment.
fn map_az_err(e: AzureError) -> VfsError {
    if let Some(http_err) = e.as_http_error() {
        let status = u16::from(http_err.status());
        // Azure service messages never contain credential material; safe to surface.
        let msg = http_err.error_message().unwrap_or("azure error");
        classify_http_status(status, msg)
    } else {
        // Sanitise: do NOT box the raw error (source chain may include the request URL).
        // Only the kind string — a non-secret enum variant name — is safe to forward.
        let msg = format!("azure connection error ({:?})", e.kind());
        VfsError::Connection(Box::new(std::io::Error::other(msg)))
    }
}

/// Convert a `time::OffsetDateTime` represented as `(unix_seconds, nanoseconds)` to a
/// [`SystemTime`], without panicking on extreme timestamps.
///
/// The `From<OffsetDateTime> for SystemTime` impl in `time 0.3.x` can panic for dates outside the
/// platform's `SystemTime` range. This helper uses only the unix timestamp components and clamps
/// extreme values to `UNIX_EPOCH`.
///
/// `time 0.3` uses **floor semantics**: `unix_secs` is the floored whole-second part and `nanos`
/// is always a *positive* sub-second offset within that second (0 ≤ nanos < 1_000_000_000). So
/// the actual instant is `unix_secs + nanos / 1e9`. For a negative `unix_secs` with `nanos > 0`,
/// the result is closer to the epoch than the absolute value of `unix_secs` suggests.
///
/// Example: -0.5 s → `unix_secs = -1`, `nanos = 500_000_000` → UNIX_EPOCH − 0.5 s.
fn odt_to_system_time(unix_secs: i64, nanos: u32) -> SystemTime {
    if unix_secs >= 0 {
        SystemTime::UNIX_EPOCH
            .checked_add(std::time::Duration::new(unix_secs as u64, nanos))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    } else {
        // For a floored negative second with a positive sub-second offset:
        // actual distance before UNIX_EPOCH = |unix_secs| seconds minus the nanos forward offset.
        // Equivalently: subtract (|unix_secs| - 1) seconds and (1e9 - nanos) nanoseconds,
        // with the nanos == 0 case handled separately to avoid underflow on the subtraction.
        let (secs_back, nanos_back) = if nanos == 0 {
            (unix_secs.unsigned_abs(), 0u32)
        } else {
            (unix_secs.unsigned_abs() - 1, 1_000_000_000 - nanos)
        };
        SystemTime::UNIX_EPOCH
            .checked_sub(std::time::Duration::new(secs_back, nanos_back))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

// ---------------------------------------------------------------------------
// Unit tests — no cloud or local emulator required
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // classify_http_status ---------------------------------------------------

    #[test]
    fn http_404_maps_to_not_found() {
        assert!(
            matches!(
                classify_http_status(404, "blob not found"),
                VfsError::NotFound(_)
            ),
            "404 must map to NotFound"
        );
    }

    #[test]
    fn http_403_maps_to_forbidden() {
        assert!(
            matches!(
                classify_http_status(403, "forbidden"),
                VfsError::Forbidden(_)
            ),
            "403 must map to Forbidden"
        );
    }

    #[test]
    fn http_401_maps_to_auth() {
        assert!(
            matches!(classify_http_status(401, "unauthorized"), VfsError::Auth),
            "401 must map to Auth"
        );
    }

    #[test]
    fn http_429_is_retryable() {
        assert!(
            matches!(
                classify_http_status(429, "too many requests"),
                VfsError::Backend {
                    retryable: true,
                    ..
                }
            ),
            "429 must be retryable"
        );
    }

    #[test]
    fn http_500_is_retryable() {
        assert!(
            matches!(
                classify_http_status(500, "internal error"),
                VfsError::Backend {
                    retryable: true,
                    ..
                }
            ),
            "500 must be retryable"
        );
    }

    #[test]
    fn http_503_is_retryable() {
        assert!(
            matches!(
                classify_http_status(503, "service unavailable"),
                VfsError::Backend {
                    retryable: true,
                    ..
                }
            ),
            "503 must be retryable"
        );
    }

    #[test]
    fn http_408_is_retryable() {
        assert!(
            matches!(
                classify_http_status(408, "request timeout"),
                VfsError::Backend {
                    retryable: true,
                    ..
                }
            ),
            "408 must be retryable"
        );
    }

    #[test]
    fn http_400_is_not_retryable() {
        assert!(
            matches!(
                classify_http_status(400, "bad request"),
                VfsError::Backend {
                    retryable: false,
                    ..
                }
            ),
            "400 must not be retryable"
        );
    }

    #[test]
    fn http_409_is_not_retryable() {
        assert!(
            matches!(
                classify_http_status(409, "conflict"),
                VfsError::Backend {
                    retryable: false,
                    ..
                }
            ),
            "409 must not be retryable"
        );
    }

    // odt_to_system_time -----------------------------------------------------

    #[test]
    fn unix_epoch_converts_to_system_time_epoch() {
        assert_eq!(
            odt_to_system_time(0, 0),
            SystemTime::UNIX_EPOCH,
            "unix timestamp 0 must equal UNIX_EPOCH"
        );
    }

    #[test]
    fn positive_timestamp_converts_correctly() {
        let st = odt_to_system_time(1_000_000, 0);
        let elapsed = st
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("1_000_000 seconds after epoch must be representable");
        assert_eq!(elapsed.as_secs(), 1_000_000);
    }

    #[test]
    fn negative_timestamp_converts_correctly() {
        // A whole second before the epoch — nanos = 0 case.
        let st = odt_to_system_time(-1, 0);
        let before = SystemTime::UNIX_EPOCH
            .duration_since(st)
            .expect("UNIX_EPOCH minus 1 second must be before epoch");
        assert_eq!(before.as_secs(), 1);
    }

    #[test]
    fn negative_timestamp_with_nanos_converts_correctly() {
        // time 0.3 floor semantics: -0.5 s is represented as unix_secs = -1, nanos = 500_000_000.
        // The true distance from epoch is 0.5 s, NOT 1.5 s (the old buggy result).
        let st = odt_to_system_time(-1, 500_000_000);
        let before = SystemTime::UNIX_EPOCH
            .duration_since(st)
            .expect("result must be before epoch");
        assert_eq!(
            before.as_millis(),
            500,
            "must be exactly 500 ms before epoch"
        );
    }

    #[test]
    fn nanoseconds_are_preserved() {
        let st = odt_to_system_time(1, 500_000_000);
        let elapsed = st
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("1.5 seconds must be representable");
        assert_eq!(elapsed.as_secs(), 1);
        assert_eq!(elapsed.subsec_nanos(), 500_000_000);
    }

    // AzureConnectParams -----------------------------------------------------

    #[test]
    fn new_has_correct_fields_and_no_endpoint() {
        let p = AzureConnectParams::new("myaccount", "mycontainer");
        assert_eq!(p.account, "myaccount");
        assert_eq!(p.container, "mycontainer");
        assert!(p.endpoint.is_none(), "new() must not set an endpoint");
    }

    #[test]
    fn for_emulator_sets_all_fields() {
        let p = AzureConnectParams::for_emulator(
            "devstoreaccount1",
            "test-container",
            "http://127.0.0.1:10000/devstoreaccount1",
        );
        assert_eq!(p.account, "devstoreaccount1");
        assert_eq!(p.container, "test-container");
        assert_eq!(
            p.endpoint.as_deref(),
            Some("http://127.0.0.1:10000/devstoreaccount1"),
            "for_emulator() must set endpoint"
        );
    }

    #[test]
    fn sas_token_credential_parses_a_valid_token() {
        // `StorageCredentials::sas_token` validates the token at construction; a well-formed token
        // must build (the adapter maps a parse failure to `VfsError::Auth`, discarding the token).
        let token = "sv=2021-06-08&ss=b&srt=co&sp=rwdlacuptfx&se=2025-01-01T00:00:00Z&sig=AAAA";
        assert!(StorageCredentials::sas_token(token).is_ok());
    }

    #[tokio::test]
    async fn azure_ad_credential_is_rejected() {
        // `AzureAd` is reserved in the vault but not yet wired in the adapter — it must fail closed
        // with `VfsError::Auth` before any network/client construction.
        let cred = CredentialSecret::Azure(AzureCredential::AzureAd);
        // `ObjectStoreVfs` (the Ok type) has no Debug, so match instead of `.unwrap_err()`.
        let r = azure_connect(
            ConnectionId(0),
            &AzureConnectParams::new("account", "container"),
            &cred,
            "",
        )
        .await;
        assert!(matches!(r, Err(VfsError::Auth)));
    }

    #[tokio::test]
    async fn non_azure_credential_is_rejected() {
        use cairn_vault::SshCredential;
        let cred = CredentialSecret::Ssh(SshCredential::Agent);
        let r = azure_connect(
            ConnectionId(0),
            &AzureConnectParams::new("account", "container"),
            &cred,
            "",
        )
        .await;
        assert!(matches!(r, Err(VfsError::Auth)));
    }
}
