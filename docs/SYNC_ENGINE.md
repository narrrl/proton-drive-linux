# Sync engine redesign (draft)

Status: proposal. Phase 1 (a resumable walk of Drive) is implemented; the rest is for review.

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

The pass trace (`sync/trace.rs`) is how we measure this design against the current one. Get
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

What is built:

- **Schema 48:** `sync_walk` (root, start time), `sync_walk_dir` (each folder found:
  listed or not, short listing) and `sync_walk_item` (each path read).
- **Saving:** `walk_remote` is a queue of folders, not a recursion. Each listing is saved in
  one transaction together with the subfolders it found. A failed listing is not saved, so
  the next walk asks for it again.
- **Resuming:** a pass resumes a saved walk when its root is the same and it is less than
  24 hours old. Otherwise it starts over.
- **Invariant:** a saved listing is never used against a baseline newer than itself.
  - SQLite triggers drop a folder's saved walk when any of its baseline rows is inserted,
    deleted or changed (filling in a revision id is the only exception), and when the
    folder's path, root or mode changes, or the folder is removed.
  - The pass drops the walk itself once it is complete, before it applies anything. If that
    fails, the pass fails.
  - The push pass drops it as well.
  - Without this, a move to another synced folder (`relocate.rs`) would leave a listing that
    reads the moved file as deleted on Drive.
- **Tests:** `a_saved_walk_resumes_where_it_stopped` and
  `a_baseline_change_drops_the_saved_walk` (DB), and
  `a_walk_of_drive_cut_short_resumes_where_it_stopped` (sim).

## Plan of work (separate PRs)

1. **Frontier checkpoint.**
   - Done: a resumable walk of Drive, described above.
   - Left: a resumable local walk; per-subtree apply, once a round-level guard exists.
2. **Step scheduler.** Replace `run_burst` and the per-pass lock with steps and priorities.
3. **Local dirty set.** `SyncMsg::Dirty`, the `sync_dirty` table, overflow leading to FullCheck.
4. **Remote dirty set.** Event-to-path mapping, `ContinuityLost` leading to FullCheck.
5. **Drop the 120 s poll** for the daily audit, once 3 and 4 have run for a release with the
   poll still on, and traces show the dirty sets catch everything the poll finds.

## Testing

- Model-based sim test: random local and Drive edits, random restarts. After every quiet period,
  the incremental engine must reach the same local tree, Drive tree and baseline as a full walk
  from scratch.
- Trace assertions, as in `a_mirror_pass_reports_where_its_time_went`: a Steady pass over one
  changed file lists one folder and reads one node, at any tree size.
