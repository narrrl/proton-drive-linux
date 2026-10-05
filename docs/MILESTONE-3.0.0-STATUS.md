# 3.0.0: what is done and what is left

Tracks `docs/MILESTONE-3.0.0.md`. Update it with every commit that moves a phase.

## Done

- **Phase 0**: fake Drive, simulation runs, CI job (2.9.0).
- **Phase 1**: queued-op preconditions and own sealed revisions in the database (2.9.0).
- **Phase 2**: local ids, inode = local id, one tree for every mount (2.10.0).
- **Phase 3, first part**: mkdir, create, rename, unlink and rmdir on the mount are recorded
  locally and sent from the queue; `"local_first": false` brings back the old path (5e0f30c).
- Fixes from the simulation and account runs since then: B125, B127, B130 and B141 to B192.
- **Sync issues, first cut** (§6.2): refusals sorted into issues, shown at once in the app,
  tray and CLI; Export button and `pdfs sync export`; `pdfs sync issues`; Discard button and
  `pdfs sync discard`; a warning badge on the node in the file browser.

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
   - [x] A name collision lands under a conflict name (create, rename, move, re-homed create)
         and goes in the activity log, as does a queued edit kept as a conflict copy. The app
         words each one in the user's language (the last one since B181).
   - [x] Discard an issue on purpose (after export, or for a change with no content), and
         undo the local change it stood for: a node made here goes, a node Drive has goes back
         to what Drive has. Control request, `pdfs sync discard`, Discard button with a
         confirmation; translated in every catalog.
   - [x] Show the issue on the node itself: a folder listing carries each node's issue, and
         the file browser shows a warning badge with what it means. The listing is published
         again when the set of issues changes.
2. **Planner and executor (§5.4, §5.5)**: ops keyed by local id; remove `mint_local_uid`,
   `finish_create` and `adopt_real_uid`; a landing create sets `remote_uid` on its row.
   - [x] Queued ops carry `lid` and `parent_lid` (schema 39). The claim reads the parent's uid
         through `parent_lid`, and a landing folder rewrites the ops inside it by local id.
   - [x] Node rows link to their parent by lid (`nodes.parent_lid`, schema 40), and a restart
         places each node under its parent's row as it is now.
   - [x] Queries over children, subtrees and ancestors by `parent_lid`, and no rewrite of the
         children's `parent_uid` when a folder lands (R3), nor of the ops made inside it (R2).
   - [x] No more placeholders: a node made here goes by its local id until it lands. In four
         parts:
     - [x] 6a: a node made here goes by `local~<lid>`, a function of its row, instead of a
           minted `local~<ms>-<n>`; schema 41 rewrites the old placeholders (nodes, ops, pins).
     - [x] 6b: an op is found by its node's local id under either uid, the claim keeps one op
           per local id on the wire, and the drain sends the uid the row has now;
           `remap_local_uid` is gone.
     - [x] 6c: children, subtrees, ancestors and ops' parents by lid, so a landing create
           rewrites no other row or op; what is read for a uid names the parent as its row has it
           now. The row keeps its stand-in in `nodes.uid` rather than `NULL`: it is as stable,
           and `NULL` would have changed some 40 node queries for no change in behaviour. Pins
           stay keyed by uid and move with the row in the landing transaction (B148).
     - [x] 6d: a node written under its stand-in after it landed updates its landed row
           instead of bringing the placeholder row back, so a write that finds its create
           landed takes the uid from the row and no longer polls the tree for up to 2 s
           (`landed_uid` is gone). In memory, `pending`, the uploads and the tree stay keyed by
           the uid Drive knows: ops are matched by lid in the database (6b), `hidden` and
           `creating` only ever hold remote uids, and `landed_row_uid` is the one place at the
           mount's edge where a stand-in a handle still holds becomes the real uid.
   - [x] An executor that runs ops in dependency order, many at once: 16 drain workers instead
         of 3, and a worker that lands an op wakes the idle ones, which slept through the ops it
         made claimable. The claim already keeps the order: a create waits for its parent's
         uid, a node's ops go one at a time. Ops stay blocking calls on worker threads: each
         one spends its time waiting on Drive, so tasks would buy nothing threads do not.
