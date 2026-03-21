//! Concurrent piece reservation state for per-peer dispatch.
//!
//! Each peer exclusively owns pieces it is downloading. Block dispatch within
//! a reserved piece is sequential (block 0, 1, 2, ...). When a peer disconnects
//! mid-piece, the piece is released and another peer can re-request all blocks.

use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

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

    /// Count pieces in Reserved or Endgame state (i.e., in-flight).
    ///
    /// This is the authoritative in-flight count — unlike `piece_owner` which
    /// only learns about reservations when chunks arrive, this reflects CAS
    /// reservations immediately.
    pub fn in_flight_count(&self) -> u32 {
        self.states
            .iter()
            .filter(|a| {
                let v = a.load(Ordering::Relaxed);
                v == PieceState::Reserved as u8 || v == PieceState::Endgame as u8
            })
            .count() as u32
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
    /// Shared block-level request/receipt tracking for steal visibility.
    block_maps: Option<Arc<BlockMaps>>,
    /// Shared queue of pieces available for block-level stealing.
    steal_candidates: Option<Arc<StealCandidates>>,
    /// M120: Per-piece write guards to prevent steal/write races.
    piece_write_guards: Option<Arc<PieceWriteGuards>>,
}

/// Tracks block-by-block progress within a single piece.
struct CurrentPiece {
    piece: u32,
    next_block: u32,
    total_blocks: u32,
}

#[allow(dead_code)]
impl PeerDispatchState {
    pub fn new(snapshot: Arc<AvailabilitySnapshot>, lengths: Lengths) -> Self {
        Self {
            snapshot,
            cursor: 0,
            current_piece: None,
            lengths,
            block_maps: None,
            steal_candidates: None,
            piece_write_guards: None,
        }
    }

    /// Set the shared block maps for steal visibility.
    pub fn set_block_maps(&mut self, bm: Arc<BlockMaps>) {
        self.block_maps = Some(bm);
    }

    /// Set the shared steal candidates queue.
    pub fn set_steal_candidates(&mut self, sc: Arc<StealCandidates>) {
        self.steal_candidates = Some(sc);
    }

    /// Set the per-piece write guards for steal/write race prevention.
    pub fn set_piece_write_guards(&mut self, pwg: Arc<PieceWriteGuards>) {
        self.piece_write_guards = Some(pwg);
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
                // Register in block maps for steal visibility.
                if let Some(ref bm) = self.block_maps {
                    bm.mark_requested(piece, block_idx);
                }
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
                // Mark block 0 as requested in block maps.
                if let Some(ref bm) = self.block_maps {
                    bm.mark_requested(piece, 0);
                }
                self.current_piece = Some(CurrentPiece {
                    piece,
                    next_block: 1,
                    total_blocks,
                });
                return Some(self.make_block_request(piece, 0));
            }
        }

        // 3. Steal path: find unrequested blocks in other peers' pieces.
        // Bound attempts to avoid spinning when queue has many exhausted pieces.
        const MAX_STEAL_ATTEMPTS: usize = 32;
        if let (Some(bm), Some(sc)) = (&self.block_maps, &self.steal_candidates) {
            for _ in 0..MAX_STEAL_ATTEMPTS {
                let Some(piece) = sc.pop() else { break };

                // Skip if peer doesn't have this piece.
                if !peer_bitfield.get(piece) {
                    sc.push(piece); // put it back for other peers
                    continue;
                }

                // Skip if piece is already Complete or Unwanted.
                let state = atomic_states.get(piece);
                if state == PieceState::Complete || state == PieceState::Unwanted {
                    continue; // don't put back — it's done
                }

                // M120: Skip if a write is in-flight for this piece.
                if let Some(ref pwg) = self.piece_write_guards
                    && !pwg.try_write(piece)
                {
                    sc.push(piece); // put back — write in progress
                    continue;
                }

                let total_blocks = self.lengths.chunks_in_piece(piece);

                // Find an unrequested block.
                if let Some(block_idx) = bm.next_unrequested(piece, total_blocks) {
                    // Atomically claim it.
                    let was_already_set = bm.mark_requested(piece, block_idx);
                    if was_already_set {
                        // Another peer beat us — put piece back and try again.
                        sc.push(piece);
                        continue;
                    }
                    // Check if piece still has more unrequested blocks.
                    if bm.next_unrequested(piece, total_blocks).is_some() {
                        sc.push(piece); // still has work — put back for others
                    }
                    // DON'T set current_piece — stolen blocks are one-at-a-time.
                    // (we don't "own" this piece, so we shouldn't dispatch sequential blocks)
                    return Some(self.make_block_request(piece, block_idx));
                }
                // No unrequested blocks — piece is fully requested, don't put back.
            }
        }

        None // all phases exhausted
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

/// Pre-allocated atomic bit arrays for tracking per-block request and receipt
/// status across all pieces.
///
/// Enables multiple peers to claim individual blocks within the same piece via
/// lock-free atomic operations. Each piece has two bit arrays (`requested` and
/// `received`), stored as flat `Vec<AtomicU64>` indexed by piece and block.
///
/// This is the core data structure for M103 per-block stealing: when a piece
/// is in steal mode, peers race to claim individual blocks via `mark_requested`
/// (atomic `fetch_or`), achieving zero-contention dispatch.
#[derive(Debug)]
pub(crate) struct BlockMaps {
    /// Bit array tracking which blocks have been requested (one bit per block).
    requested: Vec<AtomicU64>,
    /// Bit array tracking which blocks have been received (one bit per block).
    received: Vec<AtomicU64>,
    /// Number of `AtomicU64` words per piece. Computed as `ceil(max_blocks / 64)`.
    words_per_piece: u32,
}

