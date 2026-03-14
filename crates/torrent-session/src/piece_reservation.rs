//! Concurrent piece reservation state for per-peer dispatch.
//!
//! Each peer exclusively owns pieces it is downloading. Block dispatch within
//! a reserved piece is sequential (block 0, 1, 2, ...). When a peer disconnects
//! mid-piece, the piece is released and another peer can re-request all blocks.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use rustc_hash::FxHashMap;
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

/// Piece-level state for lock-free CAS reservation.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PieceState {
    Available = 0,
    Reserved = 1,
    Complete = 2,
    Unwanted = 3,
    Endgame = 4,
}

impl PieceState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Available,
            1 => Self::Reserved,
            2 => Self::Complete,
            3 => Self::Unwanted,
            4 => Self::Endgame,
            _ => Self::Available, // defensive fallback
        }
    }
}

/// Lock-free per-piece state array. Shared across all peer tasks via `Arc`.
///
/// Each piece occupies one `AtomicU8`. Peers reserve pieces via CAS
/// (compare-and-swap) without any mutex or RwLock.
#[derive(Debug)]
pub(crate) struct AtomicPieceStates {
    states: Vec<AtomicU8>,
}

#[allow(dead_code)]
impl AtomicPieceStates {
    pub fn new(num_pieces: u32, we_have: &Bitfield, wanted: &Bitfield) -> Self {
        let states: Vec<AtomicU8> = (0..num_pieces)
            .map(|i| {
                let val = if we_have.get(i) {
                    PieceState::Complete as u8
                } else if !wanted.get(i) {
                    PieceState::Unwanted as u8
                } else {
                    PieceState::Available as u8
                };
                AtomicU8::new(val)
            })
            .collect();
        Self { states }
    }

    /// Attempt to reserve a piece via CAS.
    pub fn try_reserve(&self, index: u32) -> bool {
        let atom = &self.states[index as usize];
        if atom
            .compare_exchange(
                PieceState::Available as u8,
                PieceState::Reserved as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            return true;
        }
        let current = atom.load(Ordering::Relaxed);
        current == PieceState::Endgame as u8
    }

    pub fn mark_complete(&self, index: u32) {
        self.states[index as usize].store(PieceState::Complete as u8, Ordering::Relaxed);
    }

    pub fn release(&self, index: u32) {
        self.states[index as usize].store(PieceState::Available as u8, Ordering::Relaxed);
    }

    pub fn transition_to_endgame(&self, index: u32) {
        self.states[index as usize].store(PieceState::Endgame as u8, Ordering::Relaxed);
    }

    pub fn mark_unwanted(&self, index: u32) {
        self.states[index as usize].store(PieceState::Unwanted as u8, Ordering::Relaxed);
    }

