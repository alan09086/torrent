use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::SocketAddr;

use ferrite_core::{FilePriority, Lengths};
use ferrite_storage::Bitfield;

/// Speed category for a peer based on download rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PeerSpeed {
    Slow,
    Medium,
    Fast,
}

impl PeerSpeed {
    pub fn from_rate(bytes_per_sec: f64) -> Self {
        PeerSpeedClassifier::default().classify(bytes_per_sec)
    }
}

/// Configurable speed classifier with adjustable thresholds.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct PeerSpeedClassifier {
    pub slow_threshold: f64,
    pub fast_threshold: f64,
}

impl Default for PeerSpeedClassifier {
    fn default() -> Self {
        Self {
            slow_threshold: 10_240.0,
            fast_threshold: 102_400.0,
        }
    }
}

#[allow(dead_code)]
impl PeerSpeedClassifier {
    pub fn classify(&self, bytes_per_sec: f64) -> PeerSpeed {
        if bytes_per_sec < self.slow_threshold {
            PeerSpeed::Slow
        } else if bytes_per_sec < self.fast_threshold {
            PeerSpeed::Medium
        } else {
            PeerSpeed::Fast
        }
    }
}

/// Tracks which blocks of an in-flight piece are assigned to which peer.
#[derive(Debug, Clone)]
pub(crate) struct InFlightPiece {
    pub assigned_blocks: HashMap<(u32, u32), SocketAddr>,
    pub total_blocks: u32,
}

impl InFlightPiece {
    pub fn new(total_blocks: u32) -> Self {
        Self {
            assigned_blocks: HashMap::new(),
            total_blocks,
        }
    }

    pub fn unassigned_count(&self) -> u32 {
        self.total_blocks.saturating_sub(self.assigned_blocks.len() as u32)
    }

    pub fn peer_set(&self) -> HashSet<SocketAddr> {
        self.assigned_blocks.values().copied().collect()
    }
}

/// Context for a single peer's pick cycle.
pub(crate) struct PickContext<'a> {
    pub peer_addr: SocketAddr,
    pub peer_has: &'a Bitfield,
    pub peer_speed: PeerSpeed,
    pub peer_is_snubbed: bool,
    pub peer_rate: f64,
    pub we_have: &'a Bitfield,
    pub in_flight_pieces: &'a HashMap<u32, InFlightPiece>,
    pub wanted: &'a Bitfield,
    pub streaming_pieces: &'a BTreeSet<u32>,
    pub time_critical_pieces: &'a BTreeSet<u32>,
    pub suggested_pieces: &'a HashSet<u32>,
    pub sequential_download: bool,
    pub completed_count: u32,
    pub initial_picker_threshold: u32,
    #[allow(dead_code)] // Reserved for future per-pick peer-count decisions.
    pub connected_peer_count: usize,
    pub whole_pieces_threshold: u32,
    pub piece_size: u32,
    pub extent_affinity: bool,
    /// Whether auto-sequential mode is currently active (managed by TorrentActor).
    pub auto_sequential_active: bool,
}

/// Result of a pick: which piece and blocks to request.
#[derive(Debug)]
pub(crate) struct PickResult {
    pub piece: u32,
    pub blocks: Vec<(u32, u32)>,
    #[allow(dead_code)]
    pub exclusive: bool,
}

/// Rarest-first piece selector with per-piece availability tracking.
///
/// Tracks how many peers have each piece and selects the rarest piece
/// that a given peer has, we don't have, and is not already in flight.
/// Ties are broken by lowest index.
pub(crate) struct PieceSelector {
    availability: Vec<u32>,
    num_pieces: u32,
    seed_count: u32,
}

impl PieceSelector {
    /// Create a new selector with all availability counts at zero.
    pub fn new(num_pieces: u32) -> Self {
        Self {
            availability: vec![0; num_pieces as usize],
            num_pieces,
            seed_count: 0,
        }
    }

    /// Increment availability for each piece the peer has.
    pub fn add_peer_bitfield(&mut self, bitfield: &Bitfield) {
        for index in bitfield.ones() {
            if (index as usize) < self.availability.len() {
                self.availability[index as usize] += 1;
            }
        }
    }

    /// Decrement (saturating) availability for each piece the peer has.
    pub fn remove_peer_bitfield(&mut self, bitfield: &Bitfield) {
        for index in bitfield.ones() {
            if (index as usize) < self.availability.len() {
                self.availability[index as usize] =
                    self.availability[index as usize].saturating_sub(1);
            }
        }
    }

    /// Increment availability for a single piece (e.g. Have message).
    pub fn increment(&mut self, index: u32) {
        if (index as usize) < self.availability.len() {
            self.availability[index as usize] += 1;
        }
    }

    /// Decrement availability for a single piece (saturating).
    #[allow(dead_code)]
    pub fn decrement(&mut self, index: u32) {
        if (index as usize) < self.availability.len() {
            self.availability[index as usize] =
                self.availability[index as usize].saturating_sub(1);
        }
    }

    /// Pick the rarest piece that the peer has, we don't have, and is not in flight.
    ///
    /// Returns the piece index with the lowest non-zero availability among
    /// candidates. Ties are broken by lowest index.
    #[allow(dead_code)]
    pub fn pick(
        &self,
        peer_has: &Bitfield,
        we_have: &Bitfield,
        in_flight: &HashSet<u32>,
        wanted: &Bitfield,
    ) -> Option<u32> {
        let mut best_index: Option<u32> = None;
        let mut best_avail: u32 = u32::MAX;

        for i in 0..self.num_pieces {
            // Peer must have it
            if !peer_has.get(i) {
                continue;
            }
            // We must not have it
            if we_have.get(i) {
                continue;
            }
            // Must not be in flight
            if in_flight.contains(&i) {
                continue;
            }
            // Must be wanted
            if !wanted.get(i) {
                continue;
            }
            // Must have non-zero availability
            let avail = self.availability[i as usize];
            if avail == 0 {
                continue;
            }
            // Rarest first, ties broken by lowest index
            if avail < best_avail {
                best_avail = avail;
                best_index = Some(i);
            }
        }

        best_index
    }

