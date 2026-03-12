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
    /// Each peer's bitfield (which pieces they have).
    peer_bitfields: FxHashMap<SocketAddr, Bitfield>,
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
            peer_bitfields: FxHashMap::default(),
            availability: vec![0; num_pieces as usize],
            priority_pieces: BTreeSet::new(),
            piece_notify: Arc::clone(&notify),
            lengths,
            num_pieces,
            max_in_flight,
            endgame_active: false,
        };
        (state, notify)
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
        self.peer_bitfields.insert(addr, bitfield);
        self.peer_pieces.entry(addr).or_default();
        self.piece_notify.notify_waiters();
    }

    /// Remove a peer entirely: release its pieces, remove its bitfield,
    /// and update availability.
    pub fn remove_peer(&mut self, addr: SocketAddr) {
        // Release all pieces owned by this peer.
        if let Some(pieces) = self.peer_pieces.remove(&addr) {
            for piece in pieces {
                self.piece_owner.remove(&piece);
            }
        }
        self.current_progress.remove(&addr);

        // Update availability for all pieces this peer had.
        if let Some(bitfield) = self.peer_bitfields.remove(&addr) {
            for piece in 0..self.num_pieces {
                if bitfield.get(piece) {
                    self.availability[piece as usize] =
                        self.availability[piece as usize].saturating_sub(1);
                }
            }
        }

        self.piece_notify.notify_waiters();
    }

    /// Update a peer's bitfield when they announce a new piece (HAVE message).
    pub fn peer_have(&mut self, addr: SocketAddr, piece: u32) {
        if piece >= self.num_pieces {
            return;
        }
        if let Some(bf) = self.peer_bitfields.get_mut(&addr)
            && !bf.get(piece)
        {
            bf.set(piece);
            self.availability[piece as usize] += 1;
            self.piece_notify.notify_waiters();
        }
    }

    /// Release all pieces owned by a peer WITHOUT removing its bitfield entry.
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
        self.piece_notify.notify_waiters();
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
        self.piece_notify.notify_waiters();
    }

    // ---- State updates ----

    /// Bulk-update the `we_have` bitfield (e.g., after fast-resume verification).
    pub fn update_we_have(&mut self, we_have: &Bitfield) {
        self.we_have = we_have.clone();
    }

    /// Bulk-update the `wanted` bitfield (e.g., after file priority change).
    pub fn update_wanted(&mut self, wanted: &Bitfield) {
        self.wanted = wanted.clone();
        self.piece_notify.notify_waiters();
    }

    /// Set the priority pieces (streaming, time-critical).
    pub fn set_priority_pieces(&mut self, pieces: BTreeSet<u32>) {
        self.priority_pieces = pieces;
        self.piece_notify.notify_waiters();
    }

    /// Enable or disable end-game mode.
    ///
    /// When deactivating (active=false), notifies waiters so peer tasks
    /// blocked in `acquire_and_reserve` wake up and resume requesting.
    pub fn set_endgame(&mut self, active: bool) {
        self.endgame_active = active;
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
    pub fn next_request(&mut self, peer_addr: SocketAddr) -> Option<BlockRequest> {
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

        let peer_has = self.peer_bitfields.get(&peer_addr)?;

        // 2. Priority pieces — O(priority_count).
        for &piece in &self.priority_pieces {
            if self.can_reserve(piece, peer_has) {
                return self.reserve_and_start(peer_addr, piece);
            }
        }

        // 3. Rarest-first — O(num_pieces).
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

        if let Some(piece) = best_piece {
            return self.reserve_and_start(peer_addr, piece);
        }

        None
    }

    // ---- Private helpers ----

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
        state.add_peer(peer_a, peer_has_all(num_pieces));
        state.add_peer(peer_b, peer_has_all(num_pieces));

        // Peer A reserves a piece.
        let req_a = state.next_request(peer_a).unwrap();
        let piece_a = req_a.piece;

        // Peer B should get a DIFFERENT piece.
        let req_b = state.next_request(peer_b).unwrap();
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
        state.add_peer(peer, peer_has_all(num_pieces));

        // First request: block 0 of some piece.
        let req0 = state.next_request(peer).unwrap();
        assert_eq!(req0.begin, 0);
        assert_eq!(req0.length, 16384);

        // Second request: block 1 of the same piece.
        let req1 = state.next_request(peer).unwrap();
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
        state.add_peer(peer, peer_has_all(num_pieces));

        // Reserve a piece (consume both blocks).
        let req0 = state.next_request(peer).unwrap();
        let piece = req0.piece;
        let _req1 = state.next_request(peer).unwrap();

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
        let next = state.next_request(peer);
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

        state.add_peer(peer_a, peer_has_all(num_pieces));
        state.add_peer(peer_b, bf_b);

        // Peer A reserves piece 0 (rarest for peer_b perspective — but
        // rarest-first picks globally rarest unreserved). Let's force
        // peer A to get piece 0 by giving it only piece 0.
        state.peer_bitfields.insert(peer_a, {
            let mut bf = Bitfield::new(num_pieces);
            bf.set(0);
            bf
        });

        let req_a = state.next_request(peer_a).unwrap();
        assert_eq!(req_a.piece, 0);

        // Peer B can't get anything (max_in_flight=1 and piece 0 is taken).
        assert!(state.next_request(peer_b).is_none());

        // Fail piece 0.
        state.fail_piece(0);

        // Now peer B should be able to reserve piece 0.
        let req_b = state.next_request(peer_b).unwrap();
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
        state.add_peer(peer_a, peer_has_all(num_pieces));
        state.add_peer(peer_b, peer_has_all(num_pieces));

        // Peer A reserves a piece.
        let req = state.next_request(peer_a).unwrap();
        let piece = req.piece;

        // Remove peer A.
        state.remove_peer(peer_a);

        // The piece should no longer be owned.
        assert!(!state.piece_owner.contains_key(&piece));

        // Peer B should be able to get that piece.
        // (It's rarest because availability is now 1 since peer_a was removed.)
        // We can verify by checking that peer B can reserve the piece.
        let mut got_piece = false;
        for _ in 0..num_pieces {
            if let Some(r) = state.next_request(peer_b) {
                if r.piece == piece {
                    got_piece = true;
                    break;
                }
                // Consume the second block to move to next piece.
                let _ = state.next_request(peer_b);
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

        state.add_peer(peer1, bf1);
        state.add_peer(peer2, bf2);
        state.add_peer(peer3, bf3);

        // Peer1 has all pieces. Should pick piece 2 (rarest, availability=1).
        let req = state.next_request(peer1).unwrap();
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
        state.add_peer(peer, peer_has_all(num_pieces));

        // Set piece 3 as high priority.
        let mut priority = BTreeSet::new();
        priority.insert(3);
        state.set_priority_pieces(priority);

        // Even though piece 0 might be rarest (equal availability), priority wins.
        let req = state.next_request(peer).unwrap();
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
        state.add_peer(peer, peer_has_all(num_pieces));

        state.set_endgame(true);
        assert!(state.is_endgame());

        let req = state.next_request(peer);
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
        state.add_peer(peer, peer_has_all(num_pieces));

        // Reserve piece 1: get both blocks.
        let r0 = state.next_request(peer).unwrap();
        let _r1 = state.next_request(peer).unwrap();
        let piece1 = r0.piece;

        // Reserve piece 2: get first block.
        let r2 = state.next_request(peer).unwrap();
        assert_ne!(r2.piece, piece1, "should get a different piece");
        let _r3 = state.next_request(peer).unwrap();

        // Now 2 pieces are in flight → next_request should return None
        // (current piece is fully dispatched, and we can't reserve new ones).
        let req = state.next_request(peer);
        assert!(req.is_none(), "should be capped at max_in_flight=2");
    }

    #[test]
    fn release_peer_pieces_keeps_bitfield() {
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
        let req = state.next_request(peer).unwrap();
        let piece = req.piece;

        // Release pieces (choke).
        state.release_peer_pieces(peer);

        // Piece should be released.
        assert!(
            !state.piece_owner.contains_key(&piece),
            "piece should be released after choke"
        );

        // But the peer's bitfield should still be there.
        assert_eq!(
            state.peer_bitfields.get(&peer),
            Some(&bf),
            "peer bitfield should be preserved after release_peer_pieces"
        );

        // Peer should be able to reserve again (unchoked).
        let req2 = state.next_request(peer);
        assert!(
            req2.is_some(),
            "peer should be able to reserve after release"
        );
    }
}
