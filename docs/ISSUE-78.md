# Issue #78: session loss on Fedora 45

Investigation on 2026-11-14 of [issue #78](https://github.com/narrrl/proton-drive-linux/issues/78),
using its report and comments through `gh`, pdfs 3.7.0/current source, and the oo7 0.7.0 tag.

## What is established

The reporter still saw the problem with oo7 0.7.0 on November 7. The original
[SetSecret persistence fix](https://github.com/linux-credentials/oo7/commit/7fb11dd111c4be8fec01996e3caa31c44fa962b3)
is present in that release. In an isolated test, three successive binary `SetSecret` updates
survived reopening the keyring file. Repeating the original persistence explanation is therefore
insufficient.

A different, reproducible failure exists at the boundary between oo7 and pdfs:

1. oo7 0.7.0's `Collection::search_inner_items` returns an empty list for a locked collection.
   `SearchItems` consequently returns **both** the unlocked and locked lists empty even when the
   matching item is on disk. The isolated test reproduces this with pdfs's exact lookup attributes.
   This is upstream [oo7 #614](https://github.com/linux-credentials/oo7/issues/614).
2. `dbus-secret-service-keyring-store` 1.0.1 maps an empty search to `keyring_core::Error::NoEntry`;
   pdfs maps that to `Error::NotLoggedIn`. The client cannot offer an unlock when no locked item
   is reported.
3. In pdfs 3.7.0 and the code before this change, `TokenSink::flush` treated `NotLoggedIn` as a
   logout, cleared the pending rotation and returned success. It skipped the error path that
   retains the new tokens in the kernel keyring and retries their persistence. The code was
   unchanged between v3.7.0 and this investigation.
4. If a token refresh encounters that reply, the saved blob retains a spent refresh token. The
   running process can keep working with the new tokens in memory; a restart then exposes the
   stale saved credentials. A regression test failed before the fix and passes afterward.

The same unsafe assumption also applies to transient empty lookups such as the race reported in
[oo7 #652](https://github.com/linux-credentials/oo7/issues/652), fixed in 0.7.1. That issue's
replacement race was not independently reproduced here; pdfs normally updates existing items
with `SetSecret`, so it must not be assumed to encounter the same-item `CreateItem` race.

## Change and limits

`TokenSink::flush` now propagates a missing-session error instead of discarding the rotation.
The existing retry, kernel-keyring recovery and unsaved-token status paths can then run. Once
the collection is unlocked and readable, the pending rotation is saved. A genuinely deleted
session is never recreated: flushing still requires a readable blob with the matching session
ID and expected refresh token. A new login causes the old rotation to be discarded as before.

This is a confirmed pdfs defect (B284), **not a confirmed complete explanation of the reporter's
cold-boot failure**. The issue has no oo7 logs or record showing that its collection was locked
when a token rotated. A locked keyring only at startup can hide a login, but does not by itself
explain `InvalidRefreshToken`. The token-loss path requires a successful refresh while the saved
entry is unreadable.

The fix does not make a locked keyring writable. If it stays unreadable until power-off, tokens
retained in process/kernel memory still cannot survive a cold boot. No new disk credential store
or credential format is introduced.

Other upstream Fedora-upgrade issues were checked: [oo7 #651](https://github.com/linux-credentials/oo7/issues/651)
creates duplicate placeholder/migrated collections and refuses writes; [oo7 #662](https://github.com/linux-credentials/oo7/issues/662)
can resolve the default alias to a locked collection. Both have fixes in
[0.7.1](https://github.com/linux-credentials/oo7/releases/tag/0.7.1), released November 10,
after the reporter's November 7 retest. Neither can be assigned as this report's cause without
matching runtime evidence.

## SetSecret is kept only in memory in Fedora's oo7

Follow-up on 2026-11-15. The reporter's `rpm -qa 'oo7*'` shows
`oo7-daemon-0.7.0~beta^1.e91b98a-1.fc45`, not the 0.7.0 release assumed above. Bodhi lists that
build as the stable oo7 for Fedora 45 and 46; neither has 0.7.0 or 0.7.1. Commit e91b98a
(October 24) predates
[7fb11dd](https://github.com/linux-credentials/oo7/commit/7fb11dd111c4be8fec01996e3caa31c44fa962b3)
(October 28): "Setting the Label or Attributes of an item, or calling SetSecret, only changed the
item in memory, so the old values came back once the daemon restarted." 0.7.0 carries it as
37e3334.

pdfs writes an existing entry with `SetSecret`, so on that build every saved token rotation was
lost at the next reboot. This explains the reporter's log: the keyring was unlocked one second
before pdfs started (they type their password at login) and still held a spent refresh token.
It does not need a locked keyring or a restarted provider.

pdfs now saves an existing entry with `CreateItem` and `replace` instead
(`crates/pdfs-core/src/secretservice.rs`, B291), which e91b98a writes to the keyring file.
Reproduced and verified with oo7-daemon built at e91b98a in a throwaway session bus:

```sh
git clone https://github.com/linux-credentials/oo7.git /tmp/oo7 && git -C /tmp/oo7 checkout e91b98a
cargo build --manifest-path /tmp/oo7/Cargo.toml -p oo7-daemon --bin oo7-daemon
OO7=/tmp/oo7/target/debug/oo7-daemon
T=$(mktemp -d /tmp/ss.XXXX); mkdir -p $T/data $T/run; chmod 700 $T/run
XDG_DATA_HOME=$T/data XDG_RUNTIME_DIR=$T/run OO7=$OO7 PDFS_SECRET_SERVICE_RESTART_TEST=1 \
  PDFS_RESTART_PROVIDER='pkill -x oo7-daemon; sleep 1; (printf pw | setsid "$OO7" --login >/dev/null 2>&1 &); sleep 2' \
  dbus-run-session -- sh -c 'printf pw | setsid "$OO7" --login >/dev/null 2>&1 & sleep 2;
    cargo test -p pdfs-core real_secret_service_keeps -- --ignored'
```

With the in-place write, the read after the restart returns the first of three saves (`r1`);
with the replacing write it returns the last (`r3`), and one item is left.

The daemon also logs a short fingerprint of the refresh token when it resumes the stored session
and each time it saves a rotation (`refresh=1a2b3c4d`). If a start is refused, compare the
fingerprint it resumed with against the last one saved in the boot before:

```sh
journalctl --user -b -1 -u proton-drive.service | grep -E 'refresh=' | tail -2
journalctl --user -b 0 -u proton-drive.service | grep -E 'refresh=' | head -2
```

The same fingerprint means the keyring kept the last save and Proton refused it; a different one
means the keyring lost it.

## A restarted Secret Service

A second path to the same symptom, independent of locking (B285). The keyring store pdfs used
holds one D-Bus connection and one Secret Service session for the whole process, and was
installed once. The session lives only in the provider's memory. When the provider restarts,
the new one answers every `GetSecret` and `SetSecret` on the old session with
`org.freedesktop.Secret.Error.NoSession`, until the pdfs process itself restarts. Reproduced
with both gnome-keyring 51.1 and oo7 0.7.1 using `--replace`.

gnome-keyring practically never restarts within a desktop session. oo7 can:
`oo7-daemon.service` has `Restart=on-failure`, the daemon allows itself to be replaced, and it
exits on a wrong credential password and is D-Bus-activated again. A daemon that met a restarted
provider kept every later token refresh only in memory and the kernel keyring, and a cold boot
then left the spent refresh token in the saved blob.

Now a platform failure drops the store and the call is retried once on a new connection. The
daemon log shows `the Secret Service failed this process's connection; reconnecting` when that
happens. To check whether a machine meets this, look for oo7 restarts in the same boot:

```sh
journalctl --user -b -1 -u oo7-daemon.service | grep -E 'Started|Stopped|Starting oo7|Main process exited'
journalctl --user -b -1 -u proton-drive.service | grep -iE 'keyring|reconnecting|refresh'
```

Reproduce in a throwaway session bus (never against the desktop's keyring; the runtime directory
must be short, as it holds a Unix socket):

```sh
T=$(mktemp -d /tmp/ss.XXXX); mkdir -p $T/data $T/run; chmod 700 $T/run
XDG_DATA_HOME=$T/data XDG_RUNTIME_DIR=$T/run PDFS_SECRET_SERVICE_RESTART_TEST=1 \
  PDFS_RESTART_PROVIDER='echo -n pw | gnome-keyring-daemon --replace --unlock --components=secrets >/dev/null' \
  dbus-run-session -- sh -c 'echo -n pw | gnome-keyring-daemon --unlock --components=secrets >/dev/null; sleep 1;
    cargo test -p pdfs-core real_secret_service -- --ignored'
```

For oo7, start `oo7-daemon --login` with the password on stdin instead, and restart it with
`--login --replace` in the background.

## Reproduce without a Proton account or desktop keyring

The oo7 fixture uses fake tokens, a temporary disk keyring and a private peer-to-peer D-Bus
connection. It asserts the observed 0.7.0 behavior, including the incorrect empty locked lookup;
passing means that behavior was reproduced. It does not simulate Proton refresh or a full boot.

From the pdfs repository, choose an unused temporary checkout path:

```sh
git clone --depth 1 --branch 0.7.0 https://github.com/linux-credentials/oo7.git /tmp/pdfs-oo7-repro
git -C /tmp/pdfs-oo7-repro apply "$PWD/docs/repro/oo7-0.7.0-issue78.patch"
cargo test --manifest-path /tmp/pdfs-oo7-repro/Cargo.toml \
  -p oo7-daemon --lib --features test-util issue78 -- --nocapture
```

Observed output:

```text
oo7 0.7.0: locked session is on disk but SearchItems returns (0, 0)
oo7 0.7.0: all three binary SetSecret updates survived reopening the disk keyring
test item::tests::issue78_locked_lookup_hides_saved_tokens ... ok
```

The pdfs side is covered by `cargo test -p pdfs-core auth::tests`, specifically
`a_temporarily_missing_session_does_not_discard_a_rotation` and
`retrying_a_missing_session_never_recreates_it_or_overwrites_a_new_login`.

Remaining live validation: reproduce on a Fedora 45 GNOME session across logout/shutdown,
correlating oo7 lock/write errors with pdfs refresh and retry logs, and checking the running
daemon's package version rather than just the CLI version. The investigation did not use a real
Proton account, alter a desktop keyring, or post to either GitHub issue.
