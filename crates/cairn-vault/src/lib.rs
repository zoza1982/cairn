//! Cairn's encrypted secrets vault.
//!
//! Credentials are stored in a single file encrypted with **XChaCha20-Poly1305** under a key derived
//! from a passphrase via **Argon2id**. The 192-bit nonce makes random nonces safe without
//! coordination; the authenticated header (version + KDF parameters + salt + nonce) is bound as
//! associated data, so tampering or rollback is detected on open. Writes are atomic (temp-file +
//! rename). See `docs/LLD.md` §9 and ADR-0002.
//!
//! OS-keychain unlock (per ADR-0002) is deferred; this milestone implements the passphrase path,
//! which is fully testable and the required fallback on headless hosts.

use cairn_types::{CredentialKind, CredentialShape};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

mod cred;
mod error;
pub use cairn_secrets::{ExposeSecret, SecretString};
pub use cred::{CredentialSecret, SshCredential};
pub use error::VaultError;

const MAGIC: &[u8; 8] = b"CAIRNVLT";
// v2: typed `CredentialSecret` payloads (RFC-0008). v1 (flat string secrets) is not forward-read; the
// vault has no released users, so this is a clean break rather than a migration.
const VERSION: u16 = 2;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;

/// An opaque credential identifier (stored in non-secret config to reference a vault entry).
///
/// Defined in `cairn-types` (the shared leaf) and re-exported here for backwards compatibility; see
/// RFC-0008 for why the id lives below both the vault and the broker-api boundary.
pub use cairn_types::CredentialId;

/// Argon2id key-derivation parameters (stored, authenticated, in the vault header).
#[derive(Debug, Clone, Copy)]
pub struct KdfParams {
    /// Memory cost in KiB.
    pub m_cost: u32,
    /// Iteration (time) cost.
    pub t_cost: u32,
    /// Parallelism (lanes).
    pub p_cost: u32,
    salt: [u8; SALT_LEN],
}

impl KdfParams {
    /// Recommended parameters for interactive use (~19 MiB, 2 passes).
    #[must_use]
    pub fn recommended() -> Self {
        Self {
            m_cost: 19 * 1024,
            t_cost: 2,
            p_cost: 1,
            salt: rand_array(),
        }
    }

    /// Deliberately weak parameters for fast tests. Never use outside tests.
    #[must_use]
    pub fn fast_for_tests() -> Self {
        Self {
            m_cost: 256,
            t_cost: 1,
            p_cost: 1,
            salt: rand_array(),
        }
    }
}

/// A stored credential: a non-secret id/label plus a typed [`CredentialSecret`]. Implements neither
/// `Debug` nor `Serialize` — the `secret` field has neither, and any auto-derived impl would expose
/// secret material. It is persisted only via the zeroizing wire-mirror inside [`Vault::save`], and
/// the secret is wiped from memory when the `Credential` (hence the `Vault`) drops.
#[derive(Clone)]
pub struct Credential {
    /// Stable identifier.
    pub id: CredentialId,
    /// Human-readable label.
    pub label: String,
    secret: CredentialSecret,
}

impl Credential {
    /// The typed secret payload (for the execution layer to authenticate with).
    #[must_use]
    pub fn secret(&self) -> &CredentialSecret {
        &self.secret
    }

    /// The backend family this credential authenticates against.
    #[must_use]
    pub fn kind(&self) -> CredentialKind {
        self.secret.kind()
    }

    /// A non-secret description (family + variant + delegation).
    #[must_use]
    pub fn shape(&self) -> CredentialShape {
        self.secret.shape()
    }
}

/// The on-disk credential record: the serializable, zeroizing mirror of [`Credential`]. The id/label
/// are non-secret (`zeroize(skip)`); the secret rides in the wire form and is wiped on drop.
#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct CredentialWire {
    #[zeroize(skip)]
    id: CredentialId,
    #[zeroize(skip)]
    label: String,
    secret: cred::CredentialSecretWire,
}

impl From<&Credential> for CredentialWire {
    fn from(c: &Credential) -> Self {
        Self {
            id: c.id,
            label: c.label.clone(),
            secret: (&c.secret).into(),
        }
    }
}

