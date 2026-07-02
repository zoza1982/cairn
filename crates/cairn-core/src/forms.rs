//! Connection-form data types: field descriptors, per-scheme field lists, the profile data
//! mirror, and (P5) credential method types and draft carriers.
//!
//! **Design note:** this module deliberately mirrors `cairn_config::ConnectionProfile` without
//! importing it, keeping `cairn-core` free of the `cairn-config` dependency. The effect runner in
//! the binary crate translates between the two representations when saving.
//!
//! **P5 isolation invariant:** `CredentialDraft` holds `SecretString` fields (from `cairn-secrets`),
//! which is already in `cairn-core`'s dependency graph. `cairn-ai` and `cairn-plugin` do *not*
//! depend on `cairn-core`, so adding these types here does not widen their dependency closures.
//! The assembly of a typed `CredentialSecret` (from `cairn-vault`) happens exclusively at the
//! binary edge in `crates/cairn/src/app.rs` — `cairn-vault` is never a transitive dep of this
//! crate. Verified by the cargo-metadata isolation test in `crates/cairn-broker-api/tests/`.

use cairn_secrets::SecretString;
use std::collections::BTreeMap;
use uuid::Uuid;

// ─────────────────────────────────────── FieldSpec ───────────────────────────────────────────

/// Static descriptor for one editable field in the connection form.
///
/// All pointers are `'static` (string literals), so the descriptors live as constants and are
/// never heap-allocated. The renderer reads these to build labelled input boxes; the reducer
/// uses them to validate that required fields are non-empty on submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    /// The `endpoint` map key that will hold this field's value (e.g. `"host"`, `"bucket"`).
    pub key: &'static str,
    /// Short human-readable label shown left of the input box (e.g. `"Host"`).
    pub label: &'static str,
    /// Greyed-out example shown when the field is empty (e.g. `"192.168.1.1"`).
    pub placeholder: &'static str,
    /// Whether the form refuses to submit when this field is empty.
    pub required: bool,
    /// Whether the field holds a secret value that should be masked on screen and stored
    /// in the vault rather than the config file.
    ///
    /// **P4 scope:** this field is forward-declared for P5 (credential provisioning). In P4, no
    /// field has `secret: true`, no value is masked in the form renderer, and no value is stored
    /// in the vault — all fields are stored as plain strings in `ProfileData::endpoint`. A `// P5:`
    /// note below marks where masked storage (a `FieldValue`/`MaskedInput` layer) will be wired in,
    /// and that change will require a `security-review`.
    // P5: add masked storage for `secret: true` fields; introduce `FieldValue { plain: String }` vs
    // `FieldValue { secret: MaskedInput }` and update the renderer to show bullet masks.
    pub secret: bool,
}

// ─────────────────────────────────────── ProfileData ─────────────────────────────────────────

/// A connection profile as the pure core sees it: endpoint data only, no I/O.
///
/// This mirrors `cairn_config::ConnectionProfile` field-for-field so the binary crate can
/// convert losslessly when saving. `secret_ref` is `None` for a newly-created connection
/// (credentials are wired up in P5); on edit it is **preserved** from the existing profile so a
/// saved vault secret is never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileData {
    /// Stable UUID that identifies this profile across config reloads.
    pub id: Uuid,
    /// Scheme identifier matching one of the [`KNOWN_SCHEMES`] entries (e.g. `"ssh"`, `"s3"`).
    pub scheme: String,
    /// Human-readable name shown in the connection switcher.
    pub display_name: String,
    /// Per-field endpoint values keyed by [`FieldSpec::key`] (e.g. `{"host": "prod.example.com"}`).
    pub endpoint: BTreeMap<String, String>,
    /// Optional reference to a vault secret for this connection (P5+). `None` until credentials
    /// are configured; preserved from the existing profile when editing so it is never lost.
    pub secret_ref: Option<Uuid>,
}

// ──────────────────────────────────── Per-scheme field lists ─────────────────────────────────

static SSH_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My SSH server",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "host",
        label: "Host",
        placeholder: "192.168.1.1",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "user",
        label: "User",
        placeholder: "admin",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "port",
        label: "Port",
        placeholder: "22",
        required: false,
        secret: false,
    },
    FieldSpec {
        key: "known_hosts",
        label: "Known-hosts file",
        placeholder: "~/.ssh/known_hosts",
        required: false,
        secret: false,
    },
    FieldSpec {
        key: "host_key",
        label: "Host key check",
        placeholder: "strict|accept-new|off",
        required: false,
        secret: false,
    },
];

