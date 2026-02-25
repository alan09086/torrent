use std::collections::HashSet;

use ferrite_storage::Bitfield;

/// Rarest-first piece selector with per-piece availability tracking.
///
/// Tracks how many peers have each piece and selects the rarest piece
/// that a given peer has, we don't have, and is not already in flight.
/// Ties are broken by lowest index.
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
pub(crate) struct PieceSelector {
    availability: Vec<u32>,
    num_pieces: u32,
}

#[allow(dead_code)]
impl PieceSelector {
    /// Create a new selector with all availability counts at zero.
    pub fn new(num_pieces: u32) -> Self {
        Self {
            availability: vec![0; num_pieces as usize],
            num_pieces,
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
    pub fn pick(
        &self,
        peer_has: &Bitfield,
        we_have: &Bitfield,
        in_flight: &HashSet<u32>,
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

        // Should pick piece 1 (avail=1, lowest index among ties with piece 3)
        let picked = sel.pick(&peer_has, &we_have, &in_flight);
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

        // Should pick piece 2 (avail=2), since 0 and 1 are already had
        let picked = sel.pick(&peer_has, &we_have, &in_flight);
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

        // Piece 0 is rarest but in flight, should pick piece 1
        let picked = sel.pick(&peer_has, &we_have, &in_flight);
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

        // Zero availability means no peers reported having these pieces
        let picked = sel.pick(&peer_has, &we_have, &in_flight);
        assert_eq!(picked, None);

        // Also None when we have everything
        sel.availability[0] = 1;
        sel.availability[1] = 1;
        let mut we_have_all = Bitfield::new(4);
        for i in 0..4 {
            we_have_all.set(i);
        }
        let picked = sel.pick(&peer_has, &we_have_all, &in_flight);
        assert_eq!(picked, None);

        // Also None when peer has nothing
        let peer_empty = Bitfield::new(4);
        let we_have_none = Bitfield::new(4);
        let picked = sel.pick(&peer_empty, &we_have_none, &in_flight);
        assert_eq!(picked, None);
    }
}
