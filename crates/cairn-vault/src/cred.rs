//! Typed credential payloads (RFC-0008).
//!
//! [`CredentialSecret`] is the in-memory secret payload. It deliberately implements neither `Debug`,
//! `Display`, nor `Serialize`/`Deserialize`, so a secret can never be logged or serialized by
//! accident — the **only** serializable form is the `pub(crate)` zeroizing wire-mirror in this
//! module, used solely inside the vault's seal/open path. Its `Secret*` fields zeroize on drop, so an
//! unlocked secret is wiped from memory when the `Vault` is dropped (e.g. on lock).
//!
//! New backend families are added as `#[non_exhaustive]` variants in their own milestone PRs (M5/M6);
//! M3-4 ships the SSH variant that M4 consumes.

use cairn_secrets::{ExposeSecret, SecretString};
use cairn_types::{CredentialKind, CredentialShape};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// A typed credential secret. No `Debug`/`Display`/`Serialize` — see the module docs. `Clone` is
/// allowed so the broker can hand a copy to a connecting backend; the clone's `Secret*` fields still
/// zeroize on drop.
///
/// The absence of `Debug` is a load-bearing security property (a secret must never be formattable
/// into a log line). This is enforced at compile time:
///
/// ```compile_fail
/// fn _must_not_compile(c: &cairn_vault::CredentialSecret) {
///     let _ = format!("{c:?}"); // CredentialSecret has no Debug impl — by design.
/// }
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub enum CredentialSecret {
    /// An SSH/SFTP credential.
    Ssh(SshCredential),
    /// An AWS / S3-compatible credential.
    Aws(AwsCredential),
    /// A Google Cloud (GCS) credential.
    Gcp(GcpCredential),
    /// An Azure Blob Storage credential.
    Azure(AzureCredential),
}

impl CredentialSecret {
    /// The backend family this secret authenticates against.
    #[must_use]
    pub fn kind(&self) -> CredentialKind {
        match self {
            Self::Ssh(_) => CredentialKind::Ssh,
            Self::Aws(_) => CredentialKind::Aws,
            Self::Gcp(_) => CredentialKind::Gcp,
            Self::Azure(_) => CredentialKind::Azure,
        }
    }

    /// A non-secret description (family + variant + whether it delegates).
    #[must_use]
    pub fn shape(&self) -> CredentialShape {
        match self {
            Self::Ssh(s) => {
                CredentialShape::new(CredentialKind::Ssh, s.variant(), s.is_delegation())
            }
            Self::Aws(a) => {
                CredentialShape::new(CredentialKind::Aws, a.variant(), a.is_delegation())
            }
            Self::Gcp(g) => {
                CredentialShape::new(CredentialKind::Gcp, g.variant(), g.is_delegation())
            }
            Self::Azure(a) => {
                CredentialShape::new(CredentialKind::Azure, a.variant(), a.is_delegation())
            }
        }
    }
}

/// SSH authentication material (per RFC-0003 / the M4 design).
#[derive(Clone)]
#[non_exhaustive]
pub enum SshCredential {
    /// Password authentication.
    Password(SecretString),
    /// Public-key authentication; `key_pem` is an OpenSSH/PEM private key, optionally encrypted.
    PrivateKey {
        /// The private key in PEM/OpenSSH form.
        key_pem: SecretString,
        /// Passphrase, if the key is encrypted.
        passphrase: Option<SecretString>,
    },
    /// Reference an on-disk private-key file. The private-key *bytes* are read from disk at
    /// connect time and never stored in the vault — only the file path (non-secret) and the
    /// optional decryption passphrase are kept in the vault entry.
    ///
    /// This is the preferred form for locally-managed keys: the vault holds a *reference*, not
    /// a copy, so key rotation on disk is reflected immediately at next connect.
    PrivateKeyFile {
        /// Path to the private key file (non-secret; stored in clear in the vault entry).
        path: std::path::PathBuf,
        /// Passphrase for an encrypted key file, if any.
        passphrase: Option<SecretString>,
    },
    /// Delegate to the running SSH agent — no key material is stored in the vault.
    Agent,
}

