# M76: Memory Consolidation & Startup Speed

## Problem

Benchmarking against rqbit reveals torrent uses 2.5x more RSS (98 MB vs 40 MB) and suffers 4.3x more L3 cache misses (9.1M vs 2.1M). The larger working set causes constant cache thrashing, burning 1.9x more CPU cycles. Wall time is also 2.3x longer (45s vs 20s) despite matching rqbit's download speed, indicating significant startup/ramp-up overhead.

**Root causes identified:**
1. `PieceSelector` (2,271 lines) is mostly dead code — `pick_blocks()` is never called post-M75. The struct and all `pick_*` methods are dead. Only used as an availability counter (7 method call sites) plus 2 free functions (`build_wanted_pieces`, `evaluate_auto_sequential`) that are still live. Its `availability: Vec<u32>` duplicates `PieceReservationState.availability`.
2. `in_flight_pieces: FxHashMap<u32, InFlightPiece>` is never populated — checked in 20+ locations, all no-ops. Also causes `evaluate_auto_sequential` to always see 0 in-flight, preventing auto-sequential activation.
3. `PickContext`, `InFlightPiece`, and all `pick_*` methods in `PieceSelector` are dead code — all piece selection now happens via `PieceReservationState::next_request()`.
4. `peer_bitfields` in `PieceReservationState` duplicates `peers[addr].bitfield` in the actor (~46 KB for 128 peers). Both copies are needed (actor for choking/end-game, reservation state for `next_request()`), but the reservation copy could be replaced by a parameter.
5. Slow initial peer connection ramp-up — 5s connect interval delays first connections after discovery.

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

### Part 1: Remove PieceSelector (Dead Code)

**Current state:** `PieceSelector` is a 2,271-line module containing `pick_blocks()`, `PickContext`, `pick_partial()`, `pick_rarest_new()`, `pick_sequential()`, `pick_random()`, `pick_reverse_rarest()`, `preferred_extent()`, and `InFlightPiece`. Post-M75, `pick_blocks()` is never called — all piece selection happens in `PieceReservationState::next_request()`.

`PieceSelector` method calls in `torrent.rs` (7 sites):
- `availability()` — read for stats (lines 1898, 5841)
- `add_peer_bitfield(&bitfield)` — update availability on peer connect (line 3254)
- `increment(index)` — update availability on Have message (line 3275)
- `remove_peer_bitfield(&peer.bitfield)` — update availability on disconnect (lines 3455, 5514, 5654)

Free functions still called from production code (3 sites):
- `crate::piece_selector::build_wanted_pieces(...)` — lines 187, 4001, 4828
- `crate::piece_selector::evaluate_auto_sequential(...)` — line 1987

Additional `PieceSelector` construction:
- `PieceSelector::new(num_pieces)` — line 4811 (metadata arrival re-initialization)

`PieceReservationState` already has its own `availability: Vec<u32>` with equivalent `add_peer()`, `peer_have()`, `remove_peer()` methods that update it. This is a pure duplicate.

**Change:** Remove `PieceSelector` struct and all its methods from production use. Replace the 7 method call sites in `torrent.rs` with reads from `reservation_state.read().availability()` for stats (add `pub fn availability(&self) -> &[u32]` accessor to `PieceReservationState`). Remove the `PieceSelector::new()` construction at line 4811. The availability updates already happen via the reservation state methods (`add_peer`, `peer_have`, `remove_peer`).

**Keep free functions live:** `build_wanted_pieces()` and `evaluate_auto_sequential()` are NOT methods on `PieceSelector` — they are standalone functions that must remain in production. Gate only the `PieceSelector` struct, its impl blocks, `PickContext`, `PickResult`, `PeerSpeed`, `PeerSpeedClassifier`, and related types behind `#[cfg(test)]`. The free functions and their imports stay ungated.

**Fix `evaluate_auto_sequential`:** Currently uses `self.in_flight_pieces.len()` (always 0) as the `in_flight_count` argument, so auto-sequential can never activate. After removing `in_flight_pieces`, replace with `reservation_state.read().in_flight_count()`.

