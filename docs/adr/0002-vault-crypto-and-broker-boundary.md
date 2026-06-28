# ADR-0002: Secrets vault crypto and the AI/plugin broker boundary

- **Status:** Accepted
- **Date:** 2026-06-27
- **Deciders:** Maintainer, with `security-engineer`, `ai-integration-engineer`, `plugin-systems-engineer`

## Context

Cairn stores credentials for many backends and runs an agentic AI plus untrusted WASM plugins. The
bar (PRD §7.4, CLAUDE.md §9/§11): no plaintext secrets on disk ever; secrets redacted in logs and
`Debug`; the AI never sees raw secrets and acts only through the same permissioned layer as the user;
plan→confirm with individual confirmation of irreversible actions. Secrets must also be portable
(hybrid model: encrypted vault + OS keychain + optional external managers).

## Decision

1. **At-rest crypto: XChaCha20-Poly1305**, one vault file with an authenticated header (bound as
   AAD), an encrypted index, and per-entry sealed records each under their own DEK. Atomic writes;
   monotonic `generation` for rollback detection; advisory file lock; versioned format with a forward
   migration chain.
2. **Key hierarchy: unlock-secret → KEK → DEKs.** Default stores a random KEK in the OS keychain
   (`keyring`); fallback derives the KEK via **Argon2id** from a passphrase (auto-offered when no
   keychain, e.g. headless Linux). Auto-lock on idle zeroizes keys.
3. **Memory hygiene:** `secrecy`/`zeroize` types with no `Debug`/`Serialize`; a `tracing` redaction
   layer; best-effort `mlock`/no-core-dump.
4. **The broker boundary:** a dedicated `cairn-broker` is the sole resolver of credential references
   to live secrets and the only caller of vault execution. `cairn-ai` and `cairn-plugin` depend only
   on the broker, speak opaque handles, and have **no tool/host-call that returns or accepts a
   secret**. `broker.authorize` validates handle+scope without mutating; `broker.execute` resolves
   credentials only inside execution. plan→confirm→execute is enforced by the broker/UI (not the
   model); Irreversible/Delete/Exec steps confirm individually. Every action is journaled with its
   actor.

## Consequences

### Positive
- The secret-isolation guarantee is structural (dependency graph + closed tool/host surface), so
  prompt injection or a malicious plugin cannot read or exfiltrate a secret.
- 192-bit nonces make random nonces safe on a frequently-rewritten file with no counter management;
  constant-time in software on all targets (incl. ARM/musl), no AES-NI dependency.
- Per-entry DEKs minimize plaintext in RAM and enable rekey/rotation.

### Negative / trade-offs
- More moving parts (KEK layer, broker indirection) than encrypting entries directly.
- A root attacker on a live, unlocked session can still win — documented honestly in SECURITY.md.
- Plugins must put brokered credentials in headers (not URL-embedded) for token substitution.

## Alternatives considered
- **AES-256-GCM** — 96-bit nonce reuse risk on many rewrites + risky software fallback; rejected as default.
- **`age` as the vault format** — great for portable export (reused there), wrong for a keychain-bound, frequently-rewritten local DB.
- **OS keychain only (no vault)** — not portable across machines, weak on headless Linux; rejected.
- **Letting the AI call the vault with policy checks** — one bug = secret exposure; rejected in favor of structural isolation.

## References
- [LLD](../LLD.md) §9, §10.2, §11.3.