3. **Speed (§7)**: run independent ops in parallel, batch trashes and moves; measure 1,000 files
   and 100 folders on Wi-Fi against LAN (target: within 10 %).
   - [x] The measurement: `a_thousand_files_drain_as_fast_on_wifi_as_on_lan` in `sim/daemon.rs`,
         run with `PDFS_SIM_MEASURE=1`. It prints the times and asserts nothing yet.
   - [x] Baseline on eb1c105, three drain threads: the syscalls take 3.7 s on LAN and 4.3 s on
         Wi-Fi; the drain takes 138 s on LAN and 938 s on Wi-Fi, 6.8 times as long.
   - [x] Sixteen workers: the syscalls take 5.2 s on LAN and 5.0 s on Wi-Fi, which meets the
         target; the drain takes 47 s on LAN and 247 s on Wi-Fi. Most of a create on Wi-Fi is
         its read-back waiting for Drive to list the new node (250 ms, then 1 s), so batching
         (step 8) helps trashes and moves, not creates.
   - [x] Batched trashes and moves: left for after 3.0.0 (see below). The target is met
         without them, the drain waits on the creates' read-back, and splitting one answer into
         many results is where the B15x to B17x races lived.
4. **Done-when checks**:
   - [ ] Simulation passes on the Wi-Fi profile: `one_client_on_a_slow_link` failed in CI on
         seeds 2 and 3. Two bugs (B127, B151) and a harness gap: a settle did not read the log, so
         it could not explain a known bug's damage. All three fixed; not yet confirmed in CI.
         Replays then found two races on the LAN profile too (B152, B153), both fixed. CI on
         7269cb5 failed LAN seed 9, a regression from the B153 fix (B154), and Wi-Fi seed 2,
         a write whose answer was lost (B155). Both fixed. One slow-link run of seed 2 failed
         once with a rename answering `ENOENT` as a folder move landed. Not reproduced since;
         it may be B152. Replays of slow-link seed 4 failed two of six times on a file written
         again after its create lost its answer (B156, fixed). With sixteen drain workers
         (16fca10), seed 4 failed every replay on a write after an upload that was not read
         back (B157, fixed). Seed 3 then failed four of ten runs on two renames of one file
         landing out of order (B158, fixed). Flaky-link seed 4 failed one of ten runs on a
         lookup that answered `EAGAIN` while the root was listed again after a restart (B159,
         fixed). Slow-link seed 4 also had folders reach Drive 30 s late, after the
         drain's idle poll (B160, fixed). Slow-link seed 3 failed on a file moved while its
         create was on the wire, which the mount showed under its old name (B161, fixed).
         Seed 4 failed on a folder removed and made again, whose create adopted the removed
         folder (B162, fixed). CI on 903452a passed every profile. CI on f9b1638 failed
         slow-link seed 2: a step that unlinks an open file, appends and reads it back hung
         for 150 s. The B130 matcher did not fire, and CI kept no stacks; one replay passed.
         It may be a kernel notice sent from a runtime worker by another path than B130's,
         which a68dae9 (not yet pushed then) also takes off the runtime. CI now keeps a hung
         run's stacks.
   - [ ] Known-bug list in `sim/run.rs` is empty: none open; B125, B127, B129, B130 and B132
         fixed but not yet confirmed by the runs. The confirmation run (`PDFS_SIM_KNOWN=fail`)
         on da013c2 failed `one_client_on_a_flaky_link` seeds 6 and 11 on two new bugs, both
         after restarts: a write over a create never read back (B182) and a trash that forgot
         what its create sent (B183). Replays with those fixed found a third: a create renamed
         after it lost its answer (B184). All three fixed; no known-bug matcher fired. It was
         stopped before `three_clients`. The rerun on 3073052 found a create that waited out
         a retry after a write replaced its blob (B185, fixed), and a file deleted offline
         while open that lost the bytes its handle read (B186, fixed; older than 3.0.0).
         The run on 2baad67 found that a restart before such a file's trash went out queued
         its kept bytes again, for good (B188, fixed), and changes that sat out their retry
         after an outage only the drain had met (B189, fixed; older than 3.0.0), and a file
         saved just before a stop whose release the stop dropped (B190, fixed; older too). The run
         on 1436c5f found a file deleted after its failed create was written again left on Drive
         (B191, fixed). Its test for B193 found a new empty file waiting out the idle poll (B192,
         fixed).
   - [ ] A clean account run. The one on a6f36d5 (2026-10-04, LAN: 427 passed, 12 failed)
         failed B113 and B121 on every on-demand mount, two mirror-to-mirror move cases, and
         the journal check. B121 was a bug (B149, fixed); B113's case still expected creates
         to reach Drive at once and now accepts queued ones. The move cases were the harness:
         it retried every source after one was busy, and expected a refusal where a mirror to
         mirror move carries the local copy along. The journal showed a conflict copy retrying
         into a deleted folder (B150, fixed). Not confirmed yet. The run on 2.10.0
         (2026-10-04: 260 passed, 3 failed) failed a new file that read back empty (B163,
         fixed), a 128-entry folder that listed 129 (B164, fixed), and its cleanup, which ran
         while the daemon was restarting: the stop waited on a busy thread until systemd killed
         it (B165, bounded). The next run on the working tree (2026-10-04: 259 passed, 4
         failed) found a wide folder dropping files whose create landed while it was listed
         (B166, fixed), or listing one empty: the new file that read back empty again
         (B166 too). Its cleanup found no control socket because it ran 1.5 s after the
         last restart, before the new daemon had bound it: `pdfs status` reports that as not
         running, which the run's queue wait takes for an empty queue (the script's side).
         Looking into it found the start spending 1.7 s rebuilding listings (B167, fixed).
         Its journal check caught a file removed with its folder while uploading being
         sent again, to the root (B168, fixed), and a stop that let a mirror pass go on
         until the runtime ended under it (B169, fixed). CI on 9bbc994 failed
         `one_client_on_a_good_link`: a write opened as an upload landed took the revision
         it replaced as its base, and the next write failed with `EIO` (B170, fixed). The
         suite's seed 9 found a deleted file's trash waiting out its create's backoff (B171,
         fixed). Run three at a time on 2502af6, seed 8 failed two of nine runs: a file renamed
         while its upload was read back showed its old name again (B172, fixed). The loop
         on d850265 failed seed 12 once in nine runs: a file deleted just as its create
         landed stayed on Drive (B173, fixed). The suite on 378667f failed seed 6: a rename
         in the same gap was refused with `EACCES` (B174, fixed). The loop on 378667f failed
         seed 8: a file replaced by a rename in that gap stayed on Drive (B175, fixed), and
         seed 12: a write closed as its file's create landed was dropped (B176, fixed).
         The account run on 378667f: a folder listed as its create landed asked Drive by
         its stand-in and answered `EIO` (B177, fixed). A folder trashed while sync was
         paused kept a create below it as a trash: a paused drain worker had claimed it
         (B178, fixed). A file read as its create landed read empty (B179, fixed).
         The suite on 0f3c06f: a stop gave up on a drain worker that slept through its
         wake-up (B180, fixed). The account run on 903452a (2026-10-04, Wi-Fi: 262 passed,
         1 failed) failed only its cleanup, which found no control socket: after the mirror
         target's last restart the queue wait took the missing daemon for an empty queue
         (the script's side, fixed in 6539c73).
5. **Release**: 2.10.0 is tagged; the 2.9.0 tag still waits for the user's go-ahead.

## After 3.0.0

- Remove the old online path in `filesystem.rs` and the `local_first` switch (the release after
  3.0.0).
- **Phase 4**: one applier for remote changes.
- **Phase 5**: mirror folders on the same planner and executor.
- **Batched trashes and moves** (plan step 8). Not needed for the §7 target: the syscalls take
  5.2 s on LAN and 5.0 s on Wi-Fi. The drain's time goes to the creates' read-back (250 ms,
  then 1 s each), which batching does not shorten. A batch call answers for many ops at once,
  and splitting that answer back per op is the step the B15x to B17x races came from. First
  measure `rm -r` of 1,000 files on Wi-Fi. If it is slow, drop the queued trashes of a trashed
  folder's children before batching anything: trashing the folder takes them with it.
- **Any time**: `Size` and block geometry as types; a deadline on the remote calls outside
  the drain (the drain's have one: `link::upload_deadline`, `Core::block_on_within`).
- Open questions in §12 of the milestone.
- `PLAN.md`: the acceptance script notices network drops and reports `interrupted` instead of
  TIMEOUT.
