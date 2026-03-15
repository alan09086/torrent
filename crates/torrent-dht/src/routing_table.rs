//! Kademlia routing table with k-buckets (BEP 5).
//!
//! The routing table maps 160-bit node IDs to socket addresses using
//! a binary-tree of k-buckets. Each bucket holds up to `K` (8) nodes.
//! Buckets are split when the bucket containing our own ID overflows.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use torrent_core::Id20;

/// Maximum nodes per bucket (Kademlia k parameter).
pub const K: usize = 8;

/// Maximum number of buckets (one per bit of the ID).
const MAX_BUCKETS: usize = 160;

/// Kademlia node liveness classification (BEP 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// Node has responded or sent a query within the last 15 minutes.
    Good,
    /// Node has not been active recently but has not failed repeatedly.
    Questionable,
    /// Node has failed 2 or more consecutive queries.
    Bad,
}

/// Age threshold for considering a node "active" (15 minutes).
const ACTIVE_THRESHOLD: Duration = Duration::from_secs(15 * 60);

/// A node in the routing table.
#[derive(Debug, Clone)]
pub struct RoutingNode {
    /// Node's 20-byte Kademlia ID.
    pub id: Id20,
    /// Node's socket address.
    pub addr: SocketAddr,
    /// Timestamp of the last successful interaction.
    pub last_seen: Instant,
    /// Number of consecutive failed queries.
    pub fail_count: u32,
    /// Timestamp of the last response received from this node.
    pub last_response: Option<Instant>,
    /// Timestamp of the last query received from this node.
    pub last_query: Option<Instant>,
}

impl RoutingNode {
    /// Classify this node per Kademlia liveness rules.
    ///
    /// - `Bad`: fail_count >= 2
    /// - `Good`: responded or sent a query within the last 15 minutes
    /// - `Questionable`: otherwise
    pub fn status(&self) -> NodeStatus {
        if self.fail_count >= 2 {
            return NodeStatus::Bad;
        }
        let cutoff = Instant::now() - ACTIVE_THRESHOLD;
        let active = self.last_response.is_some_and(|t| t >= cutoff)
            || self.last_query.is_some_and(|t| t >= cutoff);
        if active {
            NodeStatus::Good
        } else {
            NodeStatus::Questionable
        }
    }
}

/// A single k-bucket.
#[derive(Debug, Clone)]
struct KBucket {
    nodes: Vec<RoutingNode>,
}

impl KBucket {
    fn new() -> Self {
        KBucket {
            nodes: Vec::with_capacity(K),
        }
    }

    fn is_full(&self) -> bool {
        self.nodes.len() >= K
    }

    fn find(&self, id: &Id20) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == *id)
    }

    /// Return the node with the highest fail count, or the oldest if tied.
    fn worst_node(&self) -> Option<usize> {
        self.nodes
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.fail_count
                    .cmp(&b.fail_count)
                    .then(b.last_seen.cmp(&a.last_seen))
            })
            .map(|(i, _)| i)
    }
}

/// Kademlia routing table.
#[derive(Debug, Clone)]
pub struct RoutingTable {
    own_id: Id20,
    buckets: Vec<KBucket>,
    /// When enabled, tracks IPs to enforce one-node-per-IP (BEP 42).
    ip_set: HashSet<IpAddr>,
    /// Whether to enforce one-node-per-IP restriction.
    restrict_ips: bool,
}

/// Result of an insert operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertResult {
    /// Node was inserted (new or updated).
    Inserted,
    /// Bucket is full but could be split; caller should try again.
    BucketFull,
    /// Node was not inserted; cannot split further.
    Rejected,
}

impl RoutingTable {
    /// Create a new routing table with the given own node ID.
    pub fn new(own_id: Id20) -> Self {
        Self::new_with_config(own_id, false)
    }

    /// Create a new routing table with IP restriction setting.
    pub fn new_with_config(own_id: Id20, restrict_ips: bool) -> Self {
        RoutingTable {
            own_id,
            buckets: vec![KBucket::new()],
            ip_set: HashSet::new(),
            restrict_ips,
        }
    }

    /// Our node ID.
    pub fn own_id(&self) -> &Id20 {
        &self.own_id
    }