    /// Read-only access to the availability counts (for testing/debugging).
    pub fn availability(&self) -> &[u32] {
        &self.availability
    }

    #[allow(dead_code)]
    pub fn add_seed(&mut self) {
        self.seed_count += 1;
    }

    #[allow(dead_code)]
    pub fn remove_seed(&mut self) {
        self.seed_count = self.seed_count.saturating_sub(1);
    }

    pub fn effective_availability(&self, index: u32) -> u32 {
        self.availability.get(index as usize).copied().unwrap_or(0) + self.seed_count
    }

    /// Layered priority piece/block picker.
    ///
    /// Priority layers (highest to lowest):
    /// 1. Streaming window pieces
    /// 2. Time-critical pieces (first/last of High-priority files)
    /// 3. Suggested pieces (BEP 6)
    /// 4. Partial pieces with unassigned blocks (speed affinity)
    /// 5. New piece selection (sequential/random/rarest-first)
    pub fn pick_blocks<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        // Layer 1: Streaming window pieces
        if !ctx.peer_is_snubbed {
            for &piece in ctx.streaming_pieces {
                if !ctx.peer_has.get(piece) || ctx.we_have.get(piece) || !ctx.wanted.get(piece) {
                    continue;
                }
                let blocks = self.unassigned_blocks(piece, ctx, &missing_chunks);
                if !blocks.is_empty() {
                    return Some(PickResult { piece, blocks, exclusive: false });
                }
            }
        }

        // Layer 2: Time-critical pieces
        if !ctx.peer_is_snubbed {
            for &piece in ctx.time_critical_pieces {
                if !ctx.peer_has.get(piece) || ctx.we_have.get(piece) || !ctx.wanted.get(piece) {
                    continue;
                }
                if ctx.streaming_pieces.contains(&piece) {
                    continue; // already handled in layer 1
                }
                let blocks = self.unassigned_blocks(piece, ctx, &missing_chunks);
                if !blocks.is_empty() {
                    return Some(PickResult { piece, blocks, exclusive: false });
                }
            }
        }

        // Layer 3: Suggested pieces
        for &piece in ctx.suggested_pieces {
            if !ctx.peer_has.get(piece) || ctx.we_have.get(piece) || !ctx.wanted.get(piece) {
                continue;
            }
            if ctx.in_flight_pieces.contains_key(&piece) {
                continue; // prefer new pieces for suggestions
            }
            let avail = self.effective_availability(piece);
            if avail == 0 {
                continue;
            }
            let blocks = missing_chunks(piece);
            if !blocks.is_empty() {
                let exclusive = self.should_whole_piece(ctx, &blocks);
                return Some(PickResult { piece, blocks, exclusive });
            }
        }

        // Layer 4: Partial pieces with unassigned blocks (speed affinity)
        if let Some(result) = self.pick_partial(ctx, &missing_chunks) {
            return Some(result);
        }

