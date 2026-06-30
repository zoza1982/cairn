//! The secret-free credential **boundary** that untrusted layers depend on.
//!
//! `cairn-ai` and `cairn-plugin` depend on this crate — never on `cairn-broker` or `cairn-vault` — so
//! they cannot even *name* a secret-returning API. `Broker` in `cairn-broker` implements
//! [`CredentialDirectory`]; resolving a reference to an actual secret lives there (the execution
//! layer), which the AI/plugin crates do not depend on. A dependency-closure test
//! (`tests/isolation.rs`) enforces that `cairn-vault` never enters those crates' graphs, turning the
//! "AI never sees secrets" property from a convention into a compile-time guarantee. See RFC-0008.
//!
//! [`CredentialBroker`] extends this boundary for the plugin layer (RFC-0010 §4): a plugin names a
//! credential by opaque handle and requests a closed-vocabulary **action** (e.g. "bearer-token");
//! the host resolves the secret internally, performs the action, and returns only an ephemeral
//! *artifact* string — never the raw secret. The method signature contains no `CredentialSecret` so
//! `cairn-vault` is unreachable from `cairn-plugin` even through this trait.

use cairn_types::{CredentialId, CredentialShape};

/// A non-secret summary of a stored credential, safe to show to any actor (including the AI and
/// plugins). Carries an identifier, a human label, and a [`CredentialShape`] (family + variant +
/// whether it delegates) — never secret material.
///
/// `#[non_exhaustive]`: this is the stable boundary type; construct it via [`CredentialInfo::new`] so
/// future fields don't break call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CredentialInfo {
    /// The credential id (a non-secret handle into the vault).
    pub id: CredentialId,
    /// Human-readable label.
    pub label: String,
    /// Non-secret description: backend family, auth variant, and delegation flag.
    pub shape: CredentialShape,
}

impl CredentialInfo {
    /// Construct a non-secret credential summary.
    #[must_use]
    pub fn new(id: CredentialId, label: String, shape: CredentialShape) -> Self {
        Self { id, label, shape }
    }
}

/// The read-only, secret-free view of the credential store.
///
/// This is the only credential API the AI and plugin layers can reach. It exposes *which* credentials
/// exist (by handle + label) so an actor can reference one, without any path to the secret value.
pub trait CredentialDirectory: Send + Sync {
    /// Every stored credential's non-secret summary. Returns an empty list when the vault is locked.
    fn credentials(&self) -> Vec<CredentialInfo>;
}

// ── Plugin credential broker ───────────────────────────────────────────────────────────────────

/// A closed-vocabulary action a plugin may request on a named credential (RFC-0010 §4.2).
///
/// The host validates the action string before calling any credential resolver; an unrecognised
/// string is rejected without touching the vault. Adding a new variant is a minor-version expansion
/// of RFC-0010.
///
/// # Security
///
/// Every action is designed to return only an *ephemeral artifact* derived from the secret, never
/// the raw secret itself. The `CredentialBroker` implementor is responsible for upholding this
/// invariant in each match arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialAction {
    /// Return a short-lived bearer token.
    ///
    /// - `Aws::Static` with a `session_token`: returns the STS session token directly.
    /// - Other AWS, GCP, or Azure delegation credentials: returns an error (live token exchange is
    ///   deferred to M8-5 where the token cache is wired in).
    /// - SSH or unsupported types: returns [`CredentialBrokerError::ActionNotSupported`].
    BearerToken,
    /// Return an HTTP Basic authentication value: `base64(username:password)`.
    ///
    /// - `Aws::Static`: `base64("access_key_id:secret_access_key")`.
    /// - `Ssh::Password`: `base64(":password")` (no username stored in the vault variant).
    /// - Other types: [`CredentialBrokerError::ActionNotSupported`].
    BasicAuthHeader,
}

impl CredentialAction {
    /// Parse an action string from the guest into the closed vocabulary.
    ///
    /// Returns `None` if the string does not correspond to any known action — the caller should
    /// reject the request without touching the vault.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bearer-token" => Some(Self::BearerToken),
            "basic-auth-header" => Some(Self::BasicAuthHeader),
            _ => None,
        }
    }

    /// The canonical string representation used in journal entries.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BearerToken => "bearer-token",
            Self::BasicAuthHeader => "basic-auth-header",
        }
    }
}

/// Errors from a brokered credential action. Never contains secret material.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialBrokerError {
    /// The vault is locked; the credential cannot be resolved.
    #[error("vault is locked")]
    Locked,
    /// No credential with the given handle label exists in the vault.
    #[error("credential not found")]
    NotFound,
    /// The requested action is not supported for the credential type stored under this handle.
    #[error("action not supported for this credential type")]
    ActionNotSupported,
    /// A broker-internal error (e.g. a token exchange failure). The message is secret-free.
    #[error("broker error: {0}")]
    Internal(String),
}

/// The brokered credential capability for the plugin layer (RFC-0010 §4).
///
/// A plugin names a credential by an opaque **handle** (a label string from the vault) and
/// requests a [`CredentialAction`]. The implementation resolves the credential, performs the
/// action *entirely within the host process*, and returns only an ephemeral artifact — never
/// the raw `CredentialSecret`.
///
/// This trait lives in `cairn-broker-api` (the secret-free boundary) so `cairn-plugin` can
/// store and call it via `Arc<dyn CredentialBroker>` without depending on `cairn-vault`.
/// The concrete implementation lives in `cairn-broker` (which does hold the vault).
///
/// # Invariant
///
/// Implementations **must** ensure that no secret material appears in the return value or in
/// any error string. The ephemeral artifact (e.g., a short-lived token or base64 encoding)
/// must be the *minimum* necessary for the guest's declared purpose.
pub trait CredentialBroker: Send + Sync {
    /// Perform a brokered credential action.
    ///
    /// - `actor`: the plugin's canonical name, used for the audit journal.
    /// - `handle`: the credential label to resolve (must be in the plugin's approved grant list;
    ///   the caller is responsible for checking the grant before reaching this method).
    /// - `action`: a closed-vocabulary action from [`CredentialAction`].
    ///
    /// Returns the ephemeral artifact (e.g., `"eyJhbGci..."` for a bearer token). The raw
    /// `CredentialSecret` must not appear in the return value or any error message.
    fn use_credential(
        &self,
        actor: &str,
        handle: &str,
        action: &CredentialAction,
    ) -> Result<String, CredentialBrokerError>;
}