    /// Total number of nodes in the routing table.
    pub fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.nodes.len()).sum()
    }

    /// Whether the routing table is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of buckets.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Insert or update a node. Returns `true` if the node was added/updated.
    pub fn insert(&mut self, id: Id20, addr: SocketAddr) -> bool {
        if id == self.own_id {
            return false; // Never insert ourselves
        }
        self.insert_inner(id, addr)
    }

    fn insert_inner(&mut self, id: Id20, addr: SocketAddr) -> bool {
        let ip = addr.ip();

        // BEP 42: check if another node with this IP exists (and it's not the same node)
        if self.restrict_ips && self.ip_set.contains(&ip) {
            let bucket_idx = self.bucket_index(&id);
            if self.buckets[bucket_idx].find(&id).is_none() {
                return false; // Different node, same IP — reject
            }
        }

        let bucket_idx = self.bucket_index(&id);
        let bucket = &mut self.buckets[bucket_idx];

        // Already known — update last_seen and address
        if let Some(pos) = bucket.find(&id) {
            let old_ip = bucket.nodes[pos].addr.ip();
            bucket.nodes[pos].last_seen = Instant::now();
            bucket.nodes[pos].addr = addr;
            bucket.nodes[pos].fail_count = 0;
            // Update IP tracking if address changed
            if self.restrict_ips && old_ip != ip {
                self.ip_set.remove(&old_ip);
                self.ip_set.insert(ip);
            }
            return true;
        }

        // Room in bucket
        if !bucket.is_full() {
            bucket.nodes.push(RoutingNode {
                id,
                addr,
                last_seen: Instant::now(),
                fail_count: 0,
                last_response: None,
                last_query: None,
            });
            if self.restrict_ips {
                self.ip_set.insert(ip);
            }
            return true;
        }

        // Bucket full — try to evict a failed node
        if let Some(worst_idx) = bucket.worst_node()
            && bucket.nodes[worst_idx].fail_count > 0
        {
            // Remove old node's IP from tracking (gap fix #7)
            if self.restrict_ips {
                self.ip_set.remove(&bucket.nodes[worst_idx].addr.ip());
            }
            bucket.nodes[worst_idx] = RoutingNode {
                id,
                addr,
                last_seen: Instant::now(),
                fail_count: 0,
                last_response: None,
                last_query: None,
            };
            if self.restrict_ips {
                self.ip_set.insert(ip);
            }
            return true;
        }

        // Bucket full, all nodes good — try splitting if this is our bucket
        if self.can_split(bucket_idx) {
            self.split(bucket_idx);
            // Retry insert after split
            return self.insert_inner(id, addr);
        }

        false
    }

    /// Remove a node by ID. Returns `true` if it was present.
    pub fn remove(&mut self, id: &Id20) -> bool {
        let bucket_idx = self.bucket_index(id);
        let bucket = &mut self.buckets[bucket_idx];
        if let Some(pos) = bucket.find(id) {
            if self.restrict_ips {
                self.ip_set.remove(&bucket.nodes[pos].addr.ip());
            }
            bucket.nodes.remove(pos);
            true
        } else {
            false
        }
    }

    /// Mark a node as recently seen.
    pub fn mark_seen(&mut self, id: &Id20) {
        let bucket_idx = self.bucket_index(id);
        if let Some(pos) = self.buckets[bucket_idx].find(id) {
            self.buckets[bucket_idx].nodes[pos].last_seen = Instant::now();
            self.buckets[bucket_idx].nodes[pos].fail_count = 0;
        }
    }

    /// Increment a node's fail count.
    pub fn mark_failed(&mut self, id: &Id20) {
        let bucket_idx = self.bucket_index(id);
        if let Some(pos) = self.buckets[bucket_idx].find(id) {
            self.buckets[bucket_idx].nodes[pos].fail_count += 1;
        }
    }

    /// Return the `count` closest nodes to `target`, sorted by XOR distance.
    pub fn closest(&self, target: &Id20, count: usize) -> Vec<RoutingNode> {
        let mut all: Vec<&RoutingNode> = self.buckets.iter().flat_map(|b| &b.nodes).collect();
        all.sort_by_key(|n| n.id.xor_distance(target));
        all.into_iter().take(count).cloned().collect()
    }

    /// Return bucket indices that haven't been refreshed recently.
    pub fn stale_buckets(&self, max_age: std::time::Duration) -> Vec<usize> {
        let cutoff = Instant::now() - max_age;
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| b.nodes.is_empty() || b.nodes.iter().all(|n| n.last_seen < cutoff))
            .map(|(i, _)| i)
            .collect()
    }

    /// Generate a random ID that falls within the given bucket index.
    /// Useful for refreshing stale buckets.
    pub fn random_id_in_bucket(&self, bucket_idx: usize) -> Id20 {
        let mut id = self.own_id;
        // Flip the bit at position `bucket_idx` to land in that bucket's range.
        // Buckets cover distances where the first differing bit is at position bucket_idx.
        if bucket_idx < MAX_BUCKETS {
            let byte_idx = bucket_idx / 8;
            let bit_idx = 7 - (bucket_idx % 8);
            id.0[byte_idx] ^= 1 << bit_idx;
        }
        id
    }

    /// Return all nodes in the routing table as (id, addr) pairs.
    pub fn all_nodes(&self) -> Vec<(Id20, SocketAddr)> {
        self.buckets
            .iter()
            .flat_map(|b| b.nodes.iter().map(|n| (n.id, n.addr)))
            .collect()
    }

    /// Get a reference to a node by ID.
    pub fn get(&self, id: &Id20) -> Option<&RoutingNode> {
        let bucket_idx = self.bucket_index(id);
        self.buckets[bucket_idx]
            .find(id)
            .map(|pos| &self.buckets[bucket_idx].nodes[pos])
    }

    /// Get a mutable reference to a node by ID.
    pub fn get_mut(&mut self, id: &Id20) -> Option<&mut RoutingNode> {
        let bucket_idx = self.bucket_index(id);
        let pos = self.buckets[bucket_idx].find(id)?;
        Some(&mut self.buckets[bucket_idx].nodes[pos])
    }

    /// Record a successful response from a node, resetting its fail count.
    pub fn mark_response(&mut self, id: &Id20) {
        let bucket_idx = self.bucket_index(id);
        if let Some(pos) = self.buckets[bucket_idx].find(id) {
            self.buckets[bucket_idx].nodes[pos].last_response = Some(Instant::now());
            self.buckets[bucket_idx].nodes[pos].fail_count = 0;
        }
    }

    /// Record an incoming query from a node.
    pub fn mark_query(&mut self, id: &Id20) {
        let bucket_idx = self.bucket_index(id);
        if let Some(pos) = self.buckets[bucket_idx].find(id) {
            self.buckets[bucket_idx].nodes[pos].last_query = Some(Instant::now());
        }
    }

    /// Mark all nodes in the routing table as Questionable (M97).
    ///
    /// Called when saved-state verification fails — loaded nodes may be stale.
    /// Setting `last_response = None` and `last_query = None` makes nodes
    /// Questionable (never responded, never queried). `fail_count = 0` ensures
    /// they are Questionable rather than Bad.
    pub fn mark_all_questionable(&mut self) {
        for bucket in &mut self.buckets {
            for node in &mut bucket.nodes {
                node.last_response = None;
                node.last_query = None;
                node.fail_count = 0;
            }
        }
    }

    /// Return all nodes whose status is `Questionable`.
    pub fn questionable_nodes(&self) -> Vec<(Id20, SocketAddr)> {
        self.buckets
            .iter()
            .flat_map(|b| {
                b.nodes
                    .iter()
                    .filter(|n| n.status() == NodeStatus::Questionable)
                    .map(|n| (n.id, n.addr))
            })
            .collect()
    }

    // ---- Internal ----

    /// Determine which bucket a node ID belongs to.
    ///
    /// The bucket index is the number of leading matching bits between
    /// `own_id` and `id`. If the table has fewer buckets, the last bucket
    /// is a catch-all.
    fn bucket_index(&self, id: &Id20) -> usize {
        let distance = self.own_id.xor_distance(id);
        let leading_zeros = leading_zeros_160(&distance);
        // Clamp to the last bucket
        leading_zeros.min(self.buckets.len() - 1)
    }

    /// Whether we can split the bucket at `idx`.
    /// Only the last bucket (which contains our own ID's distance range) can be split.
    fn can_split(&self, idx: usize) -> bool {
        idx == self.buckets.len() - 1 && self.buckets.len() < MAX_BUCKETS
    }

    /// Split the last bucket into two.
    fn split(&mut self, idx: usize) {
        debug_assert!(idx == self.buckets.len() - 1);
        let old_bucket = &mut self.buckets[idx];
        let nodes = std::mem::take(&mut old_bucket.nodes);

        let mut near = KBucket::new();
        let mut far = KBucket::new();

        let new_depth = self.buckets.len(); // This will be the index of the new bucket

        for node in nodes {
            let distance = self.own_id.xor_distance(&node.id);
            let lz = leading_zeros_160(&distance);
            if lz >= new_depth {
                near.nodes.push(node);
            } else {
                far.nodes.push(node);
            }
        }

        // idx stays as the "far" bucket, push "near" as the new last bucket
        self.buckets[idx] = far;
        self.buckets.push(near);
    }
}