static S3_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My S3 bucket",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "bucket",
        label: "Bucket",
        placeholder: "my-bucket",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "region",
        label: "Region",
        placeholder: "us-east-1",
        required: false,
        secret: false,
    },
    FieldSpec {
        key: "endpoint",
        label: "Endpoint URL",
        placeholder: "http://localhost:9000",
        required: false,
        secret: false,
    },
    FieldSpec {
        key: "force_path_style",
        label: "Force path style",
        placeholder: "true (for MinIO/Ceph etc)",
        required: false,
        secret: false,
    },
    FieldSpec {
        key: "root",
        label: "Root prefix",
        placeholder: "prefix/",
        required: false,
        secret: false,
    },
];

static GCS_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My GCS bucket",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "bucket",
        label: "Bucket",
        placeholder: "my-gcs-bucket",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "endpoint",
        label: "Endpoint URL",
        placeholder: "http://localhost:4443",
        required: false,
        secret: false,
    },
    FieldSpec {
        key: "root",
        label: "Root prefix",
        placeholder: "prefix/",
        required: false,
        secret: false,
    },
];

static AZURE_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My Azure container",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "account",
        label: "Storage account",
        placeholder: "mystorageaccount",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "container",
        label: "Container",
        placeholder: "mycontainer",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "endpoint",
        label: "Endpoint URL",
        placeholder: "http://localhost:10000",
        required: false,
        secret: false,
    },
    FieldSpec {
        key: "root",
        label: "Root prefix",
        placeholder: "prefix/",
        required: false,
        secret: false,
    },
];

static LOCAL_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "display_name",
        label: "Name",
        placeholder: "My local directory",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "path",
        label: "Path",
        placeholder: "/home/user/data",
        required: true,
        secret: false,
    },
];

static GENERIC_FIELDS: &[FieldSpec] = &[FieldSpec {
    key: "display_name",
    label: "Name",
    placeholder: "My connection",
    required: true,
    secret: false,
}];

/// Return the ordered list of [`FieldSpec`]s for a scheme. Always includes `display_name` first.
///
/// Returns a generic single-field list for unknown schemes so the form still functions if new
/// backends are added before their field lists land.
#[must_use]
pub fn scheme_fields(scheme: &str) -> &'static [FieldSpec] {
    match scheme {
        "ssh" | "sftp" => SSH_FIELDS,
        "s3" => S3_FIELDS,
        "gcs" => GCS_FIELDS,
        "azure" => AZURE_FIELDS,
        "local" => LOCAL_FIELDS,
        _ => GENERIC_FIELDS,
    }
}

// ──────────────────────────────────── KNOWN_SCHEMES ──────────────────────────────────────────

/// The ordered list of schemes the form's scheme-picker presents.
///
/// Each entry is `(scheme_id, display_label)` — the id is the value stored in
/// `ProfileData::scheme`; the label is shown in the picker list.
pub const KNOWN_SCHEMES: &[(&str, &str)] = &[
    ("ssh", "SSH / SFTP"),
    ("s3", "Amazon S3 / S3-compatible"),
    ("gcs", "Google Cloud Storage"),
    ("azure", "Azure Blob Storage"),
    ("local", "Local directory"),
];

// ─────────────────────────────────── P5: credential types ────────────────────────────────────

/// Detected OS credential-source availability, populated at startup by
/// [`AppEffect::DetectOsSources`](crate::AppEffect::DetectOsSources) and stored in
/// [`AppState::os_sources`](crate::AppState). Used to default the credential method picker
/// to the most-likely-working option for each scheme.
///
/// **Security invariant:** these fields record *presence* only — never secret bytes. The
/// detection reads env-var names and file existence, not file contents or key material.
#[derive(Debug, Clone, Default)]
pub struct OsSources {
    /// `true` if `SSH_AUTH_SOCK` is set in the environment (an SSH agent is reachable).
    pub ssh_agent: bool,
    /// Names of `[profile]` sections in `~/.aws/credentials`, if the file exists.
    /// Empty when the file is absent or unreadable. Never contains key values.
    pub aws_profiles: Vec<String>,
    /// `true` if Application Default Credentials appear to be available —
    /// `GOOGLE_APPLICATION_CREDENTIALS` is set, or `~/.config/gcloud/application_default_credentials.json`
    /// exists.
    pub gcp_adc: bool,
    /// `true` if Azure AD credentials are likely available — a heuristic based on the presence
    /// of `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, or `AZURE_CLIENT_SECRET` environment variables.
    pub azure_ad_likely: bool,
}

