## What this changes

<!-- The user-visible effect, in a sentence or two. -->

**Crates touched:**
**Related `docs/BUGS.md` entries:**

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

<!-- Plus whatever else you ran: acceptance suite, a live mount, manual steps. -->

## Implications

- [ ] Adds a SQLite migration (new `MIGRATION_V*`, `SCHEMA_VERSION` bumped — no shipped migration edited)
- [ ] Touches recovery, staging, or anything that can be the only copy of user data
- [ ] Changes packaging, the systemd unit, or desktop entries
- [ ] Changes the control protocol (daemon and front ends updated together)
- [ ] None of the above

<!-- Screenshots for GTK changes. -->
