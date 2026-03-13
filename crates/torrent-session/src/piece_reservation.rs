//! Concurrent piece reservation state for per-peer dispatch.
//!
//! Each peer exclusively owns pieces it is downloading. Block dispatch within
//! a reserved piece is sequential (block 0, 1, 2, ...). When a peer disconnects
//! mid-piece, the piece is released and another peer can re-request all blocks.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tokio::sync::Notify;
use torrent_core::Lengths;
use torrent_storage::Bitfield;

/// A block request to send to a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockRequest {
    /// Piece index.
    pub piece: u32,
    /// Byte offset within the piece.
    pub begin: u32,
    /// Length in bytes.
    pub length: u32,
}

/// Tracks which block a peer should request next within its reserved piece.
#[derive(Debug)]
struct PeerProgress {
    /// The piece this peer is downloading.
    piece: u32,
    /// Next block index to request (0-based).
    next_block: u32,
    /// Total blocks in this piece.
    total_blocks: u32,
}

/// Shared state for concurrent per-peer piece dispatch.
///
/// Lives behind `Arc<parking_lot::RwLock<_>>`. Request drivers call
/// [`next_request`] under a write lock; the `TorrentActor` calls mutation
/// methods during event processing.
#[derive(Debug)]
pub(crate) struct PieceReservationState {
    /// Pieces we have verified and no longer need.
    we_have: Bitfield,
    /// Pieces we want to download (file priority filter).
    wanted: Bitfield,
    /// Maps piece index to the peer that owns it exclusively.
    piece_owner: FxHashMap<u32, SocketAddr>,
    /// Maps peer to the list of pieces it currently owns.
    peer_pieces: FxHashMap<SocketAddr, Vec<u32>>,
    /// Block-level progress for each peer's current piece.
    current_progress: FxHashMap<SocketAddr, PeerProgress>,
    /// Per-piece availability count (how many peers have each piece).
    availability: Vec<u32>,
    /// High-priority pieces (streaming, time-critical).
    priority_pieces: BTreeSet<u32>,
    /// Notify waiters when new pieces become available.
    piece_notify: Arc<Notify>,
    /// Piece/chunk arithmetic.
    lengths: Lengths,
    /// Total number of pieces in the torrent.
    num_pieces: u32,
    /// Maximum number of pieces in flight across all peers.
    max_in_flight: usize,
    /// Whether end-game mode is active (disables new reservations).
    endgame_active: bool,
    /// Cached candidates sorted by descending availability (rarest at end).
    sorted_candidates: Vec<u32>,
    /// Whether the cached candidates need rebuilding.
    candidates_dirty: bool,
}

#[allow(dead_code)] // Some methods reserved for M74+ (streaming, priority, wanted updates)
impl PieceReservationState {
    /// Create a new reservation state.
    ///
    /// Returns the state and a shared `Notify` that is triggered whenever
    /// new pieces become available for reservation.
    pub fn new(
        num_pieces: u32,
        lengths: Lengths,
        max_in_flight: usize,
        we_have: Bitfield,
        wanted: Bitfield,
    ) -> (Self, Arc<Notify>) {
        let notify = Arc::new(Notify::new());
        let state = Self {
            we_have,
            wanted,
            piece_owner: FxHashMap::default(),
            peer_pieces: FxHashMap::default(),
            current_progress: FxHashMap::default(),
            availability: vec![0; num_pieces as usize],
            priority_pieces: BTreeSet::new(),
            piece_notify: Arc::clone(&notify),
            lengths,
            num_pieces,
            max_in_flight,
            endgame_active: false,
            sorted_candidates: Vec::new(),
            candidates_dirty: true,
        };
        (state, notify)
    }

    /// Read-only access to per-piece availability counts.
    pub fn availability(&self) -> &[u32] {
        &self.availability
    }

    /// Recalculate `max_in_flight` based on the current number of connected peers.
    ///
    /// Formula: `max(256, connected_peers * 3)` capped at `num_pieces / 2`,
    /// with a floor of 256.
    fn recalc_max_in_flight(&mut self, connected_peers: usize) {
        let calculated = 256usize.max(connected_peers * 3);
        self.max_in_flight = calculated.min(self.num_pieces as usize / 2).max(256);
    }

    // ---- Peer lifecycle ----

    /// Register a peer and update piece availability.
    pub fn add_peer(&mut self, addr: SocketAddr, bitfield: Bitfield) {
        // Update availability counts for all pieces this peer has.
        for piece in 0..self.num_pieces {
            if bitfield.get(piece) {
                self.availability[piece as usize] += 1;
            }
        }
        self.peer_pieces.entry(addr).or_default();
        self.recalc_max_in_flight(self.peer_pieces.len());
        self.candidates_dirty = true;
        // M77: Removed immediate notify — safety-net tick in pipeline_tick handles this
        // to avoid waking all 128 peer tasks on every new peer connection.
    }

