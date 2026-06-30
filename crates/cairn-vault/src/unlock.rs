//! Vault unlock providers — how the passphrase that opens the vault is obtained.
//!
//! [`Vault::open`](crate::Vault::open) takes a passphrase directly; an [`UnlockProvider`] is the
//! seam that *supplies* that passphrase, so callers can plug in different sources without changing
//! the vault. Three implementations ship here:
//!
//! - [`PassphraseUnlockProvider`] — wraps a passphrase supplied at construction (the headless /
//!   prompt-the-user fallback). Always available.
//! - [`KeychainUnlockProvider`] — reads/writes the passphrase in the OS keychain (Secret Service on
//!   Linux, Keychain on macOS, Credential Manager on Windows) via the `keyring` crate. Behind the
//!   non-default `keychain` feature (ADR-0006) so the lean/headless build stays free of the platform
//!   secret-store stack.
//! - [`MockUnlockProvider`] — an in-memory provider for hermetic tests (behind `cfg(test)` or the
//!   `test-utils` feature).
//!
//! Auto-lock and the TUI unlock overlay are deferred to M3-7 / app integration; this module only
//! supplies the passphrase.

use crate::{Vault, VaultError};
use cairn_secrets::SecretString;
use std::path::PathBuf;

/// Supplies the passphrase used to open the vault.
///
/// Implementations may read it from the OS keychain, hold a value supplied by the user, or (in
/// tests) return a fixed value. The trait is object-safe so callers can hold a `&dyn UnlockProvider`
/// (see [`open_with`]).
///
/// Implementations must never log, `Debug`-print, or otherwise expose the passphrase; the returned
/// [`SecretString`] zeroizes on drop.
///
/// **Blocking:** an implementation may perform blocking platform I/O (the keychain provider calls
/// synchronous Secret-Service / Keychain / Credential-Manager APIs). Per CLAUDE.md §9 the UI must
/// never block, so callers on the async/render path must invoke these methods via `spawn_blocking`
/// (or equivalent), not inline. The `Send + Sync` bound lets a `dyn UnlockProvider` be held behind an
/// `Arc` and moved into `spawn_blocking`.
pub trait UnlockProvider: Send + Sync {
    /// Return the vault passphrase, or an error if it can't be obtained.
    ///
    /// # Errors
    /// [`UnlockError::NotFound`] if no passphrase is stored (so the caller can fall back to
    /// prompting); [`UnlockError::Backend`] for an underlying keychain/storage failure.
    fn passphrase(&self) -> Result<SecretString, UnlockError>;

    /// Persist the passphrase for future unlocks (e.g. store it in the OS keychain).
    ///
    /// The default is a no-op, which is correct for providers that do not persist (such as
    /// [`PassphraseUnlockProvider`]). Check [`persists`](UnlockProvider::persists) first if you need
    /// to know whether a call will actually retain the passphrase.
    ///
    /// # Errors
    /// [`UnlockError::Backend`] if persisting fails.
    fn store(&self, _passphrase: &SecretString) -> Result<(), UnlockError> {
        Ok(())
    }

    /// Whether [`store`](UnlockProvider::store) actually persists the passphrase.
    ///
    /// `false` (the default) means `store` is a no-op — the provider holds nothing across runs, so a
    /// caller that "stored" the passphrase must still expect to prompt next time. The keychain
    /// provider returns `true`. This is a capability hint, not a guarantee that a given `store` call
    /// succeeded.
    fn persists(&self) -> bool {
        false
    }
}

/// Errors from obtaining or persisting the vault passphrase. Never carries secret material.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UnlockError {
    /// No stored passphrase was found (e.g. the keychain has no entry yet). The caller can fall back
    /// to prompting the user.
    #[error("no stored passphrase found")]
    NotFound,
    /// The underlying secret store failed. The message is a fixed, secret-free category — it never
    /// embeds the passphrase or raw stored bytes.
    #[error("keychain backend error: {0}")]
    Backend(String),
}