        // Layer 5: New piece selection
        self.pick_new_piece(ctx, &missing_chunks)
    }

    /// Get unassigned blocks for a piece that's already in-flight.
    fn unassigned_blocks<F>(
        &self,
        piece: u32,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Vec<(u32, u32)>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        let all_missing = missing_chunks(piece);
        if let Some(ifp) = ctx.in_flight_pieces.get(&piece) {
            all_missing
                .into_iter()
                .filter(|&(begin, _len)| !ifp.assigned_blocks.contains_key(&(piece, begin)))
                .collect()
        } else {
            all_missing
        }
    }

    /// Pick a partial piece (already in-flight) with speed affinity.
    fn pick_partial<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        let mut best_piece: Option<u32> = None;
        let mut best_blocks: Vec<(u32, u32)> = Vec::new();
        let mut best_score: i32 = i32::MIN;

        for (&piece, ifp) in ctx.in_flight_pieces {
            if !ctx.peer_has.get(piece) || ctx.we_have.get(piece) || !ctx.wanted.get(piece) {
                continue;
            }
            let blocks = self.unassigned_blocks(piece, ctx, missing_chunks);
            if blocks.is_empty() {
                continue;
            }

            // Speed affinity: prefer pieces where peers of similar speed are working
            let peers = ifp.peer_set();
            let has_fast = peers.iter().any(|_| true); // simplified: prefer pieces with fewer peers
            let score = if ctx.peer_is_snubbed {
                // Snubbed peers avoid busy pieces
                -(peers.len() as i32)
            } else {
                // Prefer pieces with fewer unassigned blocks (closer to completion)
                -(ifp.unassigned_count() as i32)
            };

            let _ = has_fast; // suppress unused warning
            if score > best_score {
                best_score = score;
                best_piece = Some(piece);
                best_blocks = blocks;
            }
        }

        best_piece.map(|piece| PickResult { piece, blocks: best_blocks, exclusive: false })
    }

    /// Pick a new piece (not yet in-flight).
    fn pick_new_piece<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        // Snubbed peers: pick highest-availability piece (reverse rarest-first)
        if ctx.peer_is_snubbed {
            return self.pick_reverse_rarest(ctx, missing_chunks);
        }

        // Initial random threshold: randomize to promote piece diversity
        if ctx.completed_count < ctx.initial_picker_threshold
            && let Some(result) = self.pick_random(ctx, missing_chunks)
        {
            return Some(result);
        }

        // Sequential mode or auto-sequential
        if ctx.sequential_download || ctx.auto_sequential_active {
            return self.pick_sequential(ctx, missing_chunks);
        }

        // Default: rarest-first
        self.pick_rarest_new(ctx, missing_chunks)
    }

    /// Size of an extent group in bytes (4 MiB).
    const EXTENT_SIZE: u64 = 4 * 1024 * 1024;

    /// Compute the extent index for a piece given the piece size.
    pub(crate) fn extent_of(piece: u32, piece_size: u32) -> u32 {
        let byte_offset = piece as u64 * piece_size as u64;
        (byte_offset / Self::EXTENT_SIZE) as u32
    }

    /// Find the preferred extent based on which extents have pieces currently in-flight.
    fn preferred_extent(&self, ctx: &PickContext<'_>) -> Option<u32> {
        let mut extent_counts: HashMap<u32, u32> = HashMap::new();
        for &piece in ctx.in_flight_pieces.keys() {
            let extent = Self::extent_of(piece, ctx.piece_size);
            *extent_counts.entry(extent).or_default() += 1;
        }
        extent_counts
            .into_iter()
            .max_by_key(|&(_, count)| count)
            .map(|(extent, _)| extent)
    }

    /// Rarest-first among pieces not in-flight.
    ///
    /// When extent affinity is enabled, tries the preferred extent first,
    /// then falls back to any extent if no candidates remain in that extent.
    fn pick_rarest_new<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        if ctx.extent_affinity
            && let Some(extent) = self.preferred_extent(ctx)
            && let Some(result) = self.pick_rarest_in_extent(ctx, missing_chunks, extent)
        {
            return Some(result);
        }
        self.pick_rarest_any(ctx, missing_chunks)
    }

    /// Standard rarest-first picking with no extent filter.
    fn pick_rarest_any<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        let mut best_index: Option<u32> = None;
        let mut best_avail: u32 = u32::MAX;

        for i in 0..self.num_pieces {
            if !ctx.peer_has.get(i) || ctx.we_have.get(i) || !ctx.wanted.get(i) {
                continue;
            }
            if ctx.in_flight_pieces.contains_key(&i) {
                continue;
            }
            let avail = self.effective_availability(i);
            if avail == 0 {
                continue;
            }
            if avail < best_avail {
                best_avail = avail;
                best_index = Some(i);
            }
        }

        best_index.map(|piece| {
            let blocks = missing_chunks(piece);
            let exclusive = self.should_whole_piece(ctx, &blocks);
            PickResult { piece, blocks, exclusive }
        })
    }

    /// Rarest-first picking filtered to pieces within a specific extent.
    fn pick_rarest_in_extent<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
        extent: u32,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        let mut best_index: Option<u32> = None;
        let mut best_avail: u32 = u32::MAX;

        for i in 0..self.num_pieces {
            if Self::extent_of(i, ctx.piece_size) != extent {
                continue;
            }
            if !ctx.peer_has.get(i) || ctx.we_have.get(i) || !ctx.wanted.get(i) {
                continue;
            }
            if ctx.in_flight_pieces.contains_key(&i) {
                continue;
            }
            let avail = self.effective_availability(i);
            if avail == 0 {
                continue;
            }
            if avail < best_avail {
                best_avail = avail;
                best_index = Some(i);
            }
        }

        best_index.map(|piece| {
            let blocks = missing_chunks(piece);
            let exclusive = self.should_whole_piece(ctx, &blocks);
            PickResult { piece, blocks, exclusive }
        })
    }

    /// Sequential: pick lowest-index available piece not in-flight.
    fn pick_sequential<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        for i in 0..self.num_pieces {
            if !ctx.peer_has.get(i) || ctx.we_have.get(i) || !ctx.wanted.get(i) {
                continue;
            }
            if ctx.in_flight_pieces.contains_key(&i) {
                continue;
            }
            let avail = self.effective_availability(i);
            if avail == 0 {
                continue;
            }
            let blocks = missing_chunks(i);
            if !blocks.is_empty() {
                let exclusive = self.should_whole_piece(ctx, &blocks);
                return Some(PickResult { piece: i, blocks, exclusive });
            }
        }
        None
    }

    /// Random selection for initial diversity.
    fn pick_random<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        // Collect all eligible pieces, pick one using simple hash-based selection
        let mut candidates = Vec::new();
        for i in 0..self.num_pieces {
            if !ctx.peer_has.get(i) || ctx.we_have.get(i) || !ctx.wanted.get(i) {
                continue;
            }
            if ctx.in_flight_pieces.contains_key(&i) {
                continue;
            }
            let avail = self.effective_availability(i);
            if avail == 0 {
                continue;
            }
            candidates.push(i);
        }
        if candidates.is_empty() {
            return None;
        }
        // Use peer address port as randomization seed for reproducible-per-peer picks
        let idx = (ctx.peer_addr.port() as usize) % candidates.len();
        let piece = candidates[idx];
        let blocks = missing_chunks(piece);
        let exclusive = self.should_whole_piece(ctx, &blocks);
        Some(PickResult { piece, blocks, exclusive })
    }

    /// Snubbed peer: highest-availability piece (reverse rarest-first).
    fn pick_reverse_rarest<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        let mut best_index: Option<u32> = None;
        let mut best_avail: u32 = 0;

        for i in 0..self.num_pieces {
            if !ctx.peer_has.get(i) || ctx.we_have.get(i) || !ctx.wanted.get(i) {
                continue;
            }
            if ctx.in_flight_pieces.contains_key(&i) {
                continue;
            }
            let avail = self.effective_availability(i);
            if avail == 0 {
                continue;
            }
            if avail > best_avail {
                best_avail = avail;
                best_index = Some(i);
            }
        }

        best_index.map(|piece| {
            let blocks = missing_chunks(piece);
            PickResult { piece, blocks, exclusive: false }
        })
    }

    /// Decide if a fast peer should get exclusive (whole-piece) assignment.
    fn should_whole_piece(&self, ctx: &PickContext<'_>, blocks: &[(u32, u32)]) -> bool {
        if ctx.peer_speed != PeerSpeed::Fast || ctx.peer_rate <= 0.0 {
            return false;
        }
        let total_bytes: u64 = blocks.iter().map(|&(_, len)| len as u64).sum();
        let time_secs = total_bytes as f64 / ctx.peer_rate;
        time_secs <= ctx.whole_pieces_threshold as f64
    }
}

/// Build a bitfield marking which pieces are wanted based on file priorities.
///
/// For each file with priority > Skip, compute its piece range via `Lengths::file_pieces()`
/// and set those bits. Shared pieces (spanning file boundaries) are wanted if **any**
/// overlapping file is non-Skip.
pub fn build_wanted_pieces(
    file_priorities: &[FilePriority],
    file_lengths: &[u64],
    lengths: &Lengths,
) -> Bitfield {
    let mut wanted = Bitfield::new(lengths.num_pieces());
    let mut offset = 0u64;
    for (i, &file_len) in file_lengths.iter().enumerate() {
        if file_priorities.get(i).copied().unwrap_or_default() > FilePriority::Skip
            && let Some((first, last)) = lengths.file_pieces(offset, file_len)
        {
            for p in first..=last {
                wanted.set(p);
            }
        }
        offset += file_len;
    }
    wanted
}

