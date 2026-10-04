# 3.0.0: what is done and what is left

Tracks `docs/MILESTONE-3.0.0.md`. Update it with every commit that moves a phase.

## Done

- **Phase 0**: fake Drive, simulation runs, CI job (2.9.0).
- **Phase 1**: queued-op preconditions and own sealed revisions in the database (2.9.0).
- **Phase 2**: local ids, inode = local id, one tree for every mount (2.10.0).
- **Phase 3, first part**: mkdir, create, rename, unlink and rmdir on the mount are recorded
  locally and sent from the queue; `"local_first": false` brings back the old path (5e0f30c).
- Fixes from the simulation and account runs since then: B141 to B147.
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
3. **Speed (§7)**: run independent ops in parallel, batch trashes and moves; measure 1,000 files
   and 100 folders on Wi-Fi against LAN (target: within 10 %).
4. **Done-when checks**:
   - [ ] Simulation passes on the Wi-Fi profile.
   - [ ] Known-bug list in `sim/run.rs` is empty: B125 and B130 open; B129 and B132 fixed but
         unconfirmed.
   - [ ] A clean account run. The last one (2026-10-04) failed on B70 (fixed as B145) and
         mirror/mirror "busy syncing" (the script now retries); not confirmed yet.
5. **Release**: the 2.9.0 and 2.10.0 tags still wait for the user's go-ahead.

## After 3.0.0

- Remove the old online path in `filesystem.rs` and the `local_first` switch (the release after
  3.0.0).
- **Phase 4**: one applier for remote changes.
- **Phase 5**: mirror folders on the same planner and executor.
- **Any time**: `Size` and block geometry as types; a `Deadline` on every remote call.
- Open questions in §12 of the milestone.
- `PLAN.md`: the acceptance script notices network drops and reports `interrupted` instead of
  TIMEOUT.
