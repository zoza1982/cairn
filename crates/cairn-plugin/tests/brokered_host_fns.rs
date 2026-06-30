//! Integration tests for the brokered `host::use-credential` host function (RFC-0010 §4).
//!
//! These tests operate directly on `CredentialBroker` trait implementations rather than going
//! through a real WASM guest, which keeps them fast and independent of the WASM toolchain.
//!
//! HTTP fetch tests live in `src/http_fetch.rs` as `#[cfg(test)]` unit tests (where the
//! WIT-generated `HttpRequest`/`HttpResponse` types are accessible without public re-exports).

use cairn_broker_api::{CredentialAction, CredentialBroker, CredentialBrokerError};

// ── MockBroker ─────────────────────────────────────────────────────────────────────────────────

/// A minimal in-process mock broker for testing. Stores handle → (action → artifact) pairs.
struct MockBroker {
    entries: Vec<(String, Vec<(String, String)>)>,
}

impl MockBroker {
    fn new() -> Self {
        Self { entries: vec![] }
    }

    fn with_entry(mut self, handle: &str, action: &str, artifact: &str) -> Self {
        if let Some((_, actions)) = self.entries.iter_mut().find(|(h, _)| h == handle) {
            actions.push((action.to_owned(), artifact.to_owned()));
        } else {
            self.entries.push((
                handle.to_owned(),
                vec![(action.to_owned(), artifact.to_owned())],
            ));
        }
        self
    }
}

impl CredentialBroker for MockBroker {
    fn use_credential(
        &self,
        _actor: &str,
        handle: &str,
        action: &CredentialAction,
    ) -> Result<String, CredentialBrokerError> {
        let entry = self
            .entries
            .iter()
            .find(|(h, _)| h == handle)
            .ok_or(CredentialBrokerError::NotFound)?;

        entry
            .1
            .iter()
            .find(|(a, _)| a == action.as_str())
            .map(|(_, v)| v.clone())
            .ok_or(CredentialBrokerError::ActionNotSupported)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────────────────────

/// Approved handle + action returns the artifact string.
#[test]
fn use_credential_approved_handle_returns_artifact() {
    let broker = MockBroker::new().with_entry("prod-s3", "bearer-token", "sts-token-abc123");

    let result = broker
        .use_credential("my-plugin", "prod-s3", &CredentialAction::BearerToken)
        .expect("bearer-token for prod-s3");

    assert_eq!(result, "sts-token-abc123");
    assert!(!result.is_empty());
}

/// Handle not registered in the mock → `NotFound`.
#[test]
fn use_credential_unknown_handle_returns_not_found() {
    let broker = MockBroker::new().with_entry("known", "bearer-token", "tok");
    let err = broker
        .use_credential("plugin", "not-known", &CredentialAction::BearerToken)
        .unwrap_err();
    assert!(matches!(err, CredentialBrokerError::NotFound));
}

/// Action not supported for this credential type → `ActionNotSupported`.
#[test]
fn use_credential_unsupported_action_returns_error() {
    // Only `bearer-token` registered; requesting `basic-auth-header` must fail.
    let broker = MockBroker::new().with_entry("cred", "bearer-token", "tok");
    let err = broker
        .use_credential("plugin", "cred", &CredentialAction::BasicAuthHeader)
        .unwrap_err();
    assert!(matches!(err, CredentialBrokerError::ActionNotSupported));
}

/// `CredentialAction::parse` rejects unknown strings before the vault is touched.
#[test]
fn credential_action_parse_rejects_unknown() {
    assert!(CredentialAction::parse("sudo").is_none());
    assert!(CredentialAction::parse("").is_none());
    // Vocabulary is case-sensitive.
    assert!(CredentialAction::parse("BEARER-TOKEN").is_none());
    assert!(CredentialAction::parse("Basic-Auth-Header").is_none());
}

/// `CredentialAction::parse` accepts the two defined actions.
#[test]
fn credential_action_parse_knows_both_actions() {
    assert_eq!(
        CredentialAction::parse("bearer-token"),
        Some(CredentialAction::BearerToken)
    );
    assert_eq!(
        CredentialAction::parse("basic-auth-header"),
        Some(CredentialAction::BasicAuthHeader)
    );
}

/// The artifact returned for `basic-auth-header` must be base64-encoded and must not contain
/// the raw secret. We use a pre-computed base64 value to avoid a `base64` dev-dep here;
/// `cairn-broker`'s unit tests verify the encoding is correct end-to-end.
///
/// `base64(":hunter2") = "Omh1bnRlcjI="` — confirmed via `echo -n ':hunter2' | base64`.
#[test]
fn use_credential_artifact_does_not_contain_raw_secret() {
    let raw_password = "hunter2";
    // The mock broker returns whatever artifact is registered; here we simulate what
    // `BrokerCredentialAdapter` would produce for an `Ssh::Password` credential.
    let encoded_artifact = "Omh1bnRlcjI="; // base64(":hunter2")
    let broker = MockBroker::new().with_entry("my-cred", "basic-auth-header", encoded_artifact);

    let artifact = broker
        .use_credential("plugin", "my-cred", &CredentialAction::BasicAuthHeader)
        .expect("basic-auth-header for my-cred");

    assert_eq!(artifact, encoded_artifact);
    // The raw password must not appear in the returned artifact.
    assert!(
        !artifact.contains(raw_password),
        "artifact must not contain the raw secret: {artifact}"
    );
}

/// When no broker is wired in, the host must return an error without panicking. This is
/// exercised via `CompState::use_credential`'s `None` branch in component.rs, but is also
/// useful to ensure the mock path is clean.
#[test]
fn no_broker_locked_vault_returns_broker_locked_error() {
    // The `Locked` variant is returned when the vault is locked at call time.
    struct LockedBroker;
    impl CredentialBroker for LockedBroker {
        fn use_credential(
            &self,
            _actor: &str,
            _handle: &str,
            _action: &CredentialAction,
        ) -> Result<String, CredentialBrokerError> {
            Err(CredentialBrokerError::Locked)
        }
    }
    let err = LockedBroker
        .use_credential("plugin", "cred", &CredentialAction::BearerToken)
        .unwrap_err();
    assert!(matches!(err, CredentialBrokerError::Locked));
}
