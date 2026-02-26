use std::collections::HashMap;
use std::net::SocketAddr;

use ferrite_storage::Bitfield;

/// Block requests for a given (piece, offset) pair.
type PendingBlocks = Vec<(SocketAddr, Vec<(u32, u32, u32)>)>;

/// End-game mode state tracker.
///
/// When all remaining pieces have at least one request in-flight,
/// end-game mode allows requesting the same block from multiple peers.
/// Tracks which peers have been assigned each block so we can send
/// Cancel messages when a block arrives.
pub(crate) struct EndGame {
    active: bool,
    /// (piece_index, begin) → { length, list of peers assigned this block }
    blocks: HashMap<(u32, u32), BlockEntry>,
}

#[allow(dead_code)] // length read by block_received, wired in Task 6
struct BlockEntry {
    length: u32,
    peers: Vec<SocketAddr>,
}

#[allow(dead_code)] // remaining methods wired in Tasks 5-7
impl EndGame {
    pub fn new() -> Self {
        Self {
            active: false,
            blocks: HashMap::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Activate end-game by scanning all peers' pending requests.
    /// `pending` is: [(peer_addr, [(piece, begin, length), ...])]
    pub fn activate(&mut self, pending: &PendingBlocks) {
        self.active = true;
        self.blocks.clear();
        for (addr, requests) in pending {
            for &(index, begin, length) in requests {
                self.blocks
                    .entry((index, begin))
                    .or_insert_with(|| BlockEntry {
                        length,
                        peers: Vec::new(),
                    })
                    .peers
                    .push(*addr);
            }
        }
    }

    pub fn deactivate(&mut self) {
        self.active = false;
        self.blocks.clear();
    }

    /// Get the list of peers that have been assigned a given block.
    pub fn block_requesters(&self, index: u32, begin: u32) -> &[SocketAddr] {
        self.blocks
            .get(&(index, begin))
            .map(|e| e.peers.as_slice())
            .unwrap_or(&[])
    }

    /// Number of blocks being tracked.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Pick a single block for `peer_addr` to duplicate-request.
    /// Returns `Some((index, begin, length))` or `None`.
    pub fn pick_block(
        &self,
        peer_addr: SocketAddr,
        peer_has: &Bitfield,
    ) -> Option<(u32, u32, u32)> {
        for (&(index, begin), entry) in &self.blocks {
            if !peer_has.get(index) {
                continue;
            }
            if entry.peers.contains(&peer_addr) {
                continue;
            }
            return Some((index, begin, entry.length));
        }
        None
    }

    /// Strict variant: returns `None` if `uncovered_pieces` is non-empty
    /// (meaning there are pieces with no outstanding requests — don't
    /// duplicate-request while those exist).
    pub fn pick_block_strict(
        &self,
        peer_addr: SocketAddr,
        peer_has: &Bitfield,
        uncovered_pieces: &[u32],
    ) -> Option<(u32, u32, u32)> {
        if !uncovered_pieces.is_empty() {
            return None;
        }
        self.pick_block(peer_addr, peer_has)
    }

    /// Record that a block was received from `from_peer`.
    /// Returns list of `(peer_addr, index, begin, length)` to send Cancel to.
    pub fn block_received(
        &mut self,
        index: u32,
        begin: u32,
        from_peer: SocketAddr,
    ) -> Vec<(SocketAddr, u32, u32, u32)> {
        let Some(entry) = self.blocks.remove(&(index, begin)) else {
            return Vec::new();
        };
        entry
            .peers
            .into_iter()
            .filter(|&addr| addr != from_peer)
            .map(|addr| (addr, index, begin, entry.length))
            .collect()
    }

    /// Register that `peer_addr` has been assigned block `(index, begin)`.
    pub fn register_request(&mut self, index: u32, begin: u32, peer_addr: SocketAddr) {
        if let Some(entry) = self.blocks.get_mut(&(index, begin))
            && !entry.peers.contains(&peer_addr)
        {
            entry.peers.push(peer_addr);
        }
    }

    /// Remove all entries for a disconnected peer.
    pub fn peer_disconnected(&mut self, addr: SocketAddr) {
        for entry in self.blocks.values_mut() {
            entry.peers.retain(|&a| a != addr);
        }
    }

    /// Remove all block entries for a given piece (e.g., after hash failure).
    pub fn remove_piece(&mut self, index: u32) {
        self.blocks.retain(|&(pi, _), _| pi != index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn new_is_inactive() {
        let eg = EndGame::new();
        assert!(!eg.is_active());
    }

    #[test]
    fn activate_populates_blocks_from_pending() {
        let mut eg = EndGame::new();

        let peer_a = addr(1);
        let peer_b = addr(2);
        let pending: Vec<(SocketAddr, Vec<(u32, u32, u32)>)> = vec![
            (peer_a, vec![(0, 0, 16384), (0, 16384, 16384)]),
            (peer_b, vec![(1, 0, 16384), (1, 16384, 16384)]),
        ];

        eg.activate(&pending);

        assert!(eg.is_active());
        assert_eq!(eg.block_requesters(0, 0).len(), 1);
        assert!(eg.block_requesters(0, 0).contains(&peer_a));
        assert_eq!(eg.block_requesters(1, 0).len(), 1);
        assert!(eg.block_requesters(1, 0).contains(&peer_b));
    }

    #[test]
    fn deactivate_clears_state() {
        let mut eg = EndGame::new();
        let pending = vec![(addr(1), vec![(0, 0, 16384)])];
        eg.activate(&pending);
        assert!(eg.is_active());

        eg.deactivate();
        assert!(!eg.is_active());
        assert_eq!(eg.block_requesters(0, 0).len(), 0);
    }

    #[test]
    fn pick_block_returns_unassigned_block() {
        let mut eg = EndGame::new();
        let peer_a = addr(1);
        let peer_b = addr(2);
        let pending = vec![
            (peer_a, vec![(0, 0, 16384), (0, 16384, 16384)]),
        ];
        eg.activate(&pending);

        // peer_b has piece 0 — should get one of the blocks already assigned to peer_a
        let mut peer_b_has = Bitfield::new(4);
        peer_b_has.set(0);

        let block = eg.pick_block(peer_b, &peer_b_has);
        assert!(block.is_some());
        let (idx, begin, len) = block.unwrap();
        assert_eq!(idx, 0);
        assert_eq!(len, 16384);
        assert!(begin == 0 || begin == 16384);
    }

    #[test]
    fn pick_block_skips_already_assigned_peer() {
        let mut eg = EndGame::new();
        let peer_a = addr(1);
        // Only one block, already assigned to peer_a
        let pending = vec![(peer_a, vec![(0, 0, 16384)])];
        eg.activate(&pending);

        let mut peer_a_has = Bitfield::new(4);
        peer_a_has.set(0);

        // peer_a already has this block — should get None
        let block = eg.pick_block(peer_a, &peer_a_has);
        assert!(block.is_none());
    }

    #[test]
    fn pick_block_skips_piece_peer_lacks() {
        let mut eg = EndGame::new();
        let peer_a = addr(1);
        let peer_b = addr(2);
        let pending = vec![(peer_a, vec![(0, 0, 16384)])];
        eg.activate(&pending);

        // peer_b does NOT have piece 0
        let peer_b_has = Bitfield::new(4);
        let block = eg.pick_block(peer_b, &peer_b_has);
        assert!(block.is_none());
    }

    #[test]
    fn pick_block_strict_blocks_when_piece_uncovered() {
        let mut eg = EndGame::new();
        let peer_a = addr(1);
        let pending = vec![(peer_a, vec![(0, 0, 16384)])];
        eg.activate(&pending);

        let peer_b = addr(2);
        let mut peer_b_has = Bitfield::new(4);
        peer_b_has.set(0);

        // strict=true + uncovered pieces exist → should return None
        let block = eg.pick_block_strict(peer_b, &peer_b_has, &[1u32]);
        assert!(block.is_none());

        // strict=true + no uncovered pieces → should return a block
        let block = eg.pick_block_strict(peer_b, &peer_b_has, &[]);
        assert!(block.is_some());
    }

    #[test]
    fn block_received_returns_cancel_targets() {
        let mut eg = EndGame::new();
        let peer_a = addr(1);
        let peer_b = addr(2);
        let peer_c = addr(3);

        let pending = vec![
            (peer_a, vec![(0, 0, 16384)]),
            (peer_b, vec![(0, 0, 16384)]),
            (peer_c, vec![(0, 0, 16384)]),
        ];
        eg.activate(&pending);

        // Block arrives from peer_a — should return cancels for peer_b and peer_c
        let cancels = eg.block_received(0, 0, peer_a);
        assert_eq!(cancels.len(), 2);
        assert!(cancels.contains(&(peer_b, 0, 0, 16384)));
        assert!(cancels.contains(&(peer_c, 0, 0, 16384)));
    }

    #[test]
    fn block_received_removes_entry() {
        let mut eg = EndGame::new();
        let peer_a = addr(1);
        let pending = vec![(peer_a, vec![(0, 0, 16384)])];
        eg.activate(&pending);

        let _ = eg.block_received(0, 0, peer_a);
        assert_eq!(eg.block_requesters(0, 0).len(), 0);
    }

    #[test]
    fn peer_disconnected_removes_from_all_entries() {
        let mut eg = EndGame::new();
        let peer_a = addr(1);
        let peer_b = addr(2);
        let pending = vec![
            (peer_a, vec![(0, 0, 16384)]),
            (peer_b, vec![(0, 0, 16384)]),
        ];
        eg.activate(&pending);
        assert_eq!(eg.block_requesters(0, 0).len(), 2);

        eg.peer_disconnected(peer_a);
        assert_eq!(eg.block_requesters(0, 0).len(), 1);
        assert!(eg.block_requesters(0, 0).contains(&peer_b));
    }

    #[test]
    fn remove_piece_clears_all_blocks_for_piece() {
        let mut eg = EndGame::new();
        let peer_a = addr(1);
        let pending = vec![
            (peer_a, vec![(0, 0, 16384), (0, 16384, 16384), (1, 0, 16384)]),
        ];
        eg.activate(&pending);
        assert_eq!(eg.block_requesters(0, 0).len(), 1);
        assert_eq!(eg.block_requesters(1, 0).len(), 1);

        eg.remove_piece(0);
        assert_eq!(eg.block_requesters(0, 0).len(), 0);
        assert_eq!(eg.block_requesters(0, 16384).len(), 0);
        // Piece 1 untouched
        assert_eq!(eg.block_requesters(1, 0).len(), 1);
    }

    #[test]
    fn register_request_adds_peer() {
        let mut eg = EndGame::new();
        let peer_a = addr(1);
        let peer_b = addr(2);
        let pending = vec![(peer_a, vec![(0, 0, 16384)])];
        eg.activate(&pending);

        eg.register_request(0, 0, peer_b);
        assert_eq!(eg.block_requesters(0, 0).len(), 2);
        assert!(eg.block_requesters(0, 0).contains(&peer_b));

        // Duplicate register is idempotent
        eg.register_request(0, 0, peer_b);
        assert_eq!(eg.block_requesters(0, 0).len(), 2);
    }
}
