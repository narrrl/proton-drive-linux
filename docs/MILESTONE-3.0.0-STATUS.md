# 3.0.0: what is done and what is left

Tracks `docs/MILESTONE-3.0.0.md`. Update it with every commit that moves a phase.

## Done

- **Phase 0**: fake Drive, simulation runs, CI job (2.9.0).
- **Phase 1**: queued-op preconditions and own sealed revisions in the database (2.9.0).
- **Phase 2**: local ids, inode = local id, one tree for every mount (2.10.0).
- **Phase 3, first part**: mkdir, create, rename, unlink and rmdir on the mount are recorded
  locally and sent from the queue; `"local_first": false` brings back the old path (5e0f30c).
- Fixes from the simulation and account runs since then: B127 and B141 to B155.
- **Sync issues, first cut** (§6.2): refusals sorted into issues, shown at once in the app,
  tray and CLI; Export button and `pdfs sync export`; `pdfs sync issues`.

## Phase 3: left for 3.0.0

1. **Sync issues (§6.2)**: first cut done.
   - [x] The drain sorts a refusal from Drive into an issue kind (quota, access, missing,
         limit, rejected) and stores it on the queued op (schema 38).
   - [x] An op with an issue counts as "needs attention" at once, not after six failures.
   - [x] The queue listing carries the issue kind and whether its content can be exported.
   - [x] Export a queued file's content (control request, `pdfs sync export`).
   - [x] `pdfs sync issues`, and the issue shown in `pdfs sync queue`.
   - [x] The app's Sync page shows each issue with what the user can do; Export and Retry
         buttons; translated in every catalog.
   - [ ] Discard an issue on purpose (after export, or for a change with no content), and
         undo the local change it stood for. Not in the first cut.
   - [ ] Show the issue on the node itself (file browser emblem or status). Not in the first
         cut.
2. **Planner and executor (§5.4, §5.5)**: ops keyed by local id; remove `mint_local_uid`,
   `finish_create` and `adopt_real_uid`; a landing create sets `remote_uid` on its row.
   - [x] Queued ops carry `lid` and `parent_lid` (schema 39). The claim reads the parent's uid
         through `parent_lid`, and a landing folder rewrites the ops inside it by local id.
   - [x] Node rows link to their parent by lid (`nodes.parent_lid`, schema 40), and a restart
         places each node under its parent's row as it is now.
   - [ ] Queries over children and subtrees by `parent_lid`, and no rewrite of the children's
         `parent_uid` when a folder lands (R3). Both go with the placeholders: until then the
         strings are kept in step.
   - [ ] No more placeholders: `nodes.uid` is `NULL` until a node lands. In four parts:
     - [x] 6a: a node made here goes by `local~<lid>`, a function of its row, instead of a
           minted `local~<ms>-<n>`; schema 41 rewrites the old placeholders (nodes, ops, pins).
     - [x] 6b: an op is found by its node's local id under either uid, the claim keeps one op
           per local id on the wire, and the drain sends the uid the row has now;
           `remap_local_uid` is gone.
     - [ ] 6c: a local node's row has `uid NULL`; children, subtrees and ops' parents by lid,
           so a landing create rewrites no other row or op; pins by lid.
     - [ ] 6d: in memory, `pending`, `hidden` and `creating` by lid; `is_local_uid` checks
           become `remote_uid().is_none()`; a write or rename after landing queues under the
           stand-in, so `landed_uid`, `follow_landed_create`, `landed_row_uid` and the
           `rewrite_op_target` fallback go.
   - [ ] An executor that runs ops in dependency order, many at once.
3. **Speed (§7)**: run independent ops in parallel, batch trashes and moves; measure 1,000 files
   and 100 folders on Wi-Fi against LAN (target: within 10 %).
   - [x] The measurement: `a_thousand_files_drain_as_fast_on_wifi_as_on_lan` in `sim/daemon.rs`,
         run with `PDFS_SIM_MEASURE=1`. It prints the times and asserts nothing yet.
   - [x] Baseline on eb1c105, three drain threads: the syscalls take 3.7 s on LAN and 4.3 s on
         Wi-Fi; the drain takes 138 s on LAN and 938 s on Wi-Fi, 6.8 times as long.
4. **Done-when checks**:
   - [ ] Simulation passes on the Wi-Fi profile: `one_client_on_a_slow_link` failed in CI on
         seeds 2 and 3. Two bugs (B127, B151) and a harness gap: a settle did not read the log, so
         it could not explain a known bug's damage. All three fixed; not yet confirmed in CI.
         Replays then found two races on the LAN profile too (B152, B153), both fixed. CI on
         7269cb5 failed LAN seed 9, a regression from the B153 fix (B154), and Wi-Fi seed 2,
         a write whose answer was lost (B155). Both fixed. One slow-link run of seed 2 failed
         once with a rename answering `ENOENT` as a folder move landed. Not reproduced since;
         it may be B152.
   - [ ] Known-bug list in `sim/run.rs` is empty: B125 and B130 open; B127, B129 and B132 fixed
         but unconfirmed.
   - [ ] A clean account run. The one on a6f36d5 (2026-10-04, LAN: 427 passed, 12 failed)
         failed B113 and B121 on every on-demand mount, two mirror-to-mirror move cases, and
         the journal check. B121 was a bug (B149, fixed); B113's case still expected creates
         to reach Drive at once and now accepts queued ones. The move cases were the harness:
         it retried every source after one was busy, and expected a refusal where a mirror to
         mirror move carries the local copy along. The journal showed a conflict copy retrying
         into a deleted folder (B150, fixed). Not confirmed yet.
5. **Release**: 2.10.0 is tagged; the 2.9.0 tag still waits for the user's go-ahead.

## After 3.0.0

- Remove the old online path in `filesystem.rs` and the `local_first` switch (the release after
  3.0.0).
- **Phase 4**: one applier for remote changes.
- **Phase 5**: mirror folders on the same planner and executor.
- **Any time**: `Size` and block geometry as types; a deadline on the remote calls outside
  the drain (the drain's have one: `link::upload_deadline`, `Core::block_on_within`).
- Open questions in §12 of the milestone.
- `PLAN.md`: the acceptance script notices network drops and reports `interrupted` instead of
  TIMEOUT.
