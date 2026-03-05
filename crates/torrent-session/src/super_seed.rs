//! BEP 16 Super Seeding state tracker.
//!
//! Reveals pieces one-per-peer to maximize piece diversity across the swarm.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use torrent_storage::Bitfield;

/// Tracks super-seed piece assignments and propagation.
pub(crate) struct SuperSeedState {
    /// Piece index currently assigned (revealed) to each peer.
    peer_assignments: HashMap<SocketAddr, u32>,
    /// Pieces that have been confirmed propagated (seen back via Have from another peer).
    propagated: HashSet<u32>,
}

impl SuperSeedState {
    pub fn new() -> Self {
        Self {
            peer_assignments: HashMap::new(),
            propagated: HashSet::new(),
        }
    }

    /// Pick the globally rarest piece that the peer doesn't have, hasn't been
    /// propagated, and isn't already assigned to another peer.
    /// Ties broken by lowest index.
    pub fn assign_piece(
        &mut self,
        peer: SocketAddr,
        peer_bitfield: &Bitfield,
        availability: &[u32],
        num_pieces: u32,
    ) -> Option<u32> {
        let assigned_pieces: HashSet<u32> = self.peer_assignments.values().copied().collect();

        let mut best: Option<(u32, u32)> = None; // (index, availability)

        for idx in 0..num_pieces {
            // Skip pieces the peer already has
            if peer_bitfield.get(idx) {
                continue;
            }
            // Skip already-propagated pieces
            if self.propagated.contains(&idx) {
                continue;
            }
            // Skip pieces assigned to another peer
            if assigned_pieces.contains(&idx) {
                continue;
            }

            let avail = availability.get(idx as usize).copied().unwrap_or(0);
            match best {
                Some((_, best_avail)) if avail < best_avail => {
                    best = Some((idx, avail));
                }
                None => {
                    best = Some((idx, avail));
                }
                _ => {}
            }
        }

        if let Some((idx, _)) = best {
            self.peer_assignments.insert(peer, idx);
            Some(idx)
        } else {
            None
        }
    }

    /// Called when a peer sends a Have message. If the piece matches their
    /// assignment, mark it as propagated and clear the assignment.
    /// Returns true if a new piece should be assigned.
    pub fn peer_reported_have(&mut self, peer: SocketAddr, piece: u32) -> bool {
        if let Some(&assigned) = self.peer_assignments.get(&peer)
            && assigned == piece
        {
            self.propagated.insert(piece);
            self.peer_assignments.remove(&peer);
            return true;
        }
        false
    }

    /// Clean up when a peer disconnects.
    pub fn peer_disconnected(&mut self, peer: SocketAddr) {
        self.peer_assignments.remove(&peer);
    }

    /// Returns true if the peer already has an assigned piece.
    pub fn has_assignment(&self, peer: &SocketAddr) -> bool {
        self.peer_assignments.contains_key(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    #[test]
    fn assign_rarest_piece() {
        let mut ss = SuperSeedState::new();
        let mut bf = Bitfield::new(5);
        // Peer already has piece 0
        bf.set(0);

        // Availability: piece 0=5, 1=3, 2=1, 3=2, 4=4
        let availability = vec![5, 3, 1, 2, 4];

        let picked = ss.assign_piece(addr(6881), &bf, &availability, 5);
        // Rarest piece the peer doesn't have is piece 2 (availability 1)
        assert_eq!(picked, Some(2));
    }

    #[test]
    fn peer_reported_have_triggers_propagation() {
        let mut ss = SuperSeedState::new();
        let bf = Bitfield::new(5);
        let availability = vec![1, 1, 1, 1, 1];

        // Assign piece 0 to peer
        let picked = ss.assign_piece(addr(6881), &bf, &availability, 5);
        assert_eq!(picked, Some(0));

        // Peer reports having piece 0
        assert!(ss.peer_reported_have(addr(6881), 0));

        // Next assignment should give a different piece (piece 0 is now propagated)
        let picked2 = ss.assign_piece(addr(6881), &bf, &availability, 5);
        assert!(picked2.is_some());
        assert_ne!(picked2, Some(0));
    }

    #[test]
    fn propagated_piece_not_reassigned() {
        let mut ss = SuperSeedState::new();
        let bf = Bitfield::new(3);
        let availability = vec![1, 1, 1];

        // Assign and propagate piece 0
        let picked = ss.assign_piece(addr(6881), &bf, &availability, 3);
        assert_eq!(picked, Some(0));
        assert!(ss.peer_reported_have(addr(6881), 0));

        // Now assign to a different peer — piece 0 should be skipped
        let picked2 = ss.assign_piece(addr(6882), &bf, &availability, 3);
        assert!(picked2.is_some());
        assert_ne!(picked2, Some(0));
    }
}
