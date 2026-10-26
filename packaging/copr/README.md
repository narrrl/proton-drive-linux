# COPR repository

Fedora users install from the COPR project
[`narl-rs/proton-drive-linux`](https://copr.fedorainfracloud.org/coprs/narl-rs/proton-drive-linux/),
so `dnf upgrade` brings every release. COPR builds `packaging/proton-drive-linux.spec` from two
release assets:

| Source | What it is |
| --- | --- |
| `Source0` | GitHub's archive of the `v<version>` tag |
| `Source1` | `proton-drive-linux-vendor-<version>.tar.xz`: every crate from `Cargo.lock`, made by `cargo vendor` in `release.yml` |

With the vendored crates the build needs no network, so the COPR project keeps network access
off.

## Releasing a new version

`.github/workflows/copr.yml` does this automatically:

- **On a release tag**, `release.yml` calls it after the GitHub release is published. It writes
  the version into the spec, downloads both sources with `spectool`, builds a source package
  and submits it with `copr-cli build`. The job waits for COPR, so a failed build fails the
  release workflow.
- **By hand**, run the workflow from the Actions tab with a version. The release must already
  have its vendor tarball.

It needs the `COPR_CONFIG` repository secret: the whole content of `~/.config/copr` from
<https://copr.fedorainfracloud.org/api/>. **The token expires after 180 days.** Renew it on that
page and replace the secret, or the next release's COPR job fails.

To do the same by hand:

```bash
spectool -g -C ~/rpmbuild/SOURCES packaging/proton-drive-linux.spec
rpmbuild -bs packaging/proton-drive-linux.spec
copr-cli build narl-rs/proton-drive-linux ~/rpmbuild/SRPMS/proton-drive-linux-*.src.rpm
```

## Project settings

- Chroots: `fedora-44-x86_64`, `fedora-45-x86_64` and `fedora-rawhide-x86_64`. The spec is
  `ExclusiveArch: x86_64` and needs `rust >= 1.96`.
- Network access in builds: off.