/// An [`UnlockProvider`] that returns a passphrase supplied at construction.
///
/// This is the headless / prompt-the-user fallback: the caller obtains the passphrase (from a TUI
/// prompt, an env var, etc.) and hands it over. [`store`](UnlockProvider::store) is a no-op. The
/// passphrase is held in a [`SecretString`] and zeroized on drop.
pub struct PassphraseUnlockProvider {
    passphrase: SecretString,
}

impl PassphraseUnlockProvider {
    /// Wrap a passphrase supplied by the caller.
    #[must_use]
    pub fn new(passphrase: SecretString) -> Self {
        Self { passphrase }
    }
}

impl UnlockProvider for PassphraseUnlockProvider {
    fn passphrase(&self) -> Result<SecretString, UnlockError> {
        Ok(self.passphrase.clone())
    }
}

/// Open the vault at `path`, obtaining the passphrase from `provider`.
///
/// A small convenience over `provider.passphrase()?` then [`Vault::open`].
///
/// # Blocking
/// This calls [`UnlockProvider::passphrase`] synchronously. Keychain providers block on platform
/// I/O (Secret Service / Keychain / Credential Manager); callers on an async executor must wrap this
/// in `tokio::task::spawn_blocking` (or equivalent) — see [`UnlockProvider`] for details.
///
/// # Errors
/// [`VaultError::Unlock`] if the provider can't supply a passphrase; otherwise the errors of
/// [`Vault::open`] (wrong passphrase, corrupt/unsupported file, I/O).
pub fn open_with(
    path: impl Into<PathBuf>,
    provider: &dyn UnlockProvider,
) -> Result<Vault, VaultError> {
    // `?` converts `UnlockError` into `VaultError::Unlock` via the `#[from]` impl.
    let passphrase = provider.passphrase()?;
    Vault::open(path, &passphrase)
}

// ---------------------------------------------------------------------------
// Keychain provider (behind the `keychain` feature).
// ---------------------------------------------------------------------------

/// The keychain service name under which Cairn stores its vault passphrase.
#[cfg(feature = "keychain")]
const DEFAULT_SERVICE: &str = "cairn";
/// The keychain account name for the vault passphrase entry.
#[cfg(feature = "keychain")]
const DEFAULT_ACCOUNT: &str = "vault";

/// Minimal keystore seam, so the provider's get/set/not-found logic is exercised hermetically in
/// tests against `keyring-core`'s in-memory mock store — without touching the real OS keychain.
#[cfg(feature = "keychain")]
trait Keystore: Send + Sync {
    fn get(&self) -> Result<SecretString, UnlockError>;
    fn set(&self, passphrase: &str) -> Result<(), UnlockError>;
}

/// Map a `keyring` error to an [`UnlockError`].
///
/// `NoEntry` becomes [`UnlockError::NotFound`]. Every other variant becomes a **fixed, secret-free**
/// [`UnlockError::Backend`] message: we deliberately never format the error's payload, because some
/// `keyring` error variants (e.g. `BadEncoding`/`BadDataFormat`) carry the raw stored bytes, which
/// for us would be the passphrase.
#[cfg(feature = "keychain")]
fn map_keyring_err(err: keyring::Error) -> UnlockError {
    use keyring::Error as E;
    match err {
        E::NoEntry => UnlockError::NotFound,
        E::NoDefaultStore => UnlockError::Backend("no OS keychain available".into()),
        E::NoStorageAccess(_) => UnlockError::Backend("keychain locked or inaccessible".into()),
        E::Ambiguous(_) => UnlockError::Backend("multiple matching keychain entries".into()),
        E::NotSupportedByStore(_) => {
            UnlockError::Backend("operation unsupported by keychain".into())
        }
        _ => UnlockError::Backend("keychain access failed".into()),
    }
}

/// The real OS keychain, via the `keyring` crate's platform-default store.
#[cfg(feature = "keychain")]
struct OsKeystore {
    service: String,
    account: String,
}

#[cfg(feature = "keychain")]
impl Keystore for OsKeystore {
    fn get(&self) -> Result<SecretString, UnlockError> {
        let entry = keyring::Entry::new(&self.service, &self.account).map_err(map_keyring_err)?;
        // `get_password` returns an owned `String`; move it straight into the zeroizing
        // `SecretString` (no copy, no lingering plaintext owned elsewhere).
        let password = entry.get_password().map_err(map_keyring_err)?;
        Ok(SecretString::from(password))
    }