    pub fn mark_available(&self, index: u32) {
        let _ = self.states[index as usize].compare_exchange(
            PieceState::Unwanted as u8,
            PieceState::Available as u8,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    pub fn get(&self, index: u32) -> PieceState {
        PieceState::from_u8(self.states[index as usize].load(Ordering::Relaxed))
    }

    pub fn force_reserved(&self, index: u32) {
        self.states[index as usize].store(PieceState::Reserved as u8, Ordering::Relaxed);
    }

    pub fn len(&self) -> u32 {
        self.states.len() as u32
    }
}

/// Pre-computed rarest-first ordering, cheaply cloneable via `Arc`.
///
/// Built by TorrentActor on a 500ms timer (or significant events like
/// peer join/leave). Uses bucket sort -- O(n) where n = num_pieces.
#[derive(Debug)]
pub(crate) struct AvailabilitySnapshot {
    /// Pieces ordered rarest-first. Walk from index 0 for rarest pieces.
    pub order: Vec<u32>,
    /// Monotonically increasing counter. Peer tasks compare against their
    /// cached generation to detect when the snapshot was rebuilt.
    pub generation: u64,
}

impl AvailabilitySnapshot {
    /// Build a new snapshot using bucket sort.
    ///
    /// `availability[i]` = number of peers that have piece `i`.
    /// `atomic_states` is consulted to skip Complete/Unwanted/Reserved pieces.
    /// `priority_pieces` are placed at the front regardless of availability.
    ///
    /// Bucket sort is O(n + max_avail) -- much cheaper than the O(n log n)
    /// sort in the old `rebuild_candidates()`.
    pub fn build(
        availability: &[u32],
        atomic_states: &AtomicPieceStates,
        priority_pieces: &BTreeSet<u32>,
        generation: u64,
    ) -> Self {
        let num_pieces = availability.len();
        if num_pieces == 0 {
            return Self {
                order: Vec::new(),
                generation,
            };
        }

        // Find max availability for bucket allocation.
        let max_avail = availability.iter().copied().max().unwrap_or(0) as usize;

        // Bucket sort: buckets[a] = list of pieces with availability `a`.
        let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); max_avail + 1];
        for (piece, &avail_count) in availability.iter().enumerate() {
            let state = atomic_states.get(piece as u32);
            match state {
                PieceState::Complete | PieceState::Unwanted | PieceState::Reserved => continue,
                PieceState::Available | PieceState::Endgame => {}
            }
            buckets[avail_count as usize].push(piece as u32);
        }

        // Flatten buckets: rarest (lowest availability) first.
        // Priority pieces go to the very front.
        let mut order = Vec::with_capacity(num_pieces);
        let mut non_priority = Vec::with_capacity(num_pieces);

        for bucket in &buckets {
            for &piece in bucket {
                if priority_pieces.contains(&piece) {
                    order.push(piece);
                } else {
                    non_priority.push(piece);
                }
            }
        }
        order.append(&mut non_priority);

        Self { order, generation }
    }
}

/// Per-peer dispatch state. Owned entirely by a single peer task -- no
/// sharing, no locks, no atomics on the hot path.
pub(crate) struct PeerDispatchState {
    /// Current snapshot (cheaply cloned via Arc).
    snapshot: Arc<AvailabilitySnapshot>,
    /// Cursor position within `snapshot.order`.
    cursor: usize,
    /// The piece currently being block-dispatched, if any.
    current_piece: Option<CurrentPiece>,
    /// Lengths for block arithmetic.
    lengths: Lengths,
}

/// Tracks block-by-block progress within a single piece.
struct CurrentPiece {
    piece: u32,
    next_block: u32,
    total_blocks: u32,
}

impl PeerDispatchState {
    pub fn new(snapshot: Arc<AvailabilitySnapshot>, lengths: Lengths) -> Self {
        Self {
            snapshot,
            cursor: 0,
            current_piece: None,
            lengths,
        }
    }

    /// Update the snapshot reference. Resets cursor if generation changed.
    pub fn update_snapshot(&mut self, snapshot: Arc<AvailabilitySnapshot>) {
        if snapshot.generation != self.snapshot.generation {
            self.cursor = 0;
        }
        self.snapshot = snapshot;
    }

    /// Get the next block request for this peer.
    ///
    /// Hot path: if a current piece has remaining blocks, returns immediately
    /// with no atomic operations at all.
    ///
    /// Cold path: walks the snapshot cursor, calling `try_reserve()` (CAS) on
    /// each candidate the peer has. First CAS success becomes the new
    /// `current_piece`.
    ///
    /// Returns `None` when the cursor is exhausted (caller should wait on
    /// `piece_notify`).
    pub fn next_block(
        &mut self,
        peer_bitfield: &Bitfield,
        atomic_states: &AtomicPieceStates,
    ) -> Option<BlockRequest> {
        // 1. Hot path: current piece has blocks remaining.
        if let Some(ref mut cp) = self.current_piece {
            if cp.next_block < cp.total_blocks {
                let piece = cp.piece;
                let block_idx = cp.next_block;
                cp.next_block += 1;
                return Some(self.make_block_request(piece, block_idx));
            }
            // Piece fully dispatched -- drop it.
            self.current_piece = None;
        }

        // 2. Cold path: walk snapshot for a new piece.
        while self.cursor < self.snapshot.order.len() {
            let piece = self.snapshot.order[self.cursor];
            self.cursor += 1;

            if !peer_bitfield.get(piece) {
                continue;
            }

            if atomic_states.try_reserve(piece) {
                let total_blocks = self.lengths.chunks_in_piece(piece);
                if total_blocks == 0 {
                    continue;
                }
                self.current_piece = Some(CurrentPiece {
                    piece,
                    next_block: 1,
                    total_blocks,
                });
                return Some(self.make_block_request(piece, 0));
            }
        }

        None // cursor exhausted
    }

