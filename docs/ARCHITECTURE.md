# Proton Drive Linux Client: Architecture Specification

This document provides a deep, comprehensive architectural description of the Proton Drive Linux Client (`pdfs`). It specifies the design patterns, data flows, thread models, and subsystem dependencies that govern the virtual filesystem (FUSE), database persistence, cache management, and two-way sync engine.

---

## 1. Subsystem Overview & Crate Topology

The application is modularized into four workspace crates, dividing core library logic, filesystem mounting, control-socket IPC, and front-ends.

```mermaid
graph TD
    %% Crates
    CLI["crates/pdfs-cli (CLI & Daemon Entrypoint)"]
    GUI["crates/pdfs-gui (GTK Front-end)"]
    FUSE["crates/pdfs-fuse (FUSE VFS & Sync Loop)"]
    CORE["crates/pdfs-core (DB, Cache & IPC Protocol)"]
    SDK["proton-sdk-rs (Proton Drive API & Cryptography)"]

    %% Dependencies
    CLI --> FUSE
    GUI --> CORE
    FUSE --> CORE
    CORE --> SDK
    CLI -.->|Unix Socket IPC| FUSE
    GUI -.->|Unix Socket IPC| FUSE
```

### Crate Division & Responsibility Matrix

| Crate | Primary Role | Key Components | State Management |
|---|---|---|---|
| [`pdfs-core`](file:///home/narl/dev/private/proton-drive-linux/crates/pdfs-core) | Core Infrastructure & Services | Cache Bookkeeping, Database migrations/schemas, IPC protocol payloads. | Holds the unified SQLite DB (`Db`) connection and the on-disk cache metadata (`ContentCache`). |
| [`pdfs-fuse`](file:///home/narl/dev/private/proton-drive-linux/crates/pdfs-fuse) | VFS Layer & Reconciliation | FUSE callbacks, background upload queue (`drain`), two-way sync runner. | Manages in-memory inode maps (`State`), active descriptors (`WriteHandle`), and background task threads. |
| [`pdfs-cli`](file:///home/narl/dev/private/proton-drive-linux/crates/pdfs-cli) | Command Line Interface | Command routing, daemon launcher, IPC client wrapper. | Stateless; communicates with daemon over IPC control socket. |
| [`pdfs-gui`](file:///home/narl/dev/private/proton-drive-linux/crates/pdfs-gui) | Graphical Interface | GTK Page timelines (Files, Photos, Shares, Status). | Stateless; polls daemon for status and lists timelines via IPC socket. |

---

## 2. In-Memory VFS State & File Operations

The VFS layer implements FUSE via the `fuser` crate. Because the remote storage contains base64-encoded file keys and requires cryptographic envelope parsing, raw listings and inodes are virtualized and stored in a local state directory.

### Inode and Path Resolution
* **In-Memory Cache (`State`):** Maps FUSE `u64` inodes to Proton Drive `NodeUid`s.
* **Database Row Mapping (`StoredNode`):** Stores directories, sizes, and timestamps.
* **On-Demand Loading (`ensure_children`):** If a directory is accessed, the daemon checks its database `listed` flag. If `listed = 0`, it triggers an API call to fetch remote nodes, populates the DB and in-memory caches, and returns.

```mermaid
sequenceDiagram
    autonumber
    actor User as Kernel (VFS Call)
    participant FUSE as pdfs-fuse VFS
    participant ST as State (In-Memory)
    participant DB as Db (SQLite)
    participant API as Proton API Client

    User->>FUSE: lookup(parent_ino, "report.pdf")
    FUSE->>ST: children.get(&parent_ino)
    alt Parent listing is resident in-memory
        ST-->>FUSE: returns child_ino
    else Listing missing in-memory
        FUSE->>DB: children_if_listed(parent_uid)
        alt Parent marked listed in DB
            DB-->>FUSE: returns child node metadata list
            FUSE->>ST: intern_from_db() and populate children cache
        else Parent not listed in DB
            FUSE->>API: enumerate_folder_children_node_uids()
            API-->>FUSE: list of UIDs
            FUSE->>API: enumerate_nodes(uids)
            API-->>FUSE: list of decrypted Nodes
            FUSE->>DB: upsert_nodes() & set_listed(true)
            FUSE->>ST: intern_batch() and populate children cache
        end
    end
    FUSE-->>User: returns child inode metadata (attributes, TTL)
```

---

## 3. Read Path & Block Caching Pipeline

Read requests are parallelized and served in blocks of size `BLOCK_SIZE` (4 MiB).

* **Unpinned files (Streaming):** Avoids writing full files to disk to preserve storage. Instead, blocks are kept in a fixed-size `stream_ring` (in-memory ring cache) and evicted immediately.
* **Pinned files (Persistent):** Block downloads are saved directly to `ContentCache` on disk.
* **Read-Ahead:** The reader thread spawns asynchronous tasks to pre-fetch upcoming blocks.

```mermaid
sequenceDiagram
    autonumber
    actor Kernel as Kernel Read (offset, size)
    participant FUSE as pdfs-fuse VFS
    participant Cache as ContentCache (Local Disk)
    participant Ring as StreamRing (In-Memory)
    participant API as Proton API Client

    Kernel->>FUSE: read(ino, fh, offset, size)
    FUSE->>FUSE: calculate block indices [first_block..last_block]
    loop For each block index
        alt Block in disk Cache (Pinned/Cache hit)
            FUSE->>Cache: read_block(block_idx)
            Cache-->>FUSE: block bytes
        else Block in stream ring cache
            FUSE->>Ring: get(block_idx)
            Ring-->>FUSE: block bytes
        else Block Cache Miss
            FUSE->>API: download_range(offset, len)
            API-->>FUSE: decrypted block bytes
            alt caching_enabled
                FUSE->>Cache: store_block(block_idx)
            else streaming_only
                FUSE->>Ring: insert(block_idx)
            end
        end
    end
    FUSE->>FUSE: stitch blocks and slice to offset/size
    FUSE-->>Kernel: return data buffer
```

---

## 4. Write Path & Staging/Draining Pipeline

Because Proton Drive does not support partial byte writes, modified files must be uploaded as whole new revisions.

1. **Staging writes (`WriteHandle`):** Writes are stored locally in a `scratch` file. The daemon tracks modified regions using `Intervals` (which holds ranges of edited bytes).
2. **Close/Release (`queue_revision`):** When the application closes the file descriptor, the daemon:
   - Fetches any untouched gaps from the remote base file to compile the full file.
   - Moves the scratch file to `staging` under a cryptographic checksum name.
   - Queues a pending database operation (`PendingOp`).
3. **Async Drain Thread (`run_pending_drain`):** The background drain worker picks up the database operations queue, handles revisions uploads, resolves conflicts, and cleans up staging files.

```mermaid
sequenceDiagram
    autonumber
    actor Kernel as Kernel Write (fh, offset, data)
    participant FUSE as pdfs-fuse VFS
    participant WH as WriteHandle (Scratch File)
    participant DB as Db (SQLite Queue)
    participant DR as Drain Thread
    participant API as Proton API Client

    Kernel->>FUSE: write(ino, fh, offset, data)
    FUSE->>WH: write_at(offset, data)
    FUSE->>WH: update written intervals
    FUSE-->>Kernel: return bytes_written

    Note over Kernel, FUSE: Application closes file (close(2))
    Kernel->>FUSE: release(fh)
    FUSE->>FUSE: fill_gaps() (fetch untouched remote ranges)
    FUSE->>FUSE: move scratch file to staging directory
    FUSE->>DB: enqueue_op(OP_REVISION, staged_path, meta)
    FUSE->>FUSE: record_pending_write() (update size/mtime in memory & DB)
    FUSE-->>Kernel: return success (async release)
    
    Note over DB, DR: Background Queue Processing
    DR->>DB: next_due_op()
    DB-->>DR: return OP_REVISION
    DR->>API: upload_new_revision_from(staged_path)
    API-->>DR: return new node revision metadata
    DR->>DB: delete_op()
    DR->>FUSE: refresh_after_upload() (sync local metadata with server time)
```

---

## 5. Sync Engine (Two-Way Reconciliation)

The sync engine handles offline-capable, bidirectional synchronization between the local disk and Proton Drive for directories marked in `mirror` mode.

### Lifecycle of a Sync Pass
1. **Walk Local:** Walks the local directory tree recursively, scanning sizes and modification times.
2. **Walk Remote:** Walks the remote database representation. If remote file modification times are updated, it calls the API to decrypt their sizes.
3. **Load Baseline:** Loads the `sync_entry` database table, which contains the snapshot of both sides during the *last successful sync*.
4. **Permutation Diffing:** The loop compares the three states (`local`, `remote`, `baseline`) to classify items:

```mermaid
graph TD
    %% States
    Classify{"Classify (Local, Remote, Baseline)"}

    %% Logic Rules
    Classify -->|Both Sides Match| Match["No-Op (In Sync)"]
    Classify -->|Local Changed, Remote Untouched| Upload["Upload Revision"]
    Classify -->|Remote Changed, Local Untouched| Download["Download Revision"]
    Classify -->|Both Sides Changed| Conflict["Conflict Copy (Local renamed to 'sync-conflict', remote downloaded)"]
    Classify -->|Local Deleted, Remote Untouched| RemoteDelete["Trash Remote Node"]
    Classify -->|Remote Deleted, Local Untouched| LocalDelete["Delete Local File"]
    Classify -->|New Local File, No Remote/Baseline| UploadNew["Upload New Node"]
    Classify -->|New Remote File, No Local/Baseline| DownloadNew["Download New Node"]
```

5. **Depth-Ascending Batching:** Folders are processed first to ensure hierarchies exist before files are placed. Work is executed concurrently up to a set limit.
6. **Post-Sync Settle:** On success, baseline entries are upserted, timestamps updated, and any pending mode switches (e.g. going on-demand) are evaluated.

---

## 6. IPC Socket Protocol

The CLI and GUI front-ends do not access database files or make network calls directly. They communicate with the background daemon process over a Unix domain socket.

* **Transport:** IPC over Unix Stream Socket.
* **Framing:** Line-delimited JSON payloads.
* **Control Protocol:**
  * Client sends a single JSON line (`Request`).
  * Daemon parses, handles the request, and replies with a single JSON line (`Response`).
  * Timeout durations are separated: **2 seconds** for writes (avoids hangs on defunct sockets) and **120 seconds** for reads (accommodates heavy transfers).

---

## 7. Subsystem Interaction & Thread Map

The background daemon relies on the following thread topology:

1. **Main Thread / Dispatch Loop:** Blocks on `fuser::Session` loop. Reads kernel FUSE events and hands off network-bound VFS work to the FUSE workers pool.
2. **FUSE Workers Pool (8 threads):** Bounded thread pool handling network operations (block reads, gap filling, file creations) to avoid stalling metadata operations.
3. **IPC listener Thread:** Listens on Unix socket connections, spawning a lightweight task per connection to serve front-end status/configuration requests.
4. **Sync Engine Loop Thread:** Serializes sync runs. Wakes on debounced local inotify filesystem changes, remote polling intervals, or manual user requests.
5. **Drain Queue Worker Thread:** Processes staged writes (`PendingOp`) sequentially, uploading revisions and retrying with exponential backoff on failures.
