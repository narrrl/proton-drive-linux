# Security model

This page explains what protects your files and account when you use this client, what it keeps
on your computer, and what it does not protect against. It is a summary for users. The details,
with references to the code, are in the
[threat model](ARCHITECTURE.md#10-threat-model-what-this-client-writes-to-disk-in-plaintext). To
report a vulnerability, see [SECURITY.md](../SECURITY.md).

This is an unofficial hobby project, not a Proton product. It has had no independent security
audit. The source is open under the MIT license, so you can read and build all of it yourself.

## Encryption

Proton Drive is end-to-end encrypted, and this client does not change that:

- **Files are encrypted on this computer before they are uploaded,** and decrypted on this
  computer after they are downloaded. Proton never receives your keys or the content of your
  files.
- **The cryptography comes from the SDK, not from this client.** The client uses
  [`proton-sdk` and `proton-drive-rs`](https://github.com/narrrl/proton-sdk-rs), a Rust port of
  Proton's official C# and TypeScript Drive SDKs. OpenPGP is done by the
  [rPGP](https://github.com/rpgp/rpgp) library. The client itself does not implement any
  encryption.
- **Your password is not sent to Proton.** Sign-in uses Proton's SRP protocol, which proves that
  you know the password without sending it.

## What is kept on this computer

| What | Where | Protected by |
|---|---|---|
| Session tokens and the key passphrases | The system keyring (Secret Service: GNOME Keyring, KWallet, KeePassXC) | The keyring's own encryption and your login |
| Your password | Nowhere | — |
| Cached files, thumbnails and changes waiting to upload | `~/.cache/proton-drive-linux/content/` | File modes only: **not encrypted** |
| File and folder names, the folder tree, the activity log | `~/.local/state/proton-drive-linux/cache.db` | File modes only: **not encrypted** |
| The SDK's metadata cache | `~/.local/state/proton-drive-linux/sdk_cache.db` | Encrypted with a key derived from your mailbox password |
| Settings | `~/.config/proton-drive-linux/config.json` | No secrets are stored here |

The cache holds decrypted content so that files open fast and work offline. **The client assumes
that your disk is encrypted** (LUKS, or an encrypted home directory). On an unencrypted disk,
someone who has the powered-off computer can read the cached files and the names in your Drive.

Accounts with a separate mailbox password (two-password mode) are locked by default. Their key
passphrases are kept in the kernel keyring for your login session only, and you unlock again
after each login. Storing them in the system keyring is an opt-in.

## Who can use the client

- The state, cache and config directories must belong to you and have mode `0700`. The daemon
  checks this at every start and refuses to run otherwise.
- The control socket that the app, tray and `pdfs` use has mode `0600`. Only your user account
  can connect to it.
- Any program that runs as your user can read, change and delete files in `~/ProtonDrive`, the same
  as in any other folder you own. It is your Proton Drive, not a copy: a deleted file is moved to
  the Proton Drive trash, where you can restore it.

## Network connections

The client connects only to:

- **Proton**, for the Drive and account API, and for the CAPTCHA page when Proton asks for one.
- **OpenFreeMap**, for the street map of Places in Photos, only if you turn it on in Preferences. It is off by default, because the tile server learns which parts of the
  world you look at. The default map is drawn from data shipped with the app.

There is no telemetry, no analytics and no update check. The SDK can report timings of its own
operations, and the client only writes them to its local log.

## What this client does not protect against

- **Malware running as your user.** It can read the cache, connect to the control socket, and use
  your Drive through the mount, as it can with any data you have access to.
- **Swap.** Keys and decrypted data are in ordinary memory and can be written to swap. Use
  encrypted swap.
- **The kernel page cache.** File content read through the mount is cached by the kernel like any
  other file.

## Packages

- The `.rpm` is signed with GPG.
- The `.deb` and the `.tar.gz` are built by GitHub Actions from the tagged source, and are not
  signed.
- The AUR packages build from the tagged source or from `main`, or install the release binaries
  checked against their SHA-256 sums.
- The Nix flake builds from source. The binary cache on Cachix holds builds made by CI.
