<!--
  Thanks for contributing to Cairn! Fill out every section.
  PR title must follow Conventional Commits, e.g. "feat(s3): add multipart upload".
  Remember: main is protected — this PR needs green CI and an approving review to merge.
-->

## What

<!-- A concise description of the change. -->

## Why

<!-- The motivation / problem being solved. Link the issue: Closes #123 -->

Closes #

## How

<!-- Key implementation notes, design choices, trade-offs. Link any ADR/RFC. -->

## Testing

<!-- How was this verified? Commands run, scenarios covered, new tests added. -->

## Screenshots / output

<!-- For UX/behavior changes, show before/after (ASCII is fine). -->

## Checklist

- [ ] PR title follows Conventional Commits
- [ ] Branched off `main` (not committing to `main` directly)
- [ ] `cargo fmt`, `cargo clippy -D warnings`, `cargo test`, `cargo doc` all pass locally
- [ ] Tests added/updated (regression test for bug fixes)
- [ ] Docs updated (rustdoc, `README`/`docs/`, ADR/RFC if architectural)
- [ ] `CHANGELOG.md` updated under "Unreleased"
- [ ] Ran the review gates on the diff (**bug analysis** + **code review**)
- [ ] Ran **security review** if this touches secrets / auth / crypto / process execution
- [ ] No secrets, credentials, or generated artifacts committed

## Risk & rollback

<!-- What could break? How would we revert or mitigate? -->