    fn set(&self, passphrase: &str) -> Result<(), UnlockError> {
        let entry = keyring::Entry::new(&self.service, &self.account).map_err(map_keyring_err)?;
        entry.set_password(passphrase).map_err(map_keyring_err)
    }
}

/// An [`UnlockProvider`] backed by the OS keychain (Secret Service / macOS Keychain / Windows
/// Credential Manager) via the `keyring` crate.
///
/// [`passphrase`](UnlockProvider::passphrase) reads the stored secret, mapping a missing entry to
/// [`UnlockError::NotFound`] so the caller can fall back to prompting; [`store`](UnlockProvider::store)
/// writes it. The passphrase is never held by the provider between calls — it lives only in the OS
/// store and the transient [`SecretString`] returned to the caller.
///
/// Behind the non-default `keychain` feature (ADR-0006).
///
/// # Security trade-off
/// Keychain unlock removes the Argon2id passphrase barrier from the attacker's path: any process
/// running as the logged-in user (and, with some Linux Secret-Service configurations, other apps in
/// the session) can read the stored passphrase and decrypt the vault. This shifts the vault's
/// security model from "knows the passphrase" to "has the unlocked user session." It is a deliberate,
/// opt-in convenience (feature-gated and explicitly chosen by the caller), not a default.
///
/// # Caveat
/// The `keyring`/Secret-Service code path makes transient plaintext copies of the passphrase (DBus
/// buffers, UTF-8 decode) that this crate cannot zeroize. We move keyring's returned `String`
/// straight into a [`SecretString`] (no extra copy of our own), but the platform-internal copies are
/// outside our control.
#[cfg(feature = "keychain")]
pub struct KeychainUnlockProvider {
    backend: Box<dyn Keystore>,
}

#[cfg(feature = "keychain")]
impl KeychainUnlockProvider {
    /// A provider using Cairn's default keychain service (`"cairn"`) and account (`"vault"`).
    #[must_use]
    pub fn new() -> Self {
        Self::with_service_account(DEFAULT_SERVICE, DEFAULT_ACCOUNT)
    }

    /// A provider using an explicit keychain service and account (e.g. for tests or multiple vaults).
    #[must_use]
    pub fn with_service_account(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            backend: Box::new(OsKeystore {
                service: service.into(),
                account: account.into(),
            }),
        }
    }

    /// Inject a `Keystore` backend (the hermetic mock store in tests). Kept private and test-gated so
    /// the public API stays keychain-only; expressed as a constructor rather than a struct literal so
    /// it stays correct if the struct gains fields.
    #[cfg(test)]
    fn with_backend_for_test(backend: Box<dyn Keystore>) -> Self {
        Self { backend }
    }
}

#[cfg(feature = "keychain")]
impl Default for KeychainUnlockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "keychain")]
impl UnlockProvider for KeychainUnlockProvider {
    fn passphrase(&self) -> Result<SecretString, UnlockError> {
        self.backend.get()
    }

    fn store(&self, passphrase: &SecretString) -> Result<(), UnlockError> {
        use cairn_secrets::ExposeSecret;
        self.backend.set(passphrase.expose_secret())
    }