    /// Build a `BlockRequest` using `Lengths::chunk_info`.
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

    /// Get the currently active piece, if any (for release on disconnect).
    pub fn current_piece_index(&self) -> Option<u32> {
        self.current_piece.as_ref().map(|cp| cp.piece)
    }

    /// Clear current piece (e.g., on choke -- peer must re-reserve).
    pub fn clear_current_piece(&mut self) {
        self.current_piece = None;
    }
}

use slab::Slab;

/// Arena-allocated peer index for compact piece tracking.
///
/// Maps between dense u16 slot indices and SocketAddr values.
/// Managed by TorrentActor (single-threaded, no lock needed).
pub(crate) struct PeerSlab {
    inner: Slab<SocketAddr>,
    addr_to_slot: FxHashMap<SocketAddr, u16>,
}

#[allow(dead_code)]
impl PeerSlab {
    pub fn new() -> Self {
        PeerSlab {
            inner: Slab::with_capacity(64),
            addr_to_slot: FxHashMap::default(),
        }
    }

    /// Insert a peer, returning its compact slot index.
    ///
    /// Panics if the slab exceeds u16::MAX entries (65535 peers).
    pub fn insert(&mut self, addr: SocketAddr) -> u16 {
        let key = self.inner.insert(addr);
        assert!(
            key <= u16::MAX as usize,
            "peer slab overflow: {key} > u16::MAX"
        );
        let slot = key as u16;
        self.addr_to_slot.insert(addr, slot);
        slot
    }

    /// Remove a peer by slot index.
    pub fn remove(&mut self, slot: u16) -> SocketAddr {
        let addr = self.inner.remove(slot as usize);
        self.addr_to_slot.remove(&addr);
        addr
    }

    /// Remove a peer by address. Returns the slot if found.
    pub fn remove_by_addr(&mut self, addr: &SocketAddr) -> Option<u16> {
        if let Some(slot) = self.addr_to_slot.remove(addr) {
            self.inner.remove(slot as usize);
            Some(slot)
        } else {
            None
        }
    }

    /// Look up peer address by slot index.
    pub fn get(&self, slot: u16) -> Option<&SocketAddr> {
        self.inner.get(slot as usize)
    }

    /// Look up slot index by peer address.
    pub fn slot_of(&self, addr: &SocketAddr) -> Option<u16> {
        self.addr_to_slot.get(addr).copied()
    }

    /// Check if a slot is occupied.
    pub fn contains(&self, slot: u16) -> bool {
        self.inner.contains(slot as usize)
    }

