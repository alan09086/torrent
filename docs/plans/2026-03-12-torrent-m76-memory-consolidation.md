# M76: Memory Consolidation & Startup Speed

## Problem

Benchmarking against rqbit reveals torrent uses 2.5x more RSS (98 MB vs 40 MB) and suffers 4.3x more L3 cache misses (9.1M vs 2.1M). The larger working set causes constant cache thrashing, burning 1.9x more CPU cycles. Wall time is also 2.3x longer (45s vs 20s) despite matching rqbit's download speed, indicating significant startup/ramp-up overhead.

**Root causes identified:**
1. Duplicate data structures: bitfields and availability tracked in both `PieceReservationState` and `PieceSelector`/`PeerState`
2. Dead `in_flight_pieces` HashMap and `InFlightPiece` type — never populated since M75, checked in 16 locations
3. Slow peer discovery ramp-up — 5s connect interval delays initial peer connections

## Benchmark Baseline (M75, v0.77.0)

| Metric | torrent | rqbit | Ratio |
|--------|---------|-------|:-----:|
| Speed | 71.5 MB/s | ~77 MB/s | 0.93x |
| Wall time | 45.7s | 19.9s | 2.3x |
| CPU time | 19.3s | 10.0s | 1.9x |
| RSS | 98 MB | 40 MB | 2.5x |
| Cache misses | 9.1M | 2.1M | 4.3x |
| Context switches | 412K | 112K | 3.7x |

## Changes

### Part 1: Remove Duplicate Bitfields from PieceReservationState

**Current state:** `PieceReservationState` maintains `peer_bitfields: FxHashMap<SocketAddr, Bitfield>` — an exact copy of `peers[addr].bitfield` in the actor. Both are updated in lockstep. For 128 peers with 2904 pieces (Arch ISO), each Bitfield is ~363 bytes, totalling ~46 KB duplicated.

**Change:** Remove `peer_bitfields` from `PieceReservationState`. The peer task's `acquire_and_reserve()` calls `next_request()` which calls `can_reserve()`. `can_reserve()` checks `peer_has` — but this is already checked by the piece selector when the piece was reserved. The reservation state's `piece_owner` already ensures exclusive ownership. If a peer doesn't have a piece, it was never assigned to that peer.

**Files:**
- `crates/torrent-session/src/piece_reservation.rs` — Remove `peer_bitfields` field, remove `add_peer()` bitfield parameter, remove `peer_have()` bitfield update, adjust `can_reserve()` to not check peer bitfield (the piece was selected knowing the peer has it)
- `crates/torrent-session/src/torrent.rs` — Remove all `peer_bitfields` synchronization calls (add_peer bitfield clone, peer_have bitfield update)

### Part 2: Remove Duplicate Availability from PieceReservationState

**Current state:** `PieceReservationState` maintains `availability: Vec<u32>` — an exact copy of `piece_selector.availability`. Both track per-piece peer counts. For 2904 pieces: 11.6 KB duplicated.

**Change:** Remove `availability` from `PieceReservationState`. The piece selector is the canonical owner. Reservation state doesn't need availability counts — it uses `piece_owner` for exclusive ownership and `can_reserve()` for eligibility checks. The rarest-first logic lives in `piece_selector.pick_blocks()`, not in the reservation state.

**Files:**
- `crates/torrent-session/src/piece_reservation.rs` — Remove `availability` field and all update methods
- `crates/torrent-session/src/torrent.rs` — Remove availability sync calls to reservation state

### Part 3: Remove Dead in_flight_pieces Infrastructure

**Current state:** `in_flight_pieces: FxHashMap<u32, InFlightPiece>` is a field on `TorrentActor` that is never populated (no `insert` or `entry` calls exist). It is checked in 16 locations, all of which are no-ops (get_mut returns None, len() returns 0, is_empty() returns true, iterations are empty).

**Change:** Remove the field entirely, along with all 16 reference sites. Remove `InFlightPiece` from production code (keep struct definition and tests in `piece_selector.rs` since tests use it). Remove `activate_with_inflight` from `end_game.rs` (already marked `#[allow(dead_code)]`). Remove the `InFlightPiece` import from `torrent.rs`.