/// The authentication method a user has chosen for a connection in the credential-picker stage.
///
/// Grouped by backend scheme. Reference/delegation variants (e.g. [`SshAgent`], [`AwsDefaultChain`])
/// carry no secret material and are the default when the matching OS source is detected.
/// Secret-bearing variants store key material (or a reference to an on-disk file) in the vault.
///
/// [`SshAgent`]: CredentialMethod::SshAgent
/// [`AwsDefaultChain`]: CredentialMethod::AwsDefaultChain
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialMethod {
    // ── SSH ──────────────────────────────────────────────────────────────────────────────────
    /// Delegate to the running SSH agent. No key material is stored in the vault.
    /// Preferred default when `SSH_AUTH_SOCK` is set.
    SshAgent,
    /// Reference an on-disk private-key file by path. Only the path (non-secret) and an
    /// optional passphrase are stored in the vault; the key bytes are read at connect time.
    /// Preferred default when no SSH agent is detected.
    SshPrivateKeyFile,
    /// Paste a PEM-encoded private key inline. The full PEM is stored in the vault.
    SshInlinePem,
    /// Password authentication. The password is stored in the vault.
    SshPassword,

    // ── AWS S3 / S3-compatible ────────────────────────────────────────────────────────────
    /// Delegate to the AWS SDK default provider chain (env vars, shared credentials, EC2
    /// instance metadata, …). No key material is stored in the vault.
    /// Default when no named profile is detected.
    AwsDefaultChain,
    /// Delegate to a named profile in `~/.aws/credentials`. No key material is stored.
    /// Preferred default when at least one profile is detected.
    AwsProfile,
    /// Static access key ID + secret access key (+ optional STS session token).
    AwsStatic,

    // ── GCS (method picker P5; full field capture deferred) ──────────────────────────────
    /// Delegate to Application Default Credentials (ADC). No key material is stored.
    GcpApplicationDefault,
    /// Service-account JSON key file. Field capture is deferred to a future update.
    GcpServiceAccountJson,

    // ── Azure (method picker P5; full field capture deferred) ───────────────────────────
    /// Delegate to the Azure AD / `DefaultAzureCredential` chain. No key material is stored.
    AzureAd,
    /// Storage-account shared key (account name + access key). Field capture deferred.
    AzureSharedKey,
    /// Shared-access-signature (SAS) token. Field capture deferred.
    AzureSasToken,
    /// Connection string (full `AccountName=…;AccountKey=…;…`). Field capture deferred.
    AzureConnectionString,

    // ── Edit mode ────────────────────────────────────────────────────────────────────────
    /// Keep the existing credential (edit mode only). No vault change is made; the profile's
    /// current `secret_ref` is preserved unchanged.
    KeepExisting,
}

impl CredentialMethod {
    /// A short human-readable label for the method picker list.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::SshAgent => "SSH agent (recommended)",
            Self::SshPrivateKeyFile => "Private key file (path reference)",
            Self::SshInlinePem => "Private key (paste PEM inline)",
            Self::SshPassword => "Password",
            Self::AwsDefaultChain => "Default credential chain (env/profile/metadata)",
            Self::AwsProfile => "Named AWS profile",
            Self::AwsStatic => "Static access key",
            Self::GcpApplicationDefault => "Application Default Credentials (ADC)",
            Self::GcpServiceAccountJson => "Service-account JSON key",
            Self::AzureAd => "Azure AD (DefaultAzureCredential)",
            Self::AzureSharedKey => "Shared key (account + access key)",
            Self::AzureSasToken => "SAS token",
            Self::AzureConnectionString => "Connection string",
            Self::KeepExisting => "Keep existing credential (no change)",
        }
    }

    /// Whether this method delegates to an OS source and stores no secret material in the vault.
    ///
    /// `AwsProfile` is intentionally NOT delegation — it requires the user to enter a profile
    /// name in the `CredentialFields` stage, and the name is stored in the vault so the backend
    /// can resolve which named profile to use at connect time.
    #[must_use]
    pub fn is_delegation(&self) -> bool {
        matches!(
            self,
            Self::SshAgent
                | Self::AwsDefaultChain
                | Self::GcpApplicationDefault
                | Self::AzureAd
                | Self::KeepExisting
        )
    }

    /// Whether field capture for this method is deferred to a future update.
    ///
    /// The TUI shows a "coming in a future update" note rather than an empty field list for
    /// deferred methods. The profile is still saved (without vault credentials); the backend
    /// will prompt or fail when the connection is first opened.
    #[must_use]
    pub fn is_deferred_p5(&self) -> bool {
        matches!(
            self,
            Self::GcpServiceAccountJson
                | Self::AzureSharedKey
                | Self::AzureSasToken
                | Self::AzureConnectionString
        )
    }
}