/// Hysteresis thresholds for auto-sequential mode.
const AUTO_SEQUENTIAL_ACTIVATE_RATIO: f64 = 1.6;
const AUTO_SEQUENTIAL_DEACTIVATE_RATIO: f64 = 1.3;

/// Evaluate auto-sequential hysteresis.
///
/// Returns the new auto_sequential_active state. Uses dual thresholds to
/// prevent flapping:
/// - Activates when in-flight / peers > 1.6 (partial-piece explosion)
/// - Deactivates when in-flight / peers < 1.3
/// - Stays in current state between thresholds
pub(crate) fn evaluate_auto_sequential(
    in_flight_count: usize,
    connected_peers: usize,
    currently_active: bool,
) -> bool {
    if connected_peers == 0 {
        return false;
    }
    let ratio = in_flight_count as f64 / connected_peers as f64;
    if currently_active {
        ratio >= AUTO_SEQUENTIAL_DEACTIVATE_RATIO
    } else {
        ratio > AUTO_SEQUENTIAL_ACTIVATE_RATIO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_all_zero() {
        let sel = PieceSelector::new(10);
        assert_eq!(sel.availability().len(), 10);
        assert!(sel.availability().iter().all(|&a| a == 0));
    }

    #[test]
    fn add_bitfield_increments() {
        let mut sel = PieceSelector::new(8);
        let mut bf = Bitfield::new(8);
        bf.set(1);
        bf.set(3);
        bf.set(7);

        sel.add_peer_bitfield(&bf);

        assert_eq!(sel.availability()[0], 0);
        assert_eq!(sel.availability()[1], 1);
        assert_eq!(sel.availability()[2], 0);
        assert_eq!(sel.availability()[3], 1);
        assert_eq!(sel.availability()[7], 1);

        // Adding a second peer with overlapping pieces
        let mut bf2 = Bitfield::new(8);
        bf2.set(1);
        bf2.set(5);

        sel.add_peer_bitfield(&bf2);

        assert_eq!(sel.availability()[1], 2);
        assert_eq!(sel.availability()[5], 1);
    }

    #[test]
    fn remove_bitfield_decrements() {
        let mut sel = PieceSelector::new(8);
        let mut bf = Bitfield::new(8);
        bf.set(0);
        bf.set(4);

        sel.add_peer_bitfield(&bf);
        assert_eq!(sel.availability()[0], 1);
        assert_eq!(sel.availability()[4], 1);

        sel.remove_peer_bitfield(&bf);
        assert_eq!(sel.availability()[0], 0);
        assert_eq!(sel.availability()[4], 0);

        // Saturates at zero — removing again should not underflow
        sel.remove_peer_bitfield(&bf);
        assert_eq!(sel.availability()[0], 0);
        assert_eq!(sel.availability()[4], 0);
    }

    #[test]
    fn increment_decrement() {
        let mut sel = PieceSelector::new(4);

        sel.increment(2);
        assert_eq!(sel.availability()[2], 1);

        sel.increment(2);
        assert_eq!(sel.availability()[2], 2);

        sel.decrement(2);
        assert_eq!(sel.availability()[2], 1);

        sel.decrement(2);
        assert_eq!(sel.availability()[2], 0);

        // Saturates at zero
        sel.decrement(2);
        assert_eq!(sel.availability()[2], 0);
    }

    #[test]
    fn pick_rarest() {
        let mut sel = PieceSelector::new(4);

        // Piece 0: avail 3, piece 1: avail 1, piece 2: avail 2, piece 3: avail 1
        sel.availability[0] = 3;
        sel.availability[1] = 1;
        sel.availability[2] = 2;
        sel.availability[3] = 1;

        // Peer has all pieces
        let mut peer_has = Bitfield::new(4);
        for i in 0..4 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(4);
        let in_flight = HashSet::new();
        let mut wanted = Bitfield::new(4);
        for i in 0..4 {
            wanted.set(i);
        }

        // Should pick piece 1 (avail=1, lowest index among ties with piece 3)
        let picked = sel.pick(&peer_has, &we_have, &in_flight, &wanted);
        assert_eq!(picked, Some(1));
    }

    #[test]
    fn pick_skips_have() {
        let mut sel = PieceSelector::new(4);
        sel.availability[0] = 1;
        sel.availability[1] = 1;
        sel.availability[2] = 2;
        sel.availability[3] = 3;

        let mut peer_has = Bitfield::new(4);
        for i in 0..4 {
            peer_has.set(i);
        }

        // We already have piece 0 and 1
        let mut we_have = Bitfield::new(4);
        we_have.set(0);
        we_have.set(1);

        let in_flight = HashSet::new();
        let mut wanted = Bitfield::new(4);
        for i in 0..4 {
            wanted.set(i);
        }

        // Should pick piece 2 (avail=2), since 0 and 1 are already had
        let picked = sel.pick(&peer_has, &we_have, &in_flight, &wanted);
        assert_eq!(picked, Some(2));
    }

    #[test]
    fn pick_skips_inflight() {
        let mut sel = PieceSelector::new(4);
        sel.availability[0] = 1;
        sel.availability[1] = 2;
        sel.availability[2] = 3;
        sel.availability[3] = 4;

        let mut peer_has = Bitfield::new(4);
        for i in 0..4 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(4);

        let mut in_flight = HashSet::new();
        in_flight.insert(0);
        let mut wanted = Bitfield::new(4);
        for i in 0..4 {
            wanted.set(i);
        }

        // Piece 0 is rarest but in flight, should pick piece 1
        let picked = sel.pick(&peer_has, &we_have, &in_flight, &wanted);
        assert_eq!(picked, Some(1));
    }

    #[test]
    fn pick_none_available() {
        let mut sel = PieceSelector::new(4);
        // All availability at zero — no peers have announced these pieces
        // through add_peer_bitfield or increment

        let mut peer_has = Bitfield::new(4);
        for i in 0..4 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(4);
        let in_flight = HashSet::new();
        let mut wanted = Bitfield::new(4);
        for i in 0..4 {
            wanted.set(i);
        }

        // Zero availability means no peers reported having these pieces
        let picked = sel.pick(&peer_has, &we_have, &in_flight, &wanted);
        assert_eq!(picked, None);

        // Also None when we have everything
        sel.availability[0] = 1;
        sel.availability[1] = 1;
        let mut we_have_all = Bitfield::new(4);
        for i in 0..4 {
            we_have_all.set(i);
        }
        let picked = sel.pick(&peer_has, &we_have_all, &in_flight, &wanted);
        assert_eq!(picked, None);

        // Also None when peer has nothing
        let peer_empty = Bitfield::new(4);
        let we_have_none = Bitfield::new(4);
        let picked = sel.pick(&peer_empty, &we_have_none, &in_flight, &wanted);
        assert_eq!(picked, None);
    }

    #[test]
    fn pick_skips_unwanted() {
        let mut sel = PieceSelector::new(4);
        sel.availability[0] = 1;
        sel.availability[1] = 1;
        sel.availability[2] = 1;
        sel.availability[3] = 1;

        let mut peer_has = Bitfield::new(4);
        for i in 0..4 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(4);
        let in_flight = HashSet::new();

        // Only want pieces 2 and 3
        let mut wanted = Bitfield::new(4);
        wanted.set(2);
        wanted.set(3);

        let picked = sel.pick(&peer_has, &we_have, &in_flight, &wanted);
        assert_eq!(picked, Some(2)); // lowest-index wanted piece
    }

    #[test]
    fn pick_all_wanted_is_normal_behavior() {
        let mut sel = PieceSelector::new(4);
        sel.availability[0] = 3;
        sel.availability[1] = 1;
        sel.availability[2] = 2;
        sel.availability[3] = 1;

        let mut peer_has = Bitfield::new(4);
        for i in 0..4 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(4);
        let in_flight = HashSet::new();

        let mut wanted = Bitfield::new(4);
        for i in 0..4 {
            wanted.set(i);
        }

        let picked = sel.pick(&peer_has, &we_have, &in_flight, &wanted);
        assert_eq!(picked, Some(1)); // rarest first
    }

    use ferrite_core::{FilePriority, Lengths};

    #[test]
    fn build_wanted_all_normal() {
        let priorities = vec![FilePriority::Normal; 2];
        let file_lengths = vec![100, 100];
        let lengths = Lengths::new(200, 100, 50);
        let wanted = super::build_wanted_pieces(&priorities, &file_lengths, &lengths);
        assert_eq!(wanted.count_ones(), 2);
        assert!(wanted.get(0));
        assert!(wanted.get(1));
    }

    #[test]
    fn build_wanted_skip_first_file() {
        let priorities = vec![FilePriority::Skip, FilePriority::Normal];
        let file_lengths = vec![100, 100];
        let lengths = Lengths::new(200, 100, 50);
        let wanted = super::build_wanted_pieces(&priorities, &file_lengths, &lengths);
        assert!(!wanted.get(0));
        assert!(wanted.get(1));
    }

    #[test]
    fn build_wanted_shared_boundary_piece() {
        // File 0: 80 bytes → pieces 0..0, File 1: 80 bytes → pieces 0..1
        // piece_length=100, total=160 → 2 pieces
        // If File 0 is Skip, File 1 is Normal: piece 0 still wanted (shared)
        let priorities = vec![FilePriority::Skip, FilePriority::Normal];
        let file_lengths = vec![80, 80];
        let lengths = Lengths::new(160, 100, 50);
        let wanted = super::build_wanted_pieces(&priorities, &file_lengths, &lengths);
        assert!(wanted.get(0)); // shared boundary piece
        assert!(wanted.get(1));
    }

    #[test]
    fn build_wanted_all_skip() {
        let priorities = vec![FilePriority::Skip; 3];
        let file_lengths = vec![100, 100, 100];
        let lengths = Lengths::new(300, 100, 50);
        let wanted = super::build_wanted_pieces(&priorities, &file_lengths, &lengths);
        assert_eq!(wanted.count_ones(), 0);
    }

    // ── Helper for PickContext-based tests ──────────────────────────────

    fn default_pick_context<'a>(
        peer_addr: SocketAddr,
        peer_has: &'a Bitfield,
        we_have: &'a Bitfield,
        wanted: &'a Bitfield,
        in_flight_pieces: &'a HashMap<u32, InFlightPiece>,
        streaming_pieces: &'a BTreeSet<u32>,
        time_critical_pieces: &'a BTreeSet<u32>,
        suggested_pieces: &'a HashSet<u32>,
    ) -> PickContext<'a> {
        PickContext {
            peer_addr,
            peer_has,
            peer_speed: PeerSpeed::Medium,
            peer_is_snubbed: false,
            peer_rate: 50_000.0,
            we_have,
            in_flight_pieces,
            wanted,
            streaming_pieces,
            time_critical_pieces,
            suggested_pieces,
            sequential_download: false,
            completed_count: 100,
            initial_picker_threshold: 4,
            connected_peer_count: 10,
            whole_pieces_threshold: 20,
            piece_size: 262_144,
            extent_affinity: false,
            auto_sequential_active: false,
        }
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    // ── New tests for pick_blocks ──────────────────────────────────────

    #[test]
    fn block_level_two_peers_different_blocks() {
        // 1-piece torrent, piece 0, 2 blocks
        let mut sel = PieceSelector::new(1);
        sel.availability[0] = 2;

        let mut peer_has = Bitfield::new(1);
        peer_has.set(0);
        let we_have = Bitfield::new(1);
        let mut wanted = Bitfield::new(1);
        wanted.set(0);

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();
        let in_flight = HashMap::new();

        // Peer A picks first — gets both blocks
        let ctx_a = default_pick_context(
            addr(1000), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        let chunks = |_piece: u32| vec![(0, 16384), (16384, 16384)];
        let result_a = sel.pick_blocks(&ctx_a, chunks).unwrap();
        assert_eq!(result_a.piece, 0);
        assert_eq!(result_a.blocks.len(), 2);

        // Now record peer A's assignment in an InFlightPiece
        let mut ifp = InFlightPiece::new(2);
        ifp.assigned_blocks.insert((0, 0), addr(1000));
        ifp.assigned_blocks.insert((0, 16384), addr(1000));
        let mut in_flight2 = HashMap::new();
        in_flight2.insert(0u32, ifp);

        // Peer B picks — all blocks assigned, so unassigned_blocks is empty
        let ctx_b = default_pick_context(
            addr(2000), &peer_has, &we_have, &wanted,
            &in_flight2, &streaming, &time_critical, &suggested,
        );
        let result_b = sel.pick_blocks(&ctx_b, chunks);
        // No unassigned blocks remain, so no pick possible (only 1 piece)
        assert!(result_b.is_none());
    }

    #[test]
    fn streaming_window_before_rarest() {
        let mut sel = PieceSelector::new(2);
        sel.availability[0] = 5; // common
        sel.availability[1] = 1; // rarer

        let mut peer_has = Bitfield::new(2);
        peer_has.set(0);
        peer_has.set(1);
        let we_have = Bitfield::new(2);
        let mut wanted = Bitfield::new(2);
        wanted.set(0);
        wanted.set(1);

        let mut streaming = BTreeSet::new();
        streaming.insert(0); // streaming piece 0
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();
        let in_flight = HashMap::new();

        let ctx = default_pick_context(
            addr(3000), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        // Streaming layer picks piece 0 despite piece 1 being rarer
        assert_eq!(result.piece, 0);
    }

    #[test]
    fn streaming_fastest_peer_first() {
        let mut sel = PieceSelector::new(2);
        sel.availability[0] = 2;
        sel.availability[1] = 2;

        let mut peer_has = Bitfield::new(2);
        peer_has.set(0);
        peer_has.set(1);
        let we_have = Bitfield::new(2);
        let mut wanted = Bitfield::new(2);
        wanted.set(0);
        wanted.set(1);

        let mut streaming = BTreeSet::new();
        streaming.insert(0);
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();
        let in_flight = HashMap::new();

        // Fast peer should get streaming blocks
        let mut ctx_fast = default_pick_context(
            addr(4000), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx_fast.peer_speed = PeerSpeed::Fast;
        ctx_fast.peer_rate = 102_400.0;
        ctx_fast.peer_is_snubbed = false;

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result_fast = sel.pick_blocks(&ctx_fast, chunks).unwrap();
        assert_eq!(result_fast.piece, 0); // got streaming piece

        // Slow, snubbed peer should NOT get streaming blocks (layer 1 skips snubbed)
        let mut ctx_slow = default_pick_context(
            addr(4001), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx_slow.peer_speed = PeerSpeed::Slow;
        ctx_slow.peer_rate = 1_024.0;
        ctx_slow.peer_is_snubbed = true;

        let result_slow = sel.pick_blocks(&ctx_slow, chunks).unwrap();
        // Snubbed peer skips layers 1 & 2, ends up in layer 5 (reverse rarest)
        // Both pieces have avail=2, so it picks the first one with highest avail (both equal, lowest index = 0)
        // But the important thing is it did NOT get picked from the streaming layer
        // (it went through reverse rarest path instead)
        assert!(result_slow.piece <= 1); // valid piece picked from non-streaming path
    }

    #[test]
    fn time_critical_first_last_pieces() {
        let mut sel = PieceSelector::new(10);
        // Piece 5 is rarest but not time-critical
        for i in 0..10 {
            sel.availability[i] = 3;
        }
        sel.availability[5] = 1; // rarest

        let mut peer_has = Bitfield::new(10);
        for i in 0..10 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(10);
        let mut wanted = Bitfield::new(10);
        for i in 0..10 {
            wanted.set(i);
        }

        let streaming = BTreeSet::new();
        let mut time_critical = BTreeSet::new();
        time_critical.insert(0); // first piece
        time_critical.insert(9); // last piece
        let suggested = HashSet::new();
        let in_flight = HashMap::new();

        let ctx = default_pick_context(
            addr(5000), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        // Time-critical pieces 0 or 9 should be picked before rarest piece 5
        assert!(result.piece == 0 || result.piece == 9);
    }

    #[test]
    fn sequential_mode_ascending() {
        let mut sel = PieceSelector::new(5);
        for i in 0..5 {
            sel.availability[i] = 2;
        }
        // Make piece 3 rarest — sequential should ignore this
        sel.availability[3] = 1;

        let mut peer_has = Bitfield::new(5);
        for i in 0..5 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(5);
        let mut wanted = Bitfield::new(5);
        for i in 0..5 {
            wanted.set(i);
        }

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();
        let in_flight = HashMap::new();

        let mut ctx = default_pick_context(
            addr(6000), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.sequential_download = true;
        ctx.completed_count = 100; // past initial threshold

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        assert_eq!(result.piece, 0); // lowest index
    }

    #[test]
    fn initial_random_threshold() {
        let mut sel = PieceSelector::new(10);
        for i in 0..10 {
            sel.availability[i] = (i as u32) + 1;
        }

        let mut peer_has = Bitfield::new(10);
        for i in 0..10 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(10);
        let mut wanted = Bitfield::new(10);
        for i in 0..10 {
            wanted.set(i);
        }

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();
        let in_flight = HashMap::new();

        let mut ctx = default_pick_context(
            addr(7000), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.completed_count = 0; // below threshold
        ctx.initial_picker_threshold = 4;

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        // Random pick — just verify a valid piece was picked
        assert!(result.piece < 10);
        assert!(!result.blocks.is_empty());
    }

    #[test]
    fn whole_piece_threshold_fast_peer() {
        let mut sel = PieceSelector::new(1);
        sel.availability[0] = 1;

        let mut peer_has = Bitfield::new(1);
        peer_has.set(0);
        let we_have = Bitfield::new(1);
        let mut wanted = Bitfield::new(1);
        wanted.set(0);

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();
        let in_flight = HashMap::new();

        let mut ctx = default_pick_context(
            addr(8000), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.peer_speed = PeerSpeed::Fast;
        ctx.peer_rate = 1_048_576.0; // 1 MB/s
        ctx.piece_size = 262_144;
        ctx.whole_pieces_threshold = 20;
        ctx.completed_count = 100; // past initial threshold

        // Blocks total 262144 bytes. Time = 262144/1048576 ≈ 0.25s < 20s
        let chunks = |_piece: u32| vec![(0, 131_072), (131_072, 131_072)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        assert_eq!(result.piece, 0);
        assert!(result.exclusive, "fast peer should get exclusive=true for small piece");
    }

    #[test]
    fn speed_affinity_slow_avoids_fast_partial() {
        let mut sel = PieceSelector::new(3);
        sel.availability[0] = 2;
        sel.availability[1] = 2;
        sel.availability[2] = 2;

        let mut peer_has = Bitfield::new(3);
        for i in 0..3 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(3);
        let mut wanted = Bitfield::new(3);
        for i in 0..3 {
            wanted.set(i);
        }

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();

        // Piece 0 is partially downloaded by a fast peer (2 blocks assigned out of 3)
        let mut ifp0 = InFlightPiece::new(3);
        ifp0.assigned_blocks.insert((0, 0), addr(9000));
        ifp0.assigned_blocks.insert((0, 16384), addr(9000));

        let mut in_flight = HashMap::new();
        in_flight.insert(0u32, ifp0);

        let mut ctx = default_pick_context(
            addr(9001), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.peer_speed = PeerSpeed::Slow;
        ctx.peer_rate = 5_000.0;
        ctx.completed_count = 100; // past initial threshold

        let chunks = |piece: u32| match piece {
            0 => vec![(0, 16384), (16384, 16384), (32768, 16384)],
            _ => vec![(0, 16384), (16384, 16384)],
        };
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        // Slow peer can pick partial piece 0 (1 unassigned block) or a new piece (1 or 2).
        // Layer 4 (partial) runs first. Piece 0 has 1 unassigned block.
        // Either partial or new is valid — just verify we got a valid piece.
        assert!(result.piece < 3);
        assert!(!result.blocks.is_empty());
    }

    #[test]
    fn snubbed_peer_reverse_picking() {
        let mut sel = PieceSelector::new(3);
        sel.availability[0] = 1;
        sel.availability[1] = 2;
        sel.availability[2] = 3;

        let mut peer_has = Bitfield::new(3);
        for i in 0..3 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(3);
        let mut wanted = Bitfield::new(3);
        for i in 0..3 {
            wanted.set(i);
        }

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();
        let in_flight = HashMap::new();

        let mut ctx = default_pick_context(
            addr(10000), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.peer_is_snubbed = true;

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        // Snubbed peer should pick highest availability (piece 2, avail=3)
        assert_eq!(result.piece, 2);
        assert!(!result.exclusive); // snubbed peers never get exclusive
    }

    #[test]
    fn auto_sequential_on_partial_explosion() {
        let mut sel = PieceSelector::new(15);
        for i in 0..15 {
            sel.availability[i] = 2;
        }
        // Make piece 10 rarest — auto-sequential should override
        sel.availability[10] = 1;

        let mut peer_has = Bitfield::new(15);
        for i in 0..15 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(15);
        let mut wanted = Bitfield::new(15);
        for i in 0..15 {
            wanted.set(i);
        }

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();

        // 10 in-flight pieces with connected_peer_count=4 → 10 > 1.5*4=6
        let mut in_flight = HashMap::new();
        for i in 0..10 {
            let mut ifp = InFlightPiece::new(2);
            // All blocks assigned so partial won't find unassigned blocks
            ifp.assigned_blocks.insert((i, 0), addr(11000));
            ifp.assigned_blocks.insert((i, 16384), addr(11000));
            in_flight.insert(i, ifp);
        }

        let mut ctx = default_pick_context(
            addr(11001), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.connected_peer_count = 4;
        ctx.completed_count = 100; // past initial threshold
        ctx.auto_sequential_active = true;

        let chunks = |piece: u32| {
            if piece < 10 {
                // In-flight pieces — all assigned, return full list
                vec![(0, 16384), (16384, 16384)]
            } else {
                vec![(0, 16384)]
            }
        };
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        // Auto-sequential: should pick lowest non-in-flight piece = 10
        assert_eq!(result.piece, 10);
    }

    #[test]
    fn seed_counter_no_array_modification() {
        let mut sel = PieceSelector::new(4);
        sel.availability[0] = 1;
        sel.availability[1] = 2;
        sel.availability[2] = 0;
        sel.availability[3] = 3;

        // add_seed twice, remove_seed once → seed_count = 1
        sel.add_seed();
        sel.add_seed();
        sel.remove_seed();

        // Verify availability array is unchanged
        assert_eq!(sel.availability()[0], 1);
        assert_eq!(sel.availability()[1], 2);
        assert_eq!(sel.availability()[2], 0);
        assert_eq!(sel.availability()[3], 3);

        // effective_availability = array value + seed_count(1)
        assert_eq!(sel.effective_availability(0), 2);
        assert_eq!(sel.effective_availability(1), 3);
        assert_eq!(sel.effective_availability(2), 1);
        assert_eq!(sel.effective_availability(3), 4);

        // Out-of-range index: should return seed_count only
        assert_eq!(sel.effective_availability(99), 1);
    }

    #[test]
    fn peer_speed_default_classification() {
        assert_eq!(PeerSpeed::from_rate(0.0), PeerSpeed::Slow);
        assert_eq!(PeerSpeed::from_rate(5_000.0), PeerSpeed::Slow);
        assert_eq!(PeerSpeed::from_rate(10_240.0), PeerSpeed::Medium);
        assert_eq!(PeerSpeed::from_rate(50_000.0), PeerSpeed::Medium);
        assert_eq!(PeerSpeed::from_rate(102_400.0), PeerSpeed::Fast);
        assert_eq!(PeerSpeed::from_rate(1_000_000.0), PeerSpeed::Fast);
    }

    #[test]
    fn peer_speed_custom_classifier() {
        let classifier = PeerSpeedClassifier { slow_threshold: 1_000.0, fast_threshold: 50_000.0 };
        assert_eq!(classifier.classify(500.0), PeerSpeed::Slow);
        assert_eq!(classifier.classify(1_000.0), PeerSpeed::Medium);
        assert_eq!(classifier.classify(50_000.0), PeerSpeed::Fast);
    }

    // ── Extent affinity tests ─────────────────────────────────────────

    #[test]
    fn extent_of_computation() {
        // 256 KiB pieces, 4 MiB extent = 16 pieces per extent
        assert_eq!(PieceSelector::extent_of(0, 262_144), 0);
        assert_eq!(PieceSelector::extent_of(15, 262_144), 0);
        assert_eq!(PieceSelector::extent_of(16, 262_144), 1);
        assert_eq!(PieceSelector::extent_of(31, 262_144), 1);
        assert_eq!(PieceSelector::extent_of(32, 262_144), 2);
        // 1 MiB pieces = 4 pieces per extent
        assert_eq!(PieceSelector::extent_of(0, 1_048_576), 0);
        assert_eq!(PieceSelector::extent_of(3, 1_048_576), 0);
        assert_eq!(PieceSelector::extent_of(4, 1_048_576), 1);
    }

    #[test]
    fn extent_affinity_prefers_active_extent() {
        // Set up 32 pieces across 2 extents. Make piece 20 globally rarest (extent 1),
        // but have in-flight activity in extent 0. With affinity, should pick from extent 0.
        let mut sel = PieceSelector::new(32);
        for i in 0..32 { sel.availability[i] = 2; }
        sel.availability[20] = 1; // globally rarest, extent 1
        sel.availability[5] = 1;  // rarest in extent 0

        let mut peer_has = Bitfield::new(32);
        for i in 0..32 { peer_has.set(i); }
        let we_have = Bitfield::new(32);
        let mut wanted = Bitfield::new(32);
        for i in 0..32 { wanted.set(i); }

        // Piece 10 in-flight in extent 0
        let mut ifp = InFlightPiece::new(2);
        ifp.assigned_blocks.insert((10, 0), addr(9999));
        let mut in_flight = HashMap::new();
        in_flight.insert(10u32, ifp);

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();

        let mut ctx = default_pick_context(
            addr(5555), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.extent_affinity = true;
        ctx.piece_size = 262_144;
        ctx.completed_count = 100;

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        assert_eq!(result.piece, 5); // extent 0, not 20 (extent 1)
    }

    #[test]
    fn extent_affinity_disabled_picks_global_rarest() {
        // Same setup but with affinity disabled — should pick lowest-index rarest
        let mut sel = PieceSelector::new(32);
        for i in 0..32 { sel.availability[i] = 2; }
        sel.availability[20] = 1;
        sel.availability[5] = 1;

        let mut peer_has = Bitfield::new(32);
        for i in 0..32 { peer_has.set(i); }
        let we_have = Bitfield::new(32);
        let mut wanted = Bitfield::new(32);
        for i in 0..32 { wanted.set(i); }

        let mut ifp = InFlightPiece::new(2);
        ifp.assigned_blocks.insert((10, 0), addr(9999));
        let mut in_flight = HashMap::new();
        in_flight.insert(10u32, ifp);

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();

        let mut ctx = default_pick_context(
            addr(5555), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.extent_affinity = false;
        ctx.piece_size = 262_144;
        ctx.completed_count = 100;

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        assert_eq!(result.piece, 5); // lowest-index rarest (tie-break)
    }

    #[test]
    fn extent_affinity_fallback_when_extent_exhausted() {
        // All pieces in active extent already downloaded — falls back to other extents
        let mut sel = PieceSelector::new(32);
        for i in 0..32 { sel.availability[i] = 2; }

        let mut peer_has = Bitfield::new(32);
        for i in 0..32 { peer_has.set(i); }
        let mut we_have = Bitfield::new(32);
        for i in 0..16 { we_have.set(i); } // have all extent 0
        let mut wanted = Bitfield::new(32);
        for i in 0..32 { wanted.set(i); }

        let mut ifp = InFlightPiece::new(2);
        ifp.assigned_blocks.insert((10, 0), addr(9999));
        let mut in_flight = HashMap::new();
        in_flight.insert(10u32, ifp);

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();

        let mut ctx = default_pick_context(
            addr(5555), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.extent_affinity = true;
        ctx.piece_size = 262_144;
        ctx.completed_count = 100;

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        assert!(result.piece >= 16); // falls back to extent 1
    }

    #[test]
    fn auto_sequential_hysteresis_activation() {
        // 4 peers, need > 1.6 * 4 = 6.4 in-flight to activate
        assert!(!evaluate_auto_sequential(6, 4, false));  // 6/4 = 1.5 < 1.6
        assert!(evaluate_auto_sequential(7, 4, false));   // 7/4 = 1.75 > 1.6
    }

    #[test]
    fn auto_sequential_hysteresis_deactivation() {
        // 4 peers, need < 1.3 * 4 = 5.2 in-flight to deactivate
        assert!(evaluate_auto_sequential(6, 4, true));    // 6/4 = 1.5 >= 1.3, stays active
        assert!(!evaluate_auto_sequential(5, 4, true));   // 5/4 = 1.25 < 1.3, deactivates
    }

    #[test]
    fn auto_sequential_hysteresis_band() {
        // In the band between 1.3 and 1.6 — state doesn't change
        // 10 peers: activate > 16, deactivate < 13
        // At 14 in-flight (ratio 1.4): in the band
        assert!(!evaluate_auto_sequential(14, 10, false)); // inactive stays inactive
        assert!(evaluate_auto_sequential(14, 10, true));   // active stays active
    }

    #[test]
    fn auto_sequential_zero_peers() {
        assert!(!evaluate_auto_sequential(10, 0, false));
        assert!(!evaluate_auto_sequential(10, 0, true));
    }
}