impl From<&CredentialWire> for Credential {
    fn from(w: &CredentialWire) -> Self {
        Self {
            id: w.id,
            label: w.label.clone(),
            secret: (&w.secret).into(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Store {
    creds: Vec<CredentialWire>,
}

/// An unlocked vault held in memory. The key and the typed secrets are zeroized on drop.
pub struct Vault {
    path: PathBuf,
    params: KdfParams,
    kek: Zeroizing<[u8; KEY_LEN]>,
    creds: Vec<Credential>,
}

impl Vault {
    /// Create a new vault at `path` with recommended KDF parameters. Fails if the file exists.
    ///
    /// # Errors
    /// [`VaultError::AlreadyExists`] if `path` exists; otherwise I/O or crypto errors.
    pub fn create(path: impl Into<PathBuf>, passphrase: &SecretString) -> Result<Self, VaultError> {
        Self::create_with_params(path, passphrase, KdfParams::recommended())
    }

    /// Create a new vault with explicit KDF parameters (used by tests for speed).
    ///
    /// # Errors
    /// [`VaultError::AlreadyExists`] if `path` exists; otherwise I/O or crypto errors.
    pub fn create_with_params(
        path: impl Into<PathBuf>,
        passphrase: &SecretString,
        params: KdfParams,
    ) -> Result<Self, VaultError> {
        let path = path.into();
        if path.exists() {
            return Err(VaultError::AlreadyExists);
        }
        let kek = derive_kek(passphrase, &params)?;
        let vault = Self {
            path,
            params,
            kek,
            creds: Vec::new(),
        };
        vault.save()?;
        Ok(vault)
    }

    /// Open and decrypt an existing vault with the given passphrase.
    ///
    /// # Errors
    /// [`VaultError::Decrypt`] for a wrong passphrase or tampered file; [`VaultError::Format`] /
    /// [`VaultError::Version`] for an unreadable or unsupported file; otherwise I/O errors.
    pub fn open(path: impl Into<PathBuf>, passphrase: &SecretString) -> Result<Self, VaultError> {
        let path = path.into();
        let bytes = std::fs::read(&path)?;
        let parsed = parse(&bytes)?;
        let kek = derive_kek(passphrase, &parsed.params)?;
        let cipher =
            XChaCha20Poly1305::new_from_slice(kek.as_ref()).map_err(|_| VaultError::Decrypt)?;
        let nonce = XNonce::try_from(&parsed.nonce[..]).map_err(|_| VaultError::Decrypt)?;
        let plaintext = cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: parsed.ciphertext,
                    aad: parsed.header,
                },
            )
            .map_err(|_| VaultError::Decrypt)?;
        let plaintext = Zeroizing::new(plaintext);
        let store: Store = postcard::from_bytes(&plaintext).map_err(|_| VaultError::Format)?;
        // Convert the wire records into typed in-memory credentials, then drop the wire `store`
        // immediately so its transient secret strings are zeroized before we return.
        let creds: Vec<Credential> = store.creds.iter().map(Credential::from).collect();
        drop(store);
        Ok(Self {
            path,
            params: parsed.params,
            kek,
            creds,
        })
    }

    /// Add a typed credential, returning its new id. Call [`Vault::save`] to persist.
    pub fn add(&mut self, label: &str, secret: CredentialSecret) -> CredentialId {
        let id = Uuid::new_v4();
        self.creds.push(Credential {
            id,
            label: label.to_owned(),
            secret,
        });
        id
    }

    /// Fetch a credential by id.
    #[must_use]
    pub fn get(&self, id: CredentialId) -> Option<&Credential> {
        self.creds.iter().find(|c| c.id == id)
    }

    /// Remove a credential by id, returning whether it existed. Call [`Vault::save`] to persist.
    pub fn remove(&mut self, id: CredentialId) -> bool {
        let before = self.creds.len();
        self.creds.retain(|c| c.id != id);
        self.creds.len() != before
    }

    /// List `(id, label, shape)` for all credentials — never the secret values. The
    /// [`CredentialShape`] gives the family/variant/delegation for display, without exposing material.
    #[must_use]
    pub fn infos(&self) -> Vec<(CredentialId, String, CredentialShape)> {
        self.creds
            .iter()
            .map(|c| (c.id, c.label.clone(), c.shape()))
            .collect()
    }

    /// The number of stored credentials.
    #[must_use]
    pub fn len(&self) -> usize {
        self.creds.len()
    }

    /// Whether the vault holds no credentials.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.creds.is_empty()
    }

    /// Encrypt and atomically write the vault to disk.
    ///
    /// # Errors
    /// Serialization, crypto, or I/O errors.
    pub fn save(&self) -> Result<(), VaultError> {
        // Mirror the typed creds into the zeroizing wire form just for serialization; `store` is
        // dropped at the end of this method, wiping the transient secret strings.
        let store = Store {
            creds: self.creds.iter().map(CredentialWire::from).collect(),
        };
        // The final buffer is `Zeroizing`. NB residual: `to_allocvec` grows an internal `Vec`, so any
        // intermediate buffers freed during reallocation are not wiped — a small, defense-in-depth
        // gap (the bytes are freed heap behind the encryption boundary, never logged or persisted).
        let plaintext =
            Zeroizing::new(postcard::to_allocvec(&store).map_err(|_| VaultError::Format)?);
        let nonce: [u8; NONCE_LEN] = rand_array();
        let header = build_header(&self.params, &nonce);
        let cipher = XChaCha20Poly1305::new_from_slice(self.kek.as_ref())
            .map_err(|_| VaultError::Decrypt)?;
        let xnonce = XNonce::try_from(&nonce[..]).map_err(|_| VaultError::Decrypt)?;
        let ciphertext = cipher
            .encrypt(
                &xnonce,
                Payload {
                    msg: &plaintext,
                    aad: &header,
                },
            )
            .map_err(|_| VaultError::Decrypt)?;

        let mut bytes = header;
        bytes.extend_from_slice(&ciphertext);
        atomic_write(&self.path, &bytes)
    }
}

fn derive_kek(
    passphrase: &SecretString,
    params: &KdfParams,
) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    use argon2::{Algorithm, Argon2, Params, Version};
    use cairn_secrets::ExposeSecret;
    let p = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(KEY_LEN))
        .map_err(|_| VaultError::Kdf)?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, p);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(
            passphrase.expose_secret().as_bytes(),
            &params.salt,
            key.as_mut(),
        )
        .map_err(|_| VaultError::Kdf)?;
    Ok(key)
}