    /// Number of active peers.
    pub fn len(&self) -> usize {
        self.inner.len()
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
    fn atomic_reserve_release_cycle() {
        let num_pieces = 8u32;
        let we_have = Bitfield::new(num_pieces);
        let wanted = all_pieces_wanted(num_pieces);
        let states = AtomicPieceStates::new(num_pieces, &we_have, &wanted);

        // All pieces start Available
        for i in 0..num_pieces {
            assert_eq!(states.get(i), PieceState::Available);
        }

        // Reserve piece 3
        assert!(states.try_reserve(3));
        assert_eq!(states.get(3), PieceState::Reserved);

        // Second reserve on same piece fails
        assert!(!states.try_reserve(3));

        // Release piece 3
        states.release(3);
        assert_eq!(states.get(3), PieceState::Available);

        // Can reserve again after release
        assert!(states.try_reserve(3));
        assert_eq!(states.get(3), PieceState::Reserved);

        // Mark complete — try_reserve fails
        states.mark_complete(3);
        assert_eq!(states.get(3), PieceState::Complete);
        assert!(!states.try_reserve(3));

        // force_reserved works
        states.force_reserved(3);
        assert_eq!(states.get(3), PieceState::Reserved);
    }

    #[test]
    fn atomic_unwanted_transitions() {
        let num_pieces = 4u32;
        let we_have = Bitfield::new(num_pieces);
        let mut wanted = Bitfield::new(num_pieces);
        wanted.set(0);
        wanted.set(1);
        // pieces 2,3 are unwanted

        let states = AtomicPieceStates::new(num_pieces, &we_have, &wanted);

        assert_eq!(states.get(0), PieceState::Available);
        assert_eq!(states.get(1), PieceState::Available);
        assert_eq!(states.get(2), PieceState::Unwanted);
        assert_eq!(states.get(3), PieceState::Unwanted);

        // Cannot reserve unwanted pieces
        assert!(!states.try_reserve(2));

        // mark_available only transitions Unwanted -> Available
        states.mark_available(2);
        assert_eq!(states.get(2), PieceState::Available);
        assert!(states.try_reserve(2));

        // mark_available on Reserved is a no-op (CAS fails safely)
        states.mark_available(2);
        assert_eq!(states.get(2), PieceState::Reserved);
    }

    #[test]
    fn atomic_we_have_initialized_complete() {
        let num_pieces = 4u32;
        let mut we_have = Bitfield::new(num_pieces);
        we_have.set(0);
        we_have.set(2);
        let wanted = all_pieces_wanted(num_pieces);

        let states = AtomicPieceStates::new(num_pieces, &we_have, &wanted);

        assert_eq!(states.get(0), PieceState::Complete);
        assert_eq!(states.get(1), PieceState::Available);
        assert_eq!(states.get(2), PieceState::Complete);
        assert_eq!(states.get(3), PieceState::Available);

        assert!(!states.try_reserve(0));
        assert!(states.try_reserve(1));
    }

    #[test]
    fn atomic_len() {
        let states = AtomicPieceStates::new(42, &Bitfield::new(42), &all_pieces_wanted(42));
        assert_eq!(states.len(), 42);
    }

    #[test]
    fn atomic_cas_conflict_two_threads() {
        use std::sync::Arc;

        let states = Arc::new(AtomicPieceStates::new(
            1,
            &Bitfield::new(1),
            &all_pieces_wanted(1),
        ));

        let results: Vec<bool> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let st = Arc::clone(&states);
                    s.spawn(move || st.try_reserve(0))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        // Exactly one thread wins the CAS
        let wins: usize = results.iter().filter(|&&r| r).count();
        assert_eq!(wins, 1, "exactly one CAS should succeed, got {wins}");
        assert_eq!(states.get(0), PieceState::Reserved);
    }

    #[test]
    fn atomic_endgame_allows_multiple_reservations() {
        let states = AtomicPieceStates::new(4, &Bitfield::new(4), &all_pieces_wanted(4));

        // Reserve piece 1 normally
        assert!(states.try_reserve(1));
        assert_eq!(states.get(1), PieceState::Reserved);

        // Transition to endgame
        states.transition_to_endgame(1);
        assert_eq!(states.get(1), PieceState::Endgame);

        // Multiple "reservations" succeed in endgame
        assert!(states.try_reserve(1));
        assert!(states.try_reserve(1));
        assert!(states.try_reserve(1));

        // State remains Endgame (not transitioned to Reserved)
        assert_eq!(states.get(1), PieceState::Endgame);
    }

    #[test]
    fn atomic_endgame_state_transitions() {
        let states = AtomicPieceStates::new(4, &Bitfield::new(4), &all_pieces_wanted(4));

        // Reserve pieces 0 and 1
        assert!(states.try_reserve(0));
        assert!(states.try_reserve(1));

        // Transition both to endgame
        states.transition_to_endgame(0);
        states.transition_to_endgame(1);

        // force_reserved brings piece back from Endgame
        states.force_reserved(0);
        assert_eq!(states.get(0), PieceState::Reserved);

        // release from Endgame (for pieces without an owner)
        states.release(1);
        assert_eq!(states.get(1), PieceState::Available);
    }

    #[test]
    fn peer_slab_insert_remove_lookup() {
        let mut slab = PeerSlab::new();
        let a1 = addr(1000);
        let a2 = addr(2000);
        let a3 = addr(3000);

        let s1 = slab.insert(a1);
        let s2 = slab.insert(a2);
        let s3 = slab.insert(a3);

        assert_eq!(slab.len(), 3);
        assert_eq!(slab.get(s1), Some(&a1));
        assert_eq!(slab.get(s2), Some(&a2));
        assert_eq!(slab.slot_of(&a2), Some(s2));
        assert!(slab.contains(s1));

        // Remove by slot
        let removed = slab.remove(s2);
        assert_eq!(removed, a2);
        assert_eq!(slab.len(), 2);
        assert_eq!(slab.get(s2), None);
        assert_eq!(slab.slot_of(&a2), None);

        // Remove by addr
        let slot = slab.remove_by_addr(&a3);
        assert_eq!(slot, Some(s3));
        assert_eq!(slab.len(), 1);

        // Remove nonexistent
        assert_eq!(slab.remove_by_addr(&a3), None);
    }

    #[test]
    fn peer_slab_slot_reuse() {
        let mut slab = PeerSlab::new();
        let a1 = addr(1000);
        let a2 = addr(2000);

        let s1 = slab.insert(a1);
        slab.remove(s1);

        // Slab reuses freed slot
        let s2 = slab.insert(a2);
        assert_eq!(s2, s1); // same slot index reused
        assert_eq!(slab.get(s2), Some(&a2));
    }

    #[test]
    fn snapshot_bucket_sort_rarest_first() {
        let num_pieces = 5u32;
        // availability: piece 0=3, piece 1=1, piece 2=2, piece 3=1, piece 4=3
        let availability = vec![3u32, 1, 2, 1, 3];
        let states = AtomicPieceStates::new(num_pieces, &Bitfield::new(num_pieces), &all_pieces_wanted(num_pieces));
        let priority = BTreeSet::new();

        let snap = AvailabilitySnapshot::build(&availability, &states, &priority, 1);

        assert_eq!(snap.generation, 1);
        // Rarest pieces (availability=1) should come first: pieces 1 and 3
        // Then availability=2: piece 2
        // Then availability=3: pieces 0 and 4
        assert_eq!(snap.order.len(), 5);
        // First two must be the rarest (1 and 3 in some order)
        let first_two: std::collections::HashSet<u32> = snap.order[..2].iter().copied().collect();
        assert!(first_two.contains(&1));
        assert!(first_two.contains(&3));
        // Third must be piece 2 (availability=2)
        assert_eq!(snap.order[2], 2);
        // Last two are pieces 0 and 4 (availability=3)
        let last_two: std::collections::HashSet<u32> = snap.order[3..].iter().copied().collect();
        assert!(last_two.contains(&0));
        assert!(last_two.contains(&4));
    }

    #[test]
    fn snapshot_skips_complete_reserved_unwanted() {
        let num_pieces = 5u32;
        let availability = vec![1u32; 5];
        let states = AtomicPieceStates::new(num_pieces, &Bitfield::new(num_pieces), &all_pieces_wanted(num_pieces));

        // Mark some pieces with non-Available states
        states.mark_complete(0);
        assert!(states.try_reserve(2)); // Reserved
        states.mark_unwanted(4);

        let snap = AvailabilitySnapshot::build(&availability, &states, &BTreeSet::new(), 1);

        // Only pieces 1 and 3 should be in the snapshot (Available)
        assert_eq!(snap.order.len(), 2);
        assert!(snap.order.contains(&1));
        assert!(snap.order.contains(&3));
    }

    #[test]
    fn snapshot_includes_endgame_pieces() {
        let num_pieces = 3u32;
        let availability = vec![1u32; 3];
        let states = AtomicPieceStates::new(num_pieces, &Bitfield::new(num_pieces), &all_pieces_wanted(num_pieces));

        assert!(states.try_reserve(1));
        states.transition_to_endgame(1);

        let snap = AvailabilitySnapshot::build(&availability, &states, &BTreeSet::new(), 1);

        // Pieces 0 (Available) and 1 (Endgame) should be included, piece 2 (Available) too
        assert_eq!(snap.order.len(), 3);
        assert!(snap.order.contains(&0));
        assert!(snap.order.contains(&1));
        assert!(snap.order.contains(&2));
    }

    #[test]
    fn snapshot_empty_torrent() {
        let snap = AvailabilitySnapshot::build(
            &[],
            &AtomicPieceStates::new(0, &Bitfield::new(0), &Bitfield::new(0)),
            &BTreeSet::new(),
            0,
        );
        assert!(snap.order.is_empty());
        assert_eq!(snap.generation, 0);
    }

    #[test]
    fn snapshot_priority_pieces_sort_first() {
        let num_pieces = 5u32;
        // piece 2 has the HIGHEST availability (least rare)
        let availability = vec![2u32, 3, 10, 1, 5];
        let states = AtomicPieceStates::new(num_pieces, &Bitfield::new(num_pieces), &all_pieces_wanted(num_pieces));
        let mut priority = BTreeSet::new();
        priority.insert(2); // piece 2 is priority despite high availability

        let snap = AvailabilitySnapshot::build(&availability, &states, &priority, 1);

        // Piece 2 must be at the very front despite availability=10
        assert_eq!(snap.order[0], 2, "priority piece should be first");
        // Remaining pieces in rarest-first order: 3 (avail=1), 0 (avail=2), 1 (avail=3), 4 (avail=5)
        assert_eq!(snap.order[1], 3); // avail=1
        assert_eq!(snap.order[2], 0); // avail=2
        assert_eq!(snap.order[3], 1); // avail=3
        assert_eq!(snap.order[4], 4); // avail=5
    }

    // ---- PeerDispatchState tests ----

    #[test]
    fn dispatch_state_hot_path_returns_blocks() {
        let lengths = test_lengths(); // 128 KiB total, 32 KiB pieces, 16 KiB chunks -> 4 pieces, 2 blocks each
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);

        // First call: cold path (no current piece) -> reserves a piece, returns block 0
        let b0 = ds.next_block(&peer_bf, &states).unwrap();
        assert_eq!(b0.begin, 0);
        assert_eq!(b0.length, 16384);
        let piece = b0.piece;

        // Second call: hot path -> returns block 1 of same piece
        let b1 = ds.next_block(&peer_bf, &states).unwrap();
        assert_eq!(b1.piece, piece);
        assert_eq!(b1.begin, 16384);
        assert_eq!(b1.length, 16384);

        // Third call: piece fully dispatched, cold path -> reserves next piece
        let b2 = ds.next_block(&peer_bf, &states).unwrap();
        assert_ne!(b2.piece, piece, "should get a different piece");
        assert_eq!(b2.begin, 0);
    }

