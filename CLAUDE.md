# Cairn — Engineering Rules & Workflow

These rules govern how work is done in this repository. They apply to every contributor,
human or AI. **IMPORTANT: follow them exactly.** When a request conflicts with these rules,
stop and flag the conflict rather than silently working around it.

Cairn is a modern, cross-platform Rust TUI file manager — a Midnight Commander successor where
every dual-pane is a virtual filesystem backend (local, SSH/SFTP, S3, GCS, Azure Blob, Docker,
Kubernetes), with a secure secrets vault and an agentic AI assistant. See `docs/PRD.md` for
product scope and `docs/` for design docs.

---

## 1. Golden rules (non-negotiable)

1. **Never commit or push directly to `main`.** `main` is protected. All changes land via pull
   request with at least one approving review and green CI.
2. **Every change is a PR.** No exceptions for "small" fixes. One logical change per PR.
3. **Every feature or fix is documented.** Code without docs is incomplete (see §4).
4. **Run the review gates before requesting human review:** `bug-bot` and `code-review` on the
   branch diff after each feature (see §5). Findings are addressed or explicitly deferred with a
   tracked issue.
5. **CI must be green before merge.** fmt, clippy (deny warnings), tests, and build across all
   supported platforms.
6. **No secrets in the repo, ever.** No credentials, tokens, kubeconfigs, or `.env` files. This is
   a secrets-handling tool — we hold ourselves to the highest bar. Secrets are redacted in logs.
7. **Conventional Commits** for every commit and PR title (see §3).
8. **Leave the campsite cleaner than you found it** — but unrelated cleanup goes in its own PR.

---

## 2. Branching model

- `main` — always releasable, protected, linear history (squash-merge only).
- Work branches off `main`, named: `<type>/<short-kebab-summary>` where `<type>` is one of
  `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`, `ci`, `build`.
  - e.g. `feat/s3-backend`, `fix/sftp-symlink-loop`, `docs/lld-vfs-trait`.
- Keep branches short-lived; rebase on `main` rather than letting them drift.
- Delete the branch after merge.

## 3. Commits & PR titles (Conventional Commits)

Format: `type(scope): summary`

- **type:** `feat | fix | docs | refactor | test | chore | perf | ci | build | revert`
- **scope:** the area touched — e.g. `vfs`, `s3`, `k8s`, `docker`, `vault`, `ai`, `tui`, `cli`, `ci`.
- **summary:** imperative mood, lower-case, no trailing period. ≤ 72 chars.
- Breaking changes: add `!` (`feat(vfs)!: ...`) and a `BREAKING CHANGE:` footer.

Examples:
```
feat(s3): add multipart upload with resumable parts
fix(vault): never log decrypted master key
docs(adr): record decision to use WASM for plugins
```

Co-author trailer for AI-assisted commits:
```
Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
```

## 4. Documentation requirements (every PR)

A PR is not complete until its documentation is. Depending on the change, that means:

| Change type | Required documentation |
|-------------|------------------------|
| **Any PR** | A complete PR description (use the template): what, why, how, testing, risks. |
| **New/changed feature** | Update user-facing docs (`README.md` and/or `docs/`); update `CHANGELOG.md` under "Unreleased". |
| **New/changed public API or module** | Rustdoc (`///`) on public items; module-level `//!` docs explaining purpose. |
| **Architectural decision** | An ADR in `docs/adr/` (see template). One decision per file, never edited after acceptance — supersede instead. |
| **Non-trivial design** (new backend, vault crypto, AI layer) | An RFC/design doc in `docs/rfcs/` reviewed *before* large implementation. |
| **Bug fix** | A regression test, and a one-line `CHANGELOG.md` entry. |
| **Behavior/UX change** | Screenshots or ASCII before/after in the PR description. |

Docs live with the code. Stale docs are treated as bugs.

## 5. Quality gates (run after each coding feature, before human review)

Run these on the branch diff and act on the results **before** marking a PR ready:

1. **`bug-bot`** — deep bug/edge-case/security analysis of the diff. Triage every finding:
   fix it, or open a tracked issue and note the deferral in the PR.
2. **`code-review`** — correctness + reuse/simplification/efficiency review of the diff. Apply
   high-confidence cleanups; discuss the rest.
3. For changes touching secrets, auth, crypto, or process execution: also run **`security-review`**.
4. For large/complex PRs: **`deep-review`**.

These are gates, not formalities — a PR with unaddressed high-severity findings does not merge.

## 6. Local checks (must pass before pushing)

```
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo doc --no-deps        # rustdoc must build without warnings
```

CI runs the same on Linux, macOS, and Windows. If a check is intentionally skipped, say so
explicitly in the PR — never silently.

## 7. Testing standards

- New logic ships with tests. Bug fixes ship with a regression test that fails before the fix.
- Prefer fast, deterministic unit tests; gate network/cloud integration tests behind a feature
  flag or env guard so the default `cargo test` is hermetic and offline.
- Never write tests that require real cloud credentials to pass in CI; use mocks/local emulators
  (e.g. MinIO for S3-compatible, kind for k8s) in dedicated integration jobs.

## 8. Code style & safety

- Rust stable, edition pinned via `rust-toolchain.toml`. Format with `rustfmt` (config in repo).
- `#![forbid(unsafe_code)]` by default in every crate. `unsafe` requires an RFC/ADR, a `// SAFETY:`
  comment justifying each block, and reviewer sign-off.
- Errors: libraries use typed errors (`thiserror`); the binary uses `anyhow` at the edges. No
  `unwrap()`/`expect()`/`panic!` on paths reachable from user input or backends — return errors.
- The UI must never block: long/IO operations run async with progress; no blocking calls on the
  render path.
- Secrets are zeroized after use, redacted in logs/Debug, and never passed to the AI layer.
- Match the surrounding code's idioms, naming, and comment density. Comment the *why*, not the *what*.

## 9. Dependencies

- Adding a dependency requires justification in the PR (why this crate, why not std, license, maintenance).
- Licenses must be permissive and compatible with Apache-2.0/MIT (enforced via `cargo-deny`).
- Keep the dependency tree lean; prefer well-maintained, widely-used crates.

## 10. Security & secrets

- Report vulnerabilities per `SECURITY.md` (private disclosure), never via public issues.
- Any change to credential storage, encryption, auth, or command execution requires `security-review`.
- Dependabot and `cargo audit` run in CI; advisories are triaged promptly.

## 11. Releases

- Semantic Versioning. Pre-1.0: minor = features/breaking, patch = fixes.
- `CHANGELOG.md` follows *Keep a Changelog*; entries accrue under "Unreleased" and roll into a
  version on release.
- Releases are tagged from `main`; CI builds and attaches cross-platform binaries.

## 12. For AI assistants working in this repo

- Default to **plan mode** for anything non-trivial; get the plan approved before large edits.
- Always work on a branch; never edit `main` directly. Open a PR via `gh`.
- After implementing a feature, run the §5 gates (`bug-bot`, then `code-review`) on the diff and
  address findings before handing back.
- Fill the PR template completely; update `CHANGELOG.md` and relevant docs in the same PR.
- Keep the PRD high-level; put architecture in `docs/` (LLD) and sequencing in the Implementation Plan.
- Use Conventional Commits and the co-author trailer above.
- When unsure about scope or an irreversible/outward-facing action (pushing, creating releases,
  deleting), confirm first.
