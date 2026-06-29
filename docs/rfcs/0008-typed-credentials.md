# RFC-0008: Typed per-backend credentials, keychain unlock, and the broker-api split

- **Status:** Accepted — §3 (broker-api split) implemented in M3-4 PR-A; §1/§2/§4/§5 pending
- **Author(s):** security-engineer (synthesized), software-architect
- **Date:** 2026-06-29
- **Tracking item:** M3-4 (typed credential variants), M3-3 (OS keychain + auto-lock), M3-7 (vault-unlock TUI)

## Summary

The live backends (SSH first, then S3/GCS/Azure, Docker, Kubernetes) need real credentials. Today
the vault stores a single flat `Credential { backend: String, secret: String }` and the broker's
`resolve` returns a bare `SecretString` — too weak to drive SSH key-vs-password-vs-agent auth or a
cloud SDK's credential provider, and the in-memory secret is a non-zeroized `String`.

This RFC defines:

1. A **typed `CredentialSecret` enum** (per backend, secret-bearing + delegation variants) sealed in
   the vault, with a non-secret `CredentialKind`/`CredentialShape` tag in `cairn-types`.
2. A **`cairn-broker-api` crate split** so `cairn-ai`/`cairn-plugin` can describe credentials but
   *cannot name* a secret-returning API — turning the "AI never sees secrets" property from a
   convention into a **compile-time** guarantee (enforced by a dependency-closure CI test).
3. **M3-3** OS-keychain unlock via an `UnlockProvider` abstraction with a hermetic CI fallback.
4. **M3-7** the vault-unlock TUI trigger points.

## Motivation / drift being corrected

- `Broker::resolve(..) -> SecretString` is a public inherent method on a crate `cairn-ai` **already
  depends on** (verified in `crates/cairn-ai/Cargo.toml`). The LLD §9.6 claim "the AI cannot name the
  vault" is therefore necessary-but-not-sufficient today; §3 below makes it true.
- The stored secret is a plain `String` (not zeroized while unlocked) and `Credential` derives
  `Serialize` (a secret-bearing type is serializable at all). §1.3 fixes this with a `pub(crate)`
  Zeroize wire-mirror as the *only* serializable form.
- **Out of scope / tracked separately:** the vault is a single-blob AEAD today, not the per-entry
  DEKs / encrypted index / generation / `.bak`+lock that ADR-0002 and the plan describe. The typed
  model works on the current single blob; the per-entry hardening is its own issue (don't smuggle it
  into the M3-4 PR — keeps the security-review surface bounded; CLAUDE.md §9).

## Design

### 1. The typed credential model (M3-4)

**Crate placement is the security property:**

- `CredentialSecret` + all per-backend secret variants live in **`cairn-vault`** (the only crate
  allowed to hold plaintext secret material; already depends on `cairn-secrets`), sealed into the
  existing `Store`.
- Non-secret tags `CredentialKind` / `CredentialShape` live in **`cairn-types`** (the universal leaf)
  so `cairn-config`/`cairn-broker-api`/UI can describe a credential without depending on the vault.
- The secret-free directory type `CredentialInfo` lives in **`cairn-broker-api`** (§3).

```rust
// crates/cairn-vault/src/cred.rs — secret-bearing; NOT Debug/Display/Serialize on the public type.
#[non_exhaustive]
pub enum CredentialSecret {
    Ssh(SshCredential), Aws(AwsCredential), Gcp(GcpCredential),
    Azure(AzureCredential), Kubernetes(K8sCredential), Docker(DockerCredential),
}
```

Per-backend shapes (full detail in the security design notes; load-bearing rules summarized here):

- **SSH** (first consumer, drives RFC-0003 auth): `SshAuth::{Password, PrivateKey{key,passphrase},
  Agent, AgentSock{path}}` + `HostKeyPolicy::{Strict{known_hosts}, Pinned(fingerprints), AcceptNew}`
  — **no persisted "accept any"**; host-key pins may be sealed in the record (tamper-resistant).
