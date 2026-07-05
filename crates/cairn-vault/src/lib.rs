//! Cairn's encrypted secrets vault.
//!
//! Credentials are stored in a single file encrypted with **XChaCha20-Poly1305** under a key derived
//! from a passphrase via **Argon2id**. The 192-bit nonce makes random nonces safe without
//! coordination; the authenticated header (version + KDF parameters + salt + nonce) is bound as
//! associated data, so tampering or rollback is detected on open. Writes are atomic (temp-file +
//! rename). See `docs/LLD.md` §9 and ADR-0002.
//!
//! The passphrase is supplied through an [`UnlockProvider`]: [`PassphraseUnlockProvider`] (the
//! always-available headless / prompt fallback) or — behind the non-default `keychain` feature
//! (ADR-0006) — `KeychainUnlockProvider`, which reads/writes the passphrase in the OS keychain
//! (Secret Service / macOS Keychain / Windows Credential Manager) per ADR-0002. Auto-lock and the
//! TUI unlock overlay are deferred to M3-7 / app integration.

use cairn_types::{CredentialKind, CredentialShape};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

mod cred;
mod error;
mod unlock;
pub use cairn_secrets::{ExposeSecret, SecretString};
pub use cred::{AwsCredential, AzureCredential, CredentialSecret, GcpCredential, SshCredential};
pub use error::VaultError;
pub use unlock::{open_with, PassphraseUnlockProvider, UnlockError, UnlockProvider};

/// OS-keychain unlock provider (behind the non-default `keychain` feature; see [`UnlockProvider`]).
#[cfg(feature = "keychain")]
pub use unlock::KeychainUnlockProvider;