/// The authenticated, cleartext header: MAGIC | version | m_cost | t_cost | p_cost | salt | nonce.
fn build_header(params: &KdfParams, nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    let mut h = Vec::with_capacity(8 + 2 + 12 + SALT_LEN + NONCE_LEN);
    h.extend_from_slice(MAGIC);
    h.extend_from_slice(&VERSION.to_le_bytes());
    h.extend_from_slice(&params.m_cost.to_le_bytes());
    h.extend_from_slice(&params.t_cost.to_le_bytes());
    h.extend_from_slice(&params.p_cost.to_le_bytes());
    h.extend_from_slice(&params.salt);
    h.extend_from_slice(nonce);
    h
}

struct Parsed<'a> {
    params: KdfParams,
    nonce: [u8; NONCE_LEN],
    header: &'a [u8],
    ciphertext: &'a [u8],
}

fn parse(bytes: &[u8]) -> Result<Parsed<'_>, VaultError> {
    let header_len = 8 + 2 + 12 + SALT_LEN + NONCE_LEN;
    if bytes.len() < header_len {
        return Err(VaultError::Format);
    }
    if &bytes[0..8] != MAGIC {
        return Err(VaultError::Format);
    }
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    if version != VERSION {
        return Err(VaultError::Version(version));
    }
    let m_cost = u32::from_le_bytes(bytes[10..14].try_into().expect("4 bytes"));
    let t_cost = u32::from_le_bytes(bytes[14..18].try_into().expect("4 bytes"));
    let p_cost = u32::from_le_bytes(bytes[18..22].try_into().expect("4 bytes"));
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&bytes[22..22 + SALT_LEN]);
    let mut nonce = [0u8; NONCE_LEN];
    let nonce_start = 22 + SALT_LEN;
    nonce.copy_from_slice(&bytes[nonce_start..nonce_start + NONCE_LEN]);
    Ok(Parsed {
        params: KdfParams {
            m_cost,
            t_cost,
            p_cost,
            salt,
        },
        nonce,
        header: &bytes[..header_len],
        ciphertext: &bytes[header_len..],
    })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), VaultError> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| VaultError::Io(e.error))?;
    Ok(())
}

