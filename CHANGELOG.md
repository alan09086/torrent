# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

## [0.85.0] — 2026-03-13

### Added
- **Iterative bootstrap**: DHT now performs multi-round `find_node` queries during
  bootstrap, converging on the closest nodes to our own ID. Previously, bootstrap
  sent one-shot queries to 3 DNS-resolved nodes with no follow-up — now it iterates
  through returned nodes (alpha=3, up to 6 rounds) to rapidly populate the routing table.
- **Node liveness tracking**: `NodeStatus` enum (Good/Questionable/Bad) per BEP 5 §4.1.
  Nodes that respond or query us within 15 minutes are Good; silent nodes are Questionable;
  nodes with 2+ consecutive failures are Bad and eligible for eviction.
- **Background pinger**: Questionable nodes are pinged every 3.75 minutes. Nodes that
  respond become Good again; those that don't accumulate failures and get evicted.
- **Query rate limiter**: Token-bucket rate limiter (250 queries/sec default) guards all
  outgoing KRPC queries, preventing UDP flood during rapid bootstrap or large lookups.
- **Periodic DHT persistence**: Session state (DHT nodes) saved every 60 seconds with
  atomic write-then-rename, not just on shutdown. Crash recovery now preserves recent state.
- **Proper failure tracking**: `PendingQuery` now stores the remote node's ID, so timed-out
  queries correctly increment `fail_count` on the actual node (previously used a stub that
  never marked any node as failed).

### Changed
- Default `queries_per_second` increased from 50 to 250 (both IPv4 and IPv6 configs).
- `worst_node()` eviction now correctly evicts the oldest node on tied fail counts
  (was incorrectly evicting the newest).
- `PendingQueryKind::FindNode` and `SampleInfohashes` simplified to unit variants (dead
  `target` fields removed — the target is in the KRPC message, not needed in pending state).
- `ItemLookupState`: removed dead `closest` fields (never iterated for advancement) and
  `public_key` from `Mutable` variant (comes from the response, not lookup state).
- Zero `#[allow(dead_code)]` annotations remain in the DHT actor.

### Fixed
- Dead nodes no longer occupy routing table slots forever — the `pending_node_id()` stub
  that returned `Id20::ZERO` (never matching any node) has been replaced with actual node
  ID tracking in `PendingQuery`.

## [0.84.0] — 2026-03-13

### Changed
- **Default crypto backend**: Switched from `ring` (BoringSSL) to `aws-lc-rs` (AWS-LC).
  AWS-LC's SHA-1/SHA-256 implementation is significantly faster on modern x86_64 with
  SHA-NI/AVX-512. Benchmarked: 28% less user CPU, 23% less RSS, 25% fewer page faults.
  The `ring` and `openssl` backends remain available via feature flags (`crypto-ring`,
  `crypto-openssl`).

### Added
- **Pluggable crypto backends**: New feature flags `crypto-ring`, `crypto-aws-lc` (default),
  and `crypto-openssl` on `torrent-core`, forwarded through `torrent` facade and `torrent-cli`.
  Allows compile-time selection of the SHA hashing backend. When multiple backends are enabled
  via feature unification, priority is: openssl > aws-lc > ring.
- **Crypto benchmark script**: `benchmarks/bench_crypto.sh` for A/B testing crypto backends
  with full metrics (CPU, RSS, context switches, page faults).

