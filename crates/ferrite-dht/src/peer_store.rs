//! Per-info_hash peer storage and token generation/validation.
//!
//! Tokens are generated per-IP using a rotating secret so that
//! `announce_peer` requests can be validated without persistent state.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use ferrite_core::{sha1, Id20};

/// How long peers are kept before expiry.
const PEER_EXPIRY: Duration = Duration::from_secs(30 * 60); // 30 minutes

/// How often the token secret rotates.
const TOKEN_ROTATION: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Stores peers per info_hash and generates/validates announce tokens.
#[derive(Debug)]
pub struct PeerStore {
    /// Current secret for token generation.
    secret: [u8; 20],
    /// Previous secret (still valid for token validation).
    prev_secret: [u8; 20],
    /// When the current secret was created.
    secret_created: Instant,
    /// Peers per info_hash.
    peers: HashMap<Id20, Vec<StoredPeer>>,
}

#[derive(Debug, Clone)]
struct StoredPeer {
    addr: SocketAddr,
    added: Instant,
}

impl PeerStore {
    pub fn new() -> Self {
        let secret = generate_secret();
        PeerStore {
            secret,
            prev_secret: secret,
            secret_created: Instant::now(),
            peers: HashMap::new(),
        }
    }

    /// Generate a token for the given IP address.
    pub fn generate_token(&mut self, ip: &IpAddr) -> Vec<u8> {
        self.maybe_rotate();
        make_token(&self.secret, ip)
    }

    /// Validate a token for the given IP address.
    pub fn validate_token(&mut self, token: &[u8], ip: &IpAddr) -> bool {
        self.maybe_rotate();
        let current = make_token(&self.secret, ip);
        let previous = make_token(&self.prev_secret, ip);
        token == current.as_slice() || token == previous.as_slice()
    }

    /// Add a peer for an info_hash.
    pub fn add_peer(&mut self, info_hash: Id20, addr: SocketAddr) {
        let peers = self.peers.entry(info_hash).or_default();
        // Update existing or add new
        if let Some(existing) = peers.iter_mut().find(|p| p.addr == addr) {
            existing.added = Instant::now();
        } else {
            peers.push(StoredPeer {
                addr,
                added: Instant::now(),
            });
        }
    }

    /// Get peers for an info_hash (up to `max` results).
    pub fn get_peers(&self, info_hash: &Id20, max: usize) -> Vec<SocketAddr> {
        self.peers
            .get(info_hash)
            .map(|peers| {
                let cutoff = Instant::now() - PEER_EXPIRY;
                peers
                    .iter()
                    .filter(|p| p.added > cutoff)
                    .take(max)
                    .map(|p| p.addr)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Remove expired peers from all info_hashes.
    pub fn cleanup(&mut self) {
        let cutoff = Instant::now() - PEER_EXPIRY;
        self.peers.retain(|_, peers| {
            peers.retain(|p| p.added > cutoff);
            !peers.is_empty()
        });
    }

    /// Number of info_hashes with stored peers.
    pub fn info_hash_count(&self) -> usize {
        self.peers.len()
    }

    /// Total number of stored peers across all info_hashes.
    pub fn peer_count(&self) -> usize {
        self.peers.values().map(|p| p.len()).sum()
    }

    fn maybe_rotate(&mut self) {
        if self.secret_created.elapsed() >= TOKEN_ROTATION {
            self.prev_secret = self.secret;
            self.secret = generate_secret();
            self.secret_created = Instant::now();
        }
    }
}

impl Default for PeerStore {
    fn default() -> Self {
        Self::new()
    }
}

fn make_token(secret: &[u8; 20], ip: &IpAddr) -> Vec<u8> {
    let ip_bytes = match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    let mut data = Vec::with_capacity(secret.len() + ip_bytes.len());
    data.extend_from_slice(secret);
    data.extend_from_slice(&ip_bytes);
    let hash = sha1(&data);
    hash.0[..8].to_vec() // 8-byte token is sufficient
}

fn generate_secret() -> [u8; 20] {
    use std::time::SystemTime;
    use std::cell::Cell;

    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        );
    }

    let mut secret = [0u8; 20];
    for byte in &mut secret {
        STATE.with(|s| {
            let mut x = s.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.set(x);
            *byte = x as u8;
        });
    }
    secret
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validates_same_ip() {
        let mut store = PeerStore::new();
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let token = store.generate_token(&ip);
        assert!(store.validate_token(&token, &ip));
    }

    #[test]
    fn token_rejects_different_ip() {
        let mut store = PeerStore::new();
        let ip1: IpAddr = "192.168.1.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.1".parse().unwrap();
        let token = store.generate_token(&ip1);
        assert!(!store.validate_token(&token, &ip2));
    }

    #[test]
    fn add_and_get_peers() {
        let mut store = PeerStore::new();
        let hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();

        store.add_peer(hash, addr);
        let peers = store.get_peers(&hash, 10);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0], addr);
    }

    #[test]
    fn get_peers_unknown_hash() {
        let store = PeerStore::new();
        let hash = Id20::ZERO;
        let peers = store.get_peers(&hash, 10);
        assert!(peers.is_empty());
    }

    #[test]
    fn duplicate_peer_updates() {
        let mut store = PeerStore::new();
        let hash = Id20::ZERO;
        let addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();

        store.add_peer(hash, addr);
        store.add_peer(hash, addr); // duplicate
        assert_eq!(store.peer_count(), 1);
    }

    #[test]
    fn cleanup_preserves_recent_peers() {
        let mut store = PeerStore::new();
        let hash = Id20::ZERO;
        store.add_peer(hash, "10.0.0.1:6881".parse().unwrap());
        store.cleanup();
        assert_eq!(store.peer_count(), 1);
    }
}