/// Returns the ordered list of [`CredentialMethod`]s for a scheme, from most-preferred
/// (delegation / reference-first) to least-preferred (raw secret copy).
///
/// When `is_edit` is `true`, [`KeepExisting`](CredentialMethod::KeepExisting) is prepended at
/// index 0 so the default action for an edit is to leave credentials unchanged.
///
/// Returns an empty `Vec` for schemes that require no credentials (e.g. `"local"`).
#[must_use]
pub fn credential_methods(scheme: &str, is_edit: bool) -> Vec<CredentialMethod> {
    let mut methods = match scheme {
        "ssh" | "sftp" => vec![
            CredentialMethod::SshAgent,
            CredentialMethod::SshPrivateKeyFile,
            CredentialMethod::SshInlinePem,
            CredentialMethod::SshPassword,
        ],
        "s3" => vec![
            CredentialMethod::AwsDefaultChain,
            CredentialMethod::AwsProfile,
            CredentialMethod::AwsStatic,
        ],
        "gcs" => vec![
            CredentialMethod::GcpApplicationDefault,
            CredentialMethod::GcpServiceAccountJson,
        ],
        "azure" => vec![
            CredentialMethod::AzureAd,
            CredentialMethod::AzureSharedKey,
            CredentialMethod::AzureSasToken,
            CredentialMethod::AzureConnectionString,
        ],
        // Local and unknown schemes require no credential.
        _ => vec![],
    };
    if is_edit && !methods.is_empty() {
        methods.insert(0, CredentialMethod::KeepExisting);
    }
    methods
}

/// Whether a scheme requires credentials (and thus shows the credential stage in the form).
#[must_use]
pub fn scheme_needs_credentials(scheme: &str) -> bool {
    matches!(scheme, "ssh" | "sftp" | "s3" | "gcs" | "azure")
}

/// The default cursor position in the method picker for a scheme, given the detected OS sources.
///
/// Returns 0 (the first method) for unknown schemes or when no strong preference is detectable.
#[must_use]
pub fn default_credential_cursor(scheme: &str, os: &OsSources, is_edit: bool) -> usize {
    // Edit mode always starts on KeepExisting (index 0).
    if is_edit {
        return 0;
    }
    match scheme {
        "ssh" | "sftp" => {
            if os.ssh_agent {
                0 // SshAgent
            } else {
                1 // SshPrivateKeyFile
            }
        }
        "s3" if !os.aws_profiles.is_empty() => 1, // AwsProfile
        "s3" => 0,                                // AwsDefaultChain
        "gcs" => {
            if os.gcp_adc {
                0 // GcpApplicationDefault
            } else {
                1 // GcpServiceAccountJson (deferred, but shown)
            }
        }
        "azure" => {
            if os.azure_ad_likely {
                0 // AzureAd
            } else {
                1 // AzureSharedKey (deferred, but shown)
            }
        }
        _ => 0,
    }
}

// ─────────────────── P5 credential-method field specs ────────────────────────────────────────

static SSH_PRIVATE_KEY_FILE_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "cred_path",
        label: "Key file path",
        placeholder: "~/.ssh/id_ed25519",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "cred_passphrase",
        label: "Passphrase",
        placeholder: "(leave empty if the key is unencrypted)",
        required: false,
        secret: true,
    },
];