    /// Remove a peer entirely: release its pieces and update availability.
    pub fn remove_peer(&mut self, addr: SocketAddr, bitfield: &Bitfield) {
        // Release all pieces owned by this peer.
        if let Some(pieces) = self.peer_pieces.remove(&addr) {
            for piece in pieces {
                self.piece_owner.remove(&piece);
            }
        }
        self.current_progress.remove(&addr);

        // Update availability for all pieces this peer had.
        for piece in 0..self.num_pieces {
            if bitfield.get(piece) {
                self.availability[piece as usize] =
                    self.availability[piece as usize].saturating_sub(1);
            }
        }

        self.recalc_max_in_flight(self.peer_pieces.len());
        self.candidates_dirty = true;
        self.piece_notify.notify_waiters();
    }

    /// Update availability when a peer announces a new piece (HAVE message).
    ///
    /// Dedup is handled by the caller (actor checks peer.bitfield before calling).
    pub fn peer_have(&mut self, piece: u32) {
        if piece >= self.num_pieces {
            return;
        }
        self.availability[piece as usize] += 1;
        self.candidates_dirty = true;
        // M77: Removed immediate notify — safety-net tick in pipeline_tick handles this
        // to avoid waking all 128 peer tasks on every HAVE message.
    }

    /// Release all pieces owned by a peer without removing its peer_pieces tracking.
    ///
    /// Used when a peer chokes us: we lose the in-progress pieces but keep
    /// tracking what pieces the peer has.
    pub fn release_peer_pieces(&mut self, addr: SocketAddr) {
        if let Some(pieces) = self.peer_pieces.get_mut(&addr) {
            for piece in pieces.drain(..) {
                self.piece_owner.remove(&piece);
            }
        }
        self.current_progress.remove(&addr);
        self.piece_notify.notify_waiters();
    }

    // ---- Piece lifecycle ----

    /// Mark a piece as verified. Sets `we_have`, removes the owner, and
    /// notifies waiters.
    pub fn complete_piece(&mut self, piece: u32) {
        self.we_have.set(piece);
        if let Some(owner) = self.piece_owner.remove(&piece) {
            if let Some(pieces) = self.peer_pieces.get_mut(&owner) {
                pieces.retain(|&p| p != piece);
            }
            // If this was the peer's current progress piece, remove it.
            if self
                .current_progress
                .get(&owner)
                .is_some_and(|p| p.piece == piece)
            {
                self.current_progress.remove(&owner);
            }
        }
        self.candidates_dirty = true;
        self.piece_notify.notify_one();
    }

    /// Mark a piece as failed (hash check failed). Removes the owner without
    /// setting `we_have`, so it can be re-reserved by another peer.
    pub fn fail_piece(&mut self, piece: u32) {
        if let Some(owner) = self.piece_owner.remove(&piece) {
            if let Some(pieces) = self.peer_pieces.get_mut(&owner) {
                pieces.retain(|&p| p != piece);
            }
            if self
                .current_progress
                .get(&owner)
                .is_some_and(|p| p.piece == piece)
            {
                self.current_progress.remove(&owner);
            }
        }
        self.candidates_dirty = true;
        self.piece_notify.notify_one();
    }

    // ---- State updates ----

    /// Bulk-update the `we_have` bitfield (e.g., after fast-resume verification).
    pub fn update_we_have(&mut self, we_have: &Bitfield) {
        self.we_have = we_have.clone();
        self.candidates_dirty = true;
    }

    /// Bulk-update the `wanted` bitfield (e.g., after file priority change).
    pub fn update_wanted(&mut self, wanted: &Bitfield) {
        self.wanted = wanted.clone();
        self.candidates_dirty = true;
        self.piece_notify.notify_waiters();
    }

    /// Set the priority pieces (streaming, time-critical).
    pub fn set_priority_pieces(&mut self, pieces: BTreeSet<u32>) {
        self.priority_pieces = pieces;
        self.candidates_dirty = true;
        self.piece_notify.notify_waiters();
    }

    /// Enable or disable end-game mode.
    ///
    /// When deactivating (active=false), notifies waiters so peer tasks
    /// blocked in `acquire_and_reserve` wake up and resume requesting.
    pub fn set_endgame(&mut self, active: bool) {
        self.endgame_active = active;
        self.candidates_dirty = true;
        if !active {
            self.piece_notify.notify_waiters();
        }
    }

    /// Whether end-game mode is active.
    pub fn is_endgame(&self) -> bool {
        self.endgame_active
    }

