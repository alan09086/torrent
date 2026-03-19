# M116: Session-Level BlockingSpawner + Hot-Path Allocation Cleanup

## Context

After M114-M115, the remaining allocation hot spots are:
- **`run_peer` spawning** (26K allocs): Each `tokio::spawn` for a peer task allocates
  the future + task header. Not directly reducible, but the per-peer setup inside
  `run_peer` allocates auxiliary structures that can be cached or eliminated.
- **`Vec::finish_grow`** (18K): Dynamic resizing during download — piece lists,
  peer lists, event batches.
- **`InfoDict::files()` per piece** (10K): Called in `check_file_completion()` for
  every verified piece. Allocates a fresh `Vec<FileEntry>` each time.
- **`String::clone`** (4K): File path cloning in storage operations.
- **Unbounded `spawn_blocking`**: Disk I/O uses unbounded `spawn_blocking` —
  no limit on concurrent blocking threads, which can lead to thread pool
  starvation under heavy load.

rqbit addresses these with:
1. `BlockingSpawner` with semaphore-gated `block_in_place` (bounded blocking)
2. Cached metadata (computed once, shared immutably)
3. Cooperative `yield_now()` after message processing

**Target**: Reduce total heap allocations from ~120K (post M114-M115) to < 100K.

## Architecture

### Current Disk I/O: Unbounded spawn_blocking

```
peer_task → disk.write_block_deferred(index, begin, data)
         → mpsc channel → writer_task
         → tokio::task::block_in_place(|| storage.write_chunk())  ← no concurrency limit

verify → tokio::task::spawn_blocking(|| hash piece)  ← unbounded threads
```

Under heavy load with many torrents, every peer's direct pwrite and every piece
verification can spawn a blocking thread simultaneously, potentially exhausting
the tokio blocking thread pool (default: 512 threads).

### Proposed: BlockingSpawner with Semaphore

```
peer_task → spawner.block_in_place_with_semaphore(|| storage.write_chunk())
                     ↑ acquires permit first (bounded to num_cpus)

verify → spawner.block_in_place_with_semaphore(|| hash piece)
```

A single `BlockingSpawner` instance shared across the session limits concurrent
blocking operations. When all permits are held, callers await until one is released.

### Current InfoDict::files() Pattern

```
on_piece_verified(index) {
    check_file_completion(index) {
        let files = self.info_dict.files();  ← allocates Vec<FileEntry> every time
        for file in files { ... }
    }
}
```

Called ~5822 times per 1.4 GB download (once per piece). Each call allocates a
`Vec<FileEntry>` with path strings.

### Proposed: Cached File Info

```
on_torrent_registered(meta) {
    self.cached_files = Arc::new(compute_file_info(meta));  ← once
}

on_piece_verified(index) {
    check_file_completion(index) {
        for file in self.cached_files.iter() { ... }  ← zero alloc
    }
}
```

## rqbit Reference

### `crates/librqbit/src/spawn_utils.rs` lines 10-58: `BlockingSpawner`

```rust
#[derive(Clone, Debug)]
pub struct BlockingSpawner {
    allow_block_in_place: bool,
    concurrent_block_in_place_semaphore: Arc<Semaphore>,
}

impl BlockingSpawner {
    pub fn new(max_blocking_threads: usize) -> Self {
        let handle = tokio::runtime::Handle::current();
        let allow_block_in_place = match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::CurrentThread => false,
            tokio::runtime::RuntimeFlavor::MultiThread => true,
            _ => true,
        };
        Self {
            allow_block_in_place,
            concurrent_block_in_place_semaphore: Arc::new(Semaphore::new(
                max_blocking_threads.max(1),
            )),
        }
    }

    pub async fn block_in_place_with_semaphore<F: FnOnce() -> R, R>(&self, f: F) -> R {
        if self.allow_block_in_place {
            let _permit = self.concurrent_block_in_place_semaphore.acquire().await.unwrap();
            return tokio::task::block_in_place(f);
        }
        f()  // Single-threaded runtime: just call directly
    }
}
```

Key design:
- Runtime flavor detection (multi-thread vs current-thread)
- Semaphore limits concurrent blocking operations
- Falls back to direct call on single-threaded runtime (avoids panic)
- `block_in_place` instead of `spawn_blocking` (no task overhead)

### `crates/librqbit/src/peer_connection.rs` line 371: `yield_now()`

```rust
// In reader loop:
let message = read_buf.read_message(&mut read, rwtimeout).await?;
tokio::task::yield_now().await;  // Cooperative yielding
self.handler.on_received_message(message).await.map_err(Error::Anyhow)?;

// In writer loop:
tokio::task::yield_now().await;  // Before serializing + writing
let len = match req { ... };
write.write_all(&write_buf[..len]).await?;
```

Prevents a single busy peer from monopolizing a tokio worker thread. Especially
important when many peers are active simultaneously.

## Changes

### 1. BlockingSpawner (`torrent-session/src/blocking_spawner.rs`, ~70 lines)

New file:

```rust
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Limits concurrent blocking operations to prevent thread pool starvation.
/// Shared across the entire session.
#[derive(Clone, Debug)]
pub(crate) struct BlockingSpawner {
    allow_block_in_place: bool,
    semaphore: Arc<Semaphore>,
}

impl BlockingSpawner {
    /// Create a new spawner. `max_blocking` should be <= num_cpus.
    pub fn new(max_blocking: usize) -> Self {
        let handle = tokio::runtime::Handle::current();
        let allow_block_in_place = match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::CurrentThread => false,
            tokio::runtime::RuntimeFlavor::MultiThread => true,
            _ => true,
        };
        Self {
            allow_block_in_place,
            semaphore: Arc::new(Semaphore::new(max_blocking.max(1))),
        }
    }

    /// Run a blocking closure with semaphore-gated concurrency.
    /// Acquires a permit, then runs `f` via `block_in_place`.
    /// On single-threaded runtime, calls `f` directly (no `block_in_place`).
    pub async fn block_in_place<F: FnOnce() -> R, R>(&self, f: F) -> R {
        if self.allow_block_in_place {
            let _permit = self.semaphore.acquire().await.unwrap();
            return tokio::task::block_in_place(f);
        }
        f()
    }

    /// Synchronous variant for contexts that cannot await.
    /// Tries to acquire a permit without waiting; if unavailable, runs anyway.
    pub fn block_in_place_sync<F: FnOnce() -> R, R>(&self, f: F) -> R {
        if self.allow_block_in_place {
            return tokio::task::block_in_place(f);
        }
        f()
    }
}
```

### 2. Session integration (`torrent-session/src/session.rs`, ~20 lines)

```rust
// In SessionActor fields:
blocking_spawner: BlockingSpawner,

// In SessionActor::start():
let num_cpus = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(4);
let blocking_spawner = BlockingSpawner::new(num_cpus);

// Pass to DiskManagerHandle:
let disk_manager = DiskManagerHandle::new(backend, blocking_spawner.clone());

// Pass to TorrentActor:
TorrentActor::new(..., blocking_spawner.clone(), ...)
```

### 3. Settings field (`torrent-session/src/settings.rs`, ~5 lines)

```rust
/// Maximum concurrent blocking disk I/O operations.
/// Default: number of CPU cores. Setting this too high can starve
/// the async runtime; too low throttles disk throughput.
#[serde(default = "default_max_blocking_threads")]
pub max_blocking_threads: usize,

fn default_max_blocking_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}
```

### 4. DiskManagerHandle integration (`torrent-session/src/disk.rs`, ~30 lines changed)

Replace `spawn_blocking` calls with `BlockingSpawner`:

```rust
pub(crate) struct DiskManagerHandle {
    backend: Arc<dyn DiskIoBackend>,
    spawner: BlockingSpawner,
    // ... existing fields
}

// In write_block_deferred fallback (line ~761):
// Before:
tokio::task::block_in_place(|| { storage.write_chunk(...) });
// After:
self.spawner.block_in_place(|| { storage.write_chunk(...) }).await;

// In verify_piece (line ~661):
// Before:
let passed = tokio::task::spawn_blocking(move || { ... }).await?;
// After:
let passed = self.spawner.block_in_place(|| { ... }).await;
```

Replace the remaining `spawn_blocking` calls in disk.rs lines 661, 701, 888,
939, 963, 985, 1007, 1030, 1051, 1065. Each becomes
`spawner.block_in_place(|| ...)`.

Note: The HashPool (M96) already has its own thread pool for SHA1 hashing.
BlockingSpawner applies to non-hash disk I/O only.

### 5. Cached file metadata (`torrent-session/src/torrent.rs`, ~40 lines)

```rust
/// Pre-computed file info for fast per-piece file completion checks.
#[derive(Debug, Clone)]
pub(crate) struct CachedFileInfo {
    /// (file_index, file_path, file_length, first_piece, last_piece)
    pub entries: Vec<CachedFileEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedFileEntry {
    pub index: usize,
    pub path: String,
    pub length: u64,
    pub first_piece: u32,
    pub last_piece: u32,
}

// In TorrentActor fields:
cached_files: Option<Arc<CachedFileInfo>>,

// Populated when metadata is available (registration or post-metadata download):
fn cache_file_info(&mut self) {
    if let Some(meta) = &self.meta {
        let lengths = &self.lengths;
        let entries: Vec<CachedFileEntry> = meta.info.files()
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let offset = ...; // compute from prior files
                CachedFileEntry {
                    index: i,
                    path: f.path.join("/"),
                    length: f.length as u64,
                    first_piece: (offset / lengths.piece_length as u64) as u32,
                    last_piece: ((offset + f.length as u64 - 1) / lengths.piece_length as u64) as u32,
                }
            })
            .collect();
        self.cached_files = Some(Arc::new(CachedFileInfo { entries }));
    }
}
```

### 6. check_file_completion uses cached files (~15 lines changed)