static SSH_INLINE_PEM_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "cred_key_pem",
        label: "Private key (PEM / OpenSSH format)",
        placeholder: "-----BEGIN OPENSSH PRIVATE KEY-----",
        required: true,
        secret: true,
    },
    FieldSpec {
        key: "cred_passphrase",
        label: "Passphrase",
        placeholder: "(leave empty if the key is unencrypted)",
        required: false,
        secret: true,
    },
];

static SSH_PASSWORD_FIELDS: &[FieldSpec] = &[FieldSpec {
    key: "cred_password",
    label: "Password",
    placeholder: "•••••••",
    required: true,
    secret: true,
}];

static AWS_PROFILE_FIELDS: &[FieldSpec] = &[FieldSpec {
    key: "cred_profile_name",
    label: "AWS profile name",
    placeholder: "default",
    required: true,
    secret: false,
}];

static AWS_STATIC_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "cred_access_key_id",
        label: "Access key ID",
        placeholder: "AKIAIOSFODNN7EXAMPLE",
        required: true,
        secret: false,
    },
    FieldSpec {
        key: "cred_secret_access_key",
        label: "Secret access key",
        placeholder: "•••••••••••••••••••••••••••••••••••••••",
        required: true,
        secret: true,
    },
    FieldSpec {
        key: "cred_session_token",
        label: "Session token",
        placeholder: "(leave empty — only needed for STS/AssumeRole temporary credentials)",
        required: false,
        secret: true,
    },
];

/// Returns the ordered list of [`FieldSpec`]s the user must fill in for a credential method.
///
/// Returns an empty slice for delegation methods ([`SshAgent`], [`AwsDefaultChain`], etc.) and
/// for deferred methods ([`GcpServiceAccountJson`], [`AzureSharedKey`], …). The TUI decides
/// whether to show a deferred note by calling [`CredentialMethod::is_deferred_p5`].
///
/// [`SshAgent`]: CredentialMethod::SshAgent
/// [`AwsDefaultChain`]: CredentialMethod::AwsDefaultChain
/// [`GcpServiceAccountJson`]: CredentialMethod::GcpServiceAccountJson
/// [`AzureSharedKey`]: CredentialMethod::AzureSharedKey
#[must_use]
pub fn credential_method_fields(method: &CredentialMethod) -> &'static [FieldSpec] {
    match method {
        // Delegation methods and edit-keep: no input required.
        CredentialMethod::SshAgent
        | CredentialMethod::AwsDefaultChain
        | CredentialMethod::GcpApplicationDefault
        | CredentialMethod::AzureAd
        | CredentialMethod::KeepExisting => &[],

        // Fully-implemented field-bearing methods (SSH + AWS).
        CredentialMethod::SshPrivateKeyFile => SSH_PRIVATE_KEY_FILE_FIELDS,
        CredentialMethod::SshInlinePem => SSH_INLINE_PEM_FIELDS,
        CredentialMethod::SshPassword => SSH_PASSWORD_FIELDS,
        CredentialMethod::AwsProfile => AWS_PROFILE_FIELDS,
        CredentialMethod::AwsStatic => AWS_STATIC_FIELDS,

        // Deferred P5 methods: no fields returned; TUI shows a deferred note instead.
        CredentialMethod::GcpServiceAccountJson
        | CredentialMethod::AzureSharedKey
        | CredentialMethod::AzureSasToken
        | CredentialMethod::AzureConnectionString => &[],
    }
}

// ─────────────────────────────── P5: CredentialDraft ─────────────────────────────────────────

