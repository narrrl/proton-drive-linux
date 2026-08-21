# Security Policy

## Supported versions

Fixes land on the latest release. Please reproduce on the most recent version before reporting.

## Reporting a vulnerability

**Do not open a public issue.** Report privately through GitHub's
[private vulnerability reporting](https://github.com/narrrl/proton-drive-linux/security/advisories/new),
or by email to <contact@narl.io>.

Please include the version (`pdfs --version`), your distribution and desktop environment, what an
attacker gains, and a reproduction if you have one. You can expect an acknowledgement within a few
days; a fix ships in the next release, and you are credited in the advisory unless you ask
otherwise.

## Scope

This client holds Proton account credentials and decrypted file content on the local machine, so
the interesting boundaries are:

- **Credentials.** Sessions and the mailbox password live in the system Secret Service. Anything
  that writes them elsewhere — logs, temp files, crash dumps — is a vulnerability.
- **The control socket.** Anything that can connect to `control.sock` has the daemon's
  authenticated session. It is `0600`, and the state, cache, and config directories must be real,
  current-user-owned `0700` directories; the daemon fails closed otherwise. Reports that defeat
  those checks are in scope.
- **Local cache and staging.** Decrypted block content and staged writes on disk.
- **Cryptography.** Key derivation, signature handling, and anything that could publish plaintext
  or the wrong key material to Proton.

Out of scope: vulnerabilities in Proton's own service (report those to Proton AG), and issues that
require an attacker who already has your user account on the machine.