impl SshCredential {
    fn variant(&self) -> &'static str {
        match self {
            Self::Password(_) => "password",
            Self::PrivateKey { .. } => "private-key",
            Self::PrivateKeyFile { .. } => "private-key-file",
            Self::Agent => "agent",
        }
    }

    fn is_delegation(&self) -> bool {
        // `Agent` delegates to the running SSH agent — no key material in the vault.
        // `PrivateKeyFile` stores an optional passphrase in the vault (not a pure delegation),
        // but keeps the key bytes on disk; classified as non-delegation for the shape descriptor.
        matches!(self, Self::Agent)
    }
}

/// AWS authentication material for S3 and S3-compatible stores (per the M5 design). Covers static
/// keys (incl. S3-compatible services like MinIO) plus delegation to the SDK's own credential
/// resolution (a named profile, or the full default provider chain).
#[derive(Clone)]
#[non_exhaustive]
pub enum AwsCredential {
    /// Static access keys. `session_token` is set for temporary (STS) credentials.
    Static {
        /// Access key ID — an identifier (like a username), not secret material.
        access_key_id: String,
        /// Secret access key.
        secret_access_key: SecretString,
        /// Optional STS session token, for temporary credentials.
        session_token: Option<SecretString>,
    },
    /// Delegate to a named profile in the shared AWS config/credentials files. No key material is
    /// stored in the vault.
    Profile(String),
    /// Delegate to the SDK's default provider chain (environment, shared profile, container/instance
    /// metadata, …). No key material is stored in the vault.
    DefaultChain,
}

impl AwsCredential {
    fn variant(&self) -> &'static str {
        match self {
            Self::Static { .. } => "static",
            Self::Profile(_) => "profile",
            Self::DefaultChain => "default-chain",
        }
    }

    fn is_delegation(&self) -> bool {
        matches!(self, Self::Profile(_) | Self::DefaultChain)
    }
}

/// Google Cloud authentication material for GCS (per the M5 design). Either an explicit
/// service-account key (the JSON key file is the whole secret) or delegation to Application Default
/// Credentials (env `GOOGLE_APPLICATION_CREDENTIALS`, `gcloud` login, or the instance metadata
/// server) — which stores no key material in the vault.
#[derive(Clone)]
#[non_exhaustive]
pub enum GcpCredential {
    /// A service-account key: the full JSON key-file contents (entirely secret).
    ServiceAccountKey(SecretString),
    /// Delegate to Application Default Credentials. No key material is stored in the vault.
    ApplicationDefault,
}

impl GcpCredential {
    fn variant(&self) -> &'static str {
        match self {
            Self::ServiceAccountKey(_) => "service-account",
            Self::ApplicationDefault => "application-default",
        }
    }

    fn is_delegation(&self) -> bool {
        matches!(self, Self::ApplicationDefault)
    }
}

/// Azure Blob Storage authentication material (per the M5 design). A storage-account shared key, a
/// SAS token, or delegation to Azure AD (the `DefaultAzureCredential` chain — env, managed identity,
/// `az login`, …), which stores no key material in the vault.
#[derive(Clone)]
#[non_exhaustive]
pub enum AzureCredential {
    /// Storage-account shared key: the account name (a non-secret identifier) plus its access key.
    SharedKey {
        /// Storage account name (an identifier, not secret material).
        account: String,
        /// Account access key.
        key: SecretString,
    },
    /// A shared-access-signature token (the whole token is secret).
    SasToken(SecretString),
    /// Delegate to the Azure AD credential chain. No key material is stored in the vault.
    AzureAd,
}

