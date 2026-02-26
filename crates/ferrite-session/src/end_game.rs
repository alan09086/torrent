use std::collections::HashMap;
use std::net::SocketAddr;

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

struct BlockEntry {
    length: u32,
    peers: Vec<SocketAddr>,
}

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
    pub fn activate(&mut self, pending: &[(SocketAddr, Vec<(u32, u32, u32)>)]) {
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
}