/// The credential material collected from the connection form's credential stage.
///
/// This is the pure-core representation of a credential *intent*: it carries non-secret
/// identifiers (file paths, profile names, account names) and [`SecretString`] fields for any
/// raw secrets the user entered. The effect runner in `crates/cairn/src/app.rs` assembles the
/// typed `CredentialSecret` (from `cairn-vault`) from this draft at the binary edge — the
/// only place in the codebase that imports `cairn-vault`.
///
/// ## Security invariants
///
/// - `SecretString`'s `Debug` impl always prints `Secret([REDACTED])`, so a `{:?}` of an
///   `AppEffect` or overlay containing a `CredentialDraft` never leaks key material.
/// - `Clone` is required because `AppEffect` derives `Clone`; it duplicates the `SecretString`
///   heap allocation rather than moving it. This mirrors `AppEffect::UnlockVault`'s pattern.
/// - Neither this type nor its fields are ever passed to `cairn-ai` or `cairn-plugin`.
///
/// `CredentialSecret` (in `cairn-vault`, binary crate only) is assembled from a `CredentialDraft`
/// at the binary edge; it is never a transitive dependency of `cairn-core`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CredentialDraft {
    // ── SSH ──────────────────────────────────────────────────────────────────────────────────
    /// Delegate to the running SSH agent (assembled to `SshCredential::Agent`).
    SshAgent,
    /// Reference an on-disk key file (assembled to `SshCredential::PrivateKeyFile`).
    SshPrivateKeyFile {
        /// Path to the key file (non-secret; stored as text in the vault entry).
        path: String,
        /// Optional passphrase for an encrypted key file.
        passphrase: Option<SecretString>,
    },
    /// Inline PEM key (assembled to `SshCredential::PrivateKey`).
    SshInlinePem {
        /// The PEM/OpenSSH private key text.
        key_pem: SecretString,
        /// Optional passphrase for an encrypted key.
        passphrase: Option<SecretString>,
    },
    /// SSH password (assembled to `SshCredential::Password`).
    SshPassword {
        /// The SSH login password.
        password: SecretString,
    },

    // ── AWS ──────────────────────────────────────────────────────────────────────────────────
    /// Delegate to the AWS SDK default provider chain (assembled to `AwsCredential::DefaultChain`).
    AwsDefaultChain,
    /// Named AWS profile (assembled to `AwsCredential::Profile`).
    AwsProfile {
        /// The profile name from `~/.aws/credentials`.
        profile_name: String,
    },
    /// Static AWS access keys (assembled to `AwsCredential::Static`).
    AwsStatic {
        /// Access key ID (non-secret identifier).
        access_key_id: String,
        /// Secret access key.
        secret_access_key: SecretString,
        /// Optional STS session token (for temporary/assumed-role credentials).
        session_token: Option<SecretString>,
    },

    // ── GCS ──────────────────────────────────────────────────────────────────────────────────
    /// Delegate to Application Default Credentials (assembled to `GcpCredential::ApplicationDefault`).
    GcpApplicationDefault,
    /// Service-account JSON key file (assembled to `GcpCredential::ServiceAccountKey`).
    /// Field capture for this variant is deferred to a future update (P5 follow-up).
    GcpServiceAccountJson {
        /// Full JSON key file contents.
        json: SecretString,
    },

    // ── Azure ─────────────────────────────────────────────────────────────────────────────────
    /// Delegate to Azure AD (assembled to `AzureCredential::AzureAd`).
    AzureAd,
    /// Shared key (assembled to `AzureCredential::SharedKey`). Deferred field capture.
    AzureSharedKey {
        /// Storage account name (non-secret identifier).
        account: String,
        /// Account access key.
        key: SecretString,
    },
    /// SAS token (assembled to `AzureCredential::SasToken`). Deferred field capture.
    AzureSasToken {
        /// The SAS token string.
        token: SecretString,
    },
    /// Connection string (assembled to `AzureCredential::SharedKey` after parsing). Deferred.
    ///
    /// The raw connection string is dropped immediately after the effect runner parses it.
    AzureConnectionString {
        /// Raw connection string (`AccountName=…;AccountKey=…;…`).
        connection_string: SecretString,
    },

    // ── Edit ─────────────────────────────────────────────────────────────────────────────────
    /// Keep the existing credential unchanged (only valid in edit mode).
    ///
    /// The profile's current `secret_ref` is preserved and no vault operation is performed.
    KeepExisting,
}

