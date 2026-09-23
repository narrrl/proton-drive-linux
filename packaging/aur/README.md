# AUR packages

Source of truth for the three AUR packages of this project. Each subdirectory is
the full content of one AUR git repository (`pkgbase` = directory name):

| Directory | What it does | AUR repository |
| --- | --- | --- |
| `proton-drive-for-linux` | Builds the latest tagged release from source | `ssh://aur@aur.archlinux.org/proton-drive-for-linux.git` |
| `proton-drive-for-linux-git` | Builds `main` from git | `ssh://aur@aur.archlinux.org/proton-drive-for-linux-git.git` |
| `proton-drive-for-linux-bin` | Installs the binaries from the GitHub release | `ssh://aur@aur.archlinux.org/proton-drive-for-linux-bin.git` |

The three packages conflict with each other and with the in-tree
`packaging/PKGBUILD` package (`proton-drive-linux`), and every one of them
`provides` `proton-drive-for-linux`, `proton-drive-linux`, and `pdfs`.

Each directory tracks only `PKGBUILD`, `.SRCINFO`, `.gitignore`, and `LICENSE`
(the 0BSD license of the packaging sources, per
[RFC40](https://rfc.archlinux.page/0040-license-package-sources/)); the
`.gitignore` ignores everything else so downloaded sources, `src/`, `pkg/`, and
built packages never reach the AUR.

## Releasing a new version

`.github/workflows/aur.yml` does this automatically:

- **On a release tag**, `release.yml` calls it after the GitHub release is
  published. It runs `./update.sh <version>`, pushes all three packages with
  `./push.sh`, and commits the updated recipes back to `main` as
  `chore(aur): update packages to <version>`.
- **On a push to `main` that changes `proton-drive-for-linux-git/`**, it
  regenerates that package's `.SRCINFO` and pushes only the -git package.
- **By hand**, run the workflow from the Actions tab. Give a version to publish
  a release, or leave it empty to publish only the -git package.

It needs the `AUR_SSH_PRIVATE_KEY` repository secret: a private key whose public
half is registered with the AUR account.

To do the same by hand:

1. Tag and publish the upstream release, so the source tarball and the
   `proton-drive-linux-<version>-x86_64.tar.gz` release asset exist.
2. Run `./update.sh <version>` — it rewrites `pkgver`, resets `pkgrel=1`,
   refreshes the checksums of the release-based packages, and regenerates every
   `.SRCINFO`.
3. Build and inspect the packages in a clean chroot:
   `cd proton-drive-for-linux && pkgctl build --clean` (needs `devtools`).
4. Commit here, then publish with `./push.sh` (or `./push.sh proton-drive-for-linux-bin`
   for a single package).

`proton-drive-for-linux-git` carries a `pkgver()` function, so its `pkgver` is
computed at build time; do not push a commit that only bumps it. This is why new
commits on `main` do not trigger an AUR push.

## Requirements for pushing

An SSH key registered with the AUR account and an entry in `~/.ssh/config`:

```
Host aur.archlinux.org
  IdentityFile ~/.ssh/aur
  User aur
```

`push.sh` creates a scratch clone of the AUR repository, copies the tracked
files over it, commits, and pushes to `master`.