impl AzureCredential {
    fn variant(&self) -> &'static str {
        match self {
            Self::SharedKey { .. } => "shared-key",
            Self::SasToken(_) => "sas-token",
            Self::AzureAd => "azure-ad",
        }
    }

    fn is_delegation(&self) -> bool {
        matches!(self, Self::AzureAd)
    }
}

// ---------------------------------------------------------------------------
// Wire-mirror: the ONLY serializable form. Zeroized on drop. `pub(crate)` so it cannot escape the
// vault; conversions copy through `expose_secret`/re-wrap exactly at the seal/open boundary.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub(crate) enum CredentialSecretWire {
    Ssh(SshWire),
    Aws(AwsWire),
    Gcp(GcpWire),
    Azure(AzureWire),
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub(crate) enum SshWire {
    Password(String),
    PrivateKey {
        key_pem: String,
        passphrase: Option<String>,
    },
    /// Wire-mirror of [`SshCredential::PrivateKeyFile`]. The path is stored as a UTF-8 string;
    /// non-UTF-8 paths are lossy-converted on seal and round-trip cleanly because the form only
    /// accepts user-typed paths (always valid UTF-8).
    PrivateKeyFile {
        path: String,
        passphrase: Option<String>,
    },
    Agent,
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub(crate) enum AwsWire {
    Static {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    Profile(String),
    DefaultChain,
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub(crate) enum GcpWire {
    ServiceAccountKey(String),
    ApplicationDefault,
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub(crate) enum AzureWire {
    SharedKey { account: String, key: String },
    SasToken(String),
    AzureAd,
}

// The exhaustive matches in these `From` impls are the sync guard between the public and wire enums:
// adding a `CredentialSecret`/`SshCredential`/`AwsCredential`/`GcpCredential` variant without its
// wire counterpart is a compile error.
impl From<&CredentialSecret> for CredentialSecretWire {
    fn from(c: &CredentialSecret) -> Self {
        match c {
            CredentialSecret::Ssh(s) => CredentialSecretWire::Ssh(SshWire::from(s)),
            CredentialSecret::Aws(a) => CredentialSecretWire::Aws(AwsWire::from(a)),
            CredentialSecret::Gcp(g) => CredentialSecretWire::Gcp(GcpWire::from(g)),
            CredentialSecret::Azure(a) => CredentialSecretWire::Azure(AzureWire::from(a)),
        }
    }
}

impl From<&GcpCredential> for GcpWire {
    fn from(g: &GcpCredential) -> Self {
        match g {
            GcpCredential::ServiceAccountKey(k) => {
                GcpWire::ServiceAccountKey(k.expose_secret().to_owned())
            }
            GcpCredential::ApplicationDefault => GcpWire::ApplicationDefault,
        }
    }
}

impl From<&AzureCredential> for AzureWire {
    fn from(a: &AzureCredential) -> Self {
        match a {
            AzureCredential::SharedKey { account, key } => AzureWire::SharedKey {
                account: account.clone(),
                key: key.expose_secret().to_owned(),
            },
            AzureCredential::SasToken(t) => AzureWire::SasToken(t.expose_secret().to_owned()),
            AzureCredential::AzureAd => AzureWire::AzureAd,
        }
    }
}

impl From<&AwsCredential> for AwsWire {
    fn from(a: &AwsCredential) -> Self {
        match a {
            AwsCredential::Static {
                access_key_id,
                secret_access_key,
                session_token,
            } => AwsWire::Static {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.expose_secret().to_owned(),
                session_token: session_token.as_ref().map(|t| t.expose_secret().to_owned()),
            },
            AwsCredential::Profile(p) => AwsWire::Profile(p.clone()),
            AwsCredential::DefaultChain => AwsWire::DefaultChain,
        }
    }
}

impl From<&SshCredential> for SshWire {
    fn from(s: &SshCredential) -> Self {
        // Exhaustive match: adding a new `SshCredential` variant without its wire counterpart is
        // a compile error — keeping the two types in sync at compile time.
        match s {
            SshCredential::Password(p) => SshWire::Password(p.expose_secret().to_owned()),
            SshCredential::PrivateKey {
                key_pem,
                passphrase,
            } => SshWire::PrivateKey {
                key_pem: key_pem.expose_secret().to_owned(),
                passphrase: passphrase.as_ref().map(|p| p.expose_secret().to_owned()),
            },
            SshCredential::PrivateKeyFile { path, passphrase } => SshWire::PrivateKeyFile {
                // to_string_lossy is lossless for paths entered through the form (always UTF-8);
                // non-UTF-8 OS paths are an accepted known gap for future improvement.
                path: path.to_string_lossy().into_owned(),
                passphrase: passphrase.as_ref().map(|p| p.expose_secret().to_owned()),
            },
            SshCredential::Agent => SshWire::Agent,
        }
    }
}

impl From<&CredentialSecretWire> for CredentialSecret {
    fn from(w: &CredentialSecretWire) -> Self {
        match w {
            CredentialSecretWire::Ssh(s) => CredentialSecret::Ssh(SshCredential::from(s)),
            CredentialSecretWire::Aws(a) => CredentialSecret::Aws(AwsCredential::from(a)),
            CredentialSecretWire::Gcp(g) => CredentialSecret::Gcp(GcpCredential::from(g)),
            CredentialSecretWire::Azure(a) => CredentialSecret::Azure(AzureCredential::from(a)),
        }
    }
}

impl From<&GcpWire> for GcpCredential {
    fn from(w: &GcpWire) -> Self {
        match w {
            GcpWire::ServiceAccountKey(k) => {
                GcpCredential::ServiceAccountKey(SecretString::from(k.clone()))
            }
            GcpWire::ApplicationDefault => GcpCredential::ApplicationDefault,
        }
    }
}

impl From<&AzureWire> for AzureCredential {
    fn from(w: &AzureWire) -> Self {
        match w {
            AzureWire::SharedKey { account, key } => AzureCredential::SharedKey {
                account: account.clone(),
                key: SecretString::from(key.clone()),
            },
            AzureWire::SasToken(t) => AzureCredential::SasToken(SecretString::from(t.clone())),
            AzureWire::AzureAd => AzureCredential::AzureAd,
        }
    }
}

impl From<&AwsWire> for AwsCredential {
    fn from(w: &AwsWire) -> Self {
        match w {
            AwsWire::Static {
                access_key_id,
                secret_access_key,
                session_token,
            } => AwsCredential::Static {
                access_key_id: access_key_id.clone(),
                secret_access_key: SecretString::from(secret_access_key.clone()),
                session_token: session_token
                    .as_ref()
                    .map(|t| SecretString::from(t.clone())),
            },
            AwsWire::Profile(p) => AwsCredential::Profile(p.clone()),
            AwsWire::DefaultChain => AwsCredential::DefaultChain,
        }
    }
}

impl From<&SshWire> for SshCredential {
    fn from(w: &SshWire) -> Self {
        // Exhaustive match: adding a new `SshWire` variant without its credential counterpart is
        // a compile error — keeping the two types in sync at compile time.
        match w {
            SshWire::Password(p) => SshCredential::Password(SecretString::from(p.clone())),
            SshWire::PrivateKey {
                key_pem,
                passphrase,
            } => SshCredential::PrivateKey {
                key_pem: SecretString::from(key_pem.clone()),
                passphrase: passphrase.as_ref().map(|p| SecretString::from(p.clone())),
            },
            SshWire::PrivateKeyFile { path, passphrase } => SshCredential::PrivateKeyFile {
                path: std::path::PathBuf::from(path),
                passphrase: passphrase.as_ref().map(|p| SecretString::from(p.clone())),
            },
            SshWire::Agent => SshCredential::Agent,
        }
    }
}