#[allow(dead_code)]
impl BlockMaps {
    /// Create a new `BlockMaps` for the given torrent geometry.
    ///
    /// Pre-allocates atomic bit arrays for all pieces. Each piece gets
    /// `ceil(max_blocks_per_piece / 64)` words, where max blocks is derived
    /// from the first (full-size) piece.
    pub fn new(num_pieces: u32, lengths: &Lengths) -> Self {
        let max_blocks = if num_pieces == 0 {
            0
        } else {
            lengths.chunks_in_piece(0)
        };
        let words_per_piece = max_blocks.saturating_add(63) / 64;
        let total_words = (num_pieces as usize).saturating_mul(words_per_piece as usize);

        let mut requested = Vec::with_capacity(total_words);
        let mut received = Vec::with_capacity(total_words);
        for _ in 0..total_words {
            requested.push(AtomicU64::new(0));
            received.push(AtomicU64::new(0));
        }

        Self {
            requested,
            received,
            words_per_piece,
        }
    }

    /// Atomically mark a block as requested.
    ///
    /// Returns `true` if the bit was **already set** (another peer won the
    /// race). Returns `false` if this caller claimed the block.
    ///
    /// Uses `AtomicU64::fetch_or` on the appropriate word, then checks the
    /// old value's bit to determine whether we were first.
    pub fn mark_requested(&self, piece: u32, block: u32) -> bool {
        let (word_idx, bit_mask) = self.word_and_mask(piece, block);
        let old = self.requested[word_idx].fetch_or(bit_mask, Ordering::Relaxed);
        old & bit_mask != 0
    }

    /// Mark a block as received.
    pub fn mark_received(&self, piece: u32, block: u32) {
        let (word_idx, bit_mask) = self.word_and_mask(piece, block);
        self.received[word_idx].fetch_or(bit_mask, Ordering::Relaxed);
    }

    /// Find the first block index in `[0, total_blocks)` where the requested
    /// bit is **not** set.
    ///
    /// Returns `None` if all blocks have been requested.
    pub fn next_unrequested(&self, piece: u32, total_blocks: u32) -> Option<u32> {
        let base = (piece as usize).checked_mul(self.words_per_piece as usize)?;

        for block in 0..total_blocks {
            let word_offset = block / 64;
            let bit = block % 64;
            let word_idx = base.checked_add(word_offset as usize)?;
            let word = self.requested.get(word_idx)?.load(Ordering::Relaxed);
            if word & (1u64 << bit) == 0 {
                return Some(block);
            }
        }
        None
    }

    /// Check if all blocks in `[0, total_blocks)` have their received bit set.
    pub fn all_received(&self, piece: u32, total_blocks: u32) -> bool {
        let Some(base) = (piece as usize).checked_mul(self.words_per_piece as usize) else {
            return total_blocks == 0;
        };

        for block in 0..total_blocks {
            let word_offset = block / 64;
            let bit = block % 64;
            let Some(atom) = self.received.get(base + word_offset as usize) else {
                return false;
            };
            let word = atom.load(Ordering::Relaxed);
            if word & (1u64 << bit) == 0 {
                return false;
            }
        }
        true
    }

    /// Zero all bits for a piece (both requested and received arrays).
    ///
    /// Called on piece completion or hash failure to reset block tracking.
    pub fn clear(&self, piece: u32, total_blocks: u32) {
        let base = (piece as usize).saturating_mul(self.words_per_piece as usize);
        let num_words = total_blocks.saturating_add(63) / 64;

        for w in 0..num_words as usize {
            let idx = base + w;
            if let Some(atom) = self.requested.get(idx) {
                atom.store(0, Ordering::Relaxed);
            }
            if let Some(atom) = self.received.get(idx) {
                atom.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Compute the flat array index and bit mask for a (piece, block) pair.
    fn word_and_mask(&self, piece: u32, block: u32) -> (usize, u64) {
        let base = (piece as usize).saturating_mul(self.words_per_piece as usize);
        let word_offset = (block / 64) as usize;
        let idx = base + word_offset;
        debug_assert!(
            idx < self.requested.len(),
            "BlockMaps: piece={piece} block={block} out of range (idx={idx}, len={})",
            self.requested.len()
        );
        let bit = block % 64;
        (idx, 1u64 << bit)
    }
}

/// Shared queue of pieces available for block-level stealing.
///
/// Maintained by `TorrentActor`. When a piece has unrequested blocks available
/// for stealing (e.g., original owner disconnected mid-piece), it is pushed
/// here. Peer tasks pop from the front to find steal work.
///
/// Uses `parking_lot::Mutex` (not tokio) because the lock is never held across
/// an await point — operations are trivially fast (push/pop/linear scan).
#[derive(Debug)]
pub(crate) struct StealCandidates {
    inner: Mutex<VecDeque<u32>>,
    timing: crate::timed_lock::LockTimingSettings,
}

#[allow(dead_code)]
impl StealCandidates {
    /// Create an empty steal queue.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            timing: crate::timed_lock::LockTimingSettings::default(),
        }
    }

    /// Create an empty steal queue with custom lock timing.
    pub fn with_timing(timing: crate::timed_lock::LockTimingSettings) -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            timing,
        }
    }

    /// Add a piece to the back of the steal queue (no-op if already present).
    pub fn push(&self, piece: u32) {
        let mut guard = crate::timed_lock::TimedGuard::new(
            self.inner.lock(),
            &self.timing,
            "steal_candidates",
        );
        if !guard.contains(&piece) {
            guard.push_back(piece);
        }
    }

    /// Take a piece from the front of the steal queue.
    pub fn pop(&self) -> Option<u32> {
        let mut guard = crate::timed_lock::TimedGuard::new(
            self.inner.lock(),
            &self.timing,
            "steal_candidates",
        );
        guard.pop_front()
    }

    /// Remove a specific piece from the queue (linear scan).
    ///
    /// This is O(n) but acceptable because the steal queue is small — it only
    /// contains pieces that are partially downloaded and available for stealing.
    pub fn remove(&self, piece: u32) {
        let mut guard = crate::timed_lock::TimedGuard::new(
            self.inner.lock(),
            &self.timing,
            "steal_candidates",
        );
        if let Some(pos) = guard.iter().position(|&p| p == piece) {
            guard.remove(pos);
        }
    }
}