fn rand_array<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS RNG unavailable");
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_secrets::{ExposeSecret, SecretString};

    fn pass(s: &str) -> SecretString {
        SecretString::from(s.to_owned())
    }

    /// An SSH password credential, for tests.
    fn ssh_pw(s: &str) -> CredentialSecret {
        CredentialSecret::Ssh(SshCredential::Password(SecretString::from(s.to_owned())))
    }

    #[test]
    fn create_add_save_open_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.cvlt");
        let id = {
            let mut v =
                Vault::create_with_params(&path, &pass("hunter2"), KdfParams::fast_for_tests())
                    .unwrap();
            let id = v.add("prod-ssh", ssh_pw("super-secret-key"));
            v.save().unwrap();
            id
        };

        let v = Vault::open(&path, &pass("hunter2")).unwrap();
        assert_eq!(v.len(), 1);
        let cred = v.get(id).unwrap();
        assert_eq!(cred.label, "prod-ssh");
        assert_eq!(cred.kind(), CredentialKind::Ssh);
        // The typed secret round-trips through seal/open.
        match cred.secret() {
            CredentialSecret::Ssh(SshCredential::Password(p)) => {
                assert_eq!(p.expose_secret(), "super-secret-key");
            }
            _ => panic!("expected an SSH password credential"),
        }
    }

    #[test]
    fn wrong_passphrase_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        Vault::create_with_params(&path, &pass("right"), KdfParams::fast_for_tests()).unwrap();
        assert!(matches!(
            Vault::open(&path, &pass("wrong")),
            Err(VaultError::Decrypt)
        ));
    }

    #[test]
    fn tampering_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        {
            let mut v =
                Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
            v.add("x", ssh_pw("data"));
            v.save().unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF; // flip a ciphertext byte
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Vault::open(&path, &pass("pw")),
            Err(VaultError::Decrypt)
        ));
    }

    #[test]
    fn header_tampering_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a salt byte (in the header, bound as AAD) — decryption must fail.
        bytes[24] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        assert!(Vault::open(&path, &pass("pw")).is_err());
    }

    #[test]
    fn labels_do_not_expose_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let mut v =
            Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
        v.add("a", ssh_pw("topsecret"));
        let infos = v.infos();
        assert_eq!(infos.len(), 1);
        let rendered = format!("{infos:?}");
        assert!(!rendered.contains("topsecret"));
        assert_eq!(infos[0].2.kind, CredentialKind::Ssh);
    }

    #[test]
    fn remove_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let mut v =
            Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
        let id = v.add("a", ssh_pw("k"));
        assert!(v.remove(id));
        assert!(!v.remove(id));
        assert!(v.is_empty());
    }

    #[test]
    fn create_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
        assert!(matches!(
            Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()),
            Err(VaultError::AlreadyExists)
        ));
    }

    #[test]
    fn private_key_roundtrips_with_and_without_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let (with_id, without_id) = {
            let mut v =
                Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
            let with_id = v.add(
                "encrypted-key",
                CredentialSecret::Ssh(SshCredential::PrivateKey {
                    key_pem: SecretString::from("PEMBODY".to_owned()),
                    passphrase: Some(SecretString::from("kp".to_owned())),
                }),
            );
            let without_id = v.add(
                "bare-key",
                CredentialSecret::Ssh(SshCredential::PrivateKey {
                    key_pem: SecretString::from("PEMBODY2".to_owned()),
                    passphrase: None,
                }),
            );
            v.save().unwrap();
            (with_id, without_id)
        };
        let v = Vault::open(&path, &pass("pw")).unwrap();
        match v.get(with_id).unwrap().secret() {
            CredentialSecret::Ssh(SshCredential::PrivateKey {
                key_pem,
                passphrase,
            }) => {
                assert_eq!(key_pem.expose_secret(), "PEMBODY");
                assert_eq!(passphrase.as_ref().unwrap().expose_secret(), "kp");
            }
            _ => panic!("expected a private-key credential"),
        }
        match v.get(without_id).unwrap().secret() {
            CredentialSecret::Ssh(SshCredential::PrivateKey {
                key_pem,
                passphrase,
            }) => {
                assert_eq!(key_pem.expose_secret(), "PEMBODY2");
                assert!(
                    passphrase.is_none(),
                    "no passphrase must round-trip as None"
                );
            }
            _ => panic!("expected a private-key credential"),
        }
        assert_eq!(v.get(with_id).unwrap().shape().variant, "private-key");
        assert!(!v.get(with_id).unwrap().shape().delegation);
    }

    #[test]
    fn agent_roundtrips_as_delegation_with_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let id = {
            let mut v =
                Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
            let id = v.add("agent", CredentialSecret::Ssh(SshCredential::Agent));
            v.save().unwrap();
            id
        };
        let v = Vault::open(&path, &pass("pw")).unwrap();
        assert!(matches!(
            v.get(id).unwrap().secret(),
            CredentialSecret::Ssh(SshCredential::Agent)
        ));
        let shape = v.get(id).unwrap().shape();
        assert_eq!(shape.variant, "agent");
        assert!(shape.delegation, "agent is a delegation variant");
    }

    #[test]
    fn sealed_file_does_not_contain_plaintext_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let mut v =
            Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
        v.add("k", ssh_pw("PLAINTEXT-NEEDLE"));
        v.save().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        // The on-disk blob is encrypted, so the secret must not appear in cleartext.
        assert!(
            bytes
                .windows(b"PLAINTEXT-NEEDLE".len())
                .all(|w| w != b"PLAINTEXT-NEEDLE"),
            "plaintext secret leaked into the sealed file"
        );
    }

    #[test]
    fn old_format_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // Rewrite the version field (bytes 8..10) to the old v1; `parse` must reject it before any
        // decrypt attempt.
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Vault::open(&path, &pass("pw")),
            Err(VaultError::Version(1))
        ));
    }
}
