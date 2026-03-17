//! Generic iterative Kademlia lookup.
//!
//! Implements the "closest K, query Alpha unqueried, sort by XOR distance,
//! truncate" pattern shared by `get_peers` and `find_node` lookups. Each
//! lookup is parameterised by a callback struct that holds lookup-specific
//! state (e.g. token tracking for `get_peers`, round counting for bootstrap).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use tokio::sync::mpsc;
use torrent_core::{AddressFamily, Id20};

use crate::compact::CompactNodeInfo;
use crate::routing_table::K;

/// Kademlia concurrency parameter: number of nodes queried per round.
#[cfg(test)]
const ALPHA: usize = 3;

/// Generic iterative Kademlia lookup state.
///
/// `C` is a callback struct holding lookup-specific state (e.g.
/// [`GetPeersCallbacks`] for peer lookups, [`FindNodeCallbacks`] for
/// bootstrap find_node).
pub(crate) struct IterativeLookup<C> {
    /// The target ID this lookup is converging toward.
    pub target: Id20,
    /// Best (closest-to-target) nodes discovered so far, sorted by XOR
    /// distance. Kept at most `K * 2` entries.
    pub closest: Vec<CompactNodeInfo>,
    /// Node IDs that have already been queried (to avoid re-querying).
    pub queried: HashSet<Id20>,
    /// Lookup-specific state.
    pub callbacks: C,
}

impl<C> IterativeLookup<C> {
    /// Create a new lookup targeting `target` with the given callback state.
    pub fn new(target: Id20, callbacks: C) -> Self {
        Self {
            target,
            closest: Vec::new(),
            queried: HashSet::new(),
            callbacks,
        }
    }

    /// Return up to `alpha` unqueried closest nodes, marking them as queried.
    ///
    /// Nodes are returned in closest-first order (the `closest` vec is kept
    /// sorted by XOR distance to `target`).
    pub fn next_to_query(&mut self, alpha: usize) -> Vec<CompactNodeInfo> {
        let to_query: Vec<CompactNodeInfo> = self
            .closest
            .iter()
            .filter(|n| !self.queried.contains(&n.id))
            .take(alpha)
            .copied()
            .collect();
        for node in &to_query {
            self.queried.insert(node.id);
        }
        to_query
    }

    /// Feed newly-discovered nodes into the lookup.
    ///
    /// Filters by address family, deduplicates against existing `closest`
    /// entries (by `Id20`), adds new unique nodes, re-sorts by XOR distance
    /// to `target`, and truncates to `K * 2`.
    pub fn feed_nodes(&mut self, new_nodes: Vec<CompactNodeInfo>, family: AddressFamily) {
        let family_match = |addr: &SocketAddr| match family {
            AddressFamily::V4 => addr.is_ipv4(),
            AddressFamily::V6 => addr.is_ipv6(),
        };

        for node in new_nodes {
            if family_match(&node.addr)
                && !self.closest.iter().any(|n| n.id == node.id)
            {
                self.closest.push(node);
            }
        }

        let target = self.target;
        self.closest.sort_by_key(|n| n.id.xor_distance(&target));
        self.closest.truncate(K * 2);
    }

    /// Check if lookup is exhausted (no unqueried nodes in closest AND no
    /// pending queries for this lookup in the actor's pending map).
    ///
    /// `has_pending` should be `true` if the caller still has in-flight KRPC
    /// queries for this lookup.
    pub fn is_exhausted(&self, has_pending: bool) -> bool {
        let has_unqueried = self.closest.iter().any(|n| !self.queried.contains(&n.id));
        !has_unqueried && !has_pending
    }
}

/// Callback state for `get_peers` lookups.
///
/// Holds the reply channel for streaming discovered peers back to the caller
/// and a map of announce tokens collected from responding nodes.
pub(crate) struct GetPeersCallbacks {
    /// Channel for streaming discovered peer addresses to the caller.
    pub reply: mpsc::Sender<Vec<SocketAddr>>,
    /// Announce tokens keyed by node ID: `(node_addr, token_bytes)`.
    /// Collected from `get_peers` responses for later `announce_peer`.
    pub tokens: HashMap<Id20, (SocketAddr, Vec<u8>)>,
}

