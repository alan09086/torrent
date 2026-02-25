use std::net::SocketAddr;

/// Information about a peer used by the choking algorithm.
#[derive(Debug, Clone)]
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
pub(crate) struct PeerInfo {
    pub addr: SocketAddr,
    /// Bytes/sec they are uploading TO us.
    pub download_rate: u64,
    /// Peer is interested in our data.
    pub interested: bool,
}

/// Result of a choking decision.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
pub(crate) struct ChokeDecision {
    /// Peers that should be unchoked.
    pub to_unchoke: Vec<SocketAddr>,
    /// Peers that should be choked.
    pub to_choke: Vec<SocketAddr>,
}

/// Tit-for-tat choking algorithm.
///
/// Unchokes the top-N peers by download rate plus one optimistic unchoke
/// to discover faster peers.
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
pub(crate) struct Choker {
    unchoke_slots: usize,
    optimistic_peer: Option<SocketAddr>,
}

#[allow(dead_code)]
impl Choker {
    /// Create a choker with the given number of regular unchoke slots.
    pub fn new(unchoke_slots: usize) -> Self {
        Self {
            unchoke_slots,
            optimistic_peer: None,
        }
    }

    /// Decide which peers to unchoke and choke.
    ///
    /// Algorithm:
    /// 1. Filter to interested peers only.
    /// 2. Sort by download rate descending.
    /// 3. Regular unchokes = top `unchoke_slots`.
    /// 4. Optimistic unchoke = existing optimistic peer if still interested and
    ///    not already unchoked, otherwise first remaining interested peer.
    /// 5. Everyone else gets choked.
    pub fn decide(&mut self, peers: &[PeerInfo]) -> ChokeDecision {
        let all_addrs: Vec<SocketAddr> = peers.iter().map(|p| p.addr).collect();

        // Interested peers sorted by download rate descending.
        let mut interested: Vec<&PeerInfo> = peers.iter().filter(|p| p.interested).collect();
        interested.sort_by(|a, b| b.download_rate.cmp(&a.download_rate));

        // Regular unchokes: top N by download rate.
        let regular_count = self.unchoke_slots.min(interested.len());
        let regular_unchokes: Vec<SocketAddr> =
            interested[..regular_count].iter().map(|p| p.addr).collect();

        // Optimistic unchoke selection.
        let optimistic = self.select_optimistic(&interested, &regular_unchokes);
        self.optimistic_peer = optimistic;

        // Build the final unchoke set.
        let mut to_unchoke = regular_unchokes;
        if let Some(opt) = optimistic
            && !to_unchoke.contains(&opt)
        {
            to_unchoke.push(opt);
        }

        // Everyone not in to_unchoke gets choked.
        let to_choke: Vec<SocketAddr> = all_addrs
            .into_iter()
            .filter(|a| !to_unchoke.contains(a))
            .collect();

        ChokeDecision {
            to_unchoke,
            to_choke,
        }
    }

    /// Pick a new optimistic peer from interested peers.
    ///
    /// Selects the interested peer with the lowest download rate that is not
    /// already the optimistic peer.
    pub fn rotate_optimistic(&mut self, peers: &[PeerInfo]) {
        let mut interested: Vec<&PeerInfo> = peers.iter().filter(|p| p.interested).collect();
        // Sort ascending by download rate so the first non-optimistic is picked.
        interested.sort_by(|a, b| a.download_rate.cmp(&b.download_rate));

        self.optimistic_peer = interested
            .iter()
            .find(|p| Some(p.addr) != self.optimistic_peer)
            .map(|p| p.addr);
    }