/// Per-piece write guards to prevent steal/write races.
///
/// The write path (synchronous pwrite in `on_piece_sync`) acquires a **read**
/// lock on the piece being written — multiple concurrent block writes to the
/// same piece are fine since they target different offsets.
///
/// The steal path (`next_block` Phase 3) uses `try_write()` — if any write
/// is in-flight for a piece, the exclusive lock fails and the steal skips it.
///
/// This inverted usage (read = active writes, write = steal exclusion check)
/// is intentional: it allows N concurrent block writes while giving the steal
/// path an O(1) way to detect and skip busy pieces.
#[derive(Debug)]
pub(crate) struct PieceWriteGuards {
    locks: Vec<parking_lot::RwLock<()>>,
}

impl PieceWriteGuards {
    /// Create guards for `num_pieces` pieces.
    pub fn new(num_pieces: u32) -> Self {
        Self {
            locks: (0..num_pieces)
                .map(|_| parking_lot::RwLock::new(()))
                .collect(),
        }
    }

    /// Acquire a read guard for the given piece (used by write path).
    ///
    /// Multiple concurrent block writes to the same piece are allowed.
    /// Returns `None` if piece index is out of bounds.
    #[inline]
    pub fn read(&self, piece: u32) -> Option<parking_lot::RwLockReadGuard<'_, ()>> {
        self.locks.get(piece as usize).map(|lock| lock.read())
    }

    /// Try to acquire an exclusive write lock (used by steal path).
    ///
    /// Returns `true` if no writes are in-flight for this piece (safe to steal).
    /// Returns `false` if any read guard is held (write in progress — skip).
    #[inline]
    pub fn try_write(&self, piece: u32) -> bool {
        self.locks
            .get(piece as usize)
            .is_some_and(|lock| lock.try_write().is_some())
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
        let states = AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        );
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
        let states = AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        );

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
        let states = AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        );

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
        let states = AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        );
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
        assert!(
            !first_three.contains(&b.piece),
            "should skip pre-reserved pieces"
        );
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
        assert_ne!(
            b.piece, contested_piece,
            "should skip piece reserved by another thread"
        );
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

        assert_eq!(
            reserved.len(),
            2,
            "two peers should get two different pieces"
        );
        assert_ne!(b1.piece, b2.piece);
    }

    #[test]
    fn integration_multi_peer_concurrent_dispatch() {
        use std::sync::Arc;

        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let availability = vec![2u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);

        // Simulate 4 peer tasks each trying to get a piece
        let reserved_pieces: Vec<u32> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let st = Arc::clone(&states);
                    let sn = Arc::clone(&snap);
                    let bf = peer_bf.clone();
                    let l = lengths.clone();
                    s.spawn(move || {
                        let mut ds = PeerDispatchState::new(sn, l);
                        ds.next_block(&bf, &st).map(|b| b.piece)
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .collect()
        });

        // All 4 pieces should be reserved by exactly one peer each
        let unique: std::collections::HashSet<u32> = reserved_pieces.iter().copied().collect();
        assert_eq!(unique.len(), 4, "all 4 pieces should be uniquely reserved");
        assert_eq!(reserved_pieces.len(), 4);
    }

    #[test]
    fn integration_snapshot_rebuild_during_dispatch() {
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

        // Get first piece
        let b0 = ds.next_block(&peer_bf, &states).unwrap();
        let first_piece = b0.piece;
        // Consume remaining block
        ds.next_block(&peer_bf, &states).unwrap();

        // Simulate: first piece completes, actor marks it Complete
        states.mark_complete(first_piece);

        // Actor rebuilds snapshot (Complete pieces excluded)
        let snap2 = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            2,
        ));
        ds.update_snapshot(snap2);

        // Peer should be able to continue dispatching remaining pieces
        let mut remaining = Vec::new();
        while let Some(b) = ds.next_block(&peer_bf, &states) {
            if b.begin == 0 {
                remaining.push(b.piece);
            }
        }

        // Should get the 3 remaining pieces (first_piece was Completed)
        assert_eq!(remaining.len(), 3);
        assert!(!remaining.contains(&first_piece));
    }

    #[test]
    fn integration_endgame_multi_reservation() {
        let lengths = test_lengths();
        let num_pieces = 2u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let availability = vec![3u32; num_pieces as usize];

        // Peer 1 reserves piece 0 normally
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);
        let mut ds1 = PeerDispatchState::new(Arc::clone(&snap), lengths.clone());
        let b1 = ds1.next_block(&peer_bf, &states).unwrap();
        assert_eq!(states.get(b1.piece), PieceState::Reserved);

        // Actor transitions to endgame
        states.transition_to_endgame(b1.piece);

        // Rebuild snapshot (Endgame pieces are included)
        let snap2 = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            2,
        ));

        // Peer 2 can also "reserve" the endgame piece
        let mut ds2 = PeerDispatchState::new(snap2, lengths.clone());
        let b2 = ds2.next_block(&peer_bf, &states).unwrap();

        // Both peers got pieces (peer 2 might get the endgame piece or the other one)
        // The key invariant: no deadlock, no panic, both get blocks
        assert!(b2.length > 0);
    }

    // ---- BlockMaps tests ----

    #[test]
    fn block_maps_mark_requested() {
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let bm = BlockMaps::new(4, &lengths);

        // First mark: we win the race (bit was not set).
        let was_set = bm.mark_requested(0, 0);
        assert!(
            !was_set,
            "first mark_requested should return false (we claimed it)"
        );

        // Second mark on same block: someone already claimed it.
        let was_set = bm.mark_requested(0, 0);
        assert!(
            was_set,
            "second mark_requested should return true (already set)"
        );
    }

    #[test]
    fn block_maps_mark_received() {
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let bm = BlockMaps::new(4, &lengths);

        // Not all received initially.
        assert!(!bm.all_received(0, 2));

        // Mark block 0 received — still not all.
        bm.mark_received(0, 0);
        assert!(!bm.all_received(0, 2));

        // Mark block 1 received — now all received.
        bm.mark_received(0, 1);
        assert!(bm.all_received(0, 2));
    }

    #[test]
    fn block_maps_next_unrequested() {
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let bm = BlockMaps::new(4, &lengths);

        // Initially block 0 is unrequested.
        assert_eq!(bm.next_unrequested(1, 2), Some(0));

        // Request block 0 — next unrequested is block 1.
        bm.mark_requested(1, 0);
        assert_eq!(bm.next_unrequested(1, 2), Some(1));

        // Request block 1 — no unrequested blocks remain.
        bm.mark_requested(1, 1);
        assert_eq!(bm.next_unrequested(1, 2), None);
    }

    #[test]
    fn block_maps_all_requested() {
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let bm = BlockMaps::new(4, &lengths);

        // Request all blocks in piece 2.
        bm.mark_requested(2, 0);
        bm.mark_requested(2, 1);

        // next_unrequested should return None.
        assert_eq!(bm.next_unrequested(2, 2), None);
    }

    #[test]
    fn block_maps_all_received() {
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let bm = BlockMaps::new(4, &lengths);

        // Mark all blocks in piece 3 as received.
        bm.mark_received(3, 0);
        bm.mark_received(3, 1);

        assert!(bm.all_received(3, 2));
    }

    #[test]
    fn block_maps_partial_received() {
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let bm = BlockMaps::new(4, &lengths);

        // Mark only block 0 of piece 1 as received.
        bm.mark_received(1, 0);

        assert!(
            !bm.all_received(1, 2),
            "should be false with only 1 of 2 blocks received"
        );
    }

    #[test]
    fn block_maps_clear_resets_bits() {
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let bm = BlockMaps::new(4, &lengths);

        // Set some bits.
        bm.mark_requested(0, 0);
        bm.mark_requested(0, 1);
        bm.mark_received(0, 0);

        // Clear piece 0.
        bm.clear(0, 2);

        // All bits should be zeroed.
        assert_eq!(bm.next_unrequested(0, 2), Some(0));
        assert!(!bm.all_received(0, 2));
        // mark_requested should succeed again (bit was cleared).
        assert!(!bm.mark_requested(0, 0));
    }

    #[test]
    fn block_maps_independent_pieces() {
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let bm = BlockMaps::new(4, &lengths);

        // Request all blocks in piece 0, none in piece 1.
        bm.mark_requested(0, 0);
        bm.mark_requested(0, 1);

        // Piece 0 fully requested, piece 1 untouched.
        assert_eq!(bm.next_unrequested(0, 2), None);
        assert_eq!(bm.next_unrequested(1, 2), Some(0));
    }

    // ---- StealCandidates tests ----

    #[test]
    fn steal_candidates_push_pop_fifo() {
        let sc = StealCandidates::new();

        sc.push(10);
        sc.push(20);
        sc.push(30);

        assert_eq!(sc.pop(), Some(10));
        assert_eq!(sc.pop(), Some(20));
        assert_eq!(sc.pop(), Some(30));
        assert_eq!(sc.pop(), None);
    }

    #[test]
    fn steal_candidates_remove() {
        let sc = StealCandidates::new();

        sc.push(5);
        sc.push(10);
        sc.push(15);

        // Remove middle element.
        sc.remove(10);

        assert_eq!(sc.pop(), Some(5));
        assert_eq!(sc.pop(), Some(15));
        assert_eq!(sc.pop(), None);
    }

    #[test]
    fn steal_candidates_remove_nonexistent() {
        let sc = StealCandidates::new();
        sc.push(42);

        // Removing a nonexistent element is a no-op.
        sc.remove(999);

        assert_eq!(sc.pop(), Some(42));
    }

    // ---- Phase 3 steal dispatch tests ----

    #[test]
    fn next_block_phase3_steal() {
        // Peer A reserves piece 0 (via Phase 2 CAS). Peer B has Phase 2 exhausted.
        // Peer B should enter Phase 3 and steal an unrequested block from piece 0.
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));

        // Peer A reserves piece 0 via CAS.
        assert!(states.try_reserve(0));

        // Set up block maps and mark block 0 of piece 0 as requested (peer A's work).
        let bm = Arc::new(BlockMaps::new(num_pieces, &lengths));
        bm.mark_requested(0, 0);

        // Put piece 0 in steal candidates.
        let sc = Arc::new(StealCandidates::new());
        sc.push(0);

        // Reserve all other pieces so peer B's Phase 2 is exhausted.
        assert!(states.try_reserve(1));
        assert!(states.try_reserve(2));
        assert!(states.try_reserve(3));

        // Build snapshot (all pieces are Reserved, so snapshot.order is empty).
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        assert!(
            snap.order.is_empty(),
            "all pieces reserved — snapshot should be empty"
        );

        let peer_bf = peer_has_all(num_pieces);

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);
        ds.set_block_maps(Arc::clone(&bm));
        ds.set_steal_candidates(Arc::clone(&sc));

        // Phase 2 is exhausted (empty snapshot). Phase 3 should steal block 1 of piece 0.
        let stolen = ds.next_block(&peer_bf, &states);
        assert!(
            stolen.is_some(),
            "Phase 3 should find an unrequested block to steal"
        );
        let stolen = stolen.expect("just checked is_some");
        assert_eq!(stolen.piece, 0, "should steal from piece 0");
        assert_eq!(stolen.begin, 16384, "should steal block 1 (offset 16384)");

        // current_piece should NOT be set (stolen blocks are one-at-a-time).
        assert!(
            ds.current_piece_index().is_none(),
            "steal should not set current_piece"
        );
    }

    #[test]
    fn next_block_steal_skips_complete() {
        // Put a piece in steal_candidates. Mark it Complete.
        // Phase 3 should skip it and return None.
        let lengths = test_lengths();
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));

        // Mark piece 0 complete.
        states.mark_complete(0);

        let bm = Arc::new(BlockMaps::new(num_pieces, &lengths));
        let sc = Arc::new(StealCandidates::new());
        sc.push(0); // complete piece in the steal queue

        // Reserve other pieces to exhaust Phase 2.
        assert!(states.try_reserve(1));
        assert!(states.try_reserve(2));
        assert!(states.try_reserve(3));

        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));

        let peer_bf = peer_has_all(num_pieces);
        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);
        ds.set_block_maps(Arc::clone(&bm));
        ds.set_steal_candidates(Arc::clone(&sc));

        // Phase 3 should skip the complete piece.
        assert!(ds.next_block(&peer_bf, &states).is_none());

        // The complete piece should NOT have been put back.
        assert!(
            sc.pop().is_none(),
            "complete piece should not be returned to queue"
        );
    }

    #[test]
    fn next_block_steal_requires_peer_has() {
        // Put a piece in steal_candidates. Peer's bitfield does NOT have this piece.
        // Phase 3 should skip it and put it back for other peers.
        let lengths = test_lengths();
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));

        // Reserve piece 0 (so it's in Reserved state) and put it in steal queue.
        assert!(states.try_reserve(0));
        let bm = Arc::new(BlockMaps::new(num_pieces, &lengths));
        bm.mark_requested(0, 0); // simulate some blocks requested
        let sc = Arc::new(StealCandidates::new());
        sc.push(0);

        // Reserve other pieces to exhaust Phase 2.
        assert!(states.try_reserve(1));
        assert!(states.try_reserve(2));
        assert!(states.try_reserve(3));

        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));

        // Peer does NOT have piece 0.
        let mut peer_bf = Bitfield::new(num_pieces);
        peer_bf.set(1);
        peer_bf.set(2);
        peer_bf.set(3);
        // peer_bf does NOT have piece 0

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);
        ds.set_block_maps(Arc::clone(&bm));
        ds.set_steal_candidates(Arc::clone(&sc));

        // Phase 3 should skip piece 0 (peer doesn't have it).
        assert!(ds.next_block(&peer_bf, &states).is_none());

        // Piece 0 should have been put back for other peers.
        assert_eq!(
            sc.pop(),
            Some(0),
            "piece should be returned to queue for other peers"
        );
    }

    #[test]
    fn next_block_phase1_with_block_maps() {
        // Verify Phase 1 hot path works correctly when block_maps is set —
        // mark_requested should register the block.
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let bm = Arc::new(BlockMaps::new(num_pieces, &lengths));
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);
        ds.set_block_maps(Arc::clone(&bm));

        // First call: Phase 2 cold path — reserves a piece, returns block 0.
        let b0 = ds
            .next_block(&peer_bf, &states)
            .expect("should get block 0");
        let piece = b0.piece;

        // Block 0 should be marked in block_maps (set by Phase 2).
        assert!(
            bm.mark_requested(piece, 0),
            "block 0 should already be marked requested by Phase 2"
        );

        // Second call: Phase 1 hot path — returns block 1.
        let b1 = ds
            .next_block(&peer_bf, &states)
            .expect("should get block 1");
        assert_eq!(b1.piece, piece);
        assert_eq!(b1.begin, 16384);

        // Block 1 should be marked in block_maps (set by Phase 1).
        assert!(
            bm.mark_requested(piece, 1),
            "block 1 should already be marked requested by Phase 1"
        );
    }

    #[test]
    fn next_block_phase2_with_block_maps() {
        // Verify Phase 2 cold path marks block 0 in block_maps.
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));
        let bm = Arc::new(BlockMaps::new(num_pieces, &lengths));
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);

        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);
        ds.set_block_maps(Arc::clone(&bm));

        // Phase 2 cold path: reserves first piece, returns block 0.
        let b0 = ds
            .next_block(&peer_bf, &states)
            .expect("should get block from Phase 2");
        let piece = b0.piece;
        assert_eq!(b0.begin, 0, "first block should be at offset 0");

        // Block 0 should already be marked in block_maps by Phase 2.
        let was_set = bm.mark_requested(piece, 0);
        assert!(
            was_set,
            "Phase 2 should have marked block 0 as requested in block_maps"
        );

        // Block 1 should NOT be marked yet (Phase 1 hasn't dispatched it).
        let was_set = bm.mark_requested(piece, 1);
        assert!(
            !was_set,
            "block 1 should not be marked yet — Phase 1 hasn't run for it"
        );
    }

    #[test]
    fn no_steal_when_disabled() {
        // Scenario: Phase 2 is exhausted, steal candidates exist in the swarm,
        // but this peer's PeerDispatchState has block_maps=None (simulating
        // use_block_stealing=false). Phase 3 should NOT activate.
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));

        // Reserve all pieces so Phase 2 is exhausted.
        assert!(states.try_reserve(0));
        assert!(states.try_reserve(1));
        assert!(states.try_reserve(2));
        assert!(states.try_reserve(3));

        // Build an empty snapshot (all pieces reserved).
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        assert!(
            snap.order.is_empty(),
            "all pieces reserved — snapshot should be empty"
        );

        let peer_bf = peer_has_all(num_pieces);

        // Construct PeerDispatchState WITHOUT block_maps or steal_candidates
        // (simulates use_block_stealing=false — torrent.rs passes None).
        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);
        // Explicitly do NOT call ds.set_block_maps() or ds.set_steal_candidates().

        // Phase 2 exhausted, no Phase 3 possible → should return None.
        assert!(
            ds.next_block(&peer_bf, &states).is_none(),
            "next_block must return None when block stealing is disabled (no block_maps)"
        );
    }

    // ---- M103 integration tests: block-stealing components working together ----

    /// 4 pieces, 4 blocks per piece (256 KiB total, 64 KiB pieces, 16 KiB chunks).
    fn test_lengths_4blocks() -> Lengths {
        Lengths::new(256 * 1024, 64 * 1024, 16384)
    }

    #[test]
    fn steal_completes_slow_peer_piece() {
        // Peer A reserves piece 0, dispatches blocks 0 and 1 (out of 4).
        // Peer B has Phase 2 exhausted and enters Phase 3 to steal a block.
        let lengths = test_lengths_4blocks(); // 4 pieces, 4 blocks each
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));

        // Peer A reserves piece 0 via CAS.
        assert!(states.try_reserve(0));

        // Set up block maps: blocks 0 and 1 of piece 0 are already requested by peer A.
        let bm = Arc::new(BlockMaps::new(num_pieces, &lengths));
        bm.mark_requested(0, 0);
        bm.mark_requested(0, 1);

        // Put piece 0 in steal candidates.
        let sc = Arc::new(StealCandidates::new());
        sc.push(0);

        // Reserve all other pieces so peer B's Phase 2 is exhausted.
        assert!(states.try_reserve(1));
        assert!(states.try_reserve(2));
        assert!(states.try_reserve(3));

        // Build empty snapshot (all pieces Reserved).
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        assert!(snap.order.is_empty());

        let peer_bf = peer_has_all(num_pieces);

        let mut ds_b = PeerDispatchState::new(Arc::clone(&snap), lengths);
        ds_b.set_block_maps(Arc::clone(&bm));
        ds_b.set_steal_candidates(Arc::clone(&sc));

        // Peer B should steal block 2 (the first unrequested block).
        let stolen = ds_b
            .next_block(&peer_bf, &states)
            .expect("Phase 3 should steal a block from piece 0");
        assert_eq!(stolen.piece, 0, "should steal from piece 0");
        assert_eq!(
            stolen.begin,
            2 * 16384,
            "should steal block 2 (offset 32768)"
        );

        // Block 2 should already be claimed by Phase 3 steal.
        assert!(
            bm.mark_requested(0, 2),
            "block 2 should already be claimed (Phase 3 stole it)"
        );
    }

    #[test]
    fn steal_coexists_with_owner() {
        // Peer A owns piece 0 (dispatches blocks sequentially).
        // Peer B steals block 2 from piece 0.
        // Peer A continues and dispatches block 1 via Phase 1 hot path.
        // Both peers contribute blocks — no conflict.
        let lengths = test_lengths_4blocks(); // 4 pieces, 4 blocks each
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));

        let bm = Arc::new(BlockMaps::new(num_pieces, &lengths));
        let sc = Arc::new(StealCandidates::new());
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));
        let peer_bf = peer_has_all(num_pieces);

        // Peer A: set up with block maps, dispatch block 0 (Phase 2 cold path).
        let mut ds_a = PeerDispatchState::new(Arc::clone(&snap), lengths.clone());
        ds_a.set_block_maps(Arc::clone(&bm));

        let a_block0 = ds_a
            .next_block(&peer_bf, &states)
            .expect("peer A should get block 0");
        let piece = a_block0.piece;
        assert_eq!(a_block0.begin, 0);

        // Mark piece as a steal candidate (simulating actor seeing it partially done).
        sc.push(piece);

        // Reserve remaining pieces so peer B's Phase 2 is exhausted.
        for p in 0..num_pieces {
            if p != piece {
                let _ = states.try_reserve(p);
            }
        }

        // Rebuild snapshot (all Reserved now).
        let snap2 = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            2,
        ));

        // Peer B: Phase 2 exhausted, enters Phase 3 and steals a block.
        let mut ds_b = PeerDispatchState::new(Arc::clone(&snap2), lengths.clone());
        ds_b.set_block_maps(Arc::clone(&bm));
        ds_b.set_steal_candidates(Arc::clone(&sc));

        let b_stolen = ds_b
            .next_block(&peer_bf, &states)
            .expect("peer B should steal a block");
        assert_eq!(b_stolen.piece, piece, "peer B should steal from same piece");

        // Peer A continues: Phase 1 hot path dispatches block 1.
        let a_block1 = ds_a
            .next_block(&peer_bf, &states)
            .expect("peer A should get block 1 via hot path");
        assert_eq!(a_block1.piece, piece, "peer A should continue same piece");
        assert_eq!(a_block1.begin, 16384, "peer A should get block 1");

        // Verify all three blocks are marked in block_maps.
        assert!(
            bm.mark_requested(piece, 0),
            "block 0 should already be claimed (peer A Phase 2)"
        );
        assert!(
            bm.mark_requested(piece, 1),
            "block 1 should already be claimed (peer A Phase 1)"
        );
        // The stolen block (either 2 or 3) should also be claimed.
        let stolen_block_idx = b_stolen.begin / 16384;
        assert!(
            bm.mark_requested(piece, stolen_block_idx),
            "stolen block should already be claimed (peer B Phase 3)"
        );
    }

    #[test]
    fn steal_handles_duplicate_claim() {
        // Two peers race to steal the same block. Only one should succeed
        // (the other sees was_already_set=true from fetch_or).
        let lengths = test_lengths_4blocks(); // 4 pieces, 4 blocks each
        let num_pieces = 4u32;
        let bm = Arc::new(BlockMaps::new(num_pieces, &lengths));

        // Mark blocks 0, 1, 2 as already requested. Block 3 is the only
        // unrequested block — both threads will race for it.
        bm.mark_requested(0, 0);
        bm.mark_requested(0, 1);
        bm.mark_requested(0, 2);

        // Two threads race on mark_requested for block 3.
        let results: Vec<bool> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let bm_ref = Arc::clone(&bm);
                    s.spawn(move || bm_ref.mark_requested(0, 3))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread panicked"))
                .collect()
        });

        // Exactly one thread should have won the race (was_already_set=false).
        let winners = results.iter().filter(|&&was_set| !was_set).count();
        let losers = results.iter().filter(|&&was_set| was_set).count();
        assert_eq!(winners, 1, "exactly one thread should claim the block");
        assert_eq!(
            losers, 1,
            "exactly one thread should see it already claimed"
        );

        // Block 3 must be marked as requested now.
        assert!(
            bm.mark_requested(0, 3),
            "block 3 should be marked after the race"
        );
    }

    #[test]
    fn block_maps_concurrent_access() {
        // Spawn N threads, each calling next_unrequested and then mark_requested
        // on the same piece. Verify no panics, data races, or incorrect results.
        let lengths = test_lengths_4blocks(); // 4 pieces, 4 blocks each
        let num_pieces = 4u32;
        let total_blocks = lengths.chunks_in_piece(0);
        let bm = Arc::new(BlockMaps::new(num_pieces, &lengths));

        // Spawn 8 threads, each trying to claim a block from piece 0.
        let claimed: Vec<Option<u32>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let bm_ref = Arc::clone(&bm);
                    s.spawn(move || {
                        // Find an unrequested block and try to claim it.
                        if let Some(block_idx) = bm_ref.next_unrequested(0, total_blocks) {
                            let was_set = bm_ref.mark_requested(0, block_idx);
                            if !was_set {
                                return Some(block_idx);
                            }
                        }
                        None
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread panicked"))
                .collect()
        });

        // Collect successful claims.
        let successful: Vec<u32> = claimed.into_iter().flatten().collect();

        // With 4 blocks and 8 threads, at most 4 unique claims are possible.
        assert!(
            successful.len() <= total_blocks as usize,
            "cannot claim more blocks than exist: got {} claims for {} blocks",
            successful.len(),
            total_blocks
        );

        // All claimed block indices must be unique and in-range.
        let unique: std::collections::HashSet<u32> = successful.iter().copied().collect();
        assert_eq!(
            unique.len(),
            successful.len(),
            "all claimed blocks must be unique"
        );
        for &idx in &successful {
            assert!(
                idx < total_blocks,
                "claimed block index {idx} out of range (total={total_blocks})"
            );
        }
    }

    #[test]
    fn endgame_fallback_when_steal_disabled() {
        // When block_maps is None (steal disabled), Endgame pieces should
        // still be dispatchable via Phase 2 (try_reserve returns true for
        // Endgame state).
        let lengths = test_lengths(); // 4 pieces, 2 blocks each
        let num_pieces = 4u32;
        let states = Arc::new(AtomicPieceStates::new(
            num_pieces,
            &Bitfield::new(num_pieces),
            &all_pieces_wanted(num_pieces),
        ));

        // Reserve piece 0 normally, then transition it to Endgame.
        assert!(states.try_reserve(0));
        states.transition_to_endgame(0);

        // Reserve pieces 1 and 2 so they're not available.
        assert!(states.try_reserve(1));
        assert!(states.try_reserve(2));

        // Leave piece 3 Available.

        // Build snapshot: includes piece 0 (Endgame) and piece 3 (Available).
        let availability = vec![1u32; num_pieces as usize];
        let snap = Arc::new(AvailabilitySnapshot::build(
            &availability,
            &states,
            &BTreeSet::new(),
            1,
        ));

        let peer_bf = peer_has_all(num_pieces);

        // Create PeerDispatchState WITHOUT block_maps (steal disabled).
        let mut ds = PeerDispatchState::new(Arc::clone(&snap), lengths);

        // Collect all dispatched pieces.
        let mut dispatched_pieces = std::collections::HashSet::new();
        while let Some(b) = ds.next_block(&peer_bf, &states) {
            dispatched_pieces.insert(b.piece);
        }

        // Should have dispatched from Endgame piece 0 and Available piece 3.
        assert!(
            dispatched_pieces.contains(&0),
            "Endgame piece 0 should be dispatched via Phase 2"
        );
        assert!(
            dispatched_pieces.contains(&3),
            "Available piece 3 should be dispatched via Phase 2"
        );
        assert_eq!(
            dispatched_pieces.len(),
            2,
            "only pieces 0 (Endgame) and 3 (Available) should be dispatched"
        );
    }

    // ── M120: PieceWriteGuards tests ────────────────────────────────────

    #[test]
    fn per_piece_lock_blocks_steal_during_write() {
        let guards = PieceWriteGuards::new(4);
        // Simulate a write in progress on piece 1
        let _read_guard = guards.read(1).expect("piece 1 should exist");
        // Steal path should fail
        assert!(
            !guards.try_write(1),
            "try_write should fail when read guard is held"
        );
        // Other pieces should be unaffected
        assert!(
            guards.try_write(0),
            "piece 0 should be stealable"
        );
        assert!(
            guards.try_write(2),
            "piece 2 should be stealable"
        );
    }

    #[test]
    fn per_piece_lock_allows_steal_when_idle() {
        let guards = PieceWriteGuards::new(4);
        // No read guards held — all pieces stealable
        for piece in 0..4 {
            assert!(
                guards.try_write(piece),
                "piece {piece} should be stealable when idle"
            );
        }
        // Out of bounds returns false
        assert!(
            !guards.try_write(99),
            "out-of-bounds piece should return false"
        );
    }

    #[test]
    fn per_piece_lock_concurrent_writes_allowed() {
        let guards = PieceWriteGuards::new(4);
        // Multiple read guards on the same piece should succeed
        let _g1 = guards.read(2).expect("first read guard");
        let _g2 = guards.read(2).expect("second read guard");
        let _g3 = guards.read(2).expect("third read guard");
        // All held simultaneously — simulates 3 concurrent block writes
        // to the same piece (different offsets)
        assert!(
            !guards.try_write(2),
            "steal should be blocked with 3 concurrent writes"
        );
        // Drop one — still 2 read guards
        drop(_g3);
        assert!(
            !guards.try_write(2),
            "steal should still be blocked with 2 concurrent writes"
        );
    }

    #[test]
    fn steal_candidates_no_poison_after_panic() {
        // parking_lot Mutex does not poison — usable after a panic.
        let sc = StealCandidates::new();
        sc.push(42);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = sc.inner.lock();
            panic!("intentional panic while holding lock");
        }));
        assert!(result.is_err(), "should have panicked");

        // Lock should still be usable after panic
        sc.push(99);
        assert_eq!(sc.pop(), Some(42));
        assert_eq!(sc.pop(), Some(99));
        assert_eq!(sc.pop(), None);
    }
}