/// Callback state for iterative bootstrap `find_node` lookups.
///
/// Tracks the current round and maximum rounds to bound the iterative
/// bootstrap process.
pub(crate) struct FindNodeCallbacks {
    /// Current round (incremented each time a batch of queries is sent).
    pub round: u8,
    /// Maximum number of rounds before the lookup terminates.
    pub max_rounds: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a `CompactNodeInfo` with the given id bytes and a V4 address.
    fn make_node(id_bytes: [u8; 20], port: u16) -> CompactNodeInfo {
        CompactNodeInfo {
            id: Id20(id_bytes),
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }

    /// T8: Returns up to Alpha unqueried closest nodes.
    #[test]
    fn iterative_lookup_next_to_query_alpha() {
        let target = Id20::ZERO;
        let callbacks = FindNodeCallbacks {
            round: 0,
            max_rounds: 6,
        };
        let mut lookup = IterativeLookup::new(target, callbacks);

        // Insert 5 nodes — more than ALPHA (3).
        for i in 1..=5u8 {
            let mut id = [0u8; 20];
            id[19] = i;
            lookup.closest.push(make_node(id, 6880 + u16::from(i)));
        }

        // First call: should return ALPHA (3) nodes.
        let batch1 = lookup.next_to_query(ALPHA);
        assert_eq!(batch1.len(), ALPHA);

        // Those 3 should now be marked as queried.
        assert_eq!(lookup.queried.len(), 3);

        // Second call: should return the remaining 2.
        let batch2 = lookup.next_to_query(ALPHA);
        assert_eq!(batch2.len(), 2);

        // All 5 now queried.
        assert_eq!(lookup.queried.len(), 5);

        // Third call: no unqueried nodes left.
        let batch3 = lookup.next_to_query(ALPHA);
        assert!(batch3.is_empty());
    }

    /// T9: Reports exhausted when no unqueried + no pending.
    #[test]
    fn iterative_lookup_exhausted() {
        let target = Id20::ZERO;
        let callbacks = FindNodeCallbacks {
            round: 0,
            max_rounds: 6,
        };
        let mut lookup = IterativeLookup::new(target, callbacks);

        // Empty lookup with no pending → exhausted.
        assert!(lookup.is_exhausted(false));

        // Empty lookup but pending queries → not exhausted.
        assert!(!lookup.is_exhausted(true));

        // Add a node — now there's an unqueried node.
        let mut id = [0u8; 20];
        id[19] = 1;
        lookup.closest.push(make_node(id, 6881));
        assert!(!lookup.is_exhausted(false));

        // Query it.
        let _ = lookup.next_to_query(ALPHA);
        assert_eq!(lookup.queried.len(), 1);

        // No unqueried, no pending → exhausted.
        assert!(lookup.is_exhausted(false));

        // No unqueried, but pending → not exhausted (waiting for replies).
        assert!(!lookup.is_exhausted(true));
    }

    /// T15: `GetPeersCallbacks` correctly tracks tokens.
    #[test]
    fn lookup_get_peers_token_tracking() {
        let (tx, _rx) = mpsc::channel(16);
        let target = Id20::ZERO;
        let callbacks = GetPeersCallbacks {
            reply: tx,
            tokens: HashMap::new(),
        };
        let mut lookup = IterativeLookup::new(target, callbacks);

        // Simulate receiving tokens from responding nodes.
        let node1_id = {
            let mut id = [0u8; 20];
            id[19] = 1;
            Id20(id)
        };
        let node2_id = {
            let mut id = [0u8; 20];
            id[19] = 2;
            Id20(id)
        };
        let addr1: SocketAddr = "1.2.3.4:6881".parse().unwrap();
        let addr2: SocketAddr = "5.6.7.8:6882".parse().unwrap();

        lookup
            .callbacks
            .tokens
            .insert(node1_id, (addr1, b"token_a".to_vec()));
        lookup
            .callbacks
            .tokens
            .insert(node2_id, (addr2, b"token_b".to_vec()));

        assert_eq!(lookup.callbacks.tokens.len(), 2);

        // Verify specific token values.
        let (stored_addr, stored_token) = lookup
            .callbacks
            .tokens
            .get(&node1_id)
            .expect("node1 token missing");
        assert_eq!(*stored_addr, addr1);
        assert_eq!(stored_token, b"token_a");

        let (stored_addr, stored_token) = lookup
            .callbacks
            .tokens
            .get(&node2_id)
            .expect("node2 token missing");
        assert_eq!(*stored_addr, addr2);
        assert_eq!(stored_token, b"token_b");

        // Overwrite a token (node responds again with a new token).
        lookup
            .callbacks
            .tokens
            .insert(node1_id, (addr1, b"token_a_v2".to_vec()));
        assert_eq!(lookup.callbacks.tokens.len(), 2);
        let (_, updated_token) = lookup
            .callbacks
            .tokens
            .get(&node1_id)
            .expect("node1 updated token missing");
        assert_eq!(updated_token, b"token_a_v2");
    }
}