/// In-memory unlock provider for hermetic tests (behind `cfg(test)` / the `test-utils` feature).
#[cfg(any(test, feature = "test-utils"))]
pub use unlock::MockUnlockProvider;

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
        // Use the no-clobber write so a cross-process race between the `path.exists()` check
        // above and the actual write cannot silently overwrite a populated vault.
        vault.save_new()?;
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

    /// Encrypt and atomically write the vault to disk (clobbering update path).
    ///
    /// # Errors
    /// Serialization, crypto, or I/O errors.
    pub fn save(&self) -> Result<(), VaultError> {
        let bytes = self.seal()?;
        atomic_write(&self.path, &bytes)
    }

    /// Like [`save`] but uses `persist_noclobber` so a cross-process race in the ~100 ms
    /// Argon2id window cannot silently overwrite an existing vault. Only called from
    /// [`create_with_params`].
    fn save_new(&self) -> Result<(), VaultError> {
        let bytes = self.seal()?;
        atomic_create(&self.path, &bytes)
    }

    /// Serialize and encrypt the vault, returning the sealed ciphertext bytes.
    ///
    /// Called by both [`save`] (clobbering) and [`save_new`] (no-clobber initial write).
    fn seal(&self) -> Result<Vec<u8>, VaultError> {
        // Mirror the typed creds into the zeroizing wire form just for serialization; `store` is
        // dropped at the end of this method, wiping the transient secret strings.
        let store = Store {
            creds: self.creds.iter().map(CredentialWire::from).collect(),
        };
        // NB residual: `to_allocvec` grows an internal `Vec`, so any intermediate buffers freed
        // during reallocation are not wiped — a small, defense-in-depth gap (the bytes are freed
        // heap behind the encryption boundary, never logged or persisted).
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
        Ok(bytes)
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

/// Resolve `path`'s parent directory, creating it (and any missing ancestors) if absent.
///
/// The vault file's containing directory (the platform config dir, e.g. `~/.config/cairn`) does
/// not exist on a fresh install, and `NamedTempFile::new_in` fails with a raw I/O error ("io
/// error") if it is missing — so both write paths must ensure it exists first. A directory we
/// create here is set owner-only (`0700`) on Unix, since it holds the encrypted vault; a directory
/// the user already set up is left untouched.
fn ensure_parent_dir(path: &Path) -> Result<PathBuf, VaultError> {
    let dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    // Sampled before the create so the chmod below only tightens a dir we made. Bound under
    // `cfg(unix)` because that block is its only reader — otherwise it's unused on Windows and
    // trips `-D warnings`.
    #[cfg(unix)]
    let newly_created = !dir.exists();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    if newly_created {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort: never fail the write over a permission tweak on a dir we just made.
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), VaultError> {
    use std::io::Write;
    let dir = ensure_parent_dir(path)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| VaultError::Io(e.error))?;
    Ok(())
}

/// Atomic no-clobber write — used only by [`Vault::save_new`] on the create path.
///
/// Uses `persist_noclobber` (on Unix: `link(2)`, atomic; on Windows: `MoveFileExW` without
/// `MOVEFILE_REPLACE_EXISTING`, best-effort) to close the race window between the pre-flight
/// `path.exists()` check in [`Vault::create_with_params`] and the final rename. If the
/// destination already exists the error is mapped to [`VaultError::AlreadyExists`] so callers
/// receive the same sentinel as the pre-flight check, not a raw I/O error.
fn atomic_create(path: &Path, bytes: &[u8]) -> Result<(), VaultError> {
    use std::io::Write;
    let dir = ensure_parent_dir(path)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(path).map_err(|e| {
        if e.error.kind() == std::io::ErrorKind::AlreadyExists {
            VaultError::AlreadyExists
        } else {
            VaultError::Io(e.error)
        }
    })?;
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

    /// Regression: on a fresh install the platform config dir (e.g. `~/.config/cairn`) does not
    /// exist yet, and `Vault::create` used to fail with a raw "io error" because
    /// `NamedTempFile::new_in` can't create a temp file in a nonexistent directory. The create path
    /// must now create the parent directory itself (owner-only `0700` on Unix).
    #[test]
    fn create_succeeds_when_the_parent_directory_is_missing() {
        let base = tempfile::tempdir().unwrap();
        let path = base
            .path()
            .join("does")
            .join("not")
            .join("exist")
            .join("vault.cvlt");
        assert!(!path.parent().unwrap().exists());
        let vault = Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests())
            .expect("create must succeed into a missing dir");
        assert!(path.exists(), "vault file must be written");
        // The created vault directory is owner-only (0700) on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "created vault dir must be 0700");
        }
        drop(vault);
    }

    /// Regression test for the create-path clobber window (Fix 2): `atomic_create` must return
    /// `VaultError::AlreadyExists` and leave the existing file byte-for-byte intact.
    ///
    /// This test accesses the private `atomic_create` function directly (possible from within the
    /// same module) to simulate the narrow race between the pre-flight `path.exists()` check and
    /// the actual write in [`Vault::create_with_params`].
    #[test]
    fn atomic_create_does_not_clobber_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        // Write a sentinel file to act as the "existing vault".
        let sentinel = b"existing-vault-sentinel";
        std::fs::write(&path, sentinel).unwrap();
        // atomic_create must refuse to overwrite it.
        let result = atomic_create(&path, b"would-clobber");
        assert!(
            matches!(result, Err(VaultError::AlreadyExists)),
            "atomic_create must fail with AlreadyExists when target exists, got {result:?}"
        );
        // The existing file must be byte-for-byte unchanged.
        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            after, sentinel,
            "atomic_create must not modify the existing file on failure"
        );
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
    fn aws_static_roundtrips_with_and_without_session_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let (temp_id, perm_id) = {
            let mut v =
                Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
            let temp_id = v.add(
                "sts",
                CredentialSecret::Aws(AwsCredential::Static {
                    access_key_id: "AKIATEMP".to_owned(),
                    secret_access_key: pass("temp-secret"),
                    session_token: Some(pass("session-tok")),
                }),
            );
            let perm_id = v.add(
                "perm",
                CredentialSecret::Aws(AwsCredential::Static {
                    access_key_id: "AKIAPERM".to_owned(),
                    secret_access_key: pass("perm-secret"),
                    session_token: None,
                }),
            );
            v.save().unwrap();
            (temp_id, perm_id)
        };
        let v = Vault::open(&path, &pass("pw")).unwrap();
        assert_eq!(v.get(temp_id).unwrap().kind(), CredentialKind::Aws);
        match v.get(temp_id).unwrap().secret() {
            CredentialSecret::Aws(AwsCredential::Static {
                access_key_id,
                secret_access_key,
                session_token,
            }) => {
                assert_eq!(access_key_id, "AKIATEMP");
                assert_eq!(secret_access_key.expose_secret(), "temp-secret");
                assert_eq!(
                    session_token.as_ref().unwrap().expose_secret(),
                    "session-tok"
                );
            }
            _ => panic!("expected an AWS static credential"),
        }
        match v.get(perm_id).unwrap().secret() {
            CredentialSecret::Aws(AwsCredential::Static { session_token, .. }) => {
                assert!(
                    session_token.is_none(),
                    "absent token must round-trip as None"
                );
            }
            _ => panic!("expected an AWS static credential"),
        }
        let shape = v.get(temp_id).unwrap().shape();
        assert_eq!(shape.variant, "static");
        assert!(!shape.delegation, "static keys are not a delegation");
    }

    #[test]
    fn azure_variants_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let (sk_id, sas_id, ad_id) = {
            let mut v =
                Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
            let sk_id = v.add(
                "sk",
                CredentialSecret::Azure(AzureCredential::SharedKey {
                    account: "devstoreaccount1".to_owned(),
                    key: pass("base64key=="),
                }),
            );
            let sas_id = v.add(
                "sas",
                CredentialSecret::Azure(AzureCredential::SasToken(pass("sv=2021&sig=SECRET"))),
            );
            let ad_id = v.add("ad", CredentialSecret::Azure(AzureCredential::AzureAd));
            v.save().unwrap();
            (sk_id, sas_id, ad_id)
        };
        let v = Vault::open(&path, &pass("pw")).unwrap();
        assert_eq!(v.get(sk_id).unwrap().kind(), CredentialKind::Azure);
        match v.get(sk_id).unwrap().secret() {
            CredentialSecret::Azure(AzureCredential::SharedKey { account, key }) => {
                assert_eq!(account, "devstoreaccount1");
                assert_eq!(key.expose_secret(), "base64key==");
            }
            _ => panic!("expected an Azure shared-key credential"),
        }
        match v.get(sas_id).unwrap().secret() {
            CredentialSecret::Azure(AzureCredential::SasToken(t)) => {
                assert!(t.expose_secret().contains("SECRET"));
            }
            _ => panic!("expected an Azure SAS credential"),
        }
        assert!(matches!(
            v.get(ad_id).unwrap().secret(),
            CredentialSecret::Azure(AzureCredential::AzureAd)
        ));
        assert_eq!(v.get(sk_id).unwrap().shape().variant, "shared-key");
        assert!(!v.get(sk_id).unwrap().shape().delegation);
        assert!(v.get(ad_id).unwrap().shape().delegation);
    }

    #[test]
    fn gcp_service_account_and_adc_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let (sa_id, adc_id) = {
            let mut v =
                Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
            let sa_id = v.add(
                "sa",
                CredentialSecret::Gcp(GcpCredential::ServiceAccountKey(pass(
                    "{\"type\":\"service_account\",\"private_key\":\"SECRET\"}",
                ))),
            );
            let adc_id = v.add(
                "adc",
                CredentialSecret::Gcp(GcpCredential::ApplicationDefault),
            );
            v.save().unwrap();
            (sa_id, adc_id)
        };
        let v = Vault::open(&path, &pass("pw")).unwrap();
        assert_eq!(v.get(sa_id).unwrap().kind(), CredentialKind::Gcp);
        match v.get(sa_id).unwrap().secret() {
            CredentialSecret::Gcp(GcpCredential::ServiceAccountKey(k)) => {
                assert!(k.expose_secret().contains("SECRET"));
            }
            _ => panic!("expected a GCP service-account credential"),
        }
        assert!(matches!(
            v.get(adc_id).unwrap().secret(),
            CredentialSecret::Gcp(GcpCredential::ApplicationDefault)
        ));
        assert_eq!(v.get(sa_id).unwrap().shape().variant, "service-account");
        assert!(!v.get(sa_id).unwrap().shape().delegation);
        assert!(v.get(adc_id).unwrap().shape().delegation);
    }

    #[test]
    fn aws_profile_and_default_chain_are_delegations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        let (prof_id, chain_id) = {
            let mut v =
                Vault::create_with_params(&path, &pass("pw"), KdfParams::fast_for_tests()).unwrap();
            let prof_id = v.add(
                "prof",
                CredentialSecret::Aws(AwsCredential::Profile("dev".to_owned())),
            );
            let chain_id = v.add("chain", CredentialSecret::Aws(AwsCredential::DefaultChain));
            v.save().unwrap();
            (prof_id, chain_id)
        };
        let v = Vault::open(&path, &pass("pw")).unwrap();
        match v.get(prof_id).unwrap().secret() {
            CredentialSecret::Aws(AwsCredential::Profile(name)) => assert_eq!(name, "dev"),
            _ => panic!("expected an AWS profile credential"),
        }
        assert!(matches!(
            v.get(chain_id).unwrap().secret(),
            CredentialSecret::Aws(AwsCredential::DefaultChain)
        ));
        assert!(v.get(prof_id).unwrap().shape().delegation);
        assert!(v.get(chain_id).unwrap().shape().delegation);
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
