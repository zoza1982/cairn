//! Cairn's capability broker.
//!
//! The broker is the **sole mediator** between credential references and live secrets. The AI and
//! plugin layers depend only on `cairn-broker-api` — the secret-free boundary — never on this crate
//! or `cairn-vault`, so they cannot even name a secret-returning API (enforced by a dependency-
//! closure test, see RFC-0008). Only the execution layer holds a [`Broker`] and calls
//! [`Broker::resolve`]; every resolution is recorded in the audit [`Broker::journal`]. The secret-free
//! world view ([`Broker::credentials`] / [`CredentialDirectory`]) is what untrusted actors see.
//!
//! For this milestone the broker provides credential resolution + journaling; the full
//! authorize→confirm→execute action mediation (per ADR-0002 / LLD §9.6) is layered on in M7.

pub use cairn_broker_api::{CredentialDirectory, CredentialInfo};
use cairn_vault::{CredentialId, CredentialSecret, Vault};
use std::sync::Mutex;

/// Who is requesting an action — recorded in the audit journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    /// A direct user action.
    User,
    /// The AI assistant.
    Ai,
    /// A named plugin.
    Plugin(String),
}

/// One audit-journal entry. Never contains secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// The requesting actor.
    pub actor: Actor,
    /// A short, secret-free description of what happened.
    pub action: String,
}

/// Broker errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BrokerError {
    /// The vault is locked; no credentials can be resolved.
    #[error("vault is locked")]
    Locked,
    /// No credential with the given id exists.
    #[error("credential not found")]
    NotFound,
}

/// Mediates access to the vault. Cheap to share behind an `Arc`.
pub struct Broker {
    vault: Mutex<Option<Vault>>,
    journal: Mutex<Vec<JournalEntry>>,
}

impl Broker {
    /// Create a broker around an unlocked vault.
    #[must_use]
    pub fn new(vault: Vault) -> Self {
        Self {
            vault: Mutex::new(Some(vault)),
            journal: Mutex::new(Vec::new()),
        }
    }

    /// Create a broker with no vault (locked).
    #[must_use]
    pub fn locked() -> Self {
        Self {
            vault: Mutex::new(None),
            journal: Mutex::new(Vec::new()),
        }
    }

    /// Whether a vault is currently unlocked.
    #[must_use]
    pub fn is_unlocked(&self) -> bool {
        self.vault.lock().expect("broker mutex").is_some()
    }

    /// Install (or replace) the unlocked vault.
    pub fn unlock(&self, vault: Vault) {
        *self.vault.lock().expect("broker mutex") = Some(vault);
    }

    /// Drop the vault, locking the broker and clearing in-memory secrets.
    pub fn lock(&self) {
        *self.vault.lock().expect("broker mutex") = None;
    }

    /// The secret-free world view: every stored credential's id/label/shape (family + variant +
    /// delegation). Safe to expose to untrusted actors (the AI, plugins).
    #[must_use]
    pub fn credentials(&self) -> Vec<CredentialInfo> {
        let guard = self.vault.lock().expect("broker mutex");
        match guard.as_ref() {
            Some(v) => v
                .infos()
                .into_iter()
                .map(|(id, label, shape)| CredentialInfo::new(id, label, shape))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Resolve a credential reference to its secret value, recording the request in the journal.
    ///
    /// This is the **only** path from a reference to a secret and is intended for the execution
    /// layer (backends performing an authenticated operation), never to be surfaced to the AI/plugin
    /// tool surface. Returns the typed [`CredentialSecret`] so a backend can authenticate with the
    /// right material (key vs password vs agent); the secret's `Secret*` fields zeroize on drop.
    ///
    /// # Errors
    /// [`BrokerError::Locked`] if the vault is locked, [`BrokerError::NotFound`] if the id is unknown.
    pub fn resolve(&self, actor: Actor, id: CredentialId) -> Result<CredentialSecret, BrokerError> {
        let guard = self.vault.lock().expect("broker mutex");
        let vault = guard.as_ref().ok_or(BrokerError::Locked)?;
        let cred = vault.get(id).ok_or(BrokerError::NotFound)?;
        let secret = cred.secret().clone();
        drop(guard);
        self.record(JournalEntry {
            actor,
            action: format!("resolve {id}"),
        });
        Ok(secret)
    }

    /// Append an entry to the audit journal.
    pub fn record(&self, entry: JournalEntry) {
        self.journal.lock().expect("broker mutex").push(entry);
    }

    /// A snapshot of the audit journal.
    #[must_use]
    pub fn journal(&self) -> Vec<JournalEntry> {
        self.journal.lock().expect("broker mutex").clone()
    }
}

/// The secret-free directory view that the AI/plugin layers consume via `cairn-broker-api` without
/// depending on this crate or the vault. Delegates to the inherent [`Broker::credentials`].
impl CredentialDirectory for Broker {
    fn credentials(&self) -> Vec<CredentialInfo> {
        Broker::credentials(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_secrets::{ExposeSecret, SecretString};
    use cairn_vault::{CredentialSecret, KdfParams, SshCredential, Vault};

    fn unlocked_with_one() -> (Broker, CredentialId) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let mut v = Vault::create_with_params(
            &path,
            &SecretString::from("pw".to_owned()),
            KdfParams::fast_for_tests(),
        )
        .unwrap();
        let id = v.add(
            "prod",
            CredentialSecret::Ssh(SshCredential::Password(SecretString::from(
                "AKIAsecret".to_owned(),
            ))),
        );
        // keep the tempdir alive for the test by leaking it; the file is read into the vault already
        std::mem::forget(dir);
        (Broker::new(v), id)
    }

    #[test]
    fn resolve_returns_secret_and_journals() {
        let (broker, id) = unlocked_with_one();
        let secret = broker.resolve(Actor::User, id).unwrap();
        match secret {
            CredentialSecret::Ssh(SshCredential::Password(p)) => {
                assert_eq!(p.expose_secret(), "AKIAsecret");
            }
            _ => panic!("expected an SSH password credential"),
        }
        let journal = broker.journal();
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].actor, Actor::User);
        // The journal entry must not contain the secret.
        assert!(!journal[0].action.contains("AKIAsecret"));
    }

    #[test]
    fn credentials_view_has_no_secret() {
        let (broker, _id) = unlocked_with_one();
        let view = broker.credentials();
        assert_eq!(view.len(), 1);
        let rendered = format!("{view:?}");
        assert!(!rendered.contains("AKIAsecret"));
        assert_eq!(view[0].shape.kind.as_str(), "ssh");
    }

    #[test]
    fn locked_broker_resolves_nothing() {
        let broker = Broker::locked();
        assert!(!broker.is_unlocked());
        // `matches!` (not `unwrap_err`) because the Ok type `CredentialSecret` intentionally has no
        // `Debug` impl — a secret must never be formattable.
        assert!(matches!(
            broker.resolve(Actor::Ai, CredentialId::nil()),
            Err(BrokerError::Locked)
        ));
        assert!(broker.credentials().is_empty());
    }

    #[test]
    fn unknown_id_is_not_found() {
        let (broker, _id) = unlocked_with_one();
        assert!(matches!(
            broker.resolve(Actor::Plugin("x".into()), CredentialId::nil()),
            Err(BrokerError::NotFound)
        ));
    }

    #[test]
    fn lock_clears_access() {
        let (broker, id) = unlocked_with_one();
        broker.lock();
        assert!(matches!(
            broker.resolve(Actor::User, id),
            Err(BrokerError::Locked)
        ));
    }
}
