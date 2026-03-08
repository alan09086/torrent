# M63 Design: Throughput & Memory Improvements

## Goal

Close the throughput and memory gap with rqbit. Current state (v0.68.0):

| Metric | torrent | rqbit | Gap |
|--------|---------|-------|-----|
| Speed | 15.8 MB/s | 70.1 MB/s | 4.4x slower |
| RSS | 93.6 MiB | 39.2 MiB | 2.4x more |
| CPU:wall | 1.59 | 0.50 | CPU is bottleneck |

CPU architecture refactor deferred to a follow-up milestone — focus here is on the three highest-impact changes from the gap analysis that improve throughput and memory without rewriting the dispatch model.

## Task 1: Gate Hashing Concurrency

**Problem:** `enqueue_verify()` and `enqueue_verify_v2()` in `disk.rs:560-630` spawn `tokio::task::spawn_blocking()` hash tasks with no concurrency limit. When multiple pieces complete simultaneously, dozens of SHA-1/SHA-256 tasks flood the blocking pool, saturating all CPU cores. This causes CPU:wall = 1.6 (162.5s CPU for 102s wall time), starving the async runtime of CPU time to process incoming blocks.

**Fix:** Add a shared `Arc<Semaphore>` (permit count = 2) for hash verification tasks. Acquire a permit before `spawn_blocking`, release on completion. This caps concurrent hashing to 2 threads, freeing CPU cores for async block processing.

**Location:** `crates/torrent-session/src/disk.rs` — `enqueue_verify()` (line 560) and `enqueue_verify_v2()` (line 610)

**Expected impact:** CPU:wall drops from 1.6 to ~0.7. Throughput improves because the async runtime gets more CPU time to process incoming blocks.

## Task 2: Batch Dispatch (N Blocks Per Wake)

**Problem:** Each request driver wake dispatches exactly 1 block (1 semaphore acquire → 1 channel send → 1 actor dispatch). With 200 peers at 70 MB/s, this is ~4,375 channel messages/sec through a single mpsc(256), creating contention and per-message overhead.

**Fix:** When the torrent actor receives a `NeedBlocks` message from a driver, dispatch up to N blocks (e.g. 8) for that peer in a single pass instead of 1. The driver acquires N permits before sending one `NeedBlocks` message. This reduces channel traffic by ~8x and amortises the per-wake overhead.

**Approach:** Modify `request_driver.rs` to acquire up to 8 permits (or whatever is available via `try_acquire_many`) before sending `NeedBlocks`. Modify `dispatch_single_block` (or add `dispatch_blocks_batch`) in `torrent.rs` to loop and dispatch multiple blocks per invocation. The `DriverMessage::NeedBlocks` variant carries the number of blocks requested.

**Location:** `crates/torrent-session/src/request_driver.rs`, `crates/torrent-session/src/torrent.rs` (`dispatch_single_block`)

**Expected impact:** ~8x fewer channel messages, reduced async overhead per block.

## Task 3: Eliminate Store Buffer (Direct Disk I/O)

**Problem:** Every in-flight piece stores all received blocks in a `Mutex<HashMap<(Id20, u32), BTreeMap<u32, Bytes>>>` (the store buffer) until piece verification completes. With `max_in_flight_pieces=32` and 512 KB pieces, this holds ~16 MiB in RAM. The Mutex also creates contention between the async runtime (inserting blocks) and the blocking pool (removing for hashing).

**Fix:** Write blocks directly to disk on receipt via the existing `DiskJob::WriteChunk` path. For verification, read blocks back from disk instead of from the store buffer. Remove the store buffer entirely.

**Approach:**
1. In `write_chunk()`, write to disk immediately (already happens via `DiskJob::WriteChunk`) and do NOT insert into the store buffer
2. In `enqueue_verify()`, read the piece data from disk (via `backend.read_chunk()` or a new `backend.read_piece()`) instead of `store_buffer.remove()`
3. Remove the `StoreBuffer` type and all references
4. This also eliminates the Mutex contention between async and blocking threads

**Location:** `crates/torrent-session/src/disk.rs` — `StoreBuffer` type (line 47), `write_chunk()`, `enqueue_verify()`

**Expected impact:** RSS drops by ~16-50 MiB (depending on piece sizes). Mutex contention eliminated.

## Task 4: Benchmark (5 trials)

Run 5-trial benchmark with the Arch ISO magnet, compare against rqbit and qbittorrent. Generate report.

## Task 5: Version Bump to 0.69.0

Bump workspace version, update CHANGELOG.md and README.md with M63 changes and benchmark results. Push to both remotes.

## Success Criteria

- CPU:wall ratio < 1.0 (was 1.59)
- RSS < 60 MiB (was 93.6 MiB)
- Speed improvement measurable (target: 25+ MB/s)
- All 1402+ tests pass