    /// Get a reference to the piece notification handle.
    pub fn piece_notify(&self) -> &Arc<Notify> {
        &self.piece_notify
    }

    /// Number of pieces currently in flight (reserved by peers).
    pub fn in_flight_count(&self) -> usize {
        self.piece_owner.len()
    }

    /// Check if a specific piece is currently in flight (reserved by a peer).
    pub fn is_piece_in_flight(&self, piece: u32) -> bool {
        self.piece_owner.contains_key(&piece)
    }

    /// Iterate over in-flight pieces and their owning peers.
    /// Used by the actor to build end-game block maps.
    pub fn in_flight_peers(&self) -> impl Iterator<Item = (u32, SocketAddr)> + '_ {
        self.piece_owner.iter().map(|(&piece, &addr)| (piece, addr))
    }

    // ---- Core dispatch ----

    /// Get the next block request for a peer.
    ///
    /// Priority order:
    /// 1. **Current piece** — peer already has a reserved piece with remaining blocks.
    /// 2. **Priority pieces** — streaming/time-critical.
    /// 3. **Rarest-first** — unreserved piece with lowest availability.
    ///
    /// Returns `None` when end-game is active, or no suitable piece is available.
    pub fn next_request(&mut self, peer_addr: SocketAddr, peer_has: &Bitfield) -> Option<BlockRequest> {
        if self.endgame_active {
            return None;
        }

        // 1. Current piece — O(1) lookup.
        if let Some(progress) = self.current_progress.get(&peer_addr) {
            if progress.next_block < progress.total_blocks {
                let piece = progress.piece;
                let block_idx = progress.next_block;
                let request = self.make_block_request(piece, block_idx);
                // Advance the block counter.
                self.current_progress.get_mut(&peer_addr).unwrap().next_block += 1;
                return Some(request);
            }
            // Current piece fully dispatched — remove progress so we can reserve a new one.
            self.current_progress.remove(&peer_addr);
        }

        // Check in-flight cap before trying to reserve a new piece.
        if self.piece_owner.len() >= self.max_in_flight {
            return None;
        }

        // 2. Priority pieces — O(priority_count).
        for &piece in &self.priority_pieces {
            if self.can_reserve(piece, peer_has) {
                return self.reserve_and_start(peer_addr, piece);
            }
        }

        // 3. Rarest-first from cached candidates.
        if self.candidates_dirty {
            self.rebuild_candidates();
        }
        for &piece in self.sorted_candidates.iter().rev() {
            if self.can_reserve(piece, peer_has) {
                return self.reserve_and_start(peer_addr, piece);
            }
        }

        None
    }

    // ---- Two-phase dispatch methods ----

    /// Phase 1 (WRITE lock, O(1)): Dispatch the next block from the peer's
    /// currently reserved piece, if any remains.
    ///
    /// Removes `current_progress` when the piece is fully dispatched so the
    /// caller can immediately proceed to [`find_candidate`] + [`try_reserve`].
    pub fn try_current_piece(&mut self, peer_addr: SocketAddr) -> Option<BlockRequest> {
        if let Some(progress) = self.current_progress.get(&peer_addr) {
            if progress.next_block < progress.total_blocks {
                let piece = progress.piece;
                let block_idx = progress.next_block;
                let request = self.make_block_request(piece, block_idx);
                self.current_progress.get_mut(&peer_addr).unwrap().next_block += 1;
                return Some(request);
            }
            // Piece fully dispatched — clear so the caller can reserve a new one.
            self.current_progress.remove(&peer_addr);
        }
        None
    }

    /// Phase 2 (READ lock, O(n)): Find the best unreserved piece this peer can download.
    ///
    /// Returns `None` when end-game is active, the in-flight cap is reached, or
    /// no suitable piece is available.  Returns a piece index (not a
    /// `BlockRequest`) so the caller can upgrade to a WRITE lock and call
    /// [`try_reserve`] only when a candidate actually exists.
    pub fn find_candidate(&self, peer_has: &Bitfield) -> Option<u32> {
        if self.endgame_active {
            return None;
        }
        if self.piece_owner.len() >= self.max_in_flight {
            return None;
        }

        // Priority pieces first.
        for &piece in &self.priority_pieces {
            if self.can_reserve(piece, peer_has) {
                return Some(piece);
            }
        }

        // Rarest-first from cached candidates (rarest at end, iterate in reverse).
        // Cache may be stale (READ lock), but can_reserve() filters out invalid entries.
        // If cache was never built (dirty), fall back to O(n) scan for correctness.
        if !self.candidates_dirty {
            for &piece in self.sorted_candidates.iter().rev() {
                if self.can_reserve(piece, peer_has) {
                    return Some(piece);
                }
            }
            return None;
        }

        // Fallback: O(n) scan when cache is dirty (READ lock can't rebuild).
        let mut best_piece = None;
        let mut best_avail = u32::MAX;
        for piece in 0..self.num_pieces {
            if self.can_reserve(piece, peer_has) {
                let avail = self.availability[piece as usize];
                if avail < best_avail {
                    best_avail = avail;
                    best_piece = Some(piece);
                }
            }
        }
        best_piece
    }

    /// Phase 3 (WRITE lock, O(1)): Claim a piece found by [`find_candidate`].
    ///
    /// Re-checks all invariants under the write lock (race guard): if the piece
    /// was taken by another peer between the READ and WRITE lock acquisitions,
    /// or the in-flight cap is now exceeded, returns `None`.
    pub fn try_reserve(
        &mut self,
        peer_addr: SocketAddr,
        piece: u32,
        peer_has: &Bitfield,
    ) -> Option<BlockRequest> {
        // Rebuild candidate cache if invalidated (under WRITE lock, safe to mutate).
        if self.candidates_dirty {
            self.rebuild_candidates();
        }
        // Re-check cap under write lock (another peer may have reserved a piece
        // between find_candidate and try_reserve).
        if self.piece_owner.len() >= self.max_in_flight {
            return None;
        }
        // Re-check reservability (another peer may have claimed this exact piece).
        if !self.can_reserve(piece, peer_has) {
            return None;
        }
        self.reserve_and_start(peer_addr, piece)
    }

    // ---- Private helpers ----

    /// Rebuild the sorted candidates cache.
    ///
    /// Collects all pieces that are wanted, not yet downloaded, and not currently
    /// reserved. Sorts by **descending** availability so the rarest pieces are
    /// at the end of the `Vec` — iteration with `.iter().rev()` yields rarest-first.
    fn rebuild_candidates(&mut self) {
        self.sorted_candidates.clear();
        for piece in 0..self.num_pieces {
            if self.wanted.get(piece)
                && !self.we_have.get(piece)
                && !self.piece_owner.contains_key(&piece)
            {
                self.sorted_candidates.push(piece);
            }
        }
        // Sort by descending availability (rarest at end for rev() iteration).
        let availability = &self.availability;
        self.sorted_candidates
            .sort_unstable_by(|&a, &b| availability[b as usize].cmp(&availability[a as usize]));
        self.candidates_dirty = false;
    }

    /// Check if a piece can be reserved by a peer.
    fn can_reserve(&self, piece: u32, peer_has: &Bitfield) -> bool {
        peer_has.get(piece)
            && self.wanted.get(piece)
            && !self.we_have.get(piece)
            && !self.piece_owner.contains_key(&piece)
    }

    /// Reserve a piece for a peer and return the first block request.
    fn reserve_and_start(&mut self, peer_addr: SocketAddr, piece: u32) -> Option<BlockRequest> {
        let total_blocks = self.lengths.chunks_in_piece(piece);
        if total_blocks == 0 {
            return None;
        }

        // Register ownership.
        self.piece_owner.insert(piece, peer_addr);
        self.peer_pieces.entry(peer_addr).or_default().push(piece);

        // Start at block 0, advance to 1.
        self.current_progress.insert(
            peer_addr,
            PeerProgress {
                piece,
                next_block: 1,
                total_blocks,
            },
        );

        Some(self.make_block_request(piece, 0))
    }

    /// Build a `BlockRequest` using `Lengths::chunk_info` for correct offsets.
    fn make_block_request(&self, piece: u32, block_idx: u32) -> BlockRequest {
        let (begin, length) = self
            .lengths
            .chunk_info(piece, block_idx)
            .expect("block_idx out of range for piece");
        BlockRequest {
            piece,
            begin,
            length,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test lengths: 128 KiB total, 32 KiB pieces, 16 KiB chunks.
    /// → 4 pieces, 2 blocks per piece.
    fn test_lengths() -> Lengths {
        Lengths::new(128 * 1024, 32768, 16384)
    }

    fn all_pieces_wanted(num_pieces: u32) -> Bitfield {
        let mut bf = Bitfield::new(num_pieces);
        for i in 0..num_pieces {
            bf.set(i);
        }
        bf
    }

    fn peer_has_all(num_pieces: u32) -> Bitfield {
        all_pieces_wanted(num_pieces)
    }

    fn addr(port: u16) -> SocketAddr {
        use std::net::{IpAddr, Ipv4Addr};
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn reserve_piece_exclusive() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer_a = addr(1000);
        let peer_b = addr(2000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer_a, bf.clone());
        state.add_peer(peer_b, bf.clone());

        // Peer A reserves a piece.
        let req_a = state.next_request(peer_a, &bf).unwrap();
        let piece_a = req_a.piece;

        // Peer B should get a DIFFERENT piece.
        let req_b = state.next_request(peer_b, &bf).unwrap();
        let piece_b = req_b.piece;

        assert_ne!(piece_a, piece_b, "two peers must not get the same piece");
    }

    #[test]
    fn sequential_blocks_within_piece() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        // First request: block 0 of some piece.
        let req0 = state.next_request(peer, &bf).unwrap();
        assert_eq!(req0.begin, 0);
        assert_eq!(req0.length, 16384);

        // Second request: block 1 of the same piece.
        let req1 = state.next_request(peer, &bf).unwrap();
        assert_eq!(req1.piece, req0.piece);
        assert_eq!(req1.begin, 16384);
        assert_eq!(req1.length, 16384);
    }

    #[test]
    fn complete_piece_frees_slot() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        // Reserve a piece (consume both blocks).
        let req0 = state.next_request(peer, &bf).unwrap();
        let piece = req0.piece;
        let _req1 = state.next_request(peer, &bf).unwrap();

        // Complete it.
        state.complete_piece(piece);

        // Verify we_have is set.
        assert!(
            state.we_have.get(piece),
            "completed piece should be in we_have"
        );

        // Verify the piece is no longer owned.
        assert!(
            !state.piece_owner.contains_key(&piece),
            "completed piece should not have an owner"
        );

        // Peer should be able to get a new piece.
        let next = state.next_request(peer, &bf);
        assert!(next.is_some(), "peer should get a new piece after completion");
    }

    #[test]
    fn fail_piece_makes_available() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        // Only allow 1 piece in flight so we can control reservation.
        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 1, we_have, wanted);

        let peer_a = addr(1000);
        let peer_b = addr(2000);

        // Peer B only has piece 0.
        let mut bf_b = Bitfield::new(num_pieces);
        bf_b.set(0);

        // Peer A only has piece 0 (to force it to reserve piece 0).
        let mut bf_a = Bitfield::new(num_pieces);
        bf_a.set(0);

        state.add_peer(peer_a, bf_a.clone());
        state.add_peer(peer_b, bf_b.clone());

        // Pass the restricted bitfield directly to next_request.
        let req_a = state.next_request(peer_a, &bf_a).unwrap();
        assert_eq!(req_a.piece, 0);

        // Peer B can't get anything (max_in_flight=1 and piece 0 is taken).
        assert!(state.next_request(peer_b, &bf_b).is_none());

        // Fail piece 0.
        state.fail_piece(0);

        // Now peer B should be able to reserve piece 0.
        let req_b = state.next_request(peer_b, &bf_b).unwrap();
        assert_eq!(req_b.piece, 0);
    }

    #[test]
    fn remove_peer_releases_pieces() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer_a = addr(1000);
        let peer_b = addr(2000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer_a, bf.clone());
        state.add_peer(peer_b, bf.clone());

        // Peer A reserves a piece.
        let req = state.next_request(peer_a, &bf).unwrap();
        let piece = req.piece;

        // Remove peer A.
        state.remove_peer(peer_a, &bf);

        // The piece should no longer be owned.
        assert!(!state.piece_owner.contains_key(&piece));

        // Peer B should be able to get that piece.
        // (It's rarest because availability is now 1 since peer_a was removed.)
        // We can verify by checking that peer B can reserve the piece.
        let mut got_piece = false;
        for _ in 0..num_pieces {
            if let Some(r) = state.next_request(peer_b, &bf) {
                if r.piece == piece {
                    got_piece = true;
                    break;
                }
                // Consume the second block to move to next piece.
                let _ = state.next_request(peer_b, &bf);
            }
        }
        assert!(got_piece, "peer B should be able to get the released piece");
    }

    #[test]
    fn rarest_first_selection() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        // Create 3 peers with different pieces to produce different availability:
        // Piece 0: 3 peers have it (availability = 3)
        // Piece 1: 2 peers have it (availability = 2)
        // Piece 2: 1 peer has it  (availability = 1) ← rarest
        // Piece 3: 3 peers have it (availability = 3)

        let peer1 = addr(1001);
        let peer2 = addr(1002);
        let peer3 = addr(1003);

        let mut bf1 = Bitfield::new(num_pieces);
        bf1.set(0);
        bf1.set(1);
        bf1.set(2);
        bf1.set(3);

        let mut bf2 = Bitfield::new(num_pieces);
        bf2.set(0);
        bf2.set(1);
        bf2.set(3);

        let mut bf3 = Bitfield::new(num_pieces);
        bf3.set(0);
        bf3.set(3);

        state.add_peer(peer1, bf1.clone());
        state.add_peer(peer2, bf2);
        state.add_peer(peer3, bf3);

        // Peer1 has all pieces. Should pick piece 2 (rarest, availability=1).
        let req = state.next_request(peer1, &bf1).unwrap();
        assert_eq!(
            req.piece, 2,
            "should pick piece 2 (rarest with availability=1)"
        );
    }

    #[test]
    fn priority_pieces_take_precedence() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        // Set piece 3 as high priority.
        let mut priority = BTreeSet::new();
        priority.insert(3);
        state.set_priority_pieces(priority);

        // Even though piece 0 might be rarest (equal availability), priority wins.
        let req = state.next_request(peer, &bf).unwrap();
        assert_eq!(
            req.piece, 3,
            "priority piece should be selected over rarest-first"
        );
    }

    #[test]
    fn endgame_returns_none() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        state.set_endgame(true);
        assert!(state.is_endgame());

        let req = state.next_request(peer, &bf);
        assert!(req.is_none(), "endgame should return None");
    }

    #[test]
    fn max_in_flight_cap() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        // Allow only 2 pieces in flight.
        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 2, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());
        // Override adaptive cap for this unit test (adaptive min is 256).
        state.max_in_flight = 2;

        // Reserve piece 1: get both blocks.
        let r0 = state.next_request(peer, &bf).unwrap();
        let _r1 = state.next_request(peer, &bf).unwrap();
        let piece1 = r0.piece;

        // Reserve piece 2: get first block.
        let r2 = state.next_request(peer, &bf).unwrap();
        assert_ne!(r2.piece, piece1, "should get a different piece");
        let _r3 = state.next_request(peer, &bf).unwrap();

        // Now 2 pieces are in flight → next_request should return None
        // (current piece is fully dispatched, and we can't reserve new ones).
        let req = state.next_request(peer, &bf);
        assert!(req.is_none(), "should be capped at max_in_flight=2");
    }

    // ---- Two-phase dispatch tests ----

    #[test]
    fn try_current_piece_returns_next_block() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        // Reserve piece via next_request (gets block 0).
        let req0 = state.next_request(peer, &bf).unwrap();
        assert_eq!(req0.begin, 0);

        // try_current_piece should return block 1 of the same piece.
        let req1 = state.try_current_piece(peer).unwrap();
        assert_eq!(req1.piece, req0.piece);
        assert_eq!(req1.begin, 16384);
    }

    #[test]
    fn try_current_piece_clears_finished_piece() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        // Reserve a piece and consume both blocks.
        let _req0 = state.next_request(peer, &bf).unwrap();
        let _req1 = state.next_request(peer, &bf).unwrap(); // block 1 via current_progress

        // All blocks dispatched; try_current_piece should return None and clear progress.
        let result = state.try_current_piece(peer);
        assert!(result.is_none(), "should be None when piece is fully dispatched");

        // current_progress entry should be cleared.
        assert!(
            !state.current_progress.contains_key(&peer),
            "current_progress should be cleared after all blocks dispatched"
        );
    }

    #[test]
    fn find_candidate_returns_rarest() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        // Availability: piece 0=3, piece 1=2, piece 2=1 (rarest), piece 3=3.
        let peer1 = addr(1001);
        let peer2 = addr(1002);
        let peer3 = addr(1003);

        let bf_all = peer_has_all(num_pieces);

        let mut bf2 = Bitfield::new(num_pieces);
        bf2.set(0);
        bf2.set(1);
        bf2.set(3);

        let mut bf3 = Bitfield::new(num_pieces);
        bf3.set(0);
        bf3.set(3);

        state.add_peer(peer1, bf_all.clone());
        state.add_peer(peer2, bf2);
        state.add_peer(peer3, bf3);

        let candidate = state.find_candidate(&bf_all).unwrap();
        assert_eq!(candidate, 2, "should pick piece 2 (rarest, availability=1)");
    }

    #[test]
    fn find_candidate_respects_max_in_flight() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 1, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());
        // Override adaptive cap for this unit test (adaptive min is 256).
        state.max_in_flight = 1;

        // Reserve the one allowed in-flight slot.
        let _req = state.next_request(peer, &bf).unwrap();

        // find_candidate should see cap reached.
        let candidate = state.find_candidate(&bf);
        assert!(candidate.is_none(), "should be None when at max_in_flight");
    }

    #[test]
    fn find_candidate_prefers_priority() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        let mut priority = BTreeSet::new();
        priority.insert(3);
        state.set_priority_pieces(priority);

        let candidate = state.find_candidate(&bf).unwrap();
        assert_eq!(candidate, 3, "priority piece should be chosen over rarest-first");
    }

    #[test]
    fn find_candidate_returns_none_in_endgame() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        state.set_endgame(true);

        let candidate = state.find_candidate(&bf);
        assert!(candidate.is_none(), "find_candidate should return None in endgame");
    }

    #[test]
    fn try_reserve_race_condition() {
        // Two peers both "found" the same piece via find_candidate; only the
        // first try_reserve should succeed; the second must return None.
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer_a = addr(1000);
        let peer_b = addr(2000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer_a, bf.clone());
        state.add_peer(peer_b, bf.clone());

        // Both peers "found" piece 0 via find_candidate.
        let contested_piece = 0u32;

        // Peer A claims it first.
        let req_a = state.try_reserve(peer_a, contested_piece, &bf);
        assert!(req_a.is_some(), "peer A should succeed");

        // Peer B tries the same piece — must fail (race condition guard).
        let req_b = state.try_reserve(peer_b, contested_piece, &bf);
        assert!(req_b.is_none(), "peer B should get None (piece already taken)");
    }

    #[test]
    fn try_reserve_rechecks_max_in_flight() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        // cap = 1
        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 1, we_have, wanted);

        let peer_a = addr(1000);
        let peer_b = addr(2000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer_a, bf.clone());
        state.add_peer(peer_b, bf.clone());
        // Override adaptive cap for this unit test (adaptive min is 256).
        state.max_in_flight = 1;

        // Peer A fills the single in-flight slot.
        let _req_a = state.next_request(peer_a, &bf).unwrap();

        // Peer B calls try_reserve (as if it found a candidate before the cap was hit).
        let req_b = state.try_reserve(peer_b, 1, &bf);
        assert!(
            req_b.is_none(),
            "try_reserve should return None when max_in_flight is already reached"
        );
    }

    #[test]
    fn release_peer_pieces_allows_re_reservation() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        // Reserve a piece.
        let req = state.next_request(peer, &bf).unwrap();
        let piece = req.piece;

        // Release pieces (choke).
        state.release_peer_pieces(peer);

        // Piece should be released.
        assert!(
            !state.piece_owner.contains_key(&piece),
            "piece should be released after choke"
        );

        // Peer should be able to reserve again (unchoked) — bitfield is
        // passed as parameter, so release_peer_pieces doesn't affect it.
        let req2 = state.next_request(peer, &bf);
        assert!(
            req2.is_some(),
            "peer should be able to reserve after release"
        );
    }

    // ---- Adaptive max_in_flight tests ----

    #[test]
    fn adaptive_max_in_flight_scales_with_peers() {
        // Use a large torrent so num_pieces/2 doesn't constrain us.
        let lengths = Lengths::new(1024 * 1024 * 1024, 32768, 16384); // 1 GiB
        let num_pieces = lengths.num_pieces();
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 256, we_have, wanted);

        // Add 100 peers.
        let bf = peer_has_all(num_pieces);
        for i in 0..100u16 {
            state.add_peer(addr(5000 + i), bf.clone());
        }

        // 100 peers * 3 = 300, which is > 256.
        assert!(
            state.max_in_flight > 256,
            "max_in_flight should scale above 256 with 100 peers, got {}",
            state.max_in_flight
        );
        assert_eq!(state.max_in_flight, 300);
    }

    #[test]
    fn adaptive_max_in_flight_bounded() {
        // Small torrent: 20 pieces → upper bound = 20 / 2 = 10, floor = 256.
        let lengths = Lengths::new(20 * 32768, 32768, 16384);
        let num_pieces = lengths.num_pieces();
        assert_eq!(num_pieces, 20);
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 256, we_have, wanted);

        // Add 200 peers → calculated = max(256, 600) = 600 → min(600, 10) = 10 → max(10, 256) = 256.
        let bf = peer_has_all(num_pieces);
        for i in 0..200u16 {
            state.add_peer(addr(6000 + i), bf.clone());
        }

        // Lower bound of 256 should always hold.
        assert_eq!(
            state.max_in_flight, 256,
            "max_in_flight should be floored at 256 for small torrents"
        );

        // Large torrent: verify upper bound caps at num_pieces / 2.
        let lengths_large = Lengths::new(512 * 32768, 32768, 16384);
        let num_pieces_large = lengths_large.num_pieces();
        assert_eq!(num_pieces_large, 512);
        let we_have_large = Bitfield::new(num_pieces_large);
        let wanted_large = all_pieces_wanted(num_pieces_large);

        let (mut state_large, _notify_large) =
            PieceReservationState::new(num_pieces_large, lengths_large, 256, we_have_large, wanted_large);

        // Add 200 peers → calculated = max(256, 600) = 600 → min(600, 256) = 256 → max(256, 256) = 256.
        let bf_large = peer_has_all(num_pieces_large);
        for i in 0..200u16 {
            state_large.add_peer(addr(7000 + i), bf_large.clone());
        }

        assert_eq!(
            state_large.max_in_flight, 256,
            "max_in_flight should be capped at num_pieces/2 = 256"
        );
    }

    #[test]
    fn adaptive_max_in_flight_decreases_on_remove() {
        // Large torrent so num_pieces/2 doesn't constrain.
        let lengths = Lengths::new(1024 * 1024 * 1024, 32768, 16384);
        let num_pieces = lengths.num_pieces();
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 256, we_have, wanted);

        // Add 100 peers.
        let bf = peer_has_all(num_pieces);
        for i in 0..100u16 {
            state.add_peer(addr(8000 + i), bf.clone());
        }
        let max_with_100 = state.max_in_flight;
        assert_eq!(max_with_100, 300); // 100 * 3

        // Remove 80 peers → 20 remain → max(256, 20*3=60) = 256.
        for i in 0..80u16 {
            state.remove_peer(addr(8000 + i), &bf);
        }

        assert!(
            state.max_in_flight < max_with_100,
            "max_in_flight should decrease after removing peers: was {}, now {}",
            max_with_100,
            state.max_in_flight
        );
        assert_eq!(
            state.max_in_flight, 256,
            "with 20 peers, max_in_flight should be 256 (floor)"
        );
    }

    // ---- Cached dispatch tests ----

    #[test]
    fn cached_dispatch_finds_rarest() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        // Availability: piece 0=3, piece 1=2, piece 2=1 (rarest), piece 3=3.
        let peer1 = addr(1001);
        let peer2 = addr(1002);
        let peer3 = addr(1003);

        let bf_all = peer_has_all(num_pieces);

        let mut bf2 = Bitfield::new(num_pieces);
        bf2.set(0);
        bf2.set(1);
        bf2.set(3);

        let mut bf3 = Bitfield::new(num_pieces);
        bf3.set(0);
        bf3.set(3);

        state.add_peer(peer1, bf_all.clone());
        state.add_peer(peer2, bf2);
        state.add_peer(peer3, bf3);

        // Build cache via next_request (which calls rebuild_candidates).
        // Verify it picks piece 2 (rarest) — same result as the uncached scan would.
        let req = state.next_request(peer1, &bf_all).unwrap();
        assert_eq!(
            req.piece, 2,
            "cached dispatch should pick piece 2 (rarest, availability=1)"
        );

        // Also verify find_candidate (READ path) returns the same rarest piece
        // after cache was built.
        // First, complete piece 2 so it's out of the way, and check next rarest.
        state.complete_piece(2);
        // Rebuild cache via next_request for peer2 (who doesn't have piece 2 anyway).
        // Now test find_candidate picks piece 1 (availability=2, next rarest).
        let candidate = state.find_candidate(&bf_all);
        assert_eq!(
            candidate,
            Some(1),
            "cached find_candidate should pick piece 1 (next rarest, availability=2)"
        );
    }

    #[test]
    fn cache_invalidation_on_peer_have() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer = addr(1000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer, bf.clone());

        // Build the cache.
        assert!(state.candidates_dirty, "should be dirty initially");
        state.rebuild_candidates();
        assert!(!state.candidates_dirty, "should be clean after rebuild");

        // peer_have should invalidate cache.
        state.peer_have(0);
        assert!(
            state.candidates_dirty,
            "cache should be dirty after peer_have()"
        );

        // try_reserve should trigger rebuild.
        let _ = state.try_reserve(peer, 0, &bf);
        assert!(
            !state.candidates_dirty,
            "cache should be clean after try_reserve() triggers rebuild"
        );
    }

    #[test]
    fn stale_cache_filters_invalid() {
        let lengths = test_lengths();
        let num_pieces = 4;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);

        let (mut state, _notify) =
            PieceReservationState::new(num_pieces, lengths, 10, we_have, wanted);

        let peer_a = addr(1000);
        let peer_b = addr(2000);
        let bf = peer_has_all(num_pieces);
        state.add_peer(peer_a, bf.clone());
        state.add_peer(peer_b, bf.clone());

        // Build cache — all 4 pieces are candidates.
        state.rebuild_candidates();
        assert_eq!(state.sorted_candidates.len(), 4);

        // Reserve piece 0 via peer_a — cache becomes stale but piece_owner blocks it.
        let _req = state.try_reserve(peer_a, 0, &bf).unwrap();

        // find_candidate (READ path) should skip piece 0 via can_reserve() guard
        // even though piece 0 is still in the stale cache.
        let candidate = state.find_candidate(&bf);
        assert!(candidate.is_some(), "should find a candidate");
        assert_ne!(
            candidate.unwrap(),
            0,
            "stale cache should skip reserved piece 0 via can_reserve()"
        );
    }
}
