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

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;
use zeroize::Zeroizing;

mod error;
pub use cairn_secrets::SecretString;
pub use error::VaultError;

const MAGIC: &[u8; 8] = b"CAIRNVLT";
const VERSION: u16 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;

/// An opaque credential identifier (stored in non-secret config to reference a vault entry).
pub type CredentialId = Uuid;

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

/// A stored credential. Its secret payload is private and only at-rest-encrypted; access it via
/// [`Credential::expose_secret`]. Does not implement `Debug` to avoid accidental leaks.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    /// Stable identifier.
    pub id: CredentialId,
    /// Human-readable label.
    pub label: String,
    /// Backend family this credential is for (e.g. `"ssh"`, `"s3"`).
    pub backend: String,
    secret: String,
}

impl Credential {
    /// Borrow the secret payload. Callers must not log or persist it in the clear.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.secret
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    creds: Vec<Credential>,
}

/// An unlocked vault held in memory. The key is zeroized on drop.
pub struct Vault {
    path: PathBuf,
    params: KdfParams,
    kek: Zeroizing<[u8; KEY_LEN]>,
    store: Store,
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
            store: Store::default(),
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
        Ok(Self {
            path,
            params: parsed.params,
            kek,
            store,
        })
    }

    /// Add a credential, returning its new id.
    pub fn add(&mut self, label: &str, backend: &str, secret: &SecretString) -> CredentialId {
        use cairn_secrets::ExposeSecret;
        let id = Uuid::new_v4();
        self.store.creds.push(Credential {
            id,
            label: label.to_owned(),
            backend: backend.to_owned(),
            secret: secret.expose_secret().to_owned(),
        });
        id
    }

    /// Fetch a credential by id.
    #[must_use]
    pub fn get(&self, id: CredentialId) -> Option<&Credential> {
        self.store.creds.iter().find(|c| c.id == id)
    }

    /// Remove a credential by id, returning whether it existed.
    pub fn remove(&mut self, id: CredentialId) -> bool {
        let before = self.store.creds.len();
        self.store.creds.retain(|c| c.id != id);
        self.store.creds.len() != before
    }

    /// List `(id, label, backend)` for all credentials — never the secret values.
    #[must_use]
    pub fn labels(&self) -> Vec<(CredentialId, String, String)> {
        self.store
            .creds
            .iter()
            .map(|c| (c.id, c.label.clone(), c.backend.clone()))
            .collect()
    }

    /// The number of stored credentials.
    #[must_use]
    pub fn len(&self) -> usize {
        self.store.creds.len()
    }

    /// Whether the vault holds no credentials.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.store.creds.is_empty()
    }

    /// Encrypt and atomically write the vault to disk.
    ///
    /// # Errors
    /// Serialization, crypto, or I/O errors.
    pub fn save(&self) -> Result<(), VaultError> {
        let plaintext =
            Zeroizing::new(postcard::to_allocvec(&self.store).map_err(|_| VaultError::Format)?);
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
    use cairn_secrets::SecretString;

    fn pass(s: &str) -> SecretString {
        SecretString::from(s.to_owned())
    }

    #[test]
    fn create_add_save_open_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.cvlt");
        let id = {
            let mut v =
                Vault::create_with_params(&path, &pass("hunter2"), KdfParams::fast_for_tests())
                    .unwrap();
            let id = v.add("prod-ssh", "ssh", &pass("super-secret-key"));
            v.save().unwrap();
            id
        };

        let v = Vault::open(&path, &pass("hunter2")).unwrap();
        assert_eq!(v.len(), 1);
        let cred = v.get(id).unwrap();
        assert_eq!(cred.label, "prod-ssh");
        assert_eq!(cred.backend, "ssh");
        assert_eq!(cred.expose_secret(), "super-secret-key");
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
            v.add("x", "s3", &pass("data"));
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
        v.add("a", "ssh", &pass("topsecret"));
        let labels = v.labels();
        assert_eq!(labels.len(), 1);
        let rendered = format!("{labels:?}");
        assert!(!rendered.contains("topsecret"));
    }

    #[test]
    fn remove_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let mut v =
            Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
        let id = v.add("a", "ssh", &pass("k"));
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
}
