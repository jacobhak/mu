# Security policy

## Reporting a vulnerability

Please report security issues privately, not in a public issue or pull request:
open the repository's **Security** tab and choose **Report a vulnerability**. That
opens a private advisory only the maintainer can see.

Include what you found, how to reproduce it, and the mu version (`mu --version`) or commit.
Reports are handled on a best-effort basis; expect an acknowledgement within a few days.

## Supported versions

Only the latest release and `main` get fixes.

## Scope

mu runs commands and reads and writes files on your behalf, so its permission
system (`docs/permissions.md`), credential storage (`docs/secrets.md`) and the
installer's checksum and attestation checks (`scripts/install.sh`) are the areas
where a report matters most.