**Handle `reservation_state` being `None`:** The `reservation_state` field is `Option<Arc<...>>` (`None` before metadata arrives). For the 2 availability read sites (lines 1898, 5841), return an empty slice when `None`. The `PieceSelector` was always initialized (even with 0 pieces), so this changes behavior only cosmetically — stats return empty before metadata.

**Files:**
- `crates/torrent-session/src/torrent.rs` — Remove `piece_selector` field, remove import, replace 7 method call sites + 1 construction, fix `evaluate_auto_sequential` argument
- `crates/torrent-session/src/piece_selector.rs` — Gate `PieceSelector` struct and related types behind `#[cfg(test)]`; keep free functions live
- `crates/torrent-session/src/piece_reservation.rs` — Add `pub fn availability(&self) -> &[u32]` accessor

### Part 2: Remove Dead in_flight_pieces Infrastructure

**Current state:** `in_flight_pieces: FxHashMap<u32, InFlightPiece>` is a field on `TorrentActor` that is never populated (zero `insert` or `entry` calls). It is referenced in 20+ locations in `torrent.rs`, all of which are no-ops (get_mut returns None, len() returns 0, is_empty() returns true, iterations are empty). Additionally, `activate_with_inflight` in `end_game.rs` is already `#[allow(dead_code)]`.

**Change:** Remove the field and all 20+ reference sites. Remove `activate_with_inflight` from `end_game.rs`. Remove `InFlightPiece` import from `torrent.rs`. Remove `build_chunk_mask` helper (marked `#[allow(dead_code)]`).

Reference sites to remove (all in `torrent.rs`):
- Lines 332, 604: initialization
- Line 1304: field declaration
- Lines 1988, 2251: stats/logging reads
- Lines 2199, 2657: iteration over empty map
- Lines 3468, 3473: disconnect cleanup
- Lines 3594, 3662: handle_piece_data lookups
- Lines 4281, 4455: piece verified/failed cleanup
- Line 5105: piece selection filter
- Lines 5512, 5517: disconnect cleanup
- Lines 5652, 5656: disconnect cleanup

**Files:**
- `crates/torrent-session/src/torrent.rs` — Remove field and all reference sites
- `crates/torrent-session/src/end_game.rs` — Remove `activate_with_inflight`, remove `InFlightPiece` import

### Part 3: Remove Duplicate peer_bitfields from PieceReservationState

**Current state:** `PieceReservationState` maintains `peer_bitfields: FxHashMap<SocketAddr, Bitfield>` — a copy of `peers[addr].bitfield` in the actor, updated in lockstep. For 128 peers × 363 bytes each = ~46 KB duplicated. The reservation state uses this in `next_request()` → `can_reserve()` to check `peer_has.get(piece)`.

**Why both copies exist:** The actor needs bitfields for choking, super-seeding, Have broadcasts, end-game block selection. The reservation state needs them so peer tasks (which don't have access to actor state) can check piece availability during `next_request()`.

**Change:** Have the peer task pass its own bitfield as a parameter to `next_request()` instead of the reservation state storing copies. The peer task already receives Bitfield and Have wire messages — it can maintain a local `Bitfield` and pass a reference through the `Arc<RwLock<_>>` API.

This requires:
1. Adding a `bitfield: Bitfield` field to the peer task's local state (in `peer.rs`)
2. Updating the peer task to maintain it from ALL bitfield-setting wire messages:
   - `Message::Bitfield(bf)` — full bitfield from peer (line ~635 of `peer.rs`)
   - `Message::Have { index }` — single piece announcement (line ~645)
   - `Message::HaveAll` — BEP 6 fast extension, sets all bits (line ~652)
   - `Message::HaveNone` — BEP 6 fast extension, clears all bits (line ~667)
   - Deferred bitfield replay (lines 245-415) — when `num_pieces` is 0 at receipt time, bitfield/HaveAll/HaveNone are deferred and replayed after metadata arrives. The local bitfield must be updated during replay, not just at initial receipt.
3. Changing `next_request(addr)` to `next_request(addr, &Bitfield)`
4. Removing `peer_bitfields` from `PieceReservationState`
5. Removing `add_peer()` bitfield parameter and `peer_have()` bitfield update

**Note:** This moves the bitfield copy from the shared RwLock into each peer task's stack, which is better for cache locality (each peer task's bitfield stays hot in its own cache lines instead of competing in the shared HashMap).

