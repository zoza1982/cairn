# Security Policy

Cairn manages credentials for many backends, so we take security seriously and ask reporters to
do the same.

## Reporting a Vulnerability

**Do not open a public issue for security vulnerabilities.**

Instead, please report privately via GitHub's
[private vulnerability reporting](https://github.com/zoza1982/cairn/security/advisories/new)
("Report a vulnerability" under the Security tab).

Please include:

- A description of the vulnerability and its impact
- Steps to reproduce (proof of concept if possible)
- Affected version(s) / commit
- Any suggested remediation

We aim to acknowledge reports within **3 business days** and to provide a remediation timeline
after triage. We will credit reporters who wish to be acknowledged once a fix is released.

## Supported Versions

While Cairn is pre-1.0, only the latest release (and `main`) receive security fixes.

## Our commitments

- No secrets are ever stored in plaintext on disk.
- Secrets are redacted in logs and error output, and are never sent to the AI layer.
- Changes to credential storage, encryption, authentication, or command execution receive a
  dedicated security review.
- Dependencies are scanned (Dependabot, `cargo audit`) and advisories triaged promptly.
