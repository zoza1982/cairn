//! The secret-free credential **boundary** that untrusted layers depend on.
//!
//! `cairn-ai` and `cairn-plugin` depend on this crate — never on `cairn-broker` or `cairn-vault` — so
//! they cannot even *name* a secret-returning API. `Broker` in `cairn-broker` implements
//! [`CredentialDirectory`]; resolving a reference to an actual secret lives there (the execution
//! layer), which the AI/plugin crates do not depend on. A dependency-closure test
//! (`tests/isolation.rs`) enforces that `cairn-vault` never enters those crates' graphs, turning the
//! "AI never sees secrets" property from a convention into a compile-time guarantee. See RFC-0008.

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