- **AWS**: `AccessKey{id,secret,session_token}` | `Profile` | `Sso` | `AssumeRole` | `DefaultChain`.
- **GCP**: `ServiceAccountKey(bytes)` | `Adc` | `Impersonate`.
- **Azure**: `AccountKey` | `Sas` | `Aad{Default|ClientSecret|ManagedIdentity}`.
- **Kubernetes**: `Kubeconfig{data}` | `KubeconfigRef{path}` | `Token` | `ClientCert{cert,key}` |
  `ExecPlugin{...}`.
- **Docker**: `Socket(path)` | `TlsClient{ca,cert,key}` | `RegistryAuth` | `CredentialHelper`.

Cross-cutting rules baked into the variants:
- **The private key is the secret; cert/CA are public** — TLS variants split them (`Vec<u8>` cert/ca,
  `SecretBox<[u8]>` key) so a partial Debug can't carry key bytes.
- **Delegation variants carry no long-lived secret** — only *how to obtain one at use time*
  (`Agent`, `Profile`, `Sso`, `DefaultChain`, `Adc`, `KubeconfigRef`, `ExecPlugin`, `CredentialHelper`,
  `Socket`). Prefer them. They are flagged `delegation: true` in the directory view.
- **`ExecPlugin` / `CredentialHelper` are arbitrary command execution** → MUST route through the
  ADR-0005 command-execution safety (argv-only, no shell, env-scrubbed, explicit cwd, output capped,
  wall-clock timeout + process-group kill, file-trust gate on the helper path). This is the sharpest
  new attack surface in M3-4 and is its own security-review checkpoint.