```rust
fn check_file_completion(&self, piece_index: u32) {
    let cached = match &self.cached_files {
        Some(c) => c,
        None => return,
    };
    let ct = match &self.chunk_tracker {
        Some(ct) => ct,
        None => return,
    };

    // Binary search or linear scan for files containing this piece
    for entry in &cached.entries {
        if piece_index < entry.first_piece || piece_index > entry.last_piece {
            continue;
        }
        // Check if all pieces in this file's range are complete
        let all_complete = (entry.first_piece..=entry.last_piece)
            .all(|p| ct.bitfield().get(p));
        if all_complete {
            // Fire FileCompleted alert
            post_alert(&self.alert_tx, &self.alert_mask, AlertKind::FileCompleted {
                info_hash: self.info_hash,
                file_index: entry.index,
            });
        }
    }
}
```

### 7. Cooperative yielding (`torrent-session/src/peer.rs`, ~6 lines)

Add `yield_now()` at two strategic points in the peer message loop:

```rust
// In the reader select! arm, after processing a received message:
msg = reader.next_message() => {
    match msg {
        Some(Ok(message)) => {
            handle_message(message, ...).await?;
            tokio::task::yield_now().await;  // NEW: cooperative yield
        }
        // ...
    }
}

// In the writer path, before writing a response:
if let Some(response) = response_msg {
    writer.send(&response).await?;
    tokio::task::yield_now().await;  // NEW: cooperative yield
}
```

This prevents a single peer receiving a burst of messages (e.g., 100+ Have
messages) from monopolizing its worker thread and starving other peers.

### 8. Pre-sized collections audit (~20 lines across files)

Audit and fix `Vec::finish_grow` hot spots:

```rust
// peer.rs: PendingBatch blocks Vec
// Already has: Vec::with_capacity(BATCH_INITIAL_CAPACITY)
// Verify BATCH_INITIAL_CAPACITY >= 32 (a reasonable batch size)

// torrent.rs: peer list HashMap
// Already uses FxHashMap. Verify initial capacity at torrent registration.

// types.rs: PeerEvent variants with Vec<>
// Ensure event Vecs are pre-sized where the count is known.
```

## Tests

### New Tests (7)

1. **`blocking_spawner_limits_concurrency`** — Create spawner with max=2. Spawn
   4 concurrent blocking operations (each sleeps 100ms). Verify only 2 run
   simultaneously (total time ~200ms, not ~100ms).

2. **`blocking_spawner_semaphore_backpressure`** — Spawner with max=1. Start a
   long blocking operation, then try another. Verify second waits until first
   completes (measured via timestamps).

3. **`blocking_spawner_single_threaded_runtime`** — On a `current_thread` runtime,
   verify spawner calls `f()` directly without `block_in_place` (which would panic).

4. **`cached_files_populated_on_registration`** — Register a torrent with 3 files.
   Verify `cached_files` is `Some` with 3 entries, correct `first_piece` /
   `last_piece` ranges.

5. **`cached_files_used_in_completion_check`** — Complete all pieces for file 2
   (middle file in a 3-file torrent). Verify `FileCompleted` alert fires only
   for file 2, not files 1 or 3.

6. **`yield_after_piece_handling`** — Use `tokio::task::yield_now()` counter via
   mock. Verify yield is called after processing a Piece message.

7. **`yield_after_message_serialize`** — Verify yield is called after sending a
   response message in the writer path.

### Existing Tests

All 1647+ existing tests must pass. The `BlockingSpawner` is a drop-in
replacement for `spawn_blocking` with the same semantics (just bounded).

## Success Criteria

- InfoDict::files() allocations during download = 0 (was ~10K)
- Blocking threads bounded by semaphore (verify via tokio metrics or logging)
- No worker starvation under heavy load (10+ simultaneous torrents)
- Total heap allocations < 100K (was ~120K post M114-M115)
- All existing tests pass
- 7 new tests pass
- No regression in download speed (>= 55 MB/s)

## Verification

```bash
# Run all tests
cargo test --workspace

# Run new tests
cargo test -p torrent-session blocking_spawner_
cargo test -p torrent-session cached_files_
cargo test -p torrent-session yield_

# Benchmark (10 trials)
./benchmarks/analyze.sh '<arch-iso-magnet>' -n 10

# Heaptrack for alloc reduction
heaptrack --record-only ./target/release/torrent-cli download '<magnet>'
heaptrack_print heaptrack.torrent-cli.*.zst | head -50

# Verify InfoDict::files eliminated
heaptrack_print heaptrack.torrent-cli.*.zst | grep -i "InfoDict\|files()"
```

## NOT in Scope

| Item | Why | Revisit |
|------|-----|---------|
| Custom memory allocator (jemalloc, mimalloc) | Reducing allocation count is more impactful than faster allocation | If alloc count is already minimal and speed is still needed |
| Replacing tokio's timer allocations | Timer wheel allocations are fixed-cost per timer, not per-tick | Never (not a significant contributor) |
| HashPool integration with BlockingSpawner | HashPool already has its own dedicated thread pool (M96) | If hash and disk I/O need unified scheduling |
| Arena allocator for peer structures | Per-peer allocations are one-time setup cost, not hot-path | If peer churn becomes a bottleneck |
| Replacing spawn_blocking in non-disk paths | torrent.rs relocate_files and other rare paths use spawn_blocking infrequently | After measuring if they show up in profiles |