    fn persists(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Mock provider (tests + the `test-utils` feature).
// ---------------------------------------------------------------------------

/// An in-memory [`UnlockProvider`] for hermetic tests of the unlock flow (no real keychain).
///
/// Available under `cfg(test)` and the `test-utils` feature so dependent crates (e.g. the broker)
/// can exercise unlock without a platform secret store.
#[cfg(any(test, feature = "test-utils"))]
pub struct MockUnlockProvider {
    stored: std::sync::Mutex<Option<SecretString>>,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockUnlockProvider {
    /// An empty provider — [`passphrase`](UnlockProvider::passphrase) returns
    /// [`UnlockError::NotFound`] until [`store`](UnlockProvider::store) is called.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stored: std::sync::Mutex::new(None),
        }
    }

    /// A provider pre-seeded with `passphrase`.
    #[must_use]
    pub fn with_passphrase(passphrase: SecretString) -> Self {
        Self {
            stored: std::sync::Mutex::new(Some(passphrase)),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Default for MockUnlockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl UnlockProvider for MockUnlockProvider {
    fn passphrase(&self) -> Result<SecretString, UnlockError> {
        let guard = self
            .stored
            .lock()
            .map_err(|_| UnlockError::Backend("mock store poisoned".into()))?;
        guard.clone().ok_or(UnlockError::NotFound)
    }

    fn store(&self, passphrase: &SecretString) -> Result<(), UnlockError> {
        let mut guard = self
            .stored
            .lock()
            .map_err(|_| UnlockError::Backend("mock store poisoned".into()))?;
        *guard = Some(passphrase.clone());
        Ok(())
    }

    // The mock retains the passphrase for the rest of its lifetime, so it behaves like a persisting
    // provider from the caller's perspective — `store` then a later `passphrase` round-trips. This
    // lets dependent crates exercise an `if provider.persists() { provider.store(..) }` unlock flow.
    fn persists(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KdfParams;
    use cairn_secrets::ExposeSecret;

    fn pass(s: &str) -> SecretString {
        SecretString::from(s.to_owned())
    }

    #[test]
    fn passphrase_provider_returns_value_and_store_is_noop() {
        let p = PassphraseUnlockProvider::new(pass("hunter2"));
        assert_eq!(p.passphrase().unwrap().expose_secret(), "hunter2");
        // The default `store` is a no-op and must succeed without persisting anything.
        assert!(!p.persists());
        p.store(&pass("ignored")).unwrap();
        assert_eq!(p.passphrase().unwrap().expose_secret(), "hunter2");
    }

    #[test]
    fn mock_provider_not_found_then_store_then_get() {
        let p = MockUnlockProvider::new();
        // The mock behaves like a persisting provider (within its lifetime), so callers can exercise
        // the `if persists() { store() }` unlock-flow guard against it.
        assert!(p.persists());
        assert!(matches!(p.passphrase(), Err(UnlockError::NotFound)));
        p.store(&pass("s3cret")).unwrap();
        assert_eq!(p.passphrase().unwrap().expose_secret(), "s3cret");
    }

    #[test]
    fn mock_provider_with_passphrase_is_preseeded() {
        let p = MockUnlockProvider::with_passphrase(pass("preseed"));
        assert_eq!(p.passphrase().unwrap().expose_secret(), "preseed");
    }

    #[test]
    fn open_with_opens_a_real_vault() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.cvlt");
        // Create a vault, then reopen it through a provider.
        Vault::create_with_params(&path, &pass("open-sesame"), KdfParams::fast_for_tests())
            .unwrap();
        let provider = PassphraseUnlockProvider::new(pass("open-sesame"));
        let v = open_with(&path, &provider).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn open_with_wrong_passphrase_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.cvlt");
        Vault::create_with_params(&path, &pass("right"), KdfParams::fast_for_tests()).unwrap();
        let provider = PassphraseUnlockProvider::new(pass("wrong"));
        assert!(matches!(
            open_with(&path, &provider),
            Err(VaultError::Decrypt)
        ));
    }

    #[test]
    fn open_with_surfaces_not_found_for_prompt_fallback() {
        // The discriminant the prompt-fallback flow matches on: an empty provider yields
        // `VaultError::Unlock(UnlockError::NotFound)` without ever touching the vault file.
        let provider = MockUnlockProvider::new();
        assert!(matches!(
            open_with("/nonexistent/vault.cvlt", &provider),
            Err(VaultError::Unlock(UnlockError::NotFound))
        ));
    }

    #[test]
    fn unlock_error_debug_and_display_carry_no_secret() {
        // Defense-in-depth: neither variant can embed secret material by construction.
        let e = UnlockError::Backend("keychain access failed".into());
        assert!(!format!("{e:?}").contains("secret"));
        assert!(!format!("{e}").is_empty());
    }

    #[test]
    fn providers_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PassphraseUnlockProvider>();
        assert_send_sync::<MockUnlockProvider>();
        // The trait object must satisfy the supertrait bound so it can be held in `Arc` and moved
        // into `spawn_blocking`.
        assert_send_sync::<std::sync::Arc<dyn UnlockProvider>>();
        #[cfg(feature = "keychain")]
        assert_send_sync::<KeychainUnlockProvider>();
    }

    // ---- Keychain provider: hermetic round-trip against keyring-core's in-memory mock store. ----
    // Each `MockKeystore` owns its own `keyring_core::mock::Store`, so there is no process-global
    // state and the test never touches the real OS keychain (no `set_default_store`, no dbus/Secret
    // Service). It exercises the same `keyring::Error::NoEntry -> NotFound` mapping production uses,
    // because `keyring` re-exports its error type from `keyring-core`.
    #[cfg(feature = "keychain")]
    struct MockKeystore {
        store: std::sync::Arc<keyring_core::CredentialStore>,
        service: String,
        account: String,
    }

    #[cfg(feature = "keychain")]
    impl MockKeystore {
        fn new() -> Self {
            let store: std::sync::Arc<keyring_core::CredentialStore> =
                keyring_core::mock::Store::new().expect("mock store");
            Self {
                store,
                service: DEFAULT_SERVICE.to_owned(),
                account: DEFAULT_ACCOUNT.to_owned(),
            }
        }
    }

    #[cfg(feature = "keychain")]
    impl Keystore for MockKeystore {
        fn get(&self) -> Result<SecretString, UnlockError> {
            let entry = self
                .store
                .build(&self.service, &self.account, None)
                .map_err(map_keyring_err)?;
            let password = entry.get_password().map_err(map_keyring_err)?;
            Ok(SecretString::from(password))
        }

        fn set(&self, passphrase: &str) -> Result<(), UnlockError> {
            let entry = self
                .store
                .build(&self.service, &self.account, None)
                .map_err(map_keyring_err)?;
            entry.set_password(passphrase).map_err(map_keyring_err)
        }
    }

    #[cfg(feature = "keychain")]
    fn keychain_provider_with_mock() -> KeychainUnlockProvider {
        KeychainUnlockProvider::with_backend_for_test(Box::new(MockKeystore::new()))
    }

    #[cfg(feature = "keychain")]
    #[test]
    fn keychain_not_found_maps_to_not_found() {
        let p = keychain_provider_with_mock();
        assert!(matches!(p.passphrase(), Err(UnlockError::NotFound)));
    }

    #[cfg(feature = "keychain")]
    #[test]
    fn keychain_store_then_passphrase_round_trips() {
        let p = keychain_provider_with_mock();
        assert!(p.persists());
        p.store(&pass("vault-master-pw")).unwrap();
        assert_eq!(p.passphrase().unwrap().expose_secret(), "vault-master-pw");
    }

    #[cfg(feature = "keychain")]
    #[test]
    fn keychain_open_with_round_trips_a_real_vault() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.cvlt");
        Vault::create_with_params(&path, &pass("kc-pw"), KdfParams::fast_for_tests()).unwrap();
        let p = keychain_provider_with_mock();
        p.store(&pass("kc-pw")).unwrap();
        let v = open_with(&path, &p).unwrap();
        assert!(v.is_empty());
    }

    // The most security-relevant branch: a keyring error variant that carries raw stored bytes
    // (here `BadEncoding`, which would hold our passphrase if it weren't valid UTF-8) must map to a
    // fixed, secret-free `Backend` message — never echoing the bytes. Locks in the no-leak guarantee
    // against future edits to `map_keyring_err`.
    #[cfg(feature = "keychain")]
    #[test]
    fn keychain_byte_carrying_error_does_not_leak_payload() {
        let err = keyring::Error::BadEncoding(b"SUPER-SECRET-BYTES".to_vec());
        let mapped = map_keyring_err(err);
        match mapped {
            UnlockError::Backend(msg) => {
                assert!(!msg.contains("SUPER-SECRET-BYTES"));
                assert_eq!(msg, "keychain access failed");
            }
            UnlockError::NotFound => panic!("a non-NoEntry error must not map to NotFound"),
        }
    }
}