**Sealing without making secrets serializable (§1.3):** the public `CredentialSecret` derives no
`Serialize`/`Debug`/`Display` (so `serde_json::to_string`/`format!("{:?}")` simply don't compile). A
`pub(crate)` `CredentialSecretWire` (deriving `Serialize`/`Deserialize`/`Zeroize`/`ZeroizeOnDrop`,
with `Vec<u8>`/`String` leaves) is the only serializable form, used only inside the AEAD seal/open
path; the `postcard` buffer is already `Zeroizing`. This also replaces the plaintext `secret: String`
with typed `Secret*` fields.

**`TokenCache`:** in-memory only (never written to the vault), keyed by `(CredentialId, scope)`, holds
short-lived derived secrets (STS/SSO/AAD/SAS/exec output), refreshed near expiry, **zeroized on lock**.

### 2. Resolve flow at connect time

`Broker::resolve(actor, id) -> CredentialSecret` (typed). The backend matches its own family and
authenticates; delegation variants are wired into the SDK's own provider (the broker does not
re-implement each cloud's chain); broker-mintable tokens go through `TokenCache`. The materialized
secret is held only long enough to establish the session, then dropped/zeroized. `cairn-backend-ssh`
stays dependency-light (`cairn-vfs`/`cairn-types` only) — the broker boundary wraps the connect call
in the binary's effect runner; the connector is a plain `async fn ssh_connect(params, cred)`.

### 3. The `cairn-broker-api` split (compile-time AI/plugin isolation)

```rust
// crates/cairn-broker-api — the ONLY thing cairn-ai / cairn-plugin depend on.
pub trait CredentialDirectory: Send + Sync {
    fn credentials(&self) -> Vec<CredentialInfo>;   // secret-free: id, label, shape, delegation, expiry?
}
```

`Broker` (with `resolve` returning `CredentialSecret`, and the `cairn-vault` dependency) stays in
`cairn-broker`, which `cairn-ai`/`cairn-plugin` **do not depend on**. They receive
`Arc<dyn CredentialDirectory>`; they cannot upcast it to `&Broker`, so `resolve` is unreachable. A CI
test on `cargo metadata` **fails if `cairn-vault` ever enters the dependency closure of `cairn-ai` or
`cairn-plugin`**, converting the isolation convention into a guarantee.

### 4. M3-3 — keychain unlock + hermetic fallback

An `UnlockProvider` yields/stores the 32-byte **KEK** (so swapping unlock methods re-wraps one key,
not the whole vault):

- `KeychainProvider` — `keyring` (4.x / keyring-core); `use_native_store()` once at startup; stores
  the raw KEK (no KDF on unlock).
- `PassphraseProvider` — the existing Argon2id path; auto-offered when no keychain backend exists
  (headless Linux). Re-calibrate `KdfParams::recommended()` toward the LLD §9.2 floor (decision in PR).
- `MockUnlockProvider` + `keyring_core::mock::Store` for hermetic unit tests; an env-passphrase
  provider for integration jobs **behind an explicit `--insecure-env-passphrase` opt-in** so it can
  never be reached in a normal build. Default `cargo test` needs no real keychain and no prompt.

**Auto-lock** is the broker's job: `Broker::lock()` drops the `Vault` (zeroizing KEK/DEKs) and the
`TokenCache`; driven from the TEA loop on an idle `Tick` (default 15 min, configurable). Lock is
instant in-memory state — never blocks the render path.

### 5. M3-7 — vault-unlock TUI

`Overlay::VaultUnlock { pending: ConnectIntent }` in the existing overlay stack. A credentialed
connect on a locked broker does not error — it parks the intent and raises the overlay; on success
the loop re-dispatches the connect. First-run offers create (passphrase or "store key in this
machine's keychain"). Method-aware prompt (keychain confirm vs no-echo passphrase field). The
credential list shows labels + `CredentialShape` only; reveal-secret is deferred (LLD §16).

## Test strategy (all hermetic, default `cargo test`)

| Invariant | Guard |
|---|---|
| No secret in `Debug`/`Serialize` | public types don't derive them → `compile_fail` doctests |
| AI/plugin cannot reach a secret | `cargo metadata` test: `cairn-vault` ∉ `cairn-ai`/`cairn-plugin` closure; `compile_fail` that `directory.resolve(..)` doesn't exist |
| No secret in logs/journal/panic | extend `cairn-secrets::redact` tests; journal carries id+kind only; typed errors (no `unwrap`/`panic`) on tamper |
| Delegation variants store no secret | per-variant seal/open round-trip asserts no `Secret*` leaf |
| Exec-plugin can't become RCE | reuse ADR-0005 safety; world-writable helper refused |
| Keys zeroized on lock | after `lock()`, `TokenCache` empty + `resolve` returns `Locked` |

## Rollout

1. `cairn-types`: `CredentialKind`/`CredentialShape`. `cairn-broker-api` crate (trait + `CredentialInfo`).
2. `cairn-vault`: `CredentialSecret` + wire-mirror + `UnlockProvider` + `TokenCache`; typed `Credential`.
3. `cairn-broker`: `resolve -> CredentialSecret`; implement `CredentialDirectory`; lock zeroizes cache.
   Repoint `cairn-ai`/`cairn-plugin` to `cairn-broker-api`; add the dep-closure CI test.
4. `cairn-config`: `ConnectionProfile.kind: Option<CredentialKind>`.
5. M3-3 providers + auto-lock; M3-7 overlay. Each step is security-reviewed.

SSH (RFC-0003 / M4) consumes `CredentialSecret::Ssh` immediately after step 3.

## Open questions

1. Keep the typed model on the single-blob `Store` for M3-4, per-entry DEKs separately? (Recommend yes.)
2. Adopt `cairn-broker-api` now vs in-crate trait split deferred? (Recommend the crate now — guarantee, not convention.)
3. Re-calibrate `KdfParams::recommended()` to the §9.2 floor now or separately?
4. Host-key pins sealed in the record vs referenced `known_hosts` file — default for `SshCredential`.
5. Migrate today's flat `Credential` vs clean break (no real users yet) — recommend migrate (versioned header exists).
