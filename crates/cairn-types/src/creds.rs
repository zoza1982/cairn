//! Non-secret credential descriptors.
//!
//! The secret material itself lives only in `cairn-vault` (the typed `CredentialSecret`); these tags
//! describe a credential's *family* and *variant* so the UI, config, broker-api, and AI can reason
//! about a credential without ever touching a secret. See RFC-0008.

/// The backend family a credential authenticates against. Non-secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CredentialKind {
    /// SSH / SFTP.
    Ssh,
    /// AWS / S3 (and S3-compatible).
    Aws,
    /// Google Cloud Storage.
    Gcp,
    /// Azure Blob Storage.
    Azure,
    /// Kubernetes.
    Kubernetes,
    /// Docker / OCI.
    Docker,
}

impl CredentialKind {
    /// A short, stable, lower-case display name (e.g. `"ssh"`, `"aws"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            Self::Aws => "aws",
            Self::Gcp => "gcp",
            Self::Azure => "azure",
            Self::Kubernetes => "kubernetes",
            Self::Docker => "docker",
        }
    }
}

impl std::fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A non-secret description of a stored credential: its family, the specific auth variant, and
/// whether it *delegates* authentication to an external authority (an agent, an SDK provider chain, a
/// credential helper) rather than holding long-lived secret material. Safe to show to any actor.
///
/// `#[non_exhaustive]`: a stable boundary type — construct via [`CredentialShape::new`] so future
/// fields don't break call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CredentialShape {
    /// The backend family.
    pub kind: CredentialKind,
    /// The specific variant, e.g. `"password"`, `"private-key"`, `"agent"`.
    pub variant: &'static str,
    /// Whether the credential delegates auth (and therefore stores no long-lived secret).
    pub delegation: bool,
}

impl CredentialShape {
    /// Construct a non-secret credential descriptor.
    #[must_use]
    pub fn new(kind: CredentialKind, variant: &'static str, delegation: bool) -> Self {
        Self {
            kind,
            variant,
            delegation,
        }
    }
}
