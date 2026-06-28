# Contributing to Cairn

Thanks for your interest in contributing! This document explains how we work. The full
engineering rules live in [`CLAUDE.md`](CLAUDE.md) — this is the human-friendly summary.

## Ground rules

- **`main` is protected.** Never push to it directly. Every change lands via pull request with a
  passing CI run and at least one approving review.
- **One logical change per PR.** Keep them focused and reviewable.
- **Document as you go.** A PR isn't done until its docs are (see below).
- **Be kind.** See the [Code of Conduct](CODE_OF_CONDUCT.md).

## Getting set up

```sh
git clone https://github.com/zoza1982/cairn.git
cd cairn
cargo build --workspace
cargo test --all-features
```

The Rust toolchain version is pinned in [`rust-toolchain.toml`](rust-toolchain.toml).

## Workflow

1. **Open or claim an issue** describing the change. For non-trivial design, write an RFC in
   `docs/rfcs/` and get feedback before building.
2. **Branch off `main`:** `git switch -c feat/short-summary` (types: `feat|fix|docs|refactor|test|chore|perf|ci|build`).
3. **Make the change** with tests and docs.
4. **Run local checks** (all must pass):
   ```sh
   cargo fmt --all
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-features
   cargo doc --no-deps
   ```
5. **Run the review gates** on your diff before requesting human review: a bug analysis pass and a
   code review pass (and a security review for anything touching secrets/auth/crypto/exec).
   Address findings or track them in an issue.
6. **Open a PR** and fill out the template completely.
7. **Get review + green CI**, then squash-merge. Delete your branch.

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/): `type(scope): summary`.

```
feat(s3): add multipart upload with resumable parts
fix(vault): never log decrypted master key
docs(adr): record decision to use WASM for plugins
```

## Documentation expectations

| You changed… | You must also… |
|--------------|----------------|
| Any code | Write a complete PR description |
| A feature / behavior | Update `README.md`/`docs/` and add a `CHANGELOG.md` entry |
| Public API or modules | Add rustdoc (`///`, `//!`) |
| Architecture | Add an ADR in `docs/adr/` |
| A non-trivial design | Add an RFC in `docs/rfcs/` first |
| A bug | Add a regression test + `CHANGELOG.md` entry |

## Security

Never file security issues publicly. Follow [SECURITY.md](SECURITY.md) for private disclosure.
Never commit secrets — this is a secrets-handling tool and we hold a high bar.

## License

By contributing, you agree your contributions are dual-licensed under Apache-2.0 OR MIT.