### Benchmark (Arch ISO ~1.45 GiB, 3 trials, ring vs aws-lc)
- User CPU: 7.30s avg (-28% from ring's 10.13s)
- RSS: 107 MB avg (-23% from ring's 139 MB)
- Speed: 55.7 MB/s avg (+13% over ring's 49.1 MB/s)
- Page faults: 55K avg (-25% from ring's 73K)
- Test count: 1442

## [0.83.0] — 2026-03-13

### Changed
- **Default encryption mode**: Changed from `PreferPlaintext` to `Disabled`. Eliminates
  23.4% CPU overhead (RC4 21.5% + DH key exchange 1.9%) and removes 3 extra RTT MSE
  handshake per connection. rqbit does not implement MSE at all — this matches its
  behaviour. Users needing encryption can opt in via settings.

### Fixed
- **AIMD initial queue depth mismatch**: Peer task's `INITIAL_QUEUE_DEPTH` was hardcoded
  to 32 while the settings default was 128. This forced 3 seconds of unnecessary AIMD
  slow-start ramp (32→64→128) on every peer connection. Now starts at 128 immediately.
- **CLI SIGTERM handling**: CLI only handled SIGINT (Ctrl+C) via `tokio::signal::ctrl_c()`.
  Benchmarks using `timeout --signal=TERM` and systemd stops bypassed the shutdown flag
  entirely — `save_session_state()` never ran, DHT nodes were lost. Now handles both
  SIGINT and SIGTERM via `tokio::select!`, enabling DHT node persistence across all
  termination scenarios.

### Benchmark (Arch ISO 2026.03.01 ~1.45 GiB, 3 trials)
- Speed: 56.8 MB/s avg (+47% over v0.82.0's 38.6 MB/s), peak 83.5 MB/s
- CPU: 13.1s avg (-29% from v0.82.0's 18.4s — crypto overhead eliminated)
- RSS: 68.8 MiB avg (-27% from v0.82.0's 94 MiB — no RC4 cipher buffers)
- Gap to rqbit narrowed from 1.9x to 1.3x (74.8 MB/s)
- Test count: 1442

## [0.82.0] — 2026-03-13

### Changed
- **Adaptive max_in_flight**: `max_in_flight` now scales dynamically with peer count
  (`max(256, peers × 3)` capped at `num_pieces / 2`). More connected peers automatically
  unlock more parallelism instead of being bottlenecked by a fixed 256 limit.
- **Cached rarest-first dispatch**: Pre-sorted candidate cache eliminates O(n) full-piece
  scans in `find_candidate()` and `next_request()`. Cache is invalidated on availability
  changes and rebuilt lazily on WRITE-lock paths. READ-lock path uses stale cache with
  `can_reserve()` guard for safe filtering.
- **AIMD pipeline**: Replaced fixed queue depth (128) with AIMD (Additive Increase,
  Multiplicative Decrease) congestion control. Slow-start doubles depth each second when
  throughput increases; steady-state adds +4 blocks/s. After 3 consecutive throughput
  decreases, depth halves. 4-tick cooldown prevents oscillation. Initial depth starts at
  32 and ramps up per-peer based on actual throughput.
- **Permit absorption for depth reduction**: When AIMD decreases target depth, excess
  semaphore permits are absorbed by not returning them on block receive, plus active
  draining of idle permits on the tick path. Unchoke handler drains stale permits before
  refilling to prevent phantom permit accumulation after Choke/Unchoke cycles.

### Fixed
- **Semaphore drain infinite loop**: The Unchoke handler's `while try_acquire().is_ok()`
  loop was an infinite loop — `SemaphorePermit`'s RAII `Drop` released the permit back
  immediately, so the count never reached zero. Same issue in pipeline tick depth-reduction
  drain. Fixed with `permit.forget()` to permanently consume permits.

### Benchmark (Arch ISO 2026.03.01 ~1.45 GiB, 3 trials)
- Speed: 38.6 MB/s avg (+31% over v0.80.0's 29.5 MB/s), peak 62.1 MB/s
- RSS: 88.9 MiB avg (-28% from v0.80.0's 123 MiB)
- Faster than qbittorrent (32.0 MB/s), gap to rqbit (77.0 MB/s) narrowed
- Test count: 1442 (+15 new tests)

## [0.81.0] — 2026-03-13

### Changed
- **Three-phase connect interval**: Replaced the binary `burst_connect` flag with a
  `ConnectPhase` enum implementing three connection phases: RampUp (0–15s, 100ms interval),
  Normal (15–60s, 500ms interval), and Steady (60s+, 5s interval). The old code used a
  single transition from 500ms to 5s at 10s. The new three-phase approach gives 5× faster
  initial peer discovery (100ms vs 500ms) over a longer ramp-up window (15s vs 10s),
  with a gradual settling period before reaching the steady-state 5s interval.

## [0.80.0] — 2026-03-13

### Changed
- **`notify_one()` replaces `notify_waiters()` in piece completion**: `complete_piece()` and
  `fail_piece()` in `piece_reservation.rs` now call `notify_one()` instead of `notify_waiters()`.
  Each completion frees exactly one slot — waking all 127 waiting peers simultaneously was
  wasteful. The safety-net periodic tick (added in M77) catches any missed wakeups.
- **Two-phase dispatch for `next_request()`**: Split the monolithic single-WRITE-lock O(n) scan
  into three phases: `try_current_piece()` (WRITE lock, O(1) hot path, ~15/16 calls),
  `find_candidate()` (READ lock, O(n) scan — runs concurrently across all peers), and
  `try_reserve()` (WRITE lock, O(1) race-guarded reservation). Allows 127 peers to scan the
  piece list simultaneously instead of serializing behind one writer.
- **Direct peer-to-disk writes**: Peer tasks now write block data directly to disk via
  `DiskHandle::enqueue_write()` instead of routing the full data payload through the actor's
  event channel. The actor receives lightweight `ChunkWritten` events (metadata only: piece
  index, begin offset, length) for bookkeeping. Removes ~1.45 GB of data transit through the
  event channel per download.

### Benchmark (Arch ISO 2026.03.01 ~1.45 GiB)
- Speed: 46.8 MB/s avg (3 trials) — flat vs M77's 48.2 MB/s (~3% delta, within swarm noise)
- RSS: 81.2 MB avg (down from M77's 89.4 MB, -9%)
- Test count: 1427 (+8 new dispatch tests)

## [0.79.0] — 2026-03-13

### Added
- **Safety-net periodic notification**: Added a 1-second `notify_waiters()` call in the
  `pipeline_tick` arm of `TorrentActor`'s select loop. Ensures request waiters are never
  permanently starved if a high-frequency notify path is removed or silent.

### Changed
- **Remove `notify_waiters()` from `add_peer()` and `peer_have()`**: These high-frequency
  methods in `piece_reservation.rs` were triggering wake storms — up to 128 context switches
  per call as all waiting peers woke simultaneously. Notifications are now consolidated into
  the periodic safety-net tick.
- **Increase event batch drain limit 256 → 512**: The event processing loop now drains up to
  512 events per iteration, reducing the frequency at which the actor yields back to the
  executor under high event throughput.
- **`pipeline_tick` uses `MissedTickBehavior::Skip`**: Prevents burst wake storms when the
  actor falls behind; missed ticks are discarded instead of firing in rapid succession.

### Benchmark (Arch ISO ~1.5 GiB, 3 trials)
- Speed: 54.2 MB/s avg (+3.2% over M76's 52.5 MB/s)
- RSS: 88 MB avg
- Test count: 1419

## [0.78.0] — 2026-03-13

### Changed
- **Remove `PieceSelector` from production code**: Gated `PieceSelector` (2,271 lines) and 5
  related types (`PeerSpeed`, `PeerSpeedClassifier`, `InFlightPiece`, `PickContext`, `PickResult`)
  behind `#[cfg(test)]`. Availability reads now go through `PieceReservationState::availability()`.
- **Remove `in_flight_pieces` dead code**: Eliminated the never-populated
  `FxHashMap<u32, InFlightPiece>` field and all 21 reference sites from `TorrentActor`. Diagnostic
  and web-seed queries now use `PieceReservationState` methods (`in_flight_count()`,
  `in_flight_peers()`, `is_piece_in_flight()`).
- **Move `peer_bitfields` to peer-local state**: Removed duplicate `FxHashMap<SocketAddr, Bitfield>`
  from `PieceReservationState`. Bitfield data is now passed as parameters to `remove_peer()`,
  `peer_have()`, and `next_request()`. Each peer task maintains its own `local_bitfield` for
  dispatch decisions. Have message dedup moved from shared state to actor level.
- **Burst-mode connection interval**: Start with 500ms connect interval for the first 10 seconds,
  then transition to 5s steady-state. Reduces startup/ramp-up overhead.
- **Fix `evaluate_auto_sequential`**: Now uses `reservation_state.in_flight_count()` instead of
  the always-zero `in_flight_pieces.len()`, allowing auto-sequential mode to actually activate.

### Benchmark (Arch ISO ~1.5 GiB, 3 trials)
- Speed: 52.5 MB/s median (no regression from M75's 56.7)
- RSS: 81 MB median (down from M75's 98 MB, -17%)
- Cache misses: 7.4M median (down from M75's 9.1M, -19%)
- Test count: 1419

## [0.77.0] — 2026-03-12

### Changed
- **Peer-integrated request dispatch**: Moved block requesting from separate `request_driver`
  tasks into each peer task's `tokio::select!` loop. Semaphore permits are now acquired and
  returned within the same task, eliminating the cross-task handoff that caused permit leaks
  in M73/M74. Architecture matches rqbit's proven single-task model.
- **Local per-peer semaphore**: Each peer task owns a `Semaphore::new(0)` that receives 128
  permits on unchoke (capped by `available_permits()`) and returns permits on Piece/RejectRequest
  receipt. No more global permit coordination.
- **End-game activation from reservation state**: `check_end_game_activation()` now uses
  `PieceReservationState::in_flight_count()` and `in_flight_peers()` to build the end-game
  block map, since the actor-side `in_flight_pieces` HashMap is no longer populated.

### Removed
- **`request_driver` module**: Deleted entirely (155 lines). Per-peer driver tasks, driver
  spawn/cancel/respawn logic, and 6+ actor-side permit return sites all eliminated.
- **`PeerState` driver fields**: Removed `semaphore`, `driver_cancel`, `driver_handle` from
  actor-side peer tracking.

### Benchmark (Arch ISO ~1.5 GiB, 3 trials)
- Speed: 56.7±11.7 MB/s (4.6x improvement over M74's 12.2 MB/s)
- CPU: 36–44% (down from M74's higher utilization)
- RSS: 86–116 MiB, stable across trials (M74 showed 69→125 MiB growth)
- rqbit reference: 82 MB/s (now at 69% of rqbit, was 15% in M74)
- Test count: 1419

## [0.76.0] — 2026-03-11

### Fixed
- **Semaphore permit leak on `try_send` failure**: Request driver replaced `try_send()`
  with cancellation-aware `send().await` via `tokio::select!`. A full channel no longer
  exits the driver (losing the permit forever); instead backpressure is applied naturally.
  Permits are explicitly returned on both cancellation and channel close.
- **Permit leak on rejected requests**: `RejectRequest` events from peers now return the
  semaphore permit that the driver consumed when sending the request. Without this, each
  rejection permanently reduces the driver's dispatch capacity.
- **Permit leak on write failures**: Early returns from `handle_piece_data()` on disk
  write errors now return the permit before bailing out.
- **Stale driver on MSE retry**: When a peer retries with plaintext after MSE failure,
  the old request driver (holding a stale `cmd_tx`) is now cancelled and cleaned up.
  Previously the stale driver would send requests into a dead channel, leaking permits
  and causing piece deadlocks.
- **MSE retry channel capacity**: Increased from 64 → 256 to match the higher throughput
  of per-peer autonomous dispatch.

### Benchmark (Arch ISO ~1.4 GiB, 3 trials)
- Speed: 12.2±5.6 MB/s (improved from M73's 10.8±6.2, still regressed from M72's 28.0)
- CPU: 25.3±6.0s (up from M73's 19.3s — high variance suggests resource accumulation)
- RSS: 87.5±30.2 MiB (up from M73's 67.6 MiB — trial 3 hit 125 MiB)
- rqbit reference: 82.2 MB/s, 9.8s CPU, 39.4 MiB RSS (6.7x speed gap)
- Test count: 1419

## [0.75.0] — 2026-03-11

### Added
- **Piece reservation state** (`PieceReservationState`): Shared state behind
  `Arc<parking_lot::RwLock<_>>` for concurrent per-peer dispatch. Piece-level
  exclusive ownership prevents the duplicate block assignment that caused M62's
  data corruption. Simplified picker: priority pieces → rarest-first.
- **Per-peer request drivers** (`request_driver`): Autonomous tokio tasks per
  unchoked peer. Each driver acquires semaphore permits (flow control), reserves
  pieces from shared state, and sends `PeerCommand::Request` directly to the
  peer task — bypassing the TorrentActor entirely for normal dispatch.
- `parking_lot` workspace dependency (fast synchronous RwLock for ~10μs
  critical sections).

### Changed
- **Architecture: actor-serialized → per-peer autonomous dispatch.** The
  TorrentActor no longer runs `batch_fill_all_peers()` or
  `request_pieces_from_peer()` for normal downloads. Drivers handle all dispatch
  reactively. The actor handles only verification, peer lifecycle, and endgame.
- `max_in_flight_pieces`: 40 → 256 (accommodates per-peer piece reservation)
- `min_memory` preset: 16 → 32 | `high_performance` preset: 80 → 512
- Pipeline tick: 500ms → 1000ms (dispatch is fully reactive via drivers)

### Removed
- `batch_fill_all_peers()` (~335 lines) — replaced by per-peer drivers
- `request_pieces_from_peer()` (~230 lines) — replaced by per-peer drivers
- `PeerCommand::RequestBatch` — drivers send individual `Request` per block
- `PeerState::block_queue` / `needs_refill` — replaced by semaphore flow control
- `refill_notify` select arm — drivers wake via `piece_notify`

### Benchmark (Arch ISO ~1.4 GiB, 3 trials)
- Speed: 10.8±6.2 MB/s (regression from 28.0 MB/s — dispatch stalls under investigation)
- CPU: 19.3±2.9s (down from 31.9s — 40% reduction)
- RSS: 67.6±4.6 MiB (down from 94.8 MiB — 29% reduction)
- Test count: 1419

## [0.74.0] — 2026-03-11

### Changed
- **Zero-alloc steal phase**: Rewrote `pick_partial` steal phase to iterate `assigned_blocks`
  directly instead of calling `missing_chunks_into()` → `Vec::extend` → `Vec::retain`. Reduces
  steal scan from O(total_chunks) to O(assigned_blocks) per in-flight piece, eliminating all
  `chunk_info()` calls, Vec allocations, and HashMap probes from the steal path.
- **Steal early-out**: Skip the O(pieces × blocks) steal scan entirely when no peer has a rate
  below the steal threshold. O(peers) check via `peer_rates.values().any()`.
- Test count: 1409

## [0.73.0] — 2026-03-11

### Added
- **ChunkMask 256-bit bitfield**: Fixed-size `[u64; 4]` bitfield tracking unassigned chunks per
  `InFlightPiece`. Maintained incrementally (clear on assign, set on disconnect/freed). Replaces
  the re-compute-and-filter pattern with O(1) bit operations.

### Changed
- **Zero-alloc block selection**: `unassigned_blocks()` now reads `ChunkMask::iter_set_bits()`
  directly instead of calling `missing_chunks_into()` → `Vec::extend` → `Vec::retain`. Eliminates
  the top 3 CPU hotspots (24.27% `Vec::retain`, 8.40% `Vec::extend`, 7.24% `missing_chunks_into`).
- **Two-phase `pick_partial` scoring**: Phase 1 scores all in-flight pieces via `count_ones()`/
  `is_empty()` (zero allocation). Phase 2 enumerates blocks only for the winning piece.
- **Inlined hot-path cross-crate functions**: `#[inline]` on `Lengths::piece_size()`,
  `chunks_in_piece()`, `chunk_info()` and `Bitfield::get()` — enables cross-crate inlining
  without LTO for functions called ~300K times per batch fill cycle.
- CPU time reduced 23% (37.1s → 28.7s on Arch ISO benchmark)
- Test count: 1405

## [0.72.0] — 2026-03-10

### Changed
- **Pre-computed per-peer block queues**: Each peer gets a `VecDeque` of pre-assigned blocks filled
  during batch passes. `handle_piece_data()` dispatch is now O(1) — pop from queue instead of running
  the 5-layer picker on every block arrival (~91K invocations eliminated per 1.4 GiB download).
- **Shared-context batch fill**: Pipeline tick clones `we_have` bitfield ONCE for all peers instead of
  per-peer, with shared scratch buffer. Eliminates O(peers) redundant allocations per tick.
- **RequestBatch channel optimization**: Batch block requests into a single `PeerCommand::RequestBatch`
  channel message, reducing mpsc channel sends from N-per-peer to 1-per-peer per refill.
- **Reactive dispatch via `tokio::sync::Notify`**: Actor wakes immediately when any peer's queue drops
  below threshold, reducing worst-case dispatch latency from 250ms to ~1ms. Notify coalesces multiple
  signals for efficient batch refill.
- **Stale block cleanup**: Verified/failed pieces are swept from all peers' block_queues. End-game
  activation drains all queues to prevent orphaned assignments.
- **`max_in_flight_pieces` 20 → 40**: More concurrent pieces for better parallelism across peers.
  Presets: `min_memory()` = 16, `high_performance()` = 80.
- **Pipeline tick 250ms → 500ms**: Safety net only — Notify handles the fast path.
- Test count: 1392

## [0.71.0] — 2026-03-10

### Changed
- **Pipeline dispatch latency reduced**: Lowered `BATCH_DISPATCH_THRESHOLD` from 32 to 4 and pipeline tick
  interval from 1s to 250ms — worst-case dispatch latency drops from 1000ms to 250ms while picker CPU
  remains under 1%.
- **Fixed-depth pipeline model**: Replaced BDP-based adaptive queue depth with fixed depth at
  `initial_queue_depth` (128). Eliminates the negative feedback loop where low throughput → low depth →
  lower throughput. EWMA rate tracking retained for stats only.
- **Event channel throughput**: Increased event channel capacity 256 → 2048 and batch-drain limit
  64 → 256. Added `biased;` to `tokio::select!` with data-processing arms first, ensuring peer events
  take priority over timer maintenance.
- **PreferPlaintext encryption mode**: New default encryption mode offers plaintext-only MSE on outbound
  connections, falling back to plaintext + RC4 if the peer rejects. Reduces RC4 CPU overhead (31.6% of
  profile) for peers that accept plaintext. `Enabled` and `Forced` modes preserved for compatibility.
- **Scratch buffer API**: Refactored piece selector closure from `Fn(u32) -> Vec<(u32, u32)>` to
  `Fn(u32, &mut Vec<(u32, u32)>)`, threading a reusable scratch buffer through all 10 picker functions.
  Eliminates ~800K temporary `Vec` allocations per download.
- **Stack-array `peer_count()`**: Replaced `HashSet<&SocketAddr>` allocation with fixed-size `[SocketAddr; 8]`
  stack array for counting unique peers per in-flight piece. Falls back to `HashSet` only when >8 unique peers.
- **`max_in_flight_pieces` 32 → 20**: Reduced default from 32 to 20, cutting store buffer memory from 16 MiB
  to 10 MiB. Presets: `min_memory()` = 12, `high_performance()` = 64.
- Test count: 1392

## [0.70.0] — 2026-03-10

### Changed
- **BoringSSL crypto via `ring`**: Replaced `sha1`/`sha2` crates with `ring` (BoringSSL hand-optimized
  AVX2 assembly). ~4.6x SHA1 throughput improvement on Broadwell (174 → ~800 MB/s). All four hash
  functions (`sha1`, `sha1_chunks`, `sha256`, `sha256_chunks`) and MSE handshake hashing now use `ring::digest`.
- **`target-cpu=native` codegen**: Added `.cargo/config.toml` enabling AVX2/SHA-NI/BMI2 instruction
  generation for all crates (binaries are build-machine specific).
- **RC4 write buffer reuse**: `MseStream::poll_write` now reuses a persistent `Vec<u8>` instead of
  allocating per write — zero allocations after the first encrypted write.
- **Default peers reduced 200 → 128**: Default `max_peers_per_torrent` lowered from 200 to 128;
  `high_performance` preset from 500 to 250. Reduces per-piece verification overhead.
- Removed all direct `sha1`/`sha2` crate dependencies from workspace.
- Test count: 1395

## [0.69.0] — 2026-03-10

### Changed
- **Adaptive queue depth (BDP model)**: Per-peer request pipeline depth now computed dynamically
  using libtorrent's Bandwidth-Delay Product formula: `depth = (ewma_rate × request_queue_time / 16384)`
  clamped to `[16, max_queue_depth]`. Fast peers get deeper pipelines (up to 250 slots), slow peers
  get shallower ones (floor of 16). Replaces fixed 128-slot depth for all peers.
- Snub override flag (`depth_overridden`) prevents BDP formula from overwriting snub depth of 1;
  cleared on un-snub via `reset_to_slow_start()`
- `request_queue_time` parameter (previously ignored with `_` prefix) now actively used in BDP computation
- Test count: 1395

## [0.68.0] — 2026-03-10

### Changed
- **SHA hardware acceleration**: Enabled `asm` feature on `sha1`/`sha2` crates for runtime CPU-dispatched
  SHA-NI instructions — accelerates piece verification and MSE/PE encryption handshakes
- **Batch dispatch threshold**: Gated per-block picker invocation behind a 32-slot free threshold,
  reducing ~91k picker calls to ~2.9k per 1.4 GiB download (~97% reduction in hot-path CPU)
- Pipeline tick safety net widened to catch both fully idle peers and peers crossing the batch threshold

## 0.67.0 — M61 Performance Optimizations

### Performance
- **O(1) pending request lookup** — replaced `PeerState.pending_requests: Vec<(u32, u32, u32)>` with `PendingRequests` newtype wrapping `FxHashMap<(u32, u32), u32>`. Eliminates O(128) linear scan on every block received (~6000/sec). 4 call sites converted from `.iter().position()` + `.swap_remove()` to `.remove(index, begin)`.
- **End-game cascade eliminated** — removed reactive re-requesting loop that triggered 50 picker invocations per block in end-game mode. Replaced with a 200ms periodic batch refill tick (`end_game_tick_interval`) with `MissedTickBehavior::Skip` to prevent burst-fire on activation. All peers with available pipeline slots refilled in a single pass.
- **END_GAME_DEPTH raised 4 → 128** — matches normal-mode pipeline depth. With depth 4, each peer only had 64KB queued (6.4ms of work at 10 MB/s), idling 97% of the time between refill ticks. With depth 128, each peer has 2MB queued (200ms of work), fully utilising the 200ms tick interval. Eliminates `final_speed=0.0 MB/s` tail stall observed in all v0.66.0 benchmark trials.
- **In-flight piece cap** — new `max_in_flight_pieces` setting (default 32) bounds store buffer memory at the piece selector level. When the cap is reached, Layers 3 (suggested) and 5 (new piece) are skipped — only partial/steal paths remain active. Memory bound: 32 × 512KB = 16 MiB for typical torrents (was unbounded).

### Added
- `benchmarks/build_report.py` — generates versioned markdown benchmark reports from CSV data
- `benchmarks/generate_report.sh` — orchestrates benchmark summarization and report generation
- `--json` output mode for `benchmarks/summarize.py`
- `docs/benchmark-report-v0.66.0.md` — baseline benchmark report
- `PendingRequests` newtype with full API: `new()`, `insert()`, `remove()`, `contains()`, `len()`, `is_empty()`, `clear()`, `iter()`
- `max_in_flight_pieces` setting in `Settings` and `TorrentConfig` (default: 32)
- `cap_reached` field in `PickContext` for piece selector backpressure
- Test count: 1390

## 0.66.0 — Performance Consistency & Tuning

### Fixed
- **Store buffer race causing hash verification failures** — `store_buffer.remove()` was called inside a spawned blocking task, creating a race window where `on_piece_hash_failed()` could trigger re-download and write new blocks before the verify task consumed the old ones, producing "frankenstein pieces" that mixed data from different download attempts (thousands of hash failures per download). Fix: extract blocks synchronously before spawning. Also added `sha1_chunks()`/`sha256_chunks()` for incremental hashing, eliminating ~512KB concatenation allocation per piece verification. Before: 235-300s (4-5 MB/s); After: 35-49s (35-54 MB/s)
- **Store buffer lock scope** — made Mutex lock scope explicit in verify path with block scope, preventing potential contention between enqueue_write and spawn_blocking verify tasks
- **Pipeline fill loop peer validation** — peer existence check and bitfield refresh each iteration of the pick_blocks loop, preventing stale context from causing wasted block picks
- **Disconnect handler re-request guard** — defensive peer existence check before re-requesting blocks from remaining peers after a disconnect
- **Event loop stalls from blocking sends** — all peer channel sends in TorrentActor converted from `.send().await` to `try_send()`, preventing the actor event loop from freezing when any peer channel is full
- **Duplicate block handling** — `handle_piece_data` now returns early when a block was already received, preventing wasted disk writes and double-counting
- **DHT stalled lookups** — `get_peers` queries now advance on timeout instead of stalling; query expiry logging added
- **DHT BEP42 node sorting** — routing table now sorts by XOR distance for correct closest-node selection; all-K parallel `get_peers` queries for faster peer discovery
- **DHT timeout** — reduced from 10s to 5s for faster peer discovery
- **Shutdown hang** — `shutdown_peers()` converted peer shutdown from blocking `.send().await` to `try_send()`; `announce_stopped()` wrapped in 3s timeout; `TorrentHandle::shutdown()` and `SessionHandle::shutdown()` wrapped in 5s/10s timeouts

### Performance
- **Actor CPU overhead reduced** — six targeted optimizations to `request_pieces_from_peer()` and the event loop: skip picker call for duplicate blocks, cache peer_rates on actor (refresh on 1s tick), clone peer bitfield once before picker loop, use `Lengths::chunks_in_piece()` arithmetic instead of iterating chunk tracker, batch-drain up to 64 peer events per `select!` iteration, share single `Instant::now()` call. CPU usage reduced from 3.3x to 1.83x vs rqbit. Before: ~26s user CPU, ~47 MB/s peak; After: ~12.6s user CPU, ~78.5 MB/s peak; rqbit: ~6.9s user CPU, ~105 MB/s peak

### Changed
- **Parallel tracker announces** — all tracker announces now fire concurrently via `tokio::task::JoinSet` instead of sequentially. Time-to-first-piece reduced from `sum(tracker_timeouts)` to `max(tracker_timeouts)`
- **Fixed permit pipeline model** — replaced EWMA-computed queue depth with fixed 128 permits (rqbit model). `queue_depth()` is now constant — zero-latency scheduling with no negative feedback loops
- **End-game pipelining** — end-game mode now sends up to 4 requests per peer (was 1), eliminating the 128x pipeline depth reduction that caused last-3% slowdowns
- **Reactive end-game cancel re-requesting** — when a block is received and duplicate requests are cancelled, freed peers immediately get new end-game blocks instead of waiting for the next tick
- **Snub timeout reduced 60s to 15s** — stalled peers now free their 128 request slots 4x faster; rqbit handles this in ~10s
- **Peer turnover increased from 4%/300s to 8%/120s** — underperforming peers cycled 4x faster (~120 replacements/hour)
- **Zombie peer pruning** — peers with empty bitfields after 30s are disconnected during choking evaluation to free connection slots (only during downloading, not seeding)
- Test count: 1386

### Benchmark (Arch ISO 1.5 GiB, 5 trials)
| Client | Time (s) | CPU (s) | Speed (MB/s) | RSS (MiB) |
|--------|----------|---------|-------------|-----------|
| torrent 0.66.0 | 100.3 ±19.3 | 37.1 ±4.7 | 15.0 ±3.5 | 170.2 ±42.9 |
| rqbit 8.1.1 | 34.4 ±24.1 | 10.4 ±0.6 | 56.7 ±27.0 | 37.5 ±3.9 |
| qbittorrent 5.1.4 | 51.5 ±10.7 | 18.1 ±2.5 | 29.1 ±5.3 | 1525.5 ±5.5 |

**Identified bottlenecks for M68:** end-game pipeline depth too shallow (4 vs 128), O(n) linear scans in pending_requests, redundant bitfield clones in hot path, reactive re-requesting triggers 2+ full picker cycles per block, peer_count() does O(n log n) sort per invocation, unbounded store buffer memory

## 0.65.0 — `torrent` v0.65.0: Rename, Non-Blocking Pipeline, Production Hardening

### Breaking
- **Workspace renamed from `ferrite` to `torrent`** — all 12 crates renamed: `ferrite-*` → `torrent-*`. Update your `Cargo.toml` dependencies and `use` imports accordingly.
- Version bumped 0.64.0 → 0.65.0

### Added
- crates.io metadata: keywords, categories, homepage, documentation URLs
- `rust-version = "1.85"` MSRV set for all publishable crates
- Non-blocking disk writes and async piece verification (M58 pipeline re-applied)
- O(1) peer dedup via `HashSet<SocketAddr>` in `handle_add_peers()`
- Criterion benchmarks for bencode parsing and piece hashing
- `rustfmt.toml` with workspace-wide formatting rules
- Workspace-level clippy lint configuration
- Resource exhaustion limits: `max_metadata_size`, `max_message_size`, `max_piece_length`, `max_outstanding_requests`
- `cargo-fuzz` scaffolding with 4 fuzz targets (bencode, peer_wire, tracker, metadata)
- Architecture documentation (`docs/architecture.md`, `docs/configuration.md`, `docs/security.md`)

### Changed
- CLI binary renamed from `ferrite` to `torrent`
- `torrent-cli` marked `publish = false`
- Test count: 1378+

## 0.64.0 — TCP Listener, UPnP Gateway Probe & Log Cleanup

### Fixed
- **TCP listener for incoming peer connections** — `SessionActor` only had uTP and SSL listeners, never binding a plain TCP accept loop on the main listen port. Incoming TCP connections were refused, preventing peers from connecting to us. Added TCP listener bind, generalized `identify_plaintext_connection` to any `AsyncRead+AsyncWrite` stream, and unified routing via `route_inbound_stream<S>`
- **UPnP discovery with gateway probe fallback** — many consumer routers have broken SSDP (don't respond to multicast M-SEARCH) but serve valid UPnP descriptions over HTTP. Added fallback that probes the default gateway on common UPnP ports (49152, 5000, 5431, 2869, 49153) and description paths (`/rootDesc.xml`, `/gatedesc.xml`, etc.)
- **UPnP IGD v2 support** — SSDP now sends both v1 and v2 M-SEARCH, and service type search includes `WANIPConnection:2` alongside v1 and `WANPPPConnection:1` (PPPoE)
- **SSDP response filtering** — discovery loop now skips non-IGD responders (e.g. Plex DLNA servers) and accepts only `InternetGatewayDevice` matches
- **Log spam reduced** — demoted routine peer churn events (uTP max timeouts, wire errors, send errors) from `warn!` to `debug!`

### Changed
- `identify_plaintext_connection` generalized from `UtpStream`-specific to generic `S: AsyncRead + AsyncWrite + Unpin + Send`
- `TorrentCommand::IncomingPeer` now carries `BoxedStream` instead of `PrefixedStream<UtpStream>`
- `BoxedStream` now implements `Debug`
- Test count: 1378

### Benchmark (Arch ISO 1.5 GiB, with UPnP port mapping)
| Metric | Value |
|--------|-------|
| Peak 5s speed | 56.0 MB/s |
| Average speed | 29.4 MB/s |
| Peak unchoked | 156 peers |
| Total peers | 200 |

## 0.63.1 — Settings Wiring & Throughput Fixes

### Fixed
- **13 TorrentConfig fields now wired from Settings** — `upload_rate_limit`, `download_rate_limit`, `strict_end_game`, `max_web_seeds`, `initial_picker_threshold`, `whole_pieces_threshold`, `snub_timeout_secs`, `readahead_pieces`, and `streaming_timeout_escalation` were hardcoded in `From<&Settings>`, silently ignoring user configuration
- **4 new TorrentConfig fields** — `storage_mode`, `block_request_timeout_secs`, `enable_lsd`, `force_proxy` now flow from Settings to per-torrent config
- **Storage mode wired to disk** — `FilesystemStorage::new()` preallocate parameter now respects `StorageMode::Full` instead of always being `false`
- **MSE PadB handling** — VC scan was broken for all real peers (desynchronized RC4 cipher on unencrypted padding)
- **uTP ext type 3** — libtorrent close_reason extension caused parse failures

### Performance
- **TorrentActor unblocked** — all peer command broadcasts converted from `.send().await` (blocking) to `try_send()` (non-blocking), preventing deadlocks on bounded channels
- **Phantom block assignments eliminated** — `assigned_blocks.insert()` moved after `try_send().is_ok()`, preventing permanent piece stalls when send fails
- **Snub detection** — peers marked as snubbed after 60s of no data while unchoked, freeing their assigned blocks
- **Pipeline tick** — `PeerPipelineState::tick()` now called every 1s for EWMA throughput updates

### Added
- 7 new Settings fields: `strict_end_game`, `max_web_seeds`, `initial_picker_threshold`, `whole_pieces_threshold`, `snub_timeout_secs`, `readahead_pieces`, `streaming_timeout_escalation`
- Regression test with non-default Settings values to prevent re-hardcoding

### Changed
- Test count: 1376 (was under-reported as 1153 due to `--fail-fast` hiding 4 crates behind pre-existing sim test failure)

## 0.63.0 — M56: Speed Optimization

### Added
- **DHT routing table persistence** — saves DHT routing table nodes on shutdown and reloads on startup for instant peer discovery (eliminates cold-start penalty)
- **Piece stealing** — faster peers steal in-flight blocks from peers running at less than 1/10th their download rate (configurable `steal_threshold_ratio`, default 10.0)
- **`initial_queue_depth`** setting — configurable per-peer initial request pipeline depth (default: 128, embedded: 16, high_performance: 256)
- **`steal_threshold_ratio`** setting — configurable steal aggressiveness (default: 10.0, high_performance: 5.0)
- **`dht_saved_nodes`** runtime field — builder API for injecting saved DHT nodes before session start

### Changed
- **Initial pipeline queue depth** raised from 2 to 128 per peer — matches rqbit's semaphore-permit model for faster ramp-up
- **PeerState::new()** now accepts configurable pipeline parameters instead of hardcoded values
- **Session state** now populates `dht_nodes` field (previously always empty `Vec::new()`)

### CLI
- **DHT state persistence** — `ferrite download` saves/loads DHT routing table to `$XDG_DATA_HOME/ferrite/session.dat` across runs

### Benchmark (Arch ISO 1.42 GiB, 3 trials)
| Client | Time (s) | Speed (MB/s) | RSS (MiB) |
|--------|----------|-------------|-----------|
| ferrite | 96.8 ±1.1 | 15.0 ±0.2 | 29.3 ±0.9 |
| rqbit | 17.1 ±0.6 | 84.9 ±2.8 | 42.3 ±1.7 |
| libtorrent | 32.7 ±0.7 | 44.4 ±0.9 | 1487.8 ±0.1 |

**Note:** No speed improvement observed — the primary bottleneck is in the transfer pipeline itself (not peer discovery, queue depth, or block assignment). DHT persistence saves 40 nodes per session. Further investigation needed for M57.

## 0.62.0 — M55: Download Speed Optimization

### Changed
- **Immediate peer connection** — DHT, PEX, tracker, and manually-added peers now trigger `try_connect_peers()` immediately instead of waiting up to 30s for the periodic connect timer
- **Raised default max_peers** from 50 to 200 per torrent
- **Reduced connect_interval** from 30s to 5s for faster catch-up on edge cases
- **`max_peers_per_torrent`** now configurable in Settings (default: 200, embedded: 30, high_performance: 500)

### Fixed
- **CLI: FilesystemStorage** — `ferrite download` was passing `None` for storage, causing the session to use MemoryStorage (in-RAM only). Now creates proper FilesystemStorage from torrent metadata.

### Benchmark (Arch ISO 1.42 GiB, 3 trials)
| Client | Time (s) | Speed (MB/s) | RSS (MiB) |
|--------|----------|-------------|-----------|
| ferrite | 87.7 ±3.6 | 16.6 ±0.7 | 27.6 ±0.8 |
| rqbit | 17.8 ±0.9 | 81.5 ±3.9 | 40.6 ±0.9 |
| libtorrent | 37.1 ±29.9 | 58.2 ±37.9 | 1487.1 ±0.3 |

**Note:** Remaining speed gap is due to DHT cold-start (no routing table persistence). See M56 roadmap.

## 0.61.0 — M54: CLI Binary & Benchmarking

### Added
- **`ferrite-cli` crate** — new workspace member providing the `ferrite` binary
- **`ferrite download`** — download torrents from magnet links or .torrent files with indicatif progress bar, Ctrl-C graceful shutdown, `--config` JSON settings, `--seed`, `--quiet`, `--no-dht`, `--port` options
- **`ferrite create`** — create .torrent files from files or directories with tracker, private, and piece size options, progress display during hashing
- **`ferrite info`** — display torrent metadata with v1/v2/hybrid support, dual info hash display, file listing, tracker listing
- **Benchmark harness** — `benchmarks/run_benchmark.sh` for comparing ferrite vs rqbit vs libtorrent (wall time, speed, RSS), with `lt_download.py` libtorrent runner and `summarize.py` results aggregation
- 7 CLI integration tests (arg parsing, error handling, create+info roundtrip)

### Dependencies
- `clap` 4.x (derive), `indicatif` 0.17, `tracing-subscriber` 0.3, `anyhow` 1, `serde_json` 1

## 0.60.0 — M53: Full Torrent Operations API Parity

### Added
- **Session wrappers**: `force_reannounce`, `tracker_list`, `scrape`, `set_file_priority`, `file_priorities` — SessionHandle wrappers for existing TorrentHandle methods
- **Per-torrent rate limits**: `set_download_limit`, `set_upload_limit`, `download_limit`, `upload_limit` with 0=unlimited convention
- **Runtime toggles**: `set_sequential_download`, `is_sequential_download`, `set_super_seeding`, `is_super_seeding`
- **Tracker management**: `add_tracker`, `replace_trackers` with `TrackerManager::replace_all()`
- **Force recheck**: `force_recheck()` with `ChunkTracker::clear()`, state transitions through Checking, peer disconnection
- **File operations**: `rename_file()` with disk rename + FileRenamed alert
- **Connection limits**: `set_max_connections`, `max_connections`, `set_max_uploads`, `max_uploads` with enforcement across 5 peer acceptance paths
- **Peer introspection**: `get_peer_info()` → `Vec<PeerInfo>` (16 fields), `get_download_queue()` → `Vec<PartialPieceInfo>`
- **Piece queries**: `have_piece`, `piece_availability`, `file_progress` with byte-accurate file-piece overlap
- **Identity queries**: `info_hashes`, `torrent_file`, `torrent_file_v2` (None for magnets before metadata)
- **Force announce**: `force_dht_announce` (dual-swarm v4/v6), `force_lsd_announce` (session-level)
- **Disk operations**: `read_piece` → `Bytes`, `flush_cache` with `DiskJob::FlushAll`
- **Niche operations**: `is_valid` (synchronous), `clear_error` (auto-resume), `file_status`, `connect_peer`
- **TorrentFlags**: bitflags convenience API (PAUSED, AUTO_MANAGED, SEQUENTIAL_DOWNLOAD, SUPER_SEEDING, UPLOAD_ONLY)
- **Alerts**: `FileCompleted`, `MetadataFailed`, `ExternalIpDetected`
- **Types**: `PeerInfo`, `PartialPieceInfo`, `FileMode`, `FileStatus`, `TorrentFlags`
- `TokenBucket::rate()` getter

### Fixed
- `verify_existing_pieces` now calls `choker.set_seed_mode(false)` on the Downloading path (latent bug activated by force_recheck)

## 0.59.0 — TorrentStats Full Parity

### Added
- Full libtorrent `torrent_status` parity: ~55-field `TorrentStats` with identity, state flags, progress, transfer counters, rates, connection details, limits, distributed copies, tracker info, timestamps, durations, error reporting
- `Default` implementation for `TorrentStats` with correct sentinel values
- `compute_progress()` helper for byte-level progress calculation
- `distributed_copies()` helper for piece availability analysis
- Event-driven tracking in 7 handler sites: download, upload, hash failure, state transitions, incoming peers, peer bitfields, storage moves
- Duration accumulation for `active_duration`, `finished_duration`, `seeding_duration` with live computation
- Session-level enrichment of `queue_position` and `auto_managed`
- `unchoke_slots()` getter on `Choker`

### Changed
- `make_stats()` rewritten to populate all fields in single peer pass
- `transition_state()` rewritten with full duration accumulation state machine
- `handle_move_storage()` refactored for `moving_storage` flag safety on all error paths
- Updated download example to show progress percentage and rates
- Version bump 0.58.0 → 0.59.0

## 0.58.0 — M52: API Documentation & Examples

### Added
- `#![warn(missing_docs)]` lint enabled on all 12 workspace crates
- Documentation for ~500+ public items across all crates
- `SessionHandle::open_file()` method for file streaming at the session level
- Four example programs: `download.rs`, `create.rs`, `stream.rs`, `dht_lookup.rs`
- "Getting Started" section in README with usage example

### Changed
- Version bump 0.57.0 → 0.58.0

## 0.57.0 — 2026-03-01

Network simulation framework. Twelve crates, 1310 tests, 27 BEPs implemented. Phase 12 (Simulation) complete. **All 51 milestones done — full libtorrent-rasterbar parity achieved.**

### M51: Network Simulation Framework

### Added
- `ferrite-sim` crate — in-process network simulation for swarm integration testing
- `BoxedStream` — type-erased bidirectional async stream wrapper (ferrite-session)
- `TransportListener` trait — object-safe TCP listener abstraction (ferrite-session)
- `NetworkFactory` — pluggable transport factory with `bind_tcp`/`connect_tcp` closures and `is_simulated` flag
- `NetworkFactory::tokio()` — production factory using real tokio TCP sockets
- `SessionHandle::start_with_transport()` — session constructor with custom transport factory
- `SessionHandle::start_full()` — canonical constructor taking all parameters (settings, plugins, backend, factory)
- `SessionHandle::add_peers()` — peer injection by info hash through session command channel
- `SimClock` — virtual clock with `advance()` for deterministic timing
- `SimNetwork` — virtual network with node IP allocation (`10.0.0.x`), TCP routing via `tokio::io::duplex` channels, per-link config, network partitioning
- `LinkConfig` — per-link latency, bandwidth, and packet loss configuration
- `sim_transport_factory()` — creates `NetworkFactory` routing through `SimNetwork`
- `SimSwarmBuilder` / `SimSwarm` — fluent builder for multi-node swarm test scenarios
- `make_test_torrent()` / `make_seeded_storage()` — test helpers for creating torrents and pre-populated storage
- 5 swarm integration tests: peer discovery, partition blocking, 3-node swarm, node count, seeded storage roundtrip
- `PeerSource` re-exported from ferrite-session root

### Changed
- All `TcpListener::bind()` / `TcpStream::connect()` calls in session and torrent actors replaced with `NetworkFactory` methods
- SSL listener changed from `Option<TcpListener>` to `Option<Box<dyn TransportListener>>`
- `from_torrent()` / `from_magnet()` gain `factory: Arc<NetworkFactory>` parameter
- DSCP marking skipped on transport-abstracted paths (BoxedStream lacks `AsRawFd`)
- 31 new tests (1310 total)

## 0.56.0 — 2026-03-01

Session statistics counters. Eleven crates, 1279 tests, 27 BEPs implemented. Phase 11 (Pluggable Interfaces) complete.

### M50: Session Statistics Counters

### Added
- `SessionCounters` — 70 atomic counters (`[AtomicI64; 70]`) covering network, disk, DHT, peers, protocol, bandwidth, and session metrics
- `MetricKind` enum (Counter/Gauge) and `SessionStatsMetric` metadata struct for metric introspection
- `session_stats_metrics()` — static metadata for all 70 counters (name, kind, description)
- `SessionStatsAlert` — new alert variant carrying full 70-metric snapshot via `AlertCategory::STATS`
- `Settings.stats_report_interval` — configurable periodic stats reporting (default 1000ms, 0=disabled)
- Periodic stats timer in `SessionActor::run()` loop with `update_session_gauges()` and `fire_stats_alert()`
- `SessionHandle::post_session_stats()` — on-demand stats snapshot request
- `SessionHandle::counters()` — direct access to `Arc<SessionCounters>` for zero-copy reads
- `ClientBuilder::stats_report_interval()` — facade builder method
- Facade re-exports: `MetricKind`, `SessionStatsMetric`, `SessionCounters`, `session_stats_metrics`, `NUM_METRICS`
- Prelude re-export: `SessionCounters`
- 25 new tests (1279 total)

### Changed
- Version bump 0.55.0 → 0.56.0

## 0.55.0 — 2026-03-01

Pluggable disk I/O interface. Eleven crates, 1254 tests, 27 BEPs implemented. Phase 11 (Pluggable Interfaces) begins.

### M49: Pluggable Disk I/O Interface

### Added
- `DiskIoBackend` trait — 14-method pluggable disk I/O interface enabling custom storage backends
- `PosixDiskIo` — default backend with ARC read cache + WriteBuffer (extracted from DiskActor)
- `MmapDiskIo` — simplified backend using kernel page cache via mmap
- `DisabledDiskIo` — null/no-op backend for network throughput benchmarking
- `create_backend_from_config()` — factory function selecting backend from DiskConfig
- `DiskIoStats` — aggregate I/O statistics for backends
- `SessionHandle::start_with_backend()` — session entry point with custom disk I/O backend
- `SessionHandle::start_with_plugins_and_backend()` — session entry point with both plugins and custom backend
- Facade: `ClientBuilder::disk_io_backend()` builder method for custom backends
- 26 new tests (1254 total)

### Changed
- `DiskActor` refactored to delegate through `Arc<dyn DiskIoBackend>` instead of owning storage/cache/buffer directly
- Version bump 0.54.0 → 0.55.0

## 0.54.0 — 2026-03-01

DSCP socket marking and anonymous mode hardening. Eleven crates, 1228 tests, 27 BEPs implemented. Phase 10 (Security & Hardening) complete.

### M48: DSCP Marking + Anonymous Mode Hardening

### Added
- `dscp` module in ferrite-session: applies DSCP/ToS markings via `setsockopt` to TCP listeners, outbound TCP connections, uTP sockets, and UDP tracker sockets
- Default DSCP value CS1 (0x08) — scavenger/low-priority traffic class
- `Settings.peer_dscp` field for configurable DSCP marking
- `PeerId::generate_anonymous()` with `-XX0000-` prefix (generic/unknown client fingerprint)
- Ext handshake suppression in anonymous mode: v, p, reqq, upload_only fields all set to None
- Tracker announce port suppression (sent as 0) in anonymous mode
- HTTP tracker user-agent suppression (empty string) via `HttpTracker::with_anonymous()`
- Facade: `ClientBuilder::peer_dscp()` builder method
- 21 new tests (1228 total)

### Changed
- Version bump 0.53.0 → 0.54.0

## 0.53.0 — 2026-03-01

SSRF mitigation, IDNA rejection, and HTTPS tracker validation. Eleven crates, 1207 tests, 27 BEPs implemented. Phase 10 (Security & Hardening) begins.

### M47: SSRF Mitigation + URL Security

### Added
- `url_guard` module in ferrite-session: centralized URL security validation for tracker and web seed URLs
- `UrlSecurityConfig` struct with 3 fields: `ssrf_mitigation`, `allow_idna`, `validate_https_trackers`
- `UrlGuardError` enum with 5 diagnostic variants: `InvalidUrl`, `LocalhostBadPath`, `LocalNetworkQueryString`, `RedirectToPrivateIp`, `IdnaDomain`
- URL validators: `validate_tracker_url()`, `validate_web_seed_url()`, `validate_redirect()`
- HTTP client builder: `build_redirect_policy()` (SSRF-safe custom reqwest redirect policy), `build_http_client()`
- `HttpTracker::with_security()` constructor with self-contained SSRF redirect policy in ferrite-tracker
- `SecurityViolation(String)` error variant in ferrite-tracker
- `TrackerManager::from_torrent_filtered()` — URL-validating version of `from_torrent()`
- `TrackerManager::add_tracker_url_validated()` — validates before adding tracker URLs
- `TrackerManager::set_metadata_filtered()` — filtered version for BEP 9 magnet metadata flow
- 3 new Settings fields: `ssrf_mitigation` (default true), `allow_idna` (default false), `validate_https_trackers` (default true)
- `UrlSecurityConfig` field on `TorrentConfig` with `From<&Settings>` conversion
- Facade: 3 `ClientBuilder` methods: `ssrf_mitigation()`, `allow_idna()`, `validate_https_trackers()`
- 51 new tests (1207 total)

### Changed
- `TorrentActor` now validates all tracker/web seed URLs through `url_guard` at parse time and redirect time
- `WebSeedTask::new()` accepts `UrlSecurityConfig` and uses `build_http_client()` for SSRF-safe HTTP clients
- BEP 9 magnet metadata path uses `set_metadata_filtered()` to validate URLs from peers
- Version bump 0.52.0 → 0.53.0

## 0.52.0 — 2026-03-01

Peer turnover algorithm for periodic replacement of underperforming peers. Eleven crates, 1156 tests, 27 BEPs implemented. Phase 9 (Swarm Intelligence) complete.

### M46: Peer Turnover

### Added
- `connected_at: Instant` field on PeerState for connection age tracking
- `run_peer_turnover()` method on TorrentActor: disconnects bottom fraction of peers ranked by download rate
- Peak download rate tracking in `update_peer_rates()` for cutoff threshold
- Turnover timer in TorrentActor run() select loop (configurable interval, default 300s)
- 4 peer exemptions: seeds, outstanding requests, parole peers, recently connected (<30s)
- Cutoff suppression: skips turnover when aggregate download rate >= cutoff * peak rate
- `PeerTurnover` alert variant (PEER category) with disconnected/replaced counts
- 3 new settings: `peer_turnover` (0.04), `peer_turnover_cutoff` (0.9), `peer_turnover_interval` (300)
- Settings validation for turnover fraction and cutoff (0.0-1.0 range)
- Facade: 3 `ClientBuilder` methods
- 11 new tests (1156 total)

### Changed
- Disconnect cleanup in peer turnover matches full PeerEvent::Disconnected handler (super-seed, suggested_to_peers, in-flight pieces)

## 0.51.0 — 2026-03-01

Mixed-mode TCP/uTP bandwidth algorithm and auto-sequential hysteresis. Eleven crates, 1145 tests, 27 BEPs implemented.

### M45: Mixed-Mode TCP/uTP + Auto-Sequential

### Added
- `MixedModeAlgorithm` enum: `PreferTcp` (90/10 split) and `PeerProportional` (bandwidth proportional to peer count)
- `RateLimiterSet::apply_mixed_mode()` dynamically adjusts per-class upload rates based on transport composition
- Peer transport tracking: `PeerState.transport` field, `PeerEvent::TransportIdentified` event
- Transport identification for outgoing connections (uTP/TCP) and incoming peers
- `transport_peer_counts()` helper on TorrentActor for TCP vs uTP peer census
- Mixed-mode tick in refill interval: adjusts per-class upload rates every 100ms
- Auto-sequential hysteresis: `evaluate_auto_sequential()` with dual thresholds (activate at 1.6x, deactivate at 1.3x) replacing ad-hoc inline check
- `auto_sequential_active` state on TorrentActor, evaluated every unchoke interval (10s)
- 2 new settings: `mixed_mode_algorithm`, `auto_sequential`
- Facade: `MixedModeAlgorithm` in prelude, 2 `ClientBuilder` methods
- 8 new tests (1145 total)

### Changed
- Auto-sequential evaluation moved from inline per-pick to TorrentActor unchoke tick with hysteresis state
- `PickContext` extended with `auto_sequential_active` field

## 0.50.0 — 2026-03-01

Piece picker enhancements — extent affinity for disk cache locality, BEP 6 SuggestPiece mode, predictive piece announce, and configurable peer speed classification. Eleven crates, 1137 tests, 27 BEPs implemented.

### M44: Piece Picker Enhancements

### Added
- Piece extent affinity: groups pieces into 4 MiB extents, preferring in-flight extents for improved disk cache locality
- `PeerSpeedClassifier` with configurable slow/fast thresholds (default 10 KB/s and 100 KB/s)
- SuggestPiece mode: sends BEP 6 SuggestPiece messages for pieces in the ARC disk read cache
- `suggest_cached_pieces()` method with 30-second periodic timer and on-verification suggests
- `PeerCommand::SuggestPiece(u32)` variant mapped to `Message::SuggestPiece`
- `DiskJob::CachedPieces` + `DiskHandle::cached_pieces()` for querying disk cache contents
- `ArcCache::cached_keys()` iterator for cache key enumeration
- Predictive piece announce: sends Have messages before hash verification completes
- `predictive_have_sent` tracking with duplicate suppression in `on_piece_verified()`
- Hash failure cleanup: removes predictive Have state on verification failure
- 4 new settings: `piece_extent_affinity`, `suggest_mode`, `max_suggest_pieces`, `predictive_piece_announce_ms`
- `Settings::high_performance()` now enables suggest mode
- Facade: 4 `ClientBuilder` methods
- 13 new tests (1137 total)

### Changed
- `PieceSelector::pick_rarest_new()` refactored into `pick_rarest_any()` + `pick_rarest_in_extent()` for extent-filtered picking
- `PickContext` extended with `extent_affinity` field

## 0.49.0 — 2026-03-01

Pluggable choking algorithms — refactored monolithic choker into Strategy pattern with four implementations. Eleven crates, 1124 tests, 27 BEPs implemented.

### M43: Seed Choking Algorithms + Rate-Based Choker

### Added
- `ChokerStrategy` trait for pluggable choking algorithm implementations
- `FixedSlotsStrategy` — extracted existing tit-for-tat choker into trait-based implementation
- `RateBasedStrategy` — dynamically adjusts unchoke slot count based on upload throughput utilization
- `SeedChokingAlgorithm` enum: `FastestUpload` (default), `RoundRobin`, `AntiLeech` — controls peer ranking in seed mode
- `ChokingAlgorithm` enum: `FixedSlots` (default), `RateBased` — selects unchoke slot strategy
- AntiLeech seed-mode: prioritizes non-seed peers (actual leechers) over seeds (hit-and-run)
- RoundRobin seed-mode: rotates unchoke slots periodically for fairness
- `is_seed` field on `PeerInfo` for bitfield-completeness + upload-only detection
- `seed_choking_algorithm` and `choking_algorithm` settings fields with JSON serialization
- TorrentActor throughput observation for rate-based dynamic slot adjustment
- Facade: `SeedChokingAlgorithm`, `ChokingAlgorithm` re-exports, 2 `ClientBuilder` methods
- 20 new tests (1124 total)

### Changed
- `Choker` refactored from monolithic struct to thin dispatcher over `Box<dyn ChokerStrategy>`
- `Choker::new(4)` remains backward-compatible (creates FixedSlots + FastestUpload internally)
- `Choker::with_algorithms()` constructor for configurable strategy selection

## 0.48.0 — 2026-03-01

BEP 35 SSL torrents — TLS-encrypted peer connections with per-torrent CA trust, SNI-based routing, and self-signed certificate generation. Eleven crates, 1104 tests, 27 BEPs implemented.

### M42: SSL Torrents (BEP 35)

### Added
- SSL/TLS transport layer using rustls 0.23 with ring crypto provider
- `ssl_cert` field on `InfoDict`, `InfoDictV2`, `TorrentMetaV1`, `TorrentMetaV2` for PEM-encoded CA certificates
- `TorrentMeta::ssl_cert()` and `is_ssl()` accessor methods
- `SslConfig` struct for per-torrent TLS configuration (client + server configs)
- `connect_tls()` and `accept_tls()` for TLS stream wrapping with SNI-based info hash routing
- `generate_self_signed_cert()` using rcgen for peer certificate generation
- Custom `SslTorrentServerVerifier` that validates peer certs against torrent-embedded CA
- `SslManager` for certificate lifecycle management — generation, loading, and per-torrent config caching
- SSL listener in `SessionActor` using `LazyConfigAcceptor` for SNI-based torrent routing
- `SpawnSslPeer` torrent command for session-to-torrent SSL peer handoff
- SSL wrapping in `try_connect_peers()` TCP path for outgoing connections
- `ssl_listen_port`, `ssl_cert_path`, `ssl_key_path` settings fields
- `SslTorrentError` alert variant
- `set_ssl_cert()` on `CreateTorrent` builder for SSL torrent creation
- `BoxedAsyncStream` type alias for unified plain/TLS stream handling
- Facade: 3 `ClientBuilder` methods (`ssl_listen_port`, `ssl_cert_path`, `ssl_key_path`)
- SSL re-exports in `ferrite::wire::ssl`
- 26 new tests (1104 total)

### Changed
- `ferrite-wire` now depends on rustls 0.23, tokio-rustls 0.26, rustls-pemfile 2, rcgen 0.13
- Explicit `builder_with_provider()` pattern for rustls workspace compatibility (avoids CryptoProvider auto-detection issues)
- v2 metadata parsing extracts `ssl-cert` from BencodeValue dict
- `v2_to_v1_compat()` propagates ssl_cert when synthesizing v1 from v2

## 0.47.0 — 2026-03-01

I2P anonymous network support via SAM v3.1 protocol bridge — tunneled peer connections over the I2P network. Eleven crates, 1078 tests, 26 BEPs implemented.

### M41: I2P Support (SAM v3.1)

### Added
- I2P anonymous network support via SAM v3.1 protocol bridge
- `I2pDestination` type with I2P-specific Base64 encoding and `.b32.i2p` address generation
- SAM v3.1 client: `SamSession`, `SamStream`, `SamReply` with full protocol lifecycle
- SAM protocol commands: HELLO, SESSION CREATE, STREAM CONNECT, STREAM ACCEPT, NAMING LOOKUP
- Control socket kept alive for session lifetime
- 8 I2P settings: `enable_i2p`, `i2p_hostname`, `i2p_port`, tunnel lengths/quantities, `allow_i2p_mixed`
- I2P `AlertCategory` flag (0x800) and `I2pSessionCreated`/`I2pError` alert variants
- `PeerSource::I2p` variant for I2P peer identification
- Session-level I2P initialization with automatic SAM session management
- Torrent-level I2P peer accept loop with synthetic address mapping
- `SamTunnelConfig` for tunnel parameter configuration
- Facade: `I2pDestination`/`I2pDestinationError` re-exports, 8 `ClientBuilder` methods
- 41 new tests (1078 total)

## 0.46.0 — 2026-03-01

BEP 55 holepunch extension — NAT traversal between NATed peers via relay-mediated simultaneous connect. Eleven crates, 1037 tests, 26 BEPs implemented.

### M40: BEP 55 Holepunch Extension

### Added
- BEP 55: holepunch extension for NAT traversal between NATed peers
- Wire-level message types: `HolepunchMessage`, `HolepunchMsgType`, `HolepunchError`
- `ut_holepunch` as built-in BEP 10 extension ID 4
- TorrentActor relay logic: forwards Connect messages to both parties (Rendezvous protocol)
- Simultaneous connect handler with uTP-first and TCP fallback
- NAT detection heuristic for peer classification
- Settings: `enable_holepunch` (default true)
- Alerts: `HolepunchSucceeded`, `HolepunchFailed`
- `PeerState` tracking: `supports_holepunch`, `appears_nated`
- `PeerEvent`/`PeerCommand` variants for holepunch messages
- `try_holepunch()` utility for initiating holepunch via relay
- Facade: `HolepunchMessage`/`HolepunchMsgType`/`HolepunchError` re-exports, `ClientBuilder::enable_holepunch()`
- 30 new tests (1037 total)

## 0.45.0 — 2026-03-01

BEP 51 DHT infohash indexing — `sample_infohashes` query for discovering info hashes in the DHT. Eleven crates, 1007 tests, 25 BEPs implemented.

### M39: BEP 51 DHT Infohash Indexing

### Added
- BEP 51: DHT infohash indexing (`sample_infohashes` query/response)
- `PeerStore::random_info_hashes()` — Fisher-Yates partial shuffle sampling
- `DhtHandle::sample_infohashes()` — public async API for querying DHT nodes
- `SampleInfohashesResult` and `SampleInfohashesResponse` KRPC types
- DHT actor incoming/outgoing `sample_infohashes` query handling
- `DhtSampleInfohashes` alert variant
- `dht_sample_infohashes_interval` settings field (forward-compat)
- 12 new tests (1007 total)

## 0.44.0 — 2026-03-01

BEP 44 DHT arbitrary data storage — put/get immutable and mutable items with ed25519 signatures. Eleven crates, 995 tests, 24 BEPs implemented.

### M38: BEP 44 DHT Arbitrary Data Storage

### Added
- BEP 44: DHT arbitrary data storage (put/get immutable and mutable items)
- `DhtStorage` trait and `InMemoryDhtStorage` pluggable backend with LRU eviction
- `DhtHandle` public API: `put_immutable()`, `get_immutable()`, `put_mutable()`, `get_mutable()`
- KRPC `get`/`put` query encoding/decoding with transaction-aware response disambiguation
- Full BEP 44 validation: value size, salt size, ed25519 signatures, CAS, sequence monotonicity
- `ImmutableItem`, `MutableItem` types with signing/verification
- `dht_max_items`, `dht_item_lifetime_secs` settings + ClientBuilder methods
- 5 DHT item alert variants (forward-compatibility stubs)
- `ed25519-dalek` dependency for ed25519 signing
- 49 new tests (995 total)

## 0.43.0 — 2026-03-01

BEP 42 DHT security extension — node ID verification, IP voting, routing table hardening. Eleven crates, 946 tests, 23 BEPs implemented.

### M37: BEP 42 DHT Security Extension

### Added
- `node_id` module in `ferrite-dht` — CRC32C-based BEP 42 node ID generation and validation
- `generate_node_id(ip, r)` — creates BEP 42-compliant node IDs with CRC32C(masked_ip | r<<29) prefix
- `is_valid_node_id(id, ip)` — validates first 21 bits match CRC32C of IP address
- `is_bep42_exempt(ip)` — exempts local/private/loopback IPs from BEP 42 enforcement
- `ExternalIpVoter` — consensus-based external IP detection with majority threshold and per-source dedup
- `IpVoteSource` enum (`Dht(u64)` / `Nat` / `Tracker`) with distinct source IDs for voter dedup
- KRPC `ip` field — compact IP+port in all DHT responses per BEP 42 (6 bytes IPv4, 18 bytes IPv6)
- `encode_compact_addr()` / `decode_compact_addr()` helpers for KRPC ip field encoding
- `DhtActor::checked_insert()` — validates BEP 42 node IDs before routing table insertion
- `DhtActor::regenerate_node_id()` — recreates compliant node ID and routing table on IP consensus change
- `DhtCommand::UpdateExternalIp` — feeds external IP from NAT/tracker sources into DHT voter
- `DhtHandle::update_external_ip()` — public API for external IP propagation to DHT
- IP consensus channel: `DhtHandle::start()` returns `(Self, mpsc::Receiver<IpAddr>)` tuple
- `RoutingTable::new_with_config(own_id, restrict_ips)` — configurable IP restriction
- Routing table IP restriction — one node per IP address via `HashSet<IpAddr>` tracking with eviction cleanup
- `DhtConfig.enforce_node_id` / `restrict_routing_ips` — BEP 42 enforcement toggles (default true)
- `Settings.dht_enforce_node_id` / `dht_restrict_routing_ips` — session-level BEP 42 settings
- `ClientBuilder::dht_enforce_node_id()` / `dht_restrict_routing_ips()` — fluent builder methods
- `AlertKind::DhtNodeIdViolation` — alert for BEP 42 node ID validation failures
- Session integration: DHT IP consensus → `SessionActor.external_ip` update + torrent propagation
- NAT→DHT IP propagation: NAT `ExternalIpDiscovered` events forwarded to DHT voter
- `node_id` types re-exported from `ferrite::dht`

### Changed
- `DhtHandle::start()` now returns `Result<(Self, mpsc::Receiver<IpAddr>)>` (was `Result<Self>`)
- `KrpcMessage` gains `sender_ip: Option<SocketAddr>` field, populated in all responses
- All DHT routing table insertions go through `checked_insert()` with BEP 42 validation

## 0.42.0 — 2026-03-01

BEP 53 magnet file selection, dual-swarm announces, pure v2 torrent support. Eleven crates, 912 tests.

### M36: BEP 53 + Dual-Swarm + Pure v2

### Added
- `FileSelection` enum (`Single(usize)` / `Range(usize, usize)`) for BEP 53 `so=` magnet parameter parsing
- `FileSelection::parse()` — parses comma-separated indices and ranges (e.g., `0,2,4-6`)
- `FileSelection::to_priorities()` — converts selections to `Vec<FilePriority>` for selective download
- `FileSelection::to_so_value()` — serializes back to `so=` format for round-trip fidelity
- `Magnet.selected_files: Option<Vec<FileSelection>>` — parsed from `so=` key, serialized in `to_uri()`
- BEP 53 file selection applied in `try_assemble_metadata()` after file priorities initialized
- `TrackerManager.info_hashes: InfoHashes` — dual-swarm tracker announces for hybrid torrents
- `TrackerManager::set_info_hashes()` — update hashes after magnet metadata resolves
- `TrackerManager::announce_with_hash()` — announce to specific hash swarm
- `TorrentActor.dht_v2_peers_rx` / `dht_v6_v2_peers_rx` — dual-swarm DHT peer receivers
- Dual-swarm DHT `get_peers()` and `announce()` for v2 truncated hash on hybrid torrents
- Post-metadata wiring: `try_assemble_metadata()` detects hybrid/v2 via `torrent_from_bytes_any()`, updates version, info_hashes, TrackerManager, and starts v2 DHT lookups
- `synthesize_v1_from_v2()` — creates virtual v1 metadata from v2 data for pure v2 torrent support
- `TorrentMeta::best_v1_info_hash()` — convenience method for v1-compatible hash extraction
- `FileSelection` re-exported in `ferrite::core` and `ferrite::prelude`

### Changed
- `handle_add_torrent()` no longer rejects `TorrentMeta::V2` — synthesizes v1 metadata instead
- TrackerManager stores `InfoHashes` instead of bare `Id20`, enabling dual-swarm announces

## 0.41.0 — 2026-03-01

Hybrid v1+v2 torrent support (BEP 52). Eleven crates, 895 tests.

### M35: Hybrid v1+v2 Torrents

### Added
- `TorrentVersion` enum (`V1Only`, `V2Only`, `Hybrid`) for version-aware dispatch
- `TorrentMeta::Hybrid(Box<TorrentMetaV1>, Box<TorrentMetaV2>)` variant with `as_v1()` / `as_v2()` accessors
- `From<TorrentMetaV1>` and `From<TorrentMetaV2>` impls for ergonomic `.into()` conversion
- Hybrid torrent detection: `meta version = 2` + `pieces` key in info dict
- `HashResult` tribool (`Passed`/`Failed`/`NotApplicable`) for dual-hash verification
- `verify_and_mark_piece_hybrid()` — dual SHA-1 + SHA-256 verification with decision matrix
- `run_v2_block_verification()` — extracted side-effect-free v2 verification for reuse
- `AlertKind::InconsistentHashes` — fatal error when v1 and v2 hashes disagree on same piece
- `on_inconsistent_hashes()` — destroys hash picker, pauses torrent, posts error alerts
- `CreateTorrent::set_version(TorrentVersion)` — create hybrid `.torrent` files
- Hybrid creation: dual SHA-1 piece + SHA-256 Merkle hashing, merged info dict, piece layers
- `FileTreeNode::to_bencode()` — serialize v2 file tree back to bencode
- `TorrentVersion` re-exported from `ferrite::core` and `ferrite::prelude`

### Changed
- `TorrentActor.is_v2: bool` → `version: TorrentVersion` with match dispatch
- `SessionHandle::add_torrent()` accepts `TorrentMeta` (was `TorrentMetaV1`)
- `SessionCommand::AddTorrent.meta` → `Box<TorrentMeta>`
- `TorrentHandle::from_torrent()` takes `version: TorrentVersion` + `meta_v2: Option<TorrentMetaV2>`
- `TorrentSource::Meta(Box<TorrentMeta>)` and `AddTorrentParams::from_torrent(TorrentMeta)`
- `CreateTorrentResult.meta` → `TorrentMeta` (was `TorrentMetaV1`)

### Notes
- Pure v2-only torrents through session return `Error::Config` (not yet supported)
- Dual-swarm tracker/DHT announces deferred (hybrid announces on v1 swarm only)
- v1-only and v2-only torrents completely unaffected

## 0.40.0 — 2026-02-28

BEP 52 session integration — full v2 verification flow. Eleven crates, 883 tests.

### M34c: BEP 52 Session Integration

### Added
- `FastResumeData` v2 fields: `info_hash2` (SHA-256) and `trees` (piece-layer hash cache)
- `PeerEvent` v2 variants: `HashesReceived`, `HashRequestRejected`, `IncomingHashRequest`
- `PeerCommand` v2 variants: `SendHashRequest`, `SendHashes`, `SendHashReject`
- Peer task hash message handlers: wire `Message` ↔ `HashRequest` conversion
- `TorrentActor` v2 fields: `hash_picker`, `is_v2`, `meta_v2`
- `verify_and_mark_piece_v2()` — per-block SHA-256 Merkle verification via `HashPicker`
- Shared `on_piece_verified()` / `on_piece_hash_failed()` — refactored from v1-only to v1+v2
- Hash event handlers: `handle_hashes_received()` with Merkle proof validation, `handle_incoming_hash_request()` (reject-only for now)

### Notes
- All gap analysis amendments applied (Gaps 1, 3, 5, 6, 10)
- v1 torrents completely unaffected — v2 path only activates when `is_v2 = true`
- Hash request serving (seeding v2) deferred to M35

## 0.39.0 — 2026-02-28

BEP 52 storage v2 and disk I/O. Eleven crates, 880 tests.

### M34b: BEP 52 Storage v2 + Disk I/O

### Added
- `TorrentStorage::verify_piece_v2()` — SHA-256 piece verification for v2 torrents
- `TorrentStorage::hash_block()` — per-block SHA-256 hashing for Merkle verification
- `ChunkTracker` v2 per-block verification tracking (`enable_v2_tracking()`, `mark_block_verified()`, `all_blocks_verified()`)
- `DiskJob::HashV2` and `DiskJob::BlockHash` disk I/O variants
- `DiskHandle::verify_piece_v2()` and `DiskHandle::hash_block()` async methods
- 11 new tests (4 storage + 5 chunk_tracker + 2 disk)

### Notes
- All changes are additive — v1 torrents completely unaffected
- Default trait implementations work with all existing storage backends (MemoryStorage, FilesystemStorage, MmapStorage)
- `ChunkTracker` v2 tracking is `Option`-wrapped — zero overhead for v1 torrents

## 0.38.0 — 2026-02-28

BEP 52 wire protocol and hash picker. Eleven crates, 869 tests.

### M34a: BEP 52 Wire Protocol + Hash Picker

### Added
- BEP 52 wire messages: `HashRequest` (msg 21), `Hashes` (msg 22), `HashReject` (msg 23) in `ferrite-wire`
- `HashRequest` type with `validate_hash_request()` in `ferrite-core`
- `MerkleTreeState` for per-file Merkle tree verification with tri-state result (`Ok`/`Unknown`/`HashFailed`)
- `HashPicker` for coordinating hash requests to peers, including proactive per-block requests
- Merkle proof validation on received piece-layer hashes (prevents malicious hash injection)
- Deferred verification: block hashes stored when piece-layer hashes unavailable, retroactively verified on arrival
- Facade re-exports of all new types in `ferrite::core` and `ferrite::prelude`
- 26 new tests (6 wire + 5 hash_request + 7 merkle_state + 8 hash_picker)

### Architecture Notes
- Hash messages are core protocol (msg IDs 21-23), not extension messages
- Proactive per-block hash requesting implemented — goes beyond libtorrent which left this as `#if 0`
- `MerkleTreeState` returns `Unknown` (not `Ok`) when piece hash is known but not all sibling blocks are present yet
- `HashPicker::pick_hashes()` takes `impl Fn(u32) -> bool` callback for bitfield-agnostic piece ownership check
- Single-file v2 assumption documented; multi-file piece-to-file mapping deferred to future milestone

## 0.37.0 — 2026-02-28

BEP 52 core v2 metadata types and parsing. Eleven crates, 843 tests.

### M33: BEP 52 Core — v2 Metadata

### Added
- `sha256(data) -> Id32` — SHA-256 hashing (uses `sha2` crate v0.10)
- `Id32::to_base32()` / `from_base32()` — base32 encoding for v2 magnet links
- `Id32::to_multihash_hex()` / `from_multihash_hex()` — BEP 52 multihash format (`0x1220` prefix)
- `InfoHashes` — unified v1/v2 hash container (libtorrent `info_hash_t` parity) with `best_v1()` truncation fallback
- `MerkleTree` — binary heap layout with `from_leaves()`, `piece_layer()`, `proof_path()`, and `verify_proof()`
- `FileTreeNode` / `V2FileAttr` / `V2FileInfo` — BEP 52 nested file tree types with `BencodeValue` parsing
- `InfoDictV2` — v2 info dict with per-file piece alignment, `file_piece_ranges()`
- `TorrentMetaV2` — v2 torrent with piece layers, `validate_piece_layers()`, `piece_layer_for_file()`, `take_piece_layers()`
- `torrent_v2_from_bytes()` — parse v2 .torrent files with raw info bytes for BEP 9
- `TorrentMeta` enum — `V1` / `V2` variants with `torrent_from_bytes_any()` auto-detection
- `Magnet` extended: `urn:btmh:` parsing for v2, hybrid magnet support, `info_hash()` backward-compat method
- Facade re-exports in `ferrite::core` and `ferrite::prelude`
- 45 new tests across 6 new modules

### Changed
- **BREAKING**: `Magnet.info_hash: Id20` replaced by `Magnet.info_hashes: InfoHashes` — use `magnet.info_hash()` for v1 compat

## 0.36.0 — 2026-02-28

Extension plugin interface for custom BEP 10 extensions. Eleven crates, 796 tests.

### M32d: Extension Plugin Interface

### Added
- `ExtensionPlugin` trait — custom BEP 10 extension message handler with lifecycle hooks
- Plugin callbacks: `name()`, `on_handshake()`, `on_message()`, `on_peer_connected()`, `on_peer_disconnected()`
- `ClientBuilder::add_extension()` — register plugins at session creation
- `ExtHandshake::new_with_plugins()` — dynamic extension ID allocation (built-in 1-3, plugins 10+)
- Plugin dispatch in peer task: index-based ID matching, peer ext ID tracking, response routing
- `Arc<Vec<Box<dyn ExtensionPlugin>>>` threading from session → torrent → peer tasks
- Facade re-exports: `ExtensionPlugin` in `ferrite::session` and `ferrite::prelude`
- 10 new tests across extension, peer, wire, and facade modules

## 0.35.0 — 2026-02-28

Live storage relocation and share/relay mode. Eleven crates, 786 tests.

### M32c: Move Storage + Share Mode

### Added
- `TorrentCommand::MoveStorage` — relocate torrent files to a new directory at runtime
- `relocate_files()` helper — atomic file relocation with rename/copy+delete fallback
- `TorrentHandle::move_storage()` and `SessionHandle::move_torrent_storage()` public API
- `AlertKind::StorageMoved` alert variant
- `TorrentState::Sharing` — share/relay mode state
- `share_mode` config on Settings and TorrentConfig (requires fast extension)
- Share mode behaviour: skip disk registration, in-memory piece relay via WriteBuffer
- LRU eviction via `ChunkTracker::oldest_piece()` for bounded memory relay
- `PeerCommand::RejectRequest` for evicted pieces
- 10 new tests across torrent, types, and settings modules

## 0.34.0 — 2026-02-28

BEP 40 canonical peer priority and per-transport-class rate limiting. Eleven crates, 776 tests.

### M32b: BEP 40 + Per-Class Rate Limits

### Added
- `canonical_peer_priority()` using CRC32C in new `peer_priority.rs` module (BEP 40)
- `NatEvent::ExternalIpDiscovered` — external IP from PCP/NAT-PMP responses
- `external_ip` field on Settings, SessionActor, and TorrentActor
- `TorrentCommand::UpdateExternalIp` for propagating external IP to torrents
- Available peers sorted by BEP 40 canonical priority when external IP known
- `PeerTransport` enum (Tcp, Utp) in `peer_state.rs`
- `RateLimiterSet` — per-transport-class rate limiting with check-before-consume pattern
- 4 new Settings fields: `tcp_upload_rate_limit`, `tcp_download_rate_limit`, `utp_upload_rate_limit`, `utp_download_rate_limit`
- `ClientBuilder` methods for per-class rate limit configuration
- 12 new tests across peer_priority, rate_limiter, settings, and facade modules

## 0.33.0 — 2026-02-28

Bidirectional BEP 9 metadata serving and peer source tracking. Eleven crates, 764 tests.

### M32a: Metadata Serving + Peer Source Tracking

### Added
- `info_bytes: Option<Bytes>` on `TorrentMetaV1` — raw info dict bytes for metadata serving
- `info_bytes` populated in `torrent_from_bytes()` and `CreateTorrent::generate()`
- `info_bytes` populated from magnet metadata assembly in `try_assemble_metadata()`
- Inline metadata request handling in `run_peer()` with `build_metadata_response()` helper
- 16 KiB metadata piece chunking with reject for out-of-range pieces
- `PeerSource` enum (Tracker, Dht, Pex, Lsd, Incoming, Resume)
- `available_peers` upgraded to `Vec<(SocketAddr, PeerSource)>` with all 7 call sites updated
- `peers_by_source: HashMap<PeerSource, usize>` in `TorrentStats`
- Facade re-exports: `PeerSource` in `ferrite::session` and `ferrite::prelude`
- 10 new tests across metainfo, peer, torrent, and facade modules

## 0.32.0 — 2026-02-28

Unified settings pack with runtime configuration, presets, and serialization. Eleven crates, 754+ tests.

### M31: Settings Pack + Runtime Config

### Added
- `Settings` struct — unified 56-field configuration replacing `SessionConfig`
- Preset profiles: `Settings::min_memory()`, `Settings::high_performance()`
- `Settings::validate()` — 7 validation checks (piece size, thread counts, proxy config)
- JSON serialization with `#[serde(default)]` for forward-compatible config files
- `From<&Settings>` impls for `DiskConfig`, `BanConfig`, `TorrentConfig`
- Helper methods: `to_dht_config()`, `to_nat_config()`, `to_utp_config()`
- `SessionHandle::settings()` — query current session settings at runtime
- `SessionHandle::apply_settings()` — runtime config mutation with validation
- `AlertKind::SettingsChanged` — STATUS-category alert fired on settings update
- Runtime rate limiter updates via `apply_settings()` (upload/download limits)
- 6 new `ClientBuilder` methods: `dht_queries_per_second`, `dht_query_timeout_secs`, `upnp_lease_duration`, `natpmp_lifetime`, `utp_max_connections`, `disk_channel_capacity`
- `Settings` in `ferrite::prelude`
- 13 new tests across settings, session, and facade

### Changed
- `SessionHandle::start()` takes `Settings` instead of `SessionConfig`
- `ClientBuilder` stores `Settings` internally; `into_config()` renamed to `into_settings()`
- `Serialize`/`Deserialize` added to `EncryptionMode` and `StorageMode`

### Removed
- `SessionConfig` struct (replaced by `Settings`)

## 0.31.0 — 2026-02-28

Torrent creation from local files/directories with full BEP support. Eleven crates, 748 tests.

### M30: Torrent Creation

### Added
- `CreateTorrent` builder — create `.torrent` files from local files or directories
- `CreateTorrentResult` — generated torrent metadata + raw `.torrent` bytes
- `auto_piece_size()` — libtorrent-style automatic piece size selection (32 KiB–4 MiB based on total size)
- Builder API: `add_file()`, `add_directory()`, `set_name()`, `set_piece_size()`, `set_comment()`, `set_creator()`, `set_creation_date()`, `set_private()`, `set_source()`
- Network metadata: `add_tracker(url, tier)`, `add_web_seed()` (BEP 19), `add_http_seed()` (BEP 17), `add_dht_node()` (BEP 5)
- BEP 47 pad file alignment: `set_pad_file_limit(Option<u64>)` — None=no padding, Some(0)=pad all, Some(n)=threshold
- File metadata: `include_mtime(bool)` for modification times, `include_symlinks(bool)` for symlink targets
- BEP 47 file attributes: auto-detected "x" (executable), "h" (hidden), "p" (pad), "l" (symlink)
- Pre-computed hashes: `set_hash(piece, Id20)` skips disk reads for pieces with known hashes
- Progress callback: `generate_with_progress(|current, total| ...)` for piece hashing progress
- `Serialize` derive added to `InfoDict` and `FileEntry` (backward-compatible, enables bencode output)
- `InfoDict::source` field — private tracker source tag
- `FileEntry::attr`, `FileEntry::mtime`, `FileEntry::symlink_path` fields (BEP 47)
- `Error::Io` and `Error::CreateTorrent` variants in ferrite-core
- Facade re-exports: `CreateTorrent`, `CreateTorrentResult` in `ferrite::core` and `ferrite::prelude`
- 14 new tests: auto piece size, single/multi-file round-trip, private+source, tracker tiers, web/HTTP seeds, pad files, progress callback, empty input error, info hash round-trip, DHT nodes, file mtime, pre-computed hashes

## 0.30.0 — 2026-02-28

IP filtering, proxy support, anonymous mode, and force proxy mode. First milestone in Phase 5 (Network Hardening & Tools). Eleven crates, 734 tests.

### M29: IP Filtering + Proxy Support

### Added
- `IpFilter` — sorted interval map-based IP address filtering with IPv4/IPv6 support
- `PortFilter` — port range filtering using the same interval map structure
- `IntervalMap<K>` — generic sorted interval map with O(log n) lookup via BTreeMap
- `parse_dat()` / `parse_p2p()` — eMule `.dat` and P2P plaintext blocklist parsers
- Local network exemption: RFC 1918, loopback, and link-local addresses always allowed
- `SharedIpFilter` wired through `SessionActor` → `TorrentActor` with checks at all 3 connection points
- `SessionHandle::set_ip_filter()` / `ip_filter()` for dynamic filter updates
- `apply_ip_filter_to_trackers` config option (default: true)
- `PeerBlocked` alert variant (PEER category)
- `ProxyType` enum: None, Socks4, Socks5, Socks5Password, Http, HttpPassword
- `ProxyConfig` with per-connection-type routing (peer, tracker, hostname resolution)
- Hand-rolled SOCKS4 handshake (RFC 1928 predecessor)
- Hand-rolled SOCKS5 handshake with RFC 1929 username/password auth
- HTTP CONNECT tunnel with optional Basic auth
- SOCKS5 UDP ASSOCIATE + `ProxiedUdpSocket` for UDP relay
- `connect_through_proxy()` — TCP tunnel through any supported proxy type
- `HttpTracker::with_proxy()` — proxy-aware HTTP tracker client
- `force_proxy` mode: mandates proxy for all connections, disables listen/UPnP/NAT-PMP/DHT/LSD
- `anonymous_mode`: suppresses client version in BEP 10 ext handshake, disables discovery
- Outbound TCP connections routed through proxy when `proxy_peer_connections` is true
- uTP skipped when proxy is active (not proxied)
- `ClientBuilder::proxy()`, `force_proxy()`, `anonymous_mode()`, `apply_ip_filter_to_trackers()`
- Facade re-exports: `IpFilter`, `IpFilterError`, `PortFilter`, `parse_dat`, `parse_p2p`, `ProxyType`, `ProxyConfig`

## 0.29.0 — 2026-02-28

Advanced piece picker with block-level priority, dynamic request queue sizing, and file streaming. Eleven crates, 689 tests.

### M28: Advanced Piece Picker + Dynamic Request Queue + File Streaming

### Added
- `PieceSelector` rewrite with 5 priority layers: streaming window, time-critical, suggested, partial (speed affinity), new piece (rarest-first/sequential/random)
- Block-level piece picking: `pick_blocks()` returns individual block assignments instead of whole pieces
- `PeerSpeed` classification (Slow/Medium/Fast) with speed-affinity partial piece assignment
- `InFlightPiece` — per-block peer assignment tracking replacing `HashSet<u32>` in-flight set
- `PickContext` — structured per-peer pick cycle input with 17 fields
- `PickResult` — pick output with piece, blocks, and exclusive flag for whole-piece assignment
- Seed counter on `PieceSelector` for effective availability without modifying per-piece arrays
- Auto-sequential mode: switches to sequential when in-flight explosion detected
- Snubbed peer reverse picking (highest availability first)
- `PeerPipelineState` — per-peer dynamic request queue with TCP-like slow-start and EWMA rate tracking
- BDP-based queue depth calculation: `rate * request_queue_time / chunk_size`, clamped to [2, max]
- Slow-start exit detection: plateau in throughput (delta < 10 KB/s between seconds)
- Block-level request timeout tracking with `timed_out_blocks()` and `most_recent_request()`
- `FileStream` — `AsyncRead + AsyncSeek` over individual torrent files for media streaming
- `FileStreamHandle` — internal handle passed from actor to construct FileStream
- `StreamingCursor` — actor-tracked cursor with readahead window for streaming priority
- `TorrentHandle::open_file()` — open a streaming reader for a file within the torrent
- Piece completion broadcasting via `broadcast::channel` for FileStream wake-on-piece
- Have-bitfield watch channel for FileStream piece availability checks
- Concurrent stream reader limiting via `Semaphore`
- `EndGame::activate_with_inflight()` — merges InFlightPiece data with pending requests
- `EndGame::pick_block_streaming()` — streaming-aware block picking in end-game mode
- `PeerState` fields: `pipeline`, `snubbed`, `last_data_received`, `suggested_pieces`
- `TorrentConfig` fields: `sequential_download`, `initial_picker_threshold`, `whole_pieces_threshold`, `snub_timeout_secs`, `readahead_pieces`, `streaming_timeout_escalation`, `max_concurrent_stream_reads`
- `SessionConfig` fields: `max_request_queue_depth`, `request_queue_time`, `block_request_timeout_secs`, `max_concurrent_stream_reads`
- `ClientBuilder` methods: `max_request_queue_depth()`, `request_queue_time()`, `block_request_timeout_secs()`, `max_concurrent_stream_reads()`
- `FileStream` re-exported in `ferrite::session` and `ferrite::prelude`
- `Error::FileSkipped` variant for attempting to stream a skipped file
- 22 new tests: 2 config, 4 pipeline, 11 piece selector, 5 streaming

### Changed
- `TorrentActor::in_flight` replaced with `in_flight_pieces: HashMap<u32, InFlightPiece>` for block-level tracking
- `request_pieces_from_peer()` rewritten to use `pick_blocks()` with pipeline queue depth
- `handle_piece_data()` updates pipeline state, clears snub on data receipt
- `check_end_game_activation()` uses `activate_with_inflight()` for block-level end-game
- `request_end_game_block()` uses `pick_block_streaming()` when streaming active
- SuggestPiece handler records to `peer.suggested_pieces`
- Unchoke tick now updates streaming cursors and time-critical pieces

## 0.28.0 — 2026-02-28

Multi-threaded piece hashing with JoinSet, Checking state with progress reporting, configurable hashing threads. Eleven crates, 665 tests.

### M27: Multi-threaded Hashing

### Added
- `TorrentState::Checking` variant — entered during piece verification on startup
- `TorrentStats.checking_progress` field (0.0–1.0) for tracking verification progress
- `AlertKind::TorrentChecked` — fires after piece verification with `pieces_have`/`pieces_total`
- `AlertKind::CheckingProgress` — fires per-piece with monotonically increasing progress
- `TorrentConfig.hashing_threads` and `SessionConfig.hashing_threads` (default: 2) — controls pipeline depth for concurrent piece verification
- `ClientBuilder::hashing_threads()` facade method
- 5 new tests: concurrent disk verify, config defaults, checking state + progress alerts, stats during check, partial data verification

### Changed
- `verify_existing_pieces()` now uses `tokio::task::JoinSet` for bounded-concurrency parallel hashing instead of sequential loop
- Torrent transitions through `Checking` → `Downloading`/`Seeding` on startup instead of going directly to `Downloading`
- Queue management handles `Checking` state as a download category

## 0.27.0 — 2026-02-28

Async disk I/O with central actor, mmap storage backend, ARC disk cache. Eleven crates, 660 tests.

### M26: Async Disk I/O + mmap Storage + ARC Disk Cache

### Added
- `StorageMode` enum in ferrite-core: Auto, Sparse, Full (fallocate), Mmap
- Full pre-allocation mode (`preallocate: bool`) for `FilesystemStorage` using `fallocate` on Linux
- `MmapStorage` backend using `memmap2` with lazy per-file mmap and multi-file spanning
- `ArcCache<K, V>` — Adaptive Replacement Cache (Megiddo & Modha 2003) with ghost lists and adaptive parameter
- `DiskActor` — central tokio task for all disk I/O, receives jobs via bounded mpsc channel
- `DiskHandle` — per-torrent async API: `write_chunk()`, `read_chunk()`, `verify_piece()`, `flush_piece()`, `move_storage()`
- `DiskManagerHandle` — session-level API: `register_torrent()`, `unregister_torrent()`, `shutdown()`
- `DiskConfig` — configurable I/O threads, storage mode, cache size, write cache ratio
- `DiskJobFlags` bitflags: FORCE_COPY, SEQUENTIAL, VOLATILE_READ, FLUSH_PIECE
- `DiskStats` — read/write bytes, cache hits/misses, write buffer usage, queued jobs
- `WriteBuffer` — per-piece write buffering with pressure-based eviction
- `AlertKind::DiskStatsUpdate(DiskStats)` alert variant
- `SessionConfig` fields: `disk_io_threads`, `storage_mode`, `disk_cache_size`, `disk_write_cache_ratio`
- `ClientBuilder` methods: `disk_io_threads()`, `storage_mode()`, `disk_cache_size()`
- Auto-flush WriteBuffer before piece hash verification
- Session disk lifecycle: register on add, unregister on remove, shutdown on exit
- Magnet torrents register storage with disk manager after metadata assembly
- Facade re-exports: `MmapStorage`, `ArcCache`, `DiskConfig`, `DiskHandle`, `DiskManagerHandle`, `DiskJobFlags`, `DiskStats`, `StorageMode`
- 23 new tests across storage, cache, disk, and session modules

### Changed
- `TorrentActor` uses `DiskHandle` instead of `Arc<dyn TorrentStorage>` — all I/O is async through DiskActor
- `TorrentHandle::from_torrent()` accepts `DiskHandle` + `DiskManagerHandle` instead of `Arc<dyn TorrentStorage>`
- `TorrentHandle::from_magnet()` accepts `DiskManagerHandle` for post-metadata storage registration
- `verify_existing_pieces()` is now async (verifies through DiskHandle)
- Piece reads for upload use async DiskHandle instead of synchronous storage

## 0.26.0 — 2026-02-28

Smart banning — peer attribution for hash failures, parole mode, session-wide bans. Eleven crates, 637 tests.

### M25: Smart Banning

### Added
- `BanConfig` — configurable max failures (default: 3) and parole toggle
- `BanManager` — session-wide ban/strike tracker shared via `Arc<RwLock<_>>`
- `ParoleState` — per-piece parole tracking (re-download from uninvolved peer)
- Piece contributor tracking: `TorrentActor` records which peers send data for each piece
- Parole mode: on hash failure, re-downloads piece from single uninvolved peer to isolate offender
- Parole success → strikes all original contributors; parole failure → strikes parole peer
- Ban checking at all connection points: `handle_add_peers`, `try_connect_peers`, `spawn_peer_from_stream_with_mode`
- `disconnect_banned_ip()` — removes all peers matching banned IP
- `AlertKind::HashFailed` extended with `contributors: Vec<IpAddr>` field
- `SessionHandle::ban_peer()`, `unban_peer()`, `banned_peers()` public API
- `SessionConfig::smart_ban_max_failures`, `smart_ban_parole` configuration fields
- `ClientBuilder::smart_ban_max_failures()`, `smart_ban_parole()` builder methods
- `SessionState::banned_peers`, `peer_strikes` persistence fields (backward compatible via `#[serde(default)]`)
- `PeerStrikeEntry` persistence struct
- `BanManager::restore()` for rebuilding from persisted state
- Facade re-exports: `BanConfig`, `PeerStrikeEntry` through `ferrite::session` and `ferrite::prelude`
- 16 new tests across ban, torrent, persistence, session, and facade modules

## 0.25.0 — 2026-02-28

Tracker scraping, tracker exchange, enhanced tracker management. Eleven crates, 621 tests.

### M24: Tracker Scrape + Enhanced Tracker Management

### Added
- BEP 48 scrape: `ScrapeInfo` type shared between HTTP and UDP, `announce_url_to_scrape()` URL conversion
- HTTP scrape: `HttpScrapeResponse`, `HttpTracker::build_scrape_url()`, `HttpTracker::scrape()` — parses bencode response with raw 20-byte hash keys
- UDP scrape: `UdpScrapeResponse`, `UdpTracker::build_scrape_request()`, `UdpTracker::parse_scrape_response()`, `UdpTracker::scrape()` — BEP 15 action=2 protocol
- `TrackerInfo` and `TrackerStatus` public types for tracker status reporting
- `TrackerManager::tracker_list()` — snapshot of all trackers with status, seeders/leechers, next announce time
- `TrackerManager::force_reannounce()` — reset all announce timers to now
- `TrackerManager::add_tracker_url()` — deduplicated tracker addition (used by lt_trackers exchange)
- `TrackerManager::scrape()` — try each tracker's scrape until one succeeds
- `consecutive_failures` and `scrape_info` tracking on `TrackerEntry`
- Announce response now threads seeders/leechers from tracker into `scrape_info`
- `AlertKind::ScrapeReply` and `AlertKind::ScrapeError` alert variants (categories: TRACKER, TRACKER|ERROR)
- lt_trackers wire protocol: `lt_trackers=3` registered in `ExtHandshake::new()`
- `LtTrackersMessage` — bencode encode/decode for tracker URL exchange between peers
- `PeerEvent::TrackersReceived` — new peer event for received tracker URLs
- Peer dispatch: incoming lt_trackers messages parsed and forwarded to TorrentActor
- TorrentActor: received tracker URLs added via `add_tracker_url()`
- `TorrentCommand::ForceReannounce`, `TrackerList`, `Scrape` command variants
- `TorrentHandle::tracker_list()`, `force_reannounce()`, `scrape()` public API methods
- Facade re-exports: `ScrapeInfo`, `announce_url_to_scrape`, `HttpScrapeResponse`, `UdpScrapeResponse`, `TrackerInfo`, `TrackerStatus`
- 22 new tests across tracker, session, wire, and facade crates

## 0.24.0 — 2026-02-27

Seeding optimizations — BEP 16 super seeding, BEP 21 upload-only, batched Have. Eleven crates, 599 tests.

### M23: Super Seeding, Upload-Only, Batched Have

### Added
- BEP 16 super seeding: `SuperSeedState` tracks per-peer piece assignments, reveals rarest pieces one-per-peer for maximum swarm diversity
- BEP 21 upload-only: `upload_only` field on `ExtHandshake`, `new_upload_only()` constructor, `is_upload_only()` accessor
- BEP 21 choker integration: upload-only peers excluded from optimistic unchoke slot but can earn regular unchoke by upload rate
- BEP 21 automatic broadcast: extension handshake with `upload_only=1` sent to all peers when transitioning to seeding
- Batched Have: `HaveBuffer` accumulates Have messages and flushes on configurable timer; auto-upgrades to full Bitfield when > 50% of pieces pending
- Redundancy elimination: Have messages not sent to peers that already have the piece (both immediate and batched modes)
- `super_seeding`, `upload_only_announce`, `have_send_delay_ms` config fields on `TorrentConfig` and `SessionConfig`
- `PeerCommand::SendExtHandshake` and `PeerCommand::SendBitfield` variants for mid-connection protocol messages
- `PeerState::upload_only` and `PeerState::super_seed_assigned` tracking fields
- `super_seeding` field on `FastResumeData` for resume persistence
- `ClientBuilder::super_seeding()`, `upload_only_announce()`, `have_send_delay_ms()` builder methods
- 14 new tests across wire, session types, choker, super_seed, have_buffer, resume_data, and facade

## 0.23.0 — 2026-02-27

HTTP/web seeding support — BEP 19 (GetRight) and BEP 17 (Hoffman). Eleven crates, 585 tests.

### M22: HTTP/Web Seeding (BEP 19, BEP 17)

### Added
- `url-list` parsing from torrent metadata with custom `UrlList` deserializer (handles both single string and list)
- `httpseeds` parsing from torrent metadata (BEP 17)
- `WebSeedTask` — background async task that downloads pieces from HTTP servers
- `WebSeedUrlBuilder` — constructs HTTP Range requests for single-file and multi-file torrents
- BEP 19 (GetRight): per-file HTTP Range requests against static web servers
- BEP 17 (Hoffman): parameterized URL requests with percent-encoded info hash
- `WebSeedMode` enum: `GetRight` (BEP 19) vs `Hoffman` (BEP 17)
- `WebSeedCommand` / `WebSeedError` types for task communication
- TorrentActor integration: `spawn_web_seeds()`, `assign_pieces_to_web_seeds()`, piece data handling
- Hash-failure banning: permanent URL ban on piece verification failure (per BEP 19 spec)
- `enable_web_seed` config field on `TorrentConfig` and `SessionConfig` (default: `true`)
- `max_web_seeds` config field on `TorrentConfig` (default: `4`)
- `WebSeedBanned` alert variant (category: STATUS)
- Resume data: `url_seeds` and `http_seeds` fields on `FastResumeData`
- `ClientBuilder::enable_web_seed()` facade builder method
- `url_encode_info_hash()` for BEP 17 percent-encoding
- reqwest dependency added to ferrite-session (rustls-tls)
- 19 new tests across metainfo, web_seed, types, resume_data, alert, and facade

## 0.22.0 — 2026-02-27

Full dual-stack IPv6 support — BEP 7, BEP 24, BEP 11 IPv6 extensions. Eleven crates, 566 tests.

### M21: IPv6 Support (BEP 7, 24)

### Added
- IPv6 compact peer format (18-byte: 16 IP + 2 port) — `parse_compact_peers6()` / `encode_compact_peers6()`
- IPv6 compact node format (38-byte: 20 ID + 16 IP + 2 port) — `CompactNodeInfo6` with encode/decode
- `encode_compact_peers()` for IPv4 (previously only parse existed)
- PEX IPv6 fields: `added6`, `added_flags6`, `dropped6` on `PexMessage` (BEP 11 extension)
- HTTP tracker `peers6` key parsing (BEP 7)
- KRPC `nodes6` field support in `GetPeersResponse` and `FindNode` responses (BEP 24)
- `AddressFamily` enum (`V4`, `V6`) in ferrite-core
- `DhtConfig::default_v6()` — IPv6 DHT bootstrap configuration (AAAA records)
- Dual DHT instances: separate `DhtActor` for IPv4 and IPv6, merged results
- DNS result filtering by address family in DHT bootstrap
- Routing table address family enforcement — rejects wrong-family addresses
- `enable_ipv6` config field on `SessionConfig` (default: `true`)
- Dual-stack TCP listeners: `[::]:port` with fallback to separate v4+v6 binds
- Dual-stack uTP sockets: separate v4 and v6 `UtpSocket` instances
- `is_local_network()` extended for IPv6: `::1`, `fe80::/10`, `fc00::/7`
- `allowed_fast_set_for_ip()` — IPv6 support with /48 prefix mask
- `ClientBuilder::enable_ipv6()` builder method
- Resume data `peers6` field codec wiring
- Facade re-exports: `AddressFamily`, `CompactNodeInfo6`, compact peers6/nodes6 codecs
- 32 new tests across tracker, dht, wire, session, and facade crates

### Design Decisions
- Two separate DhtActors (v4 and v6) rather than a single merged actor
- NAT stays IPv4-only (IPv6 doesn't need NAT traversal)
- Same port for both address families
- IPv6 DHT bootstraps from `router.bittorrent.com` (AAAA) and `dht.libtorrent.org:25401`

## 0.21.0 — 2026-02-27

Automatic NAT port mapping — PCP, NAT-PMP, UPnP IGD. Eleven crates, 534 tests.

### M20: UPnP / NAT-PMP / PCP Port Mapping

### Added
- `ferrite-nat` crate — automatic port mapping with three protocol implementations
- PCP (RFC 6887) MAP opcode encode/decode with IPv4-mapped IPv6 addressing
- NAT-PMP (RFC 6886) external address and port mapping request/response codec
- UPnP IGD SSDP M-SEARCH discovery and SOAP XML control (AddPortMapping, DeletePortMapping, GetExternalIPAddress)
- Gateway discovery via `/proc/net/route` parsing and local IP via UDP socket trick
- `NatActor` / `NatHandle` — background port mapping lifecycle with protocol fallback chain (PCP → NAT-PMP → UPnP)
- Automatic mapping renewal at half the granted lifetime
- Best-effort cleanup on shutdown (delete mappings via all protocols)
- `enable_upnp` and `enable_natpmp` config fields on `SessionConfig` (default: `true`)
- NAT event handling in `SessionActor` select loop — fires `PortMappingSucceeded` / `PortMappingFailed` alerts
- `ClientBuilder::enable_upnp()` and `enable_natpmp()` builder methods
- `ferrite::nat` facade module with full re-exports
- TCP listener bind changed from `127.0.0.1` to `0.0.0.0` for external reachability
- 23 new tests across nat, session, and facade crates

### Deferred
- IPv6 NAT traversal (M21)
- UPnP XML namespace-aware parsing (simple string search suffices)
- Per-torrent port mapping (all torrents share session port)

## 0.20.0 — 2026-02-27

uTP session integration — dual-protocol peer connections. Ten crates, 511 tests.

### M19: uTP Session Integration

### Added
- `enable_utp` config field on `TorrentConfig` and `SessionConfig` (default: `true`)
- `TorrentCommand::IncomingPeer` variant for session-routed uTP peers
- `PrefixedStream<S>` — `AsyncRead + AsyncWrite` wrapper replaying consumed preamble bytes
- `identify_plaintext_connection()` — reads 48-byte BT preamble, extracts info_hash for routing
- `spawn_peer_from_stream_with_mode()` — encryption mode override for pre-identified uTP streams
- Session-level `UtpSocket` (shared) and `UtpListener` with inbound accept loop
- Dual-protocol outbound: uTP first (5s timeout), TCP fallback (matches libtorrent behavior)
- Inbound uTP routing: session reads preamble → extracts info_hash → dispatches to correct torrent
- `TorrentHandle::send_incoming_peer()` for session → torrent peer dispatch
- `ClientBuilder::enable_utp()` builder method
- 10 new tests across session, routing, and facade crates

### Deferred
- MSE-encrypted inbound uTP routing (requires multi-skey lookup; dropped with debug log)

## 0.19.0 — 2026-02-27

uTP (BEP 29) micro transport protocol with LEDBAT congestion control. Ten crates, 501 tests.

### M18: uTP Protocol (BEP 29)

### Added
- `ferrite-utp` crate — standalone uTP implementation over UDP
- `SeqNr` — wrapping u16 sequence number arithmetic with correct circular ordering
- BEP 29 packet header encode/decode with SACK extension chain walking
- `LedbatController` — LEDBAT congestion control (100ms target delay, base delay tracking, Jacobson RTT)
- `Connection` — pure-logic state machine (SynSent → Connected → FinSent → Closed) producing `ConnAction` vectors
- `UtpStream` — `AsyncRead + AsyncWrite + Unpin + Send` facade bridging application ↔ socket actor
- `UtpSocket` — actor model (single UDP socket owner, per-connection dispatch, 50ms tick timer)
- `UtpListener` — accept incoming uTP connections
- `UtpConfig` — bind address and connection limits
- Send buffering with congestion window gating and flush-on-ACK
- SACK bitmask generation from receive buffer gaps
- Duplicate ACK counting with loss detection (3 duplicates = loss event)
- Retransmission on timeout with exponential backoff
- Facade re-exports in `ferrite::utp` and `ferrite::error::Error::Utp` variant
- 21 new tests: 4 seq, 4 packet, 4 congestion, 3 conn, 6 integration (1MB loopback transfer with SHA1 verification)

### Changed
- License changed from MIT OR Apache-2.0 to GPL-3.0-or-later

## 0.18.0 — 2026-02-27

MSE/PE connection encryption with DH key exchange and RC4 stream cipher. Nine crates, 480 tests.

### M17: Connection Encryption (MSE/PE)

### Added
- `EncryptionMode` enum: `Disabled`, `Enabled` (default, fallback), `Forced` (RC4 only)
- `MseStream<S>` — transparent `AsyncRead + AsyncWrite` wrapper with optional RC4 encryption
- `negotiate_outbound()` / `negotiate_inbound()` — MSE/PE handshake state machines
- `ferrite-wire::mse` submodule: `cipher` (RC4 with 1024-byte discard), `dh` (768-bit Diffie-Hellman), `crypto` (SHA1 key derivation), `handshake`, `stream`
- `encryption_mode` config field on `SessionConfig`, `TorrentConfig`, and `ClientBuilder`
- `ferrite_core::random_bytes()` — public RNG helper using existing xorshift64
- `EncryptionMode` re-exported in `ferrite::prelude`
- 21 new tests (RC4 roundtrip, DH key agreement, hash functions, encrypted stream, full handshake)

## 0.17.0 — 2026-02-27

Queue management with auto-manage system and position control. Nine crates, 459 tests.

### M16: Queue Management

### Added
- `SessionConfig` queue fields: `active_downloads`, `active_seeds`, `active_limit`, `active_checking`, `dont_count_slow_torrents`, `inactive_down_rate`, `inactive_up_rate`, `auto_manage_interval`, `auto_manage_startup`, `auto_manage_prefer_seeds`
- `SessionHandle` queue position API: `queue_position()`, `set_queue_position()`, `queue_position_up()`, `queue_position_down()`, `queue_position_top()`, `queue_position_bottom()`
- Auto-manage system — periodic queue evaluation that starts/stops auto-managed torrents based on active limits and inactivity detection
- `TorrentQueuePositionChanged` and `TorrentAutoManaged` alert variants
- `FastResumeData::queue_position` field for queue position persistence
- `ClientBuilder` queue config methods: 8 new builder methods for queue configuration
- `ferrite-session::queue` module — pure-logic queue position arithmetic and evaluation algorithm
- 22 new tests across queue unit tests, session integration, and facade tests

## 0.16.0 — 2026-02-26

Alerts / events system with async broadcast channels and category filtering. Nine crates, 437 tests.

### M15: Alerts / Events System

### Added
- `Alert` struct with `SystemTime` timestamp and typed `AlertKind` enum (~30 variants)
- `AlertCategory` bitflags — STATUS, ERROR, PEER, TRACKER, STORAGE, DHT, STATS, PIECE, BLOCK, PERFORMANCE, PORT_MAPPING, ALL
- `AlertStream` — per-subscriber category filter wrapper over `broadcast::Receiver<Alert>`
- `post_alert()` — free function shared by SessionActor and TorrentActor (lockless atomic mask check)
- `SessionHandle::subscribe()` — raw broadcast receiver for all alerts passing session mask
- `SessionHandle::subscribe_filtered(filter)` — convenience wrapper returning `AlertStream`
- `SessionHandle::set_alert_mask()` / `alert_mask()` — runtime category mask control (atomic, no roundtrip)
- `SessionConfig::alert_mask` (default: ALL) and `alert_channel_size` (default: 1024)
- `ClientBuilder::alert_mask()` and `alert_channel_size()` methods
- `transition_state()` on TorrentActor — automatic `StateChanged` alerts on every state transition
- `TrackerOutcome` and `AnnounceResult` — per-tracker announce outcomes for TrackerReply/TrackerError alerts
- Alerts fired at: torrent add/remove/pause/resume/finish, piece complete/hash fail, peer connect/disconnect, tracker reply/error, metadata received, state transitions
- `Serialize` / `Deserialize` derives on `Alert`, `AlertKind`, `AlertCategory`, `TorrentState`, `SessionStats` — ready for MagneTor REST API
- Facade re-exports and prelude additions: `Alert`, `AlertKind`, `AlertCategory`, `AlertStream`
- 16 new tests: 6 alert unit tests, 4 integration tests (multi-subscriber, runtime mask, per-subscriber filter, state tracking), 4 session/torrent alert tests, 2 JSON serialization round-trip tests

## 0.15.0 — 2026-02-26

Bandwidth limiting and automatic upload slot tuning. Nine crates, 422 tests.

### M14: Bandwidth Limiter + Upload Slot Tuning

### Added
- `TokenBucket` — token-bucket rate limiter (bytes/sec, 1-second burst, 100ms refill granularity)
- `is_local_network()` — RFC 1918/loopback/link-local detection for global limiter exemption
- `SlotTuner` — hill-climbing optimizer for automatic unchoke slot count adjustment
- Per-torrent rate limiting: `upload_rate_limit` and `download_rate_limit` on `TorrentConfig` (0 = unlimited)
- Global rate limiting: `upload_rate_limit` and `download_rate_limit` on `SessionConfig` (shared `Arc<Mutex<TokenBucket>>`)
- `auto_upload_slots`, `auto_upload_slots_min`, `auto_upload_slots_max` on `SessionConfig`
- Actor-level gating: download throttled via request pipelining, upload throttled via chunk dispatch
- Local network peers exempt from global limiter (per-torrent still applies)
- `Choker::set_unchoke_slots()` for SlotTuner integration
- 19 new tests: 7 rate_limiter, 6 slot_tuner, 1 choker, 2 types, 3 integration

## 0.14.0 — 2026-02-26

End-game mode for fast piece completion. Nine crates, 403 tests.

### M13: End-Game Mode

### Added
- `EndGame` struct — block-level duplicate-request tracking for fast piece completion
- O(1) activation check: `have + in_flight >= num_pieces`
- Single block per peer in end-game (no pipelining) to maximize coverage
- Cancel messages sent to all other peers on block arrival
- Deactivation on hash failure or download completion
- `strict_end_game` config on `TorrentConfig` (default: true)
- 13 new tests across EndGame unit tests and TorrentActor integration

## 0.13.0 — 2026-02-26

Selective file download with per-file priority control. Nine crates, 390 tests.

### M12: Selective File Download

### Added
- `FilePriority` enum (`Skip`, `Low`, `Normal`, `High`) with `Ord`, `Default` (Normal), serde, `From<u8>` (libtorrent-compatible mapping)
- `PieceSelector::pick()` now accepts a `wanted` bitfield to skip unwanted pieces
- `TorrentHandle::set_file_priority()` — set priority for individual files at runtime
- `TorrentHandle::file_priorities()` — query current per-file priority list
- `build_wanted_pieces()` — compute piece-level wanted bitfield from file priorities and file/piece geometry
- `FilesystemStorage` skips creating files with `Skip` priority (no disk allocation for unwanted files)
- `FastResumeData::file_priority` field — persists per-file priorities across sessions
- Facade re-exports: `ferrite::core::FilePriority`, `ferrite::session::build_wanted_pieces`
- Prelude addition: `FilePriority`
- 16 new tests across ferrite-core (6), ferrite-session (8), ferrite-storage (1), ferrite facade (1)

## 0.12.0 — 2026-02-26

Resume data and session persistence with libtorrent-compatible format. Nine crates, 374 tests.

### M11: Resume Data & Session Persistence

### Added
- `FastResumeData` struct — libtorrent-compatible bencode resume format with all standard fields (pieces, stats, trackers, peers, priorities, rate limits, embedded info dict)
- `UnfinishedPiece` — partial piece state with per-block bitmask
- `TorrentHandle::save_resume_data()` — snapshot torrent state to resume data
- `SessionHandle::save_torrent_resume_data()` — save resume data for a specific torrent
- `SessionHandle::save_session_state()` — save all torrent resume data + DHT node cache
- `SessionState` + `DhtNodeEntry` — session-level persistence types
- `validate_resume_bitfield()` — verify bitfield length matches piece count
- `SessionConfig::resume_data_dir` — configurable resume data directory
- Facade re-exports: `ferrite::core::{FastResumeData, UnfinishedPiece}`, `ferrite::session::{SessionState, DhtNodeEntry, validate_resume_bitfield}`
- Prelude additions: `FastResumeData`, `SessionState`
- 19 new tests across ferrite-core (6), ferrite-session (11), ferrite facade (2)

## 0.11.0 — 2026-02-26

Prelude, unified error, and integration tests complete the M10 facade. Eight crates, 355 tests.

### M10e: ferrite (prelude + unified error + integration tests)

### Added
- `ferrite::Error` — unified error enum wrapping all 7 per-crate error types + `std::io::Error`
- `ferrite::Result<T>` — unified result type alias
- `ferrite::prelude` — convenience re-exports (`ClientBuilder`, `AddTorrentParams`, `SessionHandle`, `TorrentHandle`, `TorrentState`, `TorrentStats`, `TorrentInfo`, `Id20`, `Magnet`, `TorrentMetaV1`, `TorrentStorage`, `FilesystemStorage`, `Error`, `Result`)
- Top-level `ferrite::Error` and `ferrite::Result` re-exports
- 3 facade tests: unified error conversions, prelude imports, full type chain

### Milestone Complete
- **M10 is now complete.** The `ferrite` crate provides a full public facade over all internal crates.

## 0.10.0 — 2026-02-25

Session re-exports and ergonomic client builder in facade crate. Eight crates, 352 tests.

### M10d: ferrite (session + ClientBuilder)

### Added
- `ferrite::session` — re-exports `SessionHandle`, `SessionConfig`, `SessionStats`, `TorrentHandle`, `TorrentConfig`, `TorrentState`, `TorrentStats`, `TorrentInfo`, `FileInfo`, `StorageFactory`, `Error`
- `ferrite::client` — new `ClientBuilder` (fluent builder for `SessionConfig` → `SessionHandle`) and `AddTorrentParams` (unified torrent source with `from_torrent`/`from_magnet`/`from_file`/`from_bytes`)
- Top-level `ferrite::ClientBuilder` and `ferrite::AddTorrentParams` re-exports
- 2 facade tests: client builder defaults + chaining, AddTorrentParams from magnet

## 0.9.0 — 2026-02-25

DHT and storage re-exports in facade crate. Eight crates, 349 tests.

### M10c: ferrite (dht + storage re-exports)

### Added
- `ferrite::dht` — re-exports `DhtHandle`, `DhtConfig`, `DhtStats`, `CompactNodeInfo`, `RoutingTable`, `KrpcMessage`, `KrpcBody`, `KrpcQuery`, `KrpcResponse`, `GetPeersResponse`, `TransactionId`, `parse_compact_nodes`, `encode_compact_nodes`, `PeerStore`, `K`, `Error`
- `ferrite::storage` — re-exports `TorrentStorage`, `Bitfield`, `ChunkTracker`, `FileMap`, `FileSegment`, `MemoryStorage`, `FilesystemStorage`, `Error`
- 2 facade tests: DHT compact node round-trip, storage bitfield operations

## 0.8.0 — 2026-02-25

Wire and tracker re-exports in facade crate. Eight crates, 347 tests.

### M10b: ferrite (wire + tracker re-exports)

### Added
- `ferrite::wire` — re-exports `Handshake`, `Message`, `MessageCodec`, `ExtHandshake`, `ExtMessage`, `MetadataMessage`, `MetadataMessageType`, `allowed_fast_set`, `Error`
- `ferrite::tracker` — re-exports `HttpTracker`, `HttpAnnounceResponse`, `UdpTracker`, `UdpAnnounceResponse`, `AnnounceRequest`, `AnnounceResponse`, `AnnounceEvent`, `parse_compact_peers`, `Error`
- 2 facade tests: handshake round-trip, announce request construction

## 0.7.0 — 2026-02-25

Facade crate scaffold with bencode and core re-exports. Eight crates, 345 tests.

### M10a: ferrite (scaffold + bencode/core re-exports)

### Added
- `ferrite` facade crate with workspace wiring
- `ferrite::bencode` — re-exports `to_bytes`, `from_bytes`, `BencodeValue`, `find_dict_key_span`, `Serializer`, `Deserializer`, `Error`
- `ferrite::core` — re-exports `Id20`, `Id32`, `PeerId`, `Magnet`, `TorrentMetaV1`, `InfoDict`, `FileEntry`, `FileInfo`, `torrent_from_bytes`, `Lengths`, `DEFAULT_CHUNK_SIZE`, `sha1`, `Error`
- 3 facade tests: bencode round-trip, core types, magnet parse

## 0.6.0 — 2026-02-25

Seeding & upload pipeline. Seven crates, 342 tests.

### M9: ferrite-session (seeding & upload pipeline)
- `PeerCommand::SendPiece` — push piece data to peer TCP stream
- `serve_incoming_requests()` — read from storage and dispatch to unchoked peers
- `TorrentState::Seeding` — new state after download completion
- Dual-mode choker: leech mode (download rate tit-for-tat) / seed mode (upload rate)
- Rolling window rate tracking (10s unchoke interval) for download and upload
- Configurable `seed_ratio_limit` on TorrentConfig/SessionConfig — stops torrent when reached
- `verify_existing_pieces()` — scan storage on startup for resume support
- Immediate seeding when all pieces verified on startup

## 0.5.0 — 2026-02-25

Complete M8 gaps: magnet links, LSD actor, AllowedFast, RejectRequest. Seven crates, 337 tests.

### M8c: ferrite-session (magnet/LSD/AllowedFast/RejectRequest)
- `add_magnet()` on `SessionHandle` — magnet link support in session manager
- `MetadataNotReady` error for torrent_info() on magnet torrents pre-BEP 9
- `LsdHandle` + `LsdActor` — full BEP 14 Local Peer Discovery UDP multicast actor
- LSD wired into SessionActor via `tokio::select!` loop with peer routing
- LSD announce triggered on add_torrent/add_magnet
- AllowedFast messages sent on peer connect (Phase 4b, BEP 6)
- Pending requests tracked as `Vec<(u32, u32, u32)>` for precise matching
- Incoming request tracking with `RejectRequest` on choke for fast peers
- `IncomingRequest` PeerEvent variant for forwarding `Message::Request`

## 0.4.0 — 2026-02-25

Session manager, BEP 6 Fast Extension, BEP 14 Local Peer Discovery. Seven crates, 331 tests.

### M8b: ferrite-session (session manager + fast extension + LSD)
- `SessionHandle` + `SessionActor` — multi-torrent session manager (actor model)
- Add/remove/pause/resume torrents, shared DHT, duplicate rejection, capacity limits
- Torrent info/stats queries via SessionHandle
- BEP 6 Fast Extension wire messages: SuggestPiece, HaveAll, HaveNone, RejectRequest, AllowedFast
- BEP 6 `allowed_fast_set()` algorithm (IP /24 mask + SHA1 chain)
- BEP 6 handshake flag (`supports_fast()` / `with_fast()`)
- BEP 6 peer behavior: HaveAll/HaveNone in Phase 4, message handling in peer task
- BEP 14 Local Peer Discovery: announce formatting (batch, MTU-aware), message parsing, rate limiting
- `TorrentState::Paused` with pause/resume on TorrentHandle
- `SessionConfig`, `TorrentInfo`, `FileInfo`, `SessionStats`, `StorageFactory` types

### ferrite-wire
- 5 BEP 6 message variants with encode/decode
- `allowed_fast_set()` public function
- `supports_fast()` / `with_fast()` handshake methods

## 0.3.0 — 2026-02-25

Tracker and DHT integration into TorrentActor. Seven crates, 299 tests.

### M8a: ferrite-session (tracker + DHT integration)
- `TrackerManager` — per-torrent tracker announce lifecycle (BEP 3/12/15)
- BEP 12 announce_list parsing with tier support and URL deduplication
- HTTP and UDP tracker classification with exponential backoff (30s → 30min)
- Tracker re-announce select! arm in TorrentActor (timer-driven, immediate first fire)
- DHT peer receiver select! arm (batched `get_peers` results → `handle_add_peers`)
- DHT re-search on connect timer when peer pool is exhausted
- Completed announce on download finish, Stopped announce on shutdown
- Tracker population on metadata assembly (magnet link flow)
- `from_torrent()` now accepts `dht: Option<DhtHandle>` parameter

## 0.2.0 — 2026-02-25

Session layer with peer management and torrent orchestration. Seven crates, 283 tests.

### M7: ferrite-session (peer + torrent)
- `PieceSelector` — rarest-first piece selection with per-piece availability tracking
- `MetadataDownloader` — BEP 9 metadata download state machine for magnet links (16 KiB pieces, SHA1 verification)
- `PexMessage` — BEP 11 Peer Exchange encode/decode with compact peer format
- `Choker` — tit-for-tat choking: top-N by download rate + optimistic unchoke rotation
- `PeerTask` — async peer connection: handshake, BEP 10 extension negotiation, framed message loop
- `TorrentActor` + `TorrentHandle` — actor model orchestration with select! event loop
- BEP 27 private torrent support (disable DHT/PEX when `info.private = 1`)
- Piece download pipeline: request → write_chunk → verify → mark_verified/mark_failed
- Integration tested with seeder/leecher on loopback TCP

## 0.1.0 — 2026-02-25

Initial development release. Six crates implemented with 231 tests and zero clippy warnings.

### M6: ferrite-storage
- `Bitfield` — compact MSB-first bit-vector matching BEP 3 wire format
- `FileMap` — piece/chunk coordinate to file segment mapping with O(log n) binary search
- `ChunkTracker` — per-piece chunk completion state with `from_bitfield` resume support
- `TorrentStorage` trait — `&self` API (Arc-shared), wire-compatible `(piece, begin, length)` coordinates
- `MemoryStorage` — `RwLock<Vec<u8>>` backend for tests and magnet-link metadata
- `FilesystemStorage` — lazy file handles, sparse file creation, per-file locking for concurrency

### M5: ferrite-dht
- Kademlia DHT implementation (BEP 5)
- Actor model with `DhtHandle` — spawns event loop, returns cloneable async handle
- KRPC message encoding/decoding (ping, find_node, get_peers, announce_peer)
- Routing table with k-buckets (K=8), bucket splitting, stale node detection
- Compact node info (26-byte encode/decode)
- Peer store with SHA1-based token generation/validation and rotating secrets

### M4: ferrite-tracker
- HTTP tracker client (reqwest-based, BEP 3)
- UDP tracker client (BEP 15 connect/announce protocol)
- Compact peer list parsing

### M3: ferrite-wire
- Peer handshake with BEP 5/10 extension flags
- Full BEP 3 message set (choke, unchoke, interested, request, piece, cancel, etc.)
- Extension handshake (BEP 10) and metadata exchange (BEP 9)
- Tokio-util codec for framed message I/O

### M2: ferrite-core
- `Id20`/`Id32` hash types with hex, base32, and XOR distance
- `.torrent` file parsing (`TorrentMetaV1`) with info-hash computation
- Magnet link parsing (BEP 9)
- `Lengths` — piece and chunk arithmetic
- `PeerId` generation (`-FE0100-` prefix)

### M1: ferrite-bencode
- Serde-compatible bencode serializer and deserializer
- `BencodeValue` type for dynamic bencode manipulation
- Zero-copy deserialization
- `find_dict_key_span` for raw byte-range extraction (used for info-hash)
- Sorted key serialization (BEP 3 requirement)