**Files:**
- `crates/torrent-session/src/torrent.rs` — Remove `in_flight_pieces` field, remove all 16 reference sites, remove `build_chunk_mask` helper, remove `InFlightPiece` import
- `crates/torrent-session/src/end_game.rs` — Remove `activate_with_inflight` method, remove `InFlightPiece` import
- `crates/torrent-session/src/piece_selector.rs` — Keep `InFlightPiece` struct (used in `PickContext` and tests) but gate production usage

### Part 4: Remove PickContext.in_flight_pieces Dependency

**Current state:** `PickContext` has an `in_flight_pieces` field used by `pick_blocks()` to skip pieces that are already in flight. Since `in_flight_pieces` is always empty, this check is always a no-op.

**Change:** Replace the `in_flight_pieces` check in `pick_blocks()` with a check against `reservation_state.piece_owner` (the canonical in-flight tracking). Since `pick_blocks()` runs in the actor context, it can read the reservation state. Alternatively, pass a simple `HashSet<u32>` of in-flight piece indices extracted from `piece_owner`.

**Files:**
- `crates/torrent-session/src/piece_selector.rs` — Change `PickContext.in_flight_pieces` from `&FxHashMap<u32, InFlightPiece>` to `&HashSet<u32>` (or a closure/bitfield)
- `crates/torrent-session/src/torrent.rs` — Build the in-flight set from `reservation_state.read().piece_owner` keys when constructing `PickContext`

### Part 5: Startup Speed — Aggressive Initial Peer Connection

**Current state:** `connect_interval` is 5s. After adding a magnet, the flow is: DHT bootstrap → tracker announce → peers arrive → wait for 5s tick → connect. This means peers sit in `available_peers` for up to 5s before the first connection attempt.

**Change:** Add an "initial burst" mode: for the first 10s after torrent start, reduce the connect interval to 500ms. After 10s, return to 5s. This ensures peers are connected within 500ms of discovery instead of up to 5s.

**Implementation:** Add a `started_at: Instant` field to `TorrentActor`. In the connect tick arm, check `if started_at.elapsed() < Duration::from_secs(10)` and use 500ms interval instead of 5s.

**Files:**
- `crates/torrent-session/src/torrent.rs` — Add `started_at` field, modify connect interval logic

### Part 6: Startup Speed — Immediate Peer Connection on Discovery

**Current state:** When peers arrive from DHT or tracker, they're added to `available_peers` and wait for the next connect tick.

**Change:** After `handle_add_peers()` adds new peers, immediately call `try_connect_peers()` if there are available connection slots. This eliminates the tick wait entirely for newly discovered peers.

**Files:**
- `crates/torrent-session/src/torrent.rs` — Add `try_connect_peers()` call after `handle_add_peers()` in DHT/tracker event handlers

## Task Breakdown

### Task 1: Remove peer_bitfields from PieceReservationState
- Remove field, adjust methods, remove sync calls in torrent.rs
- Update tests that reference peer_bitfields

### Task 2: Remove availability from PieceReservationState
- Remove field and methods, remove sync calls
- Update tests

### Task 3: Remove in_flight_pieces and InFlightPiece from production code
- Remove field from TorrentActor, remove all 16 reference sites
- Remove activate_with_inflight, remove dead imports
- Replace PickContext.in_flight_pieces with reservation-state-based check

### Task 4: Aggressive initial peer connection
- Add started_at field, burst-mode connect interval
- Add immediate try_connect_peers() after peer discovery

### Task 5: Verification
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`
- 3-trial Arch ISO benchmark comparing RSS, cache misses, wall time
- Version bump to 0.78.0

## Success Criteria

- RSS < 75 MB (down from 98 MB)
- Cache misses reduced (target < 6M, down from 9.1M)
- Wall time < 35s (down from 45.7s)
- All tests pass, zero clippy warnings
- No speed regression (maintain > 50 MB/s average)
