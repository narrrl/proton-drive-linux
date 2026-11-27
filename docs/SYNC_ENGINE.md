# Sync engine redesign (draft)

Status: proposal. Two parts are built: a resumable walk of Drive, and the remote dirty set (a
pass lists only the folders that changed on Drive). The rest is for review.

## Why

Today a pass over a mirror folder:

1. reads the whole baseline;
2. walks the whole local tree;
3. walks the whole remote tree;
4. applies changes depth by depth.

Nothing is applied before step 3 ends, and nothing from steps 2 and 3 is kept. The poll starts a
pass over every folder every 120 s. So the cost of a pass grows with the size of the folder, not
with the size of the change. On a folder of 500k files that means hours of listing to find a few
edits. A stop in those hours throws the work away, and the folder never syncs (B294, #53).

The daemon already knows what changed:

- **Local:** the inotify watcher sees every path that changes, but `event_target` reduces those
  paths to a folder id (`SyncMsg::Reconcile(id)`).
- **Remote:** `background.rs` reads Drive's event stream from a cursor saved in the DB.
  `NodeUpdated { node_uid, parent_node_uid }` and `NodeDeleted { node_uid }` name the exact nodes.
  Today only the FUSE cache uses these events.

The pass trace (`crates/pdfs-sync/src/trace.rs`) is how we measure this design against the current one. Get
traces from #53 and from a local 100k-file test folder before Phase 2, so the work goes where the
time goes.

## States

Each folder has one durable state, stored in `sync_folder` (a new `engine_state` column plus the
tables below). A restart resumes from it.

```
            add / reset
                 |
                 v
   +------> Initial(frontier) ---- frontier empty ----+
   |             |                                    |
   |        stop: frontier kept                       v
   |                                       +---> Steady(dirty, cursor) <---+
   |                                       |         |                     |
   |                          audit clean  |   overflow, cursor lost,      | dirty set
   |                                       |   watch failed, daily audit   | drained
   |                                       |         v                     |
   +----- baseline unusable ---------- FullCheck(frontier) ----------------+

  Paused and Error sit beside every state. Resume returns to the state they left.
```

- **Initial(frontier).** A first sync, or a sync from scratch.
  - The frontier is a queue, stored in the DB, of folders (rel paths) not yet reconciled.
  - The engine takes one folder from the frontier and lists it on both sides. It plans and
    applies that folder's direct children with today's `plan_file`, then queues the child
    folders and removes the folder from the frontier, in one transaction.
  - A stop loses at most one folder's work. This is the per-subtree checkpoint that B294 left
    undone.
- **Steady(dirty, cursor).** The normal state. A pass reconciles only the paths in the folder's
  dirty set:
  - **Local dirty paths:** the watcher sends `SyncMsg::Dirty(id, Vec<rel>)` and the engine stores
    the paths in `sync_dirty(folder_id, rel, side)`. The table survives a restart, so a change
    made just before a stop is not lost.
  - **Remote dirty paths:** the event loop maps an event's `node_uid`/`parent_node_uid` to
    `(folder_id, rel)` through `sync_entry.remote_uid`, which needs an index, and marks it dirty.
    A node with no row whose parent is a synced folder makes the parent dirty, which picks up
    new files.
  - **Per path:** stat it locally and read it on Drive (one `enumerate_nodes` for the batch),
    then plan with `plan_file` and apply as now. A dirty folder is listed one level deep on both
    sides, so a create or delete inside it is found.
  - Clear a dirty row only after its change is applied, in the same transaction as the baseline
    write.
- **FullCheck(frontier).** The current full walk as a safety net, with the same frontier
  checkpoint as Initial. It runs when:
  - inotify overflows (`IN_Q_OVERFLOW`) or a watch fails;
  - the event stream reports `ContinuityLost` or the cursor resets;
  - the daemon was down longer than the event retention, or this is the first start after an
    upgrade;
  - the slow audit is due (daily, configurable). This replaces the 120 s `ReconcileAll`.

  The audit is the only full walk a healthy folder ever does.

## Scheduler

The engine thread stays one thread and keeps one pass per folder (the `sync_lock`). What changes
is the unit of work: a scheduler picks the next **step**, not the next whole pass.

- A step is one frontier folder (Initial or FullCheck) or one batch of up to N dirty paths
  (Steady).
- Priority:
  1. Steady folders with dirty paths, oldest change first.
  2. Initial or FullCheck steps, round robin over folders.

  A small folder's edit then never waits behind a large folder's first sync, which removes the
  starvation of B260 and B294 by construction.
- The folder lock is taken per step, not per pass, so a mode switch or removal waits at most one
  step. `sync_abandon` and the checkpoint checks become plain "between steps".
- Progress and the trace work per step. For a pass, they report steps done and frontier left.

## What stays

- `plan_file`, `classification_order`, `Pending`, `apply_one` and the guards (`guard_local_wipe`,
  `guard_remote_wipe`, `changed_since_scan`, B211/B213/B214). Only the input changes: a few
  paths instead of a whole-tree map.
- The baseline table, used as now.
- Push-only passes for a switch to on-demand. They become a Steady drain plus a local walk.

## Risks

- **Missed events.** An event the daemon never saw leaves a path stale until the audit. The
  audit interval bounds the damage. The current design has no such gap, so the audit must stay.
- **Wipe guards on partial input.** The guards today compare whole trees. With per-folder steps
  they compare one folder's children against its baseline rows. A folder whose disk went away
  must still trip it: check the root on every step, as `reconcile_pass` does now.
- **Moves.** A rename shows up as a delete plus a create in two dirty paths. Both are in the same
  batch only if they arrive close together. Plan both sides of a dirty folder together, and give
  a delete a short delay so it can pair with a create.
- **Remote event mapping.** A node with no baseline row, deep in a new remote subtree, cannot be
  mapped to a rel path. Mark its nearest known ancestor dirty and list from there.

## Phase 1 as built: a resumable walk of Drive

The frontier above is first implemented for the walk of Drive only. Planning and applying
still see the whole tree.

Why not plan and apply per subtree yet: the wipe guards (`guard_local_wipe`,
`guard_remote_wipe`) compare the whole baseline with the whole tree. Checked per subtree, they
would refuse a user who empties one large folder on purpose. Left out, they would let a lost
listing or an unmounted disk delete one subtree at a time. Both need a round-level guard
before planning can be split up.

That guard is the deletion window (`guard_delete_window`): a folder counts the deletions it made
in the last day against the most paths it tracked in that time, and refuses a pass that would
bring the count to nine tenths. It already covers a tree lost over several whole passes. Split
planning would add each step's deletions to the same count.

What is built:

- **Schema 48:** `sync_walk` (root, start time), `sync_walk_dir` (each folder found:
  listed or not, short listing) and `sync_walk_item` (each path read).
- **Saving:** `walk_remote` is a queue of folders, not a recursion. Each listing is saved in
  one transaction together with the subfolders it found. A failed listing is not saved, so
  the next walk asks for it again.
- **Resuming:** a pass resumes a saved walk when its root is the same and it is less than
  24 hours old. Otherwise it starts over.
- **Invariant:** a saved listing is never used against a baseline newer than itself.
  - While a walk is not complete, SQLite triggers drop it when any of the folder's baseline
    rows is inserted, deleted or changed (filling in a revision id is the only exception).
  - Any walk is dropped when the folder's path, root or mode changes, or the folder is removed.
  - The push pass drops it as well.
  - Without this, a move to another synced folder (`relocate.rs`) would leave a listing that
    reads the moved file as deleted on Drive.
  - A complete walk is kept, and a baseline change marks a folder dirty instead (below).
- **Tests:** `a_saved_walk_resumes_where_it_stopped` and
  `a_baseline_change_drops_the_saved_walk` (DB), and
  `a_walk_of_drive_cut_short_resumes_where_it_stopped` (sim).

## Remote dirty set as built

The remote half of Steady, built before the scheduler because the remote walk is the cost
that grows into hours: it is network-bound, while the local walk only reads the disk.

- **Picture of Drive:** a complete walk stays in `sync_walk`, `sync_walk_dir` and
  `sync_walk_item` (schema 49 adds `sync_walk.complete`). It is the remote tree as of the
  last listing of each folder.
- **Dirty folders:** `sync_remote_dirty(seq, folder_id, dir)`, one row per folder, with a
  sequence number from `AUTOINCREMENT`.
  - The event loop (`background.rs`) calls `Db::sync_remote_changed` for each
    `NodeUpdated`/`NodeDeleted`. It marks the folder the node is in now (`parent_node_uid`)
    and the folder a saved walk last saw it in, found by `sync_walk_item.uid`. A move names
    only its new parent, so the old one comes from the walk.
  - A node whose parent no walk knows cannot be mapped, and needs no mark: it is inside a
    folder new on Drive, and the listing of that folder's parent finds the new folder and
    lists it whole.
  - Triggers on `sync_entry` mark the folder of every baseline row written while a complete
    walk exists. A pass therefore never relies on the echo of its own upload arriving in
    time: without the mark, a pass that started before the echo would read a file it just
    uploaded as missing on Drive.
  - The event loop sends `SyncMsg::Reconcile` for each folder it marked, so a change made on
    another device syncs within a poll of the event stream (10 s), not at the next 120 s poll.
- **A pass** (`start_walk`) loads the picture, then lists again each dirty folder, shallowest
  first. What a folder no longer holds is dropped with everything under it. A folder still
  there under the same uid keeps what was saved under it; a new or replaced folder is listed
  whole. The pass then drops the marks it read (up to the highest `seq`); marks made meanwhile
  stay. A folder whose listing failed or came back short is marked again.
- **When the picture is not used** (the pass walks all of Drive, resumably, and keeps the
  result):
  - the folder's root is not in the account's own volume, which is the only volume whose
    events the daemon reads;
  - the event stream has not been read in the last 30 s (right after a start, a pass waits
    up to 30 s for the first read);
  - the last full walk is more than 24 h old (the audit);
  - the stream reported `ContinuityLost`, or the daemon started without a cursor: both drop
    every saved walk.
- **Tests:** `a_complete_walk_stays_and_marks_where_the_baseline_changed`,
  `a_change_on_drive_marks_the_folders_it_touches`,
  `a_listing_done_again_drops_what_the_folder_no_longer_holds` (DB), and
  `a_pass_lists_only_the_folders_changed_on_drive` (sim: an edit, a new folder with a folder
  in it, and a deletion arrive, and a folder nothing changed in is never listed).

What a quiet pass still costs: loading the baseline and the picture from the database. The local
dirty set takes the place of the local walk, as described below.

## Root reducer as built

`pdfs_sync::root` holds one reducer per mirror folder. `step(state, input)` is pure: the passes,
the watcher, the event stream and pause are inputs, so a sequence of inputs replays exactly.

| Phase | Meaning |
|---|---|
| Discovering | No pass over the folder has finished yet. |
| Following | Passes covered the whole folder, and nothing since says a change went unreported. |
| Auditing | A doubt arrived, and the side it is about has not been walked in full since. |
| Suspended | The local root is missing, or the folder is paused. |
| Draining | A switch to on-demand is queued, and passes only push. |
| HandedOff | The folder is on-demand. Going back to a mirror starts discovery again. |

A doubt is about one side, since the disk and Drive are followed by different things that fail
on their own. The state keeps, for each side, the first doubt not yet answered (`Owed`):

| Doubt | Side | Where it comes from |
|---|---|---|
| FirstSync | both | A folder no pass has walked, or one that is a mirror again. |
| Restart | disk | The daemon started: nothing watched the disk while it was down. Drive's event stream resumes from its cursor. |
| WatchLost | disk | A watcher error, or a root that went missing and took its watch with it. |
| Overflow | disk | The watcher's queue overflowed. |
| EventGap | Drive | The event stream reported `ContinuityLost`. |
| Pushed | Drive | A switch to on-demand was called off; the push passes dropped the picture of Drive. |

A pause owes nothing by itself: the watcher and the event stream keep running for a paused
folder. A doubt that arrives while a folder is suspended is kept for when it comes back.

**Acted on:** a pass asks the state what it owes before it walks. A side that is owed is walked
in full: the disk without the kept walk, and Drive without the saved picture (a walk an earlier
pass cut short still resumes). The pass reports which sides it walked in full
(`Outcome::walked_in_full`), and only those answer a doubt. A pass from the dirty sets keeps a
Following folder Following.

The checks a pass made before (a trusted claim, a fresh event stream, the audit intervals) still
apply on top: the state can make a pass walk more, never less. Today each doubt also breaks one
of those checks, so the state adds no full walks except after a missing root. When the state is
what forces a walk, the journal says `the root state asks for a walk of the whole disk` or `of
all of Drive`.

A watch follows the folder it was set on, so a root moved away and made again is another folder
that the old watch does not see. The engine keeps the root's device and inode with each watch.
A pass that finds the root missing, or finds a root with another inode, drops the folder's trust
in its claims and asks for the watches to be set up again. Setting them up drops any watch whose
root is gone or replaced and watches the new one. Then it runs a pass over each folder it watched
anew, since a file written before the watch was in place has no event. Until a watch is back,
the disk is walked in full.

**Tests:** the `root::tests` unit tests (a doubt per side, a restart, a suspension, a handoff),
`a_restart_walks_the_disk_in_full_and_not_drive` (sim: the first pass after a restart walks the
disk and reuses the picture of Drive), and `a_root_made_again_is_watched_again` and
`a_root_replaced_between_passes_is_watched_again` (sims: a file written in a root moved away and
made again reaches Drive within a minute; the second fails without the inode check).

## Effect intents as built

The second reducer, `pdfs_sync::effect`, follows one effect a mirror pass sends: a folder made on
Drive, an upload, a new revision, a download, an adopted file or a conflict copy (the `Pending`
ops). Before an op is sent, its intent is written to `sync_intent` (schema 51): one row per
folder, path and kind, with the number of attempts and the time it was last sent. Its result
decides what happens next:

| State | Meaning |
|---|---|
| issued | Sent, or about to be; no result yet. |
| unsure | Found `issued` at a start: the daemon stopped before the result came, so the effect may or may not have happened. |
| retry | It failed, and a later pass may send it again. |
| blocked | It failed five times in a row. The journal says `sync: an effect keeps failing` once. |
| settled | Its result is in. The row goes. |

- A start moves every `issued` row to `unsure`, and says how many in the journal.
- An op sent again keeps its row and counts the attempt. When the row was `unsure`, the journal
  says `sync: sending again an effect a stopped daemon may have made`.
- A pass that looked at every path (none unscanned or deferred) settles the rows from before it
  began that it did not send again: what they were for has changed, or is done. An `unsure` one
  is logged at info.
- A change to what the folder is, or its removal, drops its rows.

The intents decide one thing so far: whether a file on Drive is an upload of this daemon's
that went unanswered.
- An upload that does not hear a success back notes the SHA-1 of the bytes it streamed in its
  intent (`sent_sha1s`, schema 52). Each attempt adds its digest.
- When a later pass would keep a conflict copy, it checks the intent first. The path may be new
  on both sides, or changed on both sides since the baseline. If Drive's file holds exactly
  bytes an unsettled upload of that path sent, the file is ours. The newer local bytes then go up
  as a revision of it, with no conflict copy. The same holds for a first-sync adopt of a
  same-size file whose bytes differ.
- This is the usual end of a lost reply to a one-request small upload. Such an upload leaves a
  whole file, never a draft. If the file was written again before the next pass, for example
  while the daemon was stopped, the next pass used to see two different files and fork a
  conflict copy of this computer's own edit.
- A file that only happens to hold the same bytes loses nothing either: its content is what
  this computer sent.
- A daemon killed in the middle of an upload notes no digest, so that case still ends in a
  conflict copy when the file is edited before the next pass. It is kept, and nothing is lost.

Otherwise nothing reads the intents to decide what to send. The apply step's own checks still
decide whether an effect that may have landed is sent again (B266 for a create, B39 for a
download). Deletions on either side are not recorded yet. Trace counts: `unsure resent`,
`retried`, `superseded`.

**Tests:**
- the `effect::tests` unit tests;
- DB: `an_effect_intent_is_kept_until_it_is_settled_or_its_folder_changes`, and
  `an_upload_notes_each_digest_it_sent_once`;
- sim `an_effect_left_unsure_by_a_stop_is_sent_again_or_settled`: of two uploads a stopped daemon
  left unsure, the one still needed is sent again and the other is settled;
- sim `a_mirror_create_whose_reply_was_lost_is_adopted_after_a_restart_and_its_edit_lands`:
  Drive makes the file and the answer is lost, then the daemon stops and the file is written
  again. After the start there is one file, holding the newer bytes, and no conflict copy.

## Plan of work (separate PRs)

1. **Frontier checkpoint.**
   - Done: a resumable walk of Drive, described above.
   - Left: per-subtree apply, once a round-level guard exists. A resumable local walk matters
     less now that the local walk is the only full walk left, and is cheap next to Drive.
2. **Remote dirty set.** Done, described above.
3. **Local dirty set.** Done:
   - The watcher marks each path an event names in `LocalDirty` (in memory): the path, with
     what is under it, and the folder it is in. A pass claims the marks before it looks at the
     disk, so a change made while it looks stays marked for the next pass, and retires them once
     it applied everything it found. A claim is trusted when the watch was live since the last
     pass claimed; a watcher error, an overflow or a restart breaks that.
   - A pass keeps what it found on disk (`LocalWalk`) for the next one, unless it changed the
     disk itself (a download, a conflict copy, a folder made or removed), failed a step, deferred
     a file or could not read a path. Its own changes are marked only once the watcher reports
     them, which may come after the next pass claims, so that pass walks in full instead.
   - The next pass with a trusted claim, the same ignore rules (`IgnoreRules::source`) and a
     full walk less than an hour old starts from the kept walk (`local reused` in the trace).
     It lists each marked folder again, walks each marked path again with what is under it, and
     reads again each file that was open for writing. Everything else is taken as it was.
   - A full walk with a trusted claim still counts the changes the claim did not cover
     (`missed local changes`). It runs at least once an hour, and after anything that breaks
     the trust.
   - A mark that was missed delays a change until the next full walk. It cannot lose one: a
     download and a local deletion check the file on disk against what the pass planned from
     before they act, and a path missing from the kept walk is one a walk found gone.
   - **Tests:** the `rewalk` unit tests in `mirror.rs` (each kind of change, ignored folders, a
     mark that came before its folder's, a file that was open for writing), and
     `a_trusted_local_dirty_set_stands_in_for_the_walk` (sim: an edit, a new folder, a deletion
     and a folder moved in reach Drive through reused walks; it fails with the watcher's marks
     switched off).
4. **Step scheduler.** Replace `run_burst` and the per-pass lock with steps and priorities.
5. **Drop the 120 s poll** for the daily audit, once 2 and 3 have run for a release with the
   poll still on, and traces show the dirty sets catch everything the poll finds.

## Testing

- Model-based sim test: random local and Drive edits, random restarts. After every quiet period,
  the incremental engine must reach the same local tree, Drive tree and baseline as a full walk
  from scratch.
- Trace assertions, as in `a_mirror_pass_reports_where_its_time_went`: a Steady pass over one
  changed file lists one folder and reads one node, at any tree size.