/// Count leading zero bits in a 160-bit (20-byte) value.
fn leading_zeros_160(id: &Id20) -> usize {
    let mut zeros = 0;
    for &byte in id.as_bytes() {
        if byte == 0 {
            zeros += 8;
        } else {
            zeros += byte.leading_zeros() as usize;
            break;
        }
    }
    zeros
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn id(byte: u8) -> Id20 {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        Id20(bytes)
    }

    fn addr(port: u16) -> SocketAddr {
        format!("10.0.0.1:{port}").parse().unwrap()
    }

    #[test]
    fn insert_and_retrieve() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        assert!(rt.insert(id(1), addr(1)));
        assert_eq!(rt.len(), 1);
        assert!(rt.get(&id(1)).is_some());
    }

    #[test]
    fn insert_self_rejected() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        assert!(!rt.insert(Id20::ZERO, addr(1)));
        assert_eq!(rt.len(), 0);
    }

    #[test]
    fn update_existing_node() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        rt.insert(id(1), addr(2)); // Update address
        assert_eq!(rt.len(), 1);
        assert_eq!(rt.get(&id(1)).unwrap().addr, addr(2));
    }

    #[test]
    fn remove_node() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        assert!(rt.remove(&id(1)));
        assert_eq!(rt.len(), 0);
        assert!(!rt.remove(&id(1))); // Already gone
    }

    #[test]
    fn closest_nodes_sorted() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        rt.insert(id(5), addr(5));
        rt.insert(id(3), addr(3));
        rt.insert(id(10), addr(10));

        let closest = rt.closest(&Id20::ZERO, 3);
        assert_eq!(closest.len(), 3);
        // XOR distance from ZERO is just the value itself
        assert_eq!(closest[0].id, id(1));
        assert_eq!(closest[1].id, id(3));
        assert_eq!(closest[2].id, id(5));
    }

    #[test]
    fn closest_fewer_than_count() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        let closest = rt.closest(&Id20::ZERO, 10);
        assert_eq!(closest.len(), 1);
    }

    #[test]
    fn bucket_splitting() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        // Insert 9 nodes — should trigger split since K=8
        for i in 1..=9u8 {
            rt.insert(id(i), addr(i as u16));
        }
        assert!(rt.bucket_count() > 1);
        assert_eq!(rt.len(), 9);
    }

    #[test]
    fn evict_failed_node() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        // Fill bucket to capacity
        for i in 1..=K as u8 {
            rt.insert(id(i), addr(i as u16));
        }
        assert_eq!(rt.len(), K);

        // Mark a node as failed
        rt.mark_failed(&id(1));
        rt.mark_failed(&id(1));

        // This should evict the failed node
        let new_id = id(100);
        assert!(rt.insert(new_id, addr(100)));
        assert!(rt.get(&new_id).is_some());
    }

    #[test]
    fn mark_seen_resets_fail_count() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        rt.mark_failed(&id(1));
        assert_eq!(rt.get(&id(1)).unwrap().fail_count, 1);
        rt.mark_seen(&id(1));
        assert_eq!(rt.get(&id(1)).unwrap().fail_count, 0);
    }

    #[test]
    fn stale_buckets_detection() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        // Empty routing table — all buckets are stale
        let stale = rt.stale_buckets(std::time::Duration::from_secs(900));
        assert_eq!(stale.len(), 1);

        // Insert a node — bucket should not be stale
        rt.insert(id(1), addr(1));
        let stale = rt.stale_buckets(std::time::Duration::from_secs(900));
        assert!(stale.is_empty());
    }

    #[test]
    fn leading_zeros_correct() {
        assert_eq!(leading_zeros_160(&Id20::ZERO), 160);
        assert_eq!(leading_zeros_160(&id(1)), 159);
        assert_eq!(leading_zeros_160(&id(128)), 152);
        let mut full = [0xFFu8; 20];
        assert_eq!(leading_zeros_160(&Id20(full)), 0);
        full[0] = 0x01;
        full[1..].fill(0);
        assert_eq!(leading_zeros_160(&Id20(full)), 7);
    }

    #[test]
    fn random_id_in_bucket_differs() {
        let rt = RoutingTable::new(Id20::ZERO);
        let rand_id = rt.random_id_in_bucket(0);
        assert_ne!(rand_id, Id20::ZERO);
    }

    // ── BEP 42 IP restriction tests ────────────────────────────────

    #[test]
    fn restrict_ips_rejects_second_node_same_ip() {
        let mut rt = RoutingTable::new_with_config(Id20::ZERO, true);
        let ip_addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        assert!(rt.insert(id(1), ip_addr));
        // Second node with same IP but different ID — rejected
        let ip_addr2: SocketAddr = "10.0.0.1:6882".parse().unwrap();
        assert!(!rt.insert(id(2), ip_addr2));
        assert_eq!(rt.len(), 1);
    }

    #[test]
    fn restrict_ips_allows_same_node_update() {
        let mut rt = RoutingTable::new_with_config(Id20::ZERO, true);
        let addr1: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let addr2: SocketAddr = "10.0.0.1:6882".parse().unwrap();
        assert!(rt.insert(id(1), addr1));
        // Same node ID updating its port — allowed
        assert!(rt.insert(id(1), addr2));
        assert_eq!(rt.len(), 1);
        assert_eq!(rt.get(&id(1)).unwrap().addr, addr2);
    }

    #[test]
    fn no_restrict_ips_allows_multiple_nodes_same_ip() {
        let mut rt = RoutingTable::new_with_config(Id20::ZERO, false);
        let addr1: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let addr2: SocketAddr = "10.0.0.1:6882".parse().unwrap();
        assert!(rt.insert(id(1), addr1));
        assert!(rt.insert(id(2), addr2));
        assert_eq!(rt.len(), 2);
    }

    #[test]
    fn restrict_ips_remove_frees_ip_slot() {
        let mut rt = RoutingTable::new_with_config(Id20::ZERO, true);
        let addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        assert!(rt.insert(id(1), addr));
        assert!(rt.remove(&id(1)));
        // IP slot is now free — different node with same IP can insert
        assert!(rt.insert(id(2), addr));
        assert_eq!(rt.len(), 1);
    }

    // ── Liveness / NodeStatus tests ────────────────────────────────

    #[test]
    fn node_status_bad_on_two_failures() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        rt.mark_failed(&id(1));
        assert_eq!(rt.get(&id(1)).unwrap().status(), NodeStatus::Questionable);
        rt.mark_failed(&id(1));
        assert_eq!(rt.get(&id(1)).unwrap().status(), NodeStatus::Bad);
    }

    #[test]
    fn node_status_good_after_mark_response() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        // Freshly inserted nodes have no last_response/last_query, so Questionable
        assert_eq!(rt.get(&id(1)).unwrap().status(), NodeStatus::Questionable);
        rt.mark_response(&id(1));
        assert_eq!(rt.get(&id(1)).unwrap().status(), NodeStatus::Good);
    }

    #[test]
    fn node_status_good_after_mark_query() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        rt.mark_query(&id(1));
        assert_eq!(rt.get(&id(1)).unwrap().status(), NodeStatus::Good);
    }

    #[test]
    fn mark_response_resets_fail_count() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        rt.mark_failed(&id(1));
        rt.mark_failed(&id(1));
        assert_eq!(rt.get(&id(1)).unwrap().status(), NodeStatus::Bad);
        rt.mark_response(&id(1));
        assert_eq!(rt.get(&id(1)).unwrap().fail_count, 0);
        assert_eq!(rt.get(&id(1)).unwrap().status(), NodeStatus::Good);
    }

    #[test]
    fn mark_response_noop_for_unknown_node() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        // Should not panic when node is not in the table
        rt.mark_response(&id(42));
    }

    #[test]
    fn mark_query_noop_for_unknown_node() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.mark_query(&id(42));
    }

    #[test]
    fn get_mut_finds_node() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        let node = rt.get_mut(&id(1));
        assert!(node.is_some());
        // Mutate through the reference
        node.unwrap().fail_count = 99;
        assert_eq!(rt.get(&id(1)).unwrap().fail_count, 99);
    }

    #[test]
    fn get_mut_returns_none_for_missing_node() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        assert!(rt.get_mut(&id(1)).is_none());
    }

    #[test]
    fn questionable_nodes_filters_correctly() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1)); // Questionable (no activity)
        rt.insert(id(2), addr(2)); // Will be Good
        rt.insert(id(3), addr(3)); // Will be Bad
        rt.mark_response(&id(2));
        rt.mark_failed(&id(3));
        rt.mark_failed(&id(3));

        let q = rt.questionable_nodes();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, id(1));
    }

    #[test]
    fn questionable_nodes_empty_when_all_good_or_bad() {
        let mut rt = RoutingTable::new(Id20::ZERO);
        rt.insert(id(1), addr(1));
        rt.insert(id(2), addr(2));
        rt.mark_response(&id(1));
        rt.mark_failed(&id(2));
        rt.mark_failed(&id(2));

        assert!(rt.questionable_nodes().is_empty());
    }

    #[test]
    fn worst_node_evicts_oldest_on_tied_fail_counts() {
        // Build a bucket manually to control insertion order and last_seen values
        let mut bucket = KBucket::new();
        let now = Instant::now();
        // Node A: inserted earlier (older last_seen), fail_count=1
        bucket.nodes.push(RoutingNode {
            id: id(1),
            addr: addr(1),
            last_seen: now - std::time::Duration::from_secs(100),
            fail_count: 1,
            last_response: None,
            last_query: None,
        });
        // Node B: inserted more recently (newer last_seen), fail_count=1
        bucket.nodes.push(RoutingNode {
            id: id(2),
            addr: addr(2),
            last_seen: now - std::time::Duration::from_secs(10),
            fail_count: 1,
            last_response: None,
            last_query: None,
        });
        // worst_node should return index 0 (the oldest node)
        let worst = bucket.worst_node().unwrap();
        assert_eq!(bucket.nodes[worst].id, id(1), "oldest node should be evicted on tied fail counts");
    }

    #[test]
    fn worst_node_prefers_highest_fail_count() {
        let mut bucket = KBucket::new();
        let now = Instant::now();
        bucket.nodes.push(RoutingNode {
            id: id(1),
            addr: addr(1),
            last_seen: now,
            fail_count: 3,
            last_response: None,
            last_query: None,
        });
        bucket.nodes.push(RoutingNode {
            id: id(2),
            addr: addr(2),
            last_seen: now - std::time::Duration::from_secs(1000),
            fail_count: 1,
            last_response: None,
            last_query: None,
        });
        let worst = bucket.worst_node().unwrap();
        assert_eq!(bucket.nodes[worst].id, id(1), "highest fail_count should win regardless of age");
    }

    #[test]
    fn restrict_ips_eviction_frees_ip_slot() {
        let mut rt = RoutingTable::new_with_config(Id20::ZERO, true);
        // Fill bucket to capacity, each with different IP
        for i in 1..=K as u8 {
            let a: SocketAddr = format!("10.0.0.{}:6881", i).parse().unwrap();
            rt.insert(id(i), a);
        }
        assert_eq!(rt.len(), K);

        // Mark a node as failed
        rt.mark_failed(&id(1));
        rt.mark_failed(&id(1));

        // New node with a different IP should evict the failed node
        let new_addr: SocketAddr = "10.0.0.100:6881".parse().unwrap();
        assert!(rt.insert(id(100), new_addr));
        assert_eq!(rt.len(), K);
        // The old IP (10.0.0.1) should be freed
        let old_addr: SocketAddr = "10.0.0.1:6882".parse().unwrap();
        assert!(rt.insert(id(200), old_addr));
    }

    #[test]
    fn mark_all_questionable_resets_liveness() {
        let own_id = Id20([0x00; 20]);
        let mut rt = RoutingTable::new(own_id);

        // Insert two nodes and mark them as responsive
        let node1 = Id20([0x80; 20]);
        let node2 = Id20([0x40; 20]);
        let addr1: SocketAddr = "192.0.2.1:6881".parse().unwrap();
        let addr2: SocketAddr = "192.0.2.2:6881".parse().unwrap();

        rt.insert(node1, addr1);
        rt.insert(node2, addr2);
        rt.mark_response(&node1);
        rt.mark_response(&node2);

        // Verify nodes are Good
        assert_eq!(rt.get(&node1).unwrap().status(), NodeStatus::Good);
        assert_eq!(rt.get(&node2).unwrap().status(), NodeStatus::Good);

        // Mark all questionable
        rt.mark_all_questionable();

        // Verify nodes are now Questionable
        assert_eq!(rt.get(&node1).unwrap().status(), NodeStatus::Questionable);
        assert_eq!(rt.get(&node2).unwrap().status(), NodeStatus::Questionable);
        assert_eq!(rt.get(&node1).unwrap().fail_count, 0);
        assert_eq!(rt.get(&node2).unwrap().fail_count, 0);
    }
}