    /// Select an optimistic unchoke peer.
    ///
    /// If the current optimistic peer is interested and not in the regular set,
    /// keep it. Otherwise pick the first interested peer not in the regular set.
    fn select_optimistic(
        &self,
        interested: &[&PeerInfo],
        regular_unchokes: &[SocketAddr],
    ) -> Option<SocketAddr> {
        // Keep existing optimistic peer if it qualifies.
        if let Some(opt) = self.optimistic_peer {
            let still_interested = interested.iter().any(|p| p.addr == opt);
            let already_regular = regular_unchokes.contains(&opt);
            if still_interested && !already_regular {
                return Some(opt);
            }
        }

        // Pick first interested peer not already in regular unchokes.
        interested
            .iter()
            .find(|p| !regular_unchokes.contains(&p.addr))
            .map(|p| p.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    fn peer(port: u16, download_rate: u64, interested: bool) -> PeerInfo {
        PeerInfo {
            addr: addr(port),
            download_rate,
            interested,
        }
    }

    #[test]
    fn unchoke_top_n() {
        let mut choker = Choker::new(4);
        let peers = vec![
            peer(6881, 100, true),
            peer(6882, 500, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 400, true),
            peer(6886, 50, true),
        ];

        let decision = choker.decide(&peers);

        // Top 4 by rate: 500, 400, 300, 200 (ports 6882, 6885, 6883, 6884).
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert!(decision.to_unchoke.contains(&addr(6885)));
        assert!(decision.to_unchoke.contains(&addr(6883)));
        assert!(decision.to_unchoke.contains(&addr(6884)));

        // Optimistic should add one more from the remaining interested peers,
        // so total unchoked should be 5 (4 regular + 1 optimistic).
        assert_eq!(decision.to_unchoke.len(), 5);

        // The remaining peer is choked.
        assert_eq!(decision.to_choke.len(), 1);
    }

    #[test]
    fn optimistic_rotation() {
        let mut choker = Choker::new(4);
        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 100, true),
            peer(6886, 50, true),
        ];

        let decision = choker.decide(&peers);

        // After decide(), an optimistic peer should be set.
        assert!(choker.optimistic_peer.is_some());

        // The optimistic peer must be in the interested set but not in the
        // top 4 (regular unchokes are 6881, 6882, 6883, 6884).
        let opt = choker.optimistic_peer.unwrap();
        assert!(opt == addr(6885) || opt == addr(6886));

        // Optimistic peer should be in the unchoke list.
        assert!(decision.to_unchoke.contains(&opt));

        // After rotation, the optimistic peer should change.
        choker.rotate_optimistic(&peers);
        let new_opt = choker.optimistic_peer.unwrap();
        assert_ne!(new_opt, opt);
    }

    #[test]
    fn fewer_peers_than_slots() {
        let mut choker = Choker::new(4);
        let peers = vec![
            peer(6881, 100, true),
            peer(6882, 200, true),
        ];

        let decision = choker.decide(&peers);

        // Both interested peers should be unchoked.
        assert!(decision.to_unchoke.contains(&addr(6881)));
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert_eq!(decision.to_unchoke.len(), 2);
        assert!(decision.to_choke.is_empty());
    }

    #[test]
    fn no_interested_peers() {
        let mut choker = Choker::new(4);
        let peers = vec![
            peer(6881, 100, false),
            peer(6882, 200, false),
            peer(6883, 300, false),
        ];

        let decision = choker.decide(&peers);

        assert!(decision.to_unchoke.is_empty());
        // All peers should be in to_choke.
        assert_eq!(decision.to_choke.len(), 3);
        assert!(decision.to_choke.contains(&addr(6881)));
        assert!(decision.to_choke.contains(&addr(6882)));
        assert!(decision.to_choke.contains(&addr(6883)));
    }

    #[test]
    fn choke_below_threshold() {
        let mut choker = Choker::new(2);
        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 100, true),
            peer(6884, 50, true),
            peer(6885, 200, false), // not interested
        ];

        let decision = choker.decide(&peers);

        // Regular unchokes: top 2 = ports 6881 (500), 6882 (400).
        assert!(decision.to_unchoke.contains(&addr(6881)));
        assert!(decision.to_unchoke.contains(&addr(6882)));

        // Optimistic adds one more interested peer (6883 or 6884).
        assert_eq!(decision.to_unchoke.len(), 3);

        // The non-unchoked peers should be in to_choke.
        // That's 2 from the remaining: one interested + the uninterested peer.
        assert_eq!(decision.to_choke.len(), 2);
        // Uninterested peer is always choked.
        assert!(decision.to_choke.contains(&addr(6885)));
    }
}
