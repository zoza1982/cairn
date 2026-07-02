# ADR-0009: Reference-First Credential Provisioning

- **Status:** Accepted
- **Date:** 2026-07-01
- **Deciders:** zoza1982

## Context

Cairn needs to let users configure authentication credentials for SSH, S3, GCS, and Azure Blob
connections via the connection form (P5 of RFC-0011). The design must satisfy two competing forces:

1. **Security isolation:** `cairn-core` (the pure TEA reducer, imported by `cairn-ai` and
   `cairn-plugin`) must never transitively depend on `cairn-vault` or `cairn-broker`. A typed
   `CredentialSecret` (no `Debug`, no `Serialize`) must never travel through the `AppEffect` bus
   or appear in AI/plugin context. Secret material must be zeroized after use.

2. **Usability:** Most users already have credentials configured elsewhere — an SSH agent, AWS
   `~/.aws/credentials` profiles, GCP Application Default Credentials, or Azure AD. Requiring
   them to copy key material into Cairn's vault would be redundant and error-prone.

The central design question is: where does raw key material live, and how does it get from the
form to the vault?

## Decision

We adopt a **reference-first** model: delegation sources (OS SSH agent, AWS default credential
chain, GCP ADC, Azure AD) are preferred over raw key material. However, all credential methods
— including delegation — store a vault entry so the connect layer can look up which credential
mechanism to use at runtime. The vault stores only a non-secret marker for delegation methods
(e.g. `SshCredential::Agent`); no private key bytes or passwords are stored.

`AwsProfile` is NOT a delegation method despite using an OS source — it requires the user to
enter a profile name, which is stored in the vault so the backend can resolve the correct named
profile at connect time.

The credential flow is:

```
SchemePicker → Fields → CredentialMethodPicker → CredentialFields → ProvisionAndSaveConnection
```

Secret material flows through these types in strict order:
- **`MaskedInput`** (in `AppState::overlay.cred_fields`) — live user input, zeroizes on drop,
  `Clone` returns empty (prevents silent duplication).
- **`CredentialDraft`** (in `AppEffect::ProvisionAndSaveConnection`) — carries `SecretString`
  fields for non-delegation methods; no `Debug`, no `Serialize`; lives only in `cairn-core`
  and crosses the effect boundary to `cairn` (binary).
- **`CredentialSecret`** (assembled at the binary edge in `cairn/src/app.rs`) — the fully typed
  vault payload; never enters `AppEffect` or `AppState`.

Four additional invariants are enforced:

- **OS-source enumeration reads names/existence only.** `DetectOsSources` checks
  `SSH_AUTH_SOCK`, `~/.aws/credentials` profile names, `GOOGLE_APPLICATION_CREDENTIALS`
  presence, and Azure AD env var presence. It never reads key bytes or secret values. The
  credentials file is read line-by-line via `BufReader` so secret key values never accumulate
  in a heap-allocated `String`.
- **Vault gating.** All methods that write a vault entry gate on vault availability: if the
  vault is locked or absent, the credential draft is pushed into `pending_save` on the
  `VaultUnlock`/`VaultCreate` overlay and executed automatically after successful unlock/create.
  Only `KeepExisting` (preserves existing `secret_ref`) and deferred-P5 methods (no vault entry
  at all) skip this gate.
- **`cred_fields` immutable borrow for validation.** Because `MaskedInput::Clone` returns an
  empty field (by design), validation of required credential fields uses an immutable borrow
  of `cred_fields` rather than a clone — otherwise all secret fields would appear empty.
- **PEM bytes zeroized.** For `PrivateKeyFile` connections, the PEM bytes read from disk are
  wrapped in `zeroize::Zeroizing<String>` and wiped when the stack frame drops, so key material
  does not linger on the heap after `decode_secret_key` has parsed it.

GCS (service-account JSON) and Azure (shared key, SAS token, connection string) non-delegation
methods are P5-present but field-capture-deferred: the user sees a "coming in a future update"
note and the profile is saved without vault credentials, so the backend prompts on first open.

## Consequences

### Positive
- `cairn-core`'s isolation invariant is preserved: `cargo metadata` confirms `cairn-ai` and
  `cairn-plugin` have no transitive dependency on `cairn-vault`, `cairn-broker`, or `cairn-secrets`.
- `CredentialSecret` (no `Debug`) is never visible to the AI or plugin layers.
- Key rotation on disk is reflected immediately for `PrivateKeyFile` connections (key read at
  connect time, not stored in vault); PEM bytes are zeroized immediately after key decode.
- OS-source credential-file enumeration reads only section headers, never key values.

### Negative / trade-offs
- The form now has 4 stages instead of 2, adding UI complexity.
- All methods, including delegation (SSH agent, AWS default chain, GCP ADC, Azure AD), require
  the vault to be open at connection-save time since they store a non-secret marker. A future
  follow-up can implement a vault-free delegation path by encoding the credential method in
  the profile config rather than a vault entry.
- After the credential stage, the form closes (secrets are drained from `cred_fields` via
  `take_secret()`). A failed `ProvisionAndSaveConnection` effect cannot reopen the form with
  secret values intact — the user must re-enter credentials.
- GCS/Azure non-delegation methods are partially deferred, meaning a user who picks "Service
  Account JSON" will save a profile that fails on first connect until a future update.

### Neutral
- `MaskedInput::Clone → empty` is a deliberate safety property, not a bug. The P5 implementation
  relies on immutable borrows for validation to avoid this footgun.

## Alternatives considered

- **Copy-everything to vault** — simpler form logic, but forces users with SSH agents and AWS
  profiles to re-enter credentials that already exist in the OS credential store. Worse UX and
  no security benefit for delegation sources.
- **Inline PEM only, no file reference** — simpler vault storage (just a string), but means
  key rotation requires re-entering the full PEM in Cairn. The `PrivateKeyFile` reference model
  is a strict improvement for users who manage keys on disk.
- **Credential form as a separate overlay** — keeping the 3-stage endpoint form separate from
  the credential stage avoids combining unrelated state in `Overlay::ConnectionForm`. Rejected:
  the 4-stage form shares `scheme`, `values`, and `editing_id` cleanly; separating would
  duplicate ownership of profile data across two overlays.

## References

- RFC-0011 §6 (credential provisioning design)
- ADR-0002 (vault crypto and broker boundary)
- `crates/cairn-core/src/forms.rs` — `CredentialMethod`, `CredentialDraft`, `OsSources`
- `crates/cairn-core/src/update.rs` — `submit_credential_draft`, `assemble_draft`
- `crates/cairn/src/app.rs` — `run_provision_and_save_connection_effect` (binary edge assembly)