// ─────────────────────────────────────────── Tests ───────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_fields_ssh_has_host_and_user() {
        let fields = scheme_fields("ssh");
        assert!(fields.iter().any(|f| f.key == "host"));
        assert!(fields.iter().any(|f| f.key == "user"));
    }

    #[test]
    fn scheme_fields_s3_has_bucket() {
        let fields = scheme_fields("s3");
        assert!(fields.iter().any(|f| f.key == "bucket"));
    }

    #[test]
    fn scheme_fields_gcs_has_bucket() {
        let fields = scheme_fields("gcs");
        assert!(fields.iter().any(|f| f.key == "bucket"));
    }

    #[test]
    fn scheme_fields_azure_has_account_and_container() {
        let fields = scheme_fields("azure");
        assert!(fields.iter().any(|f| f.key == "account"));
        assert!(fields.iter().any(|f| f.key == "container"));
    }

    #[test]
    fn scheme_fields_local_has_path() {
        let fields = scheme_fields("local");
        assert!(fields.iter().any(|f| f.key == "path"));
    }

    #[test]
    fn scheme_fields_unknown_returns_generic() {
        let fields = scheme_fields("ftp");
        assert_eq!(fields, GENERIC_FIELDS);
    }

    #[test]
    fn all_schemes_have_display_name_first() {
        for (scheme, _) in KNOWN_SCHEMES {
            let fields = scheme_fields(scheme);
            assert!(!fields.is_empty(), "scheme {scheme} has no fields");
            assert_eq!(
                fields[0].key, "display_name",
                "first field of {scheme} must be display_name"
            );
        }
    }

    #[test]
    fn known_schemes_are_non_empty() {
        assert!(!KNOWN_SCHEMES.is_empty());
        for (id, label) in KNOWN_SCHEMES {
            assert!(!id.is_empty());
            assert!(!label.is_empty());
        }
    }

    /// Every non-display_name key in `scheme_fields` must match a key that `connect/mod.rs`
    /// actually reads. This list is the ground truth extracted from `ssh_params`/`s3_params`/
    /// `gcs_params`/`azure_params`/`root_prefix`/`provider.rs` (2026-07-01). Update it if those
    /// functions change.
    #[test]
    fn scheme_fields_keys_match_connect_reader_keys() {
        // SSH / SFTP
        let ssh_known: &[&str] = &["host", "user", "port", "known_hosts", "host_key"];
        for (scheme, _) in &[("ssh", ""), ("sftp", "")] {
            for f in scheme_fields(scheme)
                .iter()
                .filter(|f| f.key != "display_name")
            {
                assert!(
                    ssh_known.contains(&f.key),
                    "SSH field '{key}' is not read by connect.rs ssh_params — check for key mismatch",
                    key = f.key
                );
            }
        }

        // S3
        let s3_known: &[&str] = &["bucket", "region", "endpoint", "force_path_style", "root"];
        for f in scheme_fields("s3")
            .iter()
            .filter(|f| f.key != "display_name")
        {
            assert!(
                s3_known.contains(&f.key),
                "S3 field '{key}' is not read by connect.rs s3_params — check for key mismatch",
                key = f.key
            );
        }

        // GCS
        let gcs_known: &[&str] = &["bucket", "endpoint", "root"];
        for f in scheme_fields("gcs")
            .iter()
            .filter(|f| f.key != "display_name")
        {
            assert!(
                gcs_known.contains(&f.key),
                "GCS field '{key}' is not read by connect.rs gcs_params — check for key mismatch",
                key = f.key
            );
        }

        // Azure
        let azure_known: &[&str] = &["account", "container", "endpoint", "root"];
        for f in scheme_fields("azure")
            .iter()
            .filter(|f| f.key != "display_name")
        {
            assert!(
                azure_known.contains(&f.key),
                "Azure field '{key}' is not read by connect.rs azure_params — check for key mismatch",
                key = f.key
            );
        }

        // Local
        let local_known: &[&str] = &["path"];
        for f in scheme_fields("local")
            .iter()
            .filter(|f| f.key != "display_name")
        {
            assert!(
                local_known.contains(&f.key),
                "Local field '{key}' is not read by the local provider — check for key mismatch",
                key = f.key
            );
        }
    }

    #[test]
    fn profile_data_roundtrip() {
        let id = Uuid::new_v4();
        let mut ep = BTreeMap::new();
        ep.insert("host".to_owned(), "example.com".to_owned());
        ep.insert("user".to_owned(), "alice".to_owned());
        let profile = ProfileData {
            id,
            scheme: "ssh".to_owned(),
            display_name: "Test SSH".to_owned(),
            endpoint: ep.clone(),
            secret_ref: None,
        };
        assert_eq!(profile.id, id);
        assert_eq!(profile.scheme, "ssh");
        assert_eq!(profile.display_name, "Test SSH");
        assert_eq!(profile.endpoint, ep);
        assert!(profile.secret_ref.is_none());
    }
}