    #[test]
    fn dispatch_state_cursor_exhausted_returns_none() {
        let lengths = test_lengths();
        let num_pieces = 2u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);

        // Exhaust all pieces (2 pieces x 2 blocks = 4 blocks)
        for _ in 0..4 {
            assert!(ds.next_block(&peer_bf, &states).is_some());
        }

        // Cursor exhausted
        assert!(ds.next_block(&peer_bf, &states).is_none());
    }

    #[test]
    fn dispatch_state_cursor_reset_on_generation_change() {
        let lengths = test_lengths();
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let availability = vec![1u32; num_pieces as usize];
        let snap1 = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);

        let mut ds = PeerDispatchState::new(Arc::clone(&snap1), lengths.clone());

        // Reserve piece from first snapshot
        let b0 = ds.next_block(&peer_bf, &states).unwrap();
        let first_piece = b0.piece;

        // Exhaust first piece
        ds.next_block(&peer_bf, &states).unwrap(); // block 1

        // Release the first piece (simulating actor releasing it)
        states.release(first_piece);

        // New snapshot with different generation
        let snap2 = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            2, // generation changed
        ));
        ds.update_snapshot(snap2);

        // Cursor should have been reset -- can see pieces from the start again
        let b_new = ds.next_block(&peer_bf, &states).unwrap();
        // The released piece should be available again at the front
        assert_eq!(b_new.begin, 0, "cursor should have reset to start");
    }

    #[test]
    fn dispatch_state_skips_pieces_peer_lacks() {
        let lengths = test_lengths();
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        // Peer only has piece 2
        let mut peer_bf = Bitfield::new(num_pieces);
        peer_bf.set(2);

        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);

        let b = ds.next_block(&peer_bf, &states).unwrap();
        assert_eq!(b.piece, 2, "should only get piece 2 which peer has");

        // Exhaust piece 2 (2 blocks)
        ds.next_block(&peer_bf, &states).unwrap();

        // No more pieces available for this peer
        assert!(ds.next_block(&peer_bf, &states).is_none());
    }

    #[test]
    fn dispatch_state_cas_failure_advances_cursor() {
        let lengths = test_lengths();
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let peer_bf = peer_has_all(num_pieces);

        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));

        // Pre-reserve all but the last piece externally (simulating other peers)
        let first_three: Vec<u32> = snap.order[..3].to_vec();
        for &p in &first_three {
            assert!(states.try_reserve(p));
        }

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);

        // Should skip the first 3 (CAS fails) and get the 4th
        let b = ds.next_block(&peer_bf, &states).unwrap();
        assert!(!first_three.contains(&b.piece), "should skip pre-reserved pieces");
    }

    #[test]
    fn dispatch_state_current_piece_and_clear() {
        let lengths = test_lengths();
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let peer_bf = peer_has_all(num_pieces);
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);

        // No current piece initially
        assert!(ds.current_piece_index().is_none());

        // Reserve a piece
        let b = ds.next_block(&peer_bf, &states).unwrap();
        assert_eq!(ds.current_piece_index(), Some(b.piece));

        // Clear current piece (simulating choke)
        ds.clear_current_piece();
        assert!(ds.current_piece_index().is_none());
    }

    #[test]
    fn snapshot_atomic_consistency_gap() {
        // Verify that stale snapshots don't cause double reservation.
        // Build a snapshot showing piece 2 as Available. Then another
        // "thread" reserves piece 2 before this peer walks to it.
        // The peer's CAS should fail, and it should advance to the next piece.
        let lengths = test_lengths();
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);

        // Pre-reserve the first piece in the snapshot order (simulating another peer)
        let contested_piece = snap.order[0];
        assert!(states.try_reserve(contested_piece));

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);

        // Peer should skip the contested piece (CAS fails) and get the next one
        let b = ds.next_block(&peer_bf, &states).unwrap();
        assert_ne!(b.piece, contested_piece, "should skip piece reserved by another thread");
    }

    #[test]
    fn multi_peer_dispatch_no_double_reservation() {
        // Two PeerDispatchStates share the same AtomicPieceStates.
        // Each should get unique pieces (no overlap).
        let lengths = test_lengths();
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);

        let mut ds1 = PeerDispatchState::new(Arc::clone(&snap), lengths.clone());
        let mut ds2 = PeerDispatchState::new(Arc::clone(&snap), lengths);

        let mut reserved = std::collections::HashSet::new();

        // Each peer reserves one piece (2 blocks each, but just check the piece)
        let b1 = ds1.next_block(&peer_bf, &states).unwrap();
        reserved.insert(b1.piece);
        let b2 = ds2.next_block(&peer_bf, &states).unwrap();
        reserved.insert(b2.piece);

        assert_eq!(reserved.len(), 2, "two peers should get two different pieces");
        assert_ne!(b1.piece, b2.piece);
    }
}
