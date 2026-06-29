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
}

impl CredentialSecret {
    /// The backend family this secret authenticates against.
    #[must_use]
    pub fn kind(&self) -> CredentialKind {
        match self {
            Self::Ssh(_) => CredentialKind::Ssh,
        }
    }

    /// A non-secret description (family + variant + whether it delegates).
    #[must_use]
    pub fn shape(&self) -> CredentialShape {
        match self {
            Self::Ssh(s) => {
                CredentialShape::new(CredentialKind::Ssh, s.variant(), s.is_delegation())
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
    /// Delegate to the running SSH agent — no key material is stored in the vault.
    Agent,
}

impl SshCredential {
    fn variant(&self) -> &'static str {
        match self {
            Self::Password(_) => "password",
            Self::PrivateKey { .. } => "private-key",
            Self::Agent => "agent",
        }
    }

    fn is_delegation(&self) -> bool {
        matches!(self, Self::Agent)
    }
}

// ---------------------------------------------------------------------------
// Wire-mirror: the ONLY serializable form. Zeroized on drop. `pub(crate)` so it cannot escape the
// vault; conversions copy through `expose_secret`/re-wrap exactly at the seal/open boundary.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub(crate) enum CredentialSecretWire {
    Ssh(SshWire),
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub(crate) enum SshWire {
    Password(String),
    PrivateKey {
        key_pem: String,
        passphrase: Option<String>,
    },
    Agent,
}

// The exhaustive matches in these `From` impls are the sync guard between the public and wire enums:
// adding a `CredentialSecret`/`SshCredential` variant without its wire counterpart is a compile error.
impl From<&CredentialSecret> for CredentialSecretWire {
    fn from(c: &CredentialSecret) -> Self {
        match c {
            CredentialSecret::Ssh(s) => CredentialSecretWire::Ssh(SshWire::from(s)),
        }
    }
}

impl From<&SshCredential> for SshWire {
    fn from(s: &SshCredential) -> Self {
        match s {
            SshCredential::Password(p) => SshWire::Password(p.expose_secret().to_owned()),
            SshCredential::PrivateKey {
                key_pem,
                passphrase,
            } => SshWire::PrivateKey {
                key_pem: key_pem.expose_secret().to_owned(),
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
        }
    }
}

impl From<&SshWire> for SshCredential {
    fn from(w: &SshWire) -> Self {
        match w {
            SshWire::Password(p) => SshCredential::Password(SecretString::from(p.clone())),
            SshWire::PrivateKey {
                key_pem,
                passphrase,
            } => SshCredential::PrivateKey {
                key_pem: SecretString::from(key_pem.clone()),
                passphrase: passphrase.as_ref().map(|p| SecretString::from(p.clone())),
            },
            SshWire::Agent => SshCredential::Agent,
        }
    }
}