**Accepted race condition:** With the parameter-passing approach, a small consistency window exists: the peer task may update its local bitfield (e.g., on `Message::Have`) before the actor processes the corresponding `PeerEvent::Have` and increments `availability`. This means `next_request()` could see a piece in the bitfield that doesn't yet appear in availability counts. This is benign — the peer genuinely has the piece, so requesting it is correct. The availability count catches up on the next actor event processing cycle.

**Files:**
- `crates/torrent-session/src/peer.rs` — Add local bitfield, pass to next_request
- `crates/torrent-session/src/piece_reservation.rs` — Remove `peer_bitfields`, change method signatures
- `crates/torrent-session/src/torrent.rs` — Update add_peer/peer_have calls

### Part 4: Startup Speed — Burst-Mode Connection Interval

**Current state:** `connect_interval` is 5s. DHT/tracker peers are already connected immediately via `try_connect_peers()` after `handle_add_peers()` (11 call sites verified). However, the periodic connection tick still runs at 5s, which delays connections for peers that arrive between discovery batches or when slots become available.

**Change:** Add burst-mode: for the first 10s after torrent start, reduce the connect interval from 5s to 500ms. This ensures peer slots freed by failed handshakes are refilled quickly during ramp-up.

**Files:**
- `crates/torrent-session/src/torrent.rs` — Add `started_at: Instant` field, conditional interval

## Task Breakdown

### Task 1: Remove PieceSelector from production code
- Add `pub fn availability(&self) -> &[u32]` accessor to `PieceReservationState`
- Remove `piece_selector` field from TorrentActor
- Replace 7 method call sites with reservation_state availability reads (handle `None` case)
- Remove `PieceSelector::new()` construction at line 4811
- Fix `evaluate_auto_sequential` to use `reservation_state.read().in_flight_count()` instead of dead `in_flight_pieces.len()`
- Gate `PieceSelector` struct, impl blocks, `PickContext`, `PickResult`, `PeerSpeed`, `PeerSpeedClassifier` behind `#[cfg(test)]`
- Keep `build_wanted_pieces()` and `evaluate_auto_sequential()` free functions live

### Task 2: Remove in_flight_pieces and related dead code
- Remove field from TorrentActor and all 20+ reference sites
- Remove `activate_with_inflight` from end_game.rs
- Remove `build_chunk_mask` helper
- Remove `InFlightPiece` import

### Task 3: Move peer bitfields to peer task local state
- Add local `bitfield: Bitfield` to peer task in peer.rs
- Update from all wire message paths: Bitfield, Have, HaveAll, HaveNone, deferred replay
- Change `next_request(addr)` → `next_request(addr, &Bitfield)`
- Remove `peer_bitfields` from PieceReservationState
- Remove `add_peer()` bitfield parameter and `peer_have()` bitfield update
- Update all call sites in torrent.rs

### Task 4: Burst-mode connection interval
- Add started_at field, 500ms connect interval for first 10s

### Task 5: Verification
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`
- 3-trial Arch ISO benchmark with `perf stat` comparing RSS, cache misses, wall time
- Version bump to 0.78.0

## Success Criteria

- RSS < 80 MB (down from 98 MB)
- Cache misses reduced (target < 7M, down from 9.1M)
- Wall time < 40s (down from 45.7s)
- All tests pass, zero clippy warnings
- No speed regression (maintain > 50 MB/s average)
