use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use torrent_core::Id20;

/// BEP 9 metadata piece size: 16 KiB.
const METADATA_PIECE_SIZE: u64 = 16384;

/// State machine for downloading torrent metadata via BEP 9 (magnet links).
///
/// Metadata is split into 16 KiB pieces. Once all pieces are received,
/// they are assembled and verified against the info_hash.
///
/// Supports full-redundancy parallel fetch: every peer that advertises
/// `ut_metadata` is sent requests for ALL missing pieces. The first
/// complete set (SHA1-verified) wins.
#[allow(dead_code)]
pub(crate) struct MetadataDownloader {
    info_hash: Id20,
    total_size: Option<u64>,
    pieces: HashMap<u32, Bytes>,
    num_pieces: Option<u32>,
    /// Peers that have been sent metadata requests (peer -> set of requested pieces).
    requested_peers: HashMap<SocketAddr, HashSet<u32>>,
    /// Peers that rejected our metadata requests — don't request from them again.
    rejected_peers: HashSet<SocketAddr>,
    /// When each piece was last requested (for timeout detection).
    piece_request_times: HashMap<u32, Instant>,
}

#[allow(dead_code)]
impl MetadataDownloader {
    /// Create a new downloader with no size known yet.
    pub fn new(info_hash: Id20) -> Self {
        Self {
            info_hash,
            total_size: None,
            pieces: HashMap::new(),
            num_pieces: None,
            requested_peers: HashMap::new(),
            rejected_peers: HashSet::new(),
            piece_request_times: HashMap::new(),
        }
    }

    /// Set the total metadata size and calculate the number of pieces.
    pub fn set_total_size(&mut self, size: u64) {
        self.total_size = Some(size);
        self.num_pieces = Some(size.div_ceil(METADATA_PIECE_SIZE) as u32);
    }

    /// Store a received piece. Returns `true` if all pieces have been received.
    pub fn piece_received(&mut self, piece: u32, data: Bytes) -> bool {
        self.pieces.insert(piece, data);
        match self.num_pieces {
            Some(n) => self.pieces.len() == n as usize,
            None => false,
        }
    }

    /// Concatenate all pieces in order and verify the SHA1 hash matches info_hash.
    ///
    /// Returns the assembled metadata bytes on success.
    pub fn assemble_and_verify(&self) -> crate::Result<Vec<u8>> {
        let num_pieces = self
            .num_pieces
            .ok_or_else(|| crate::Error::Connection("metadata incomplete".to_string()))?;

        if self.pieces.len() != num_pieces as usize {
            return Err(crate::Error::Connection("metadata incomplete".to_string()));
        }

        let mut assembled = Vec::with_capacity(self.total_size.unwrap_or(0) as usize);
        for i in 0..num_pieces {
            let piece = self
                .pieces
                .get(&i)
                .ok_or_else(|| crate::Error::Connection("metadata incomplete".to_string()))?;
            assembled.extend_from_slice(piece);
        }

        let hash = torrent_core::sha1(&assembled);
        if hash != self.info_hash {
            return Err(crate::Error::MetadataHashMismatch);
        }

        Ok(assembled)
    }

    /// Return sorted list of piece indices we don't have yet.
    ///
    /// Returns an empty vec if `num_pieces` is not yet known.
    pub fn missing_pieces(&self) -> Vec<u32> {
        match self.num_pieces {
            None => Vec::new(),
            Some(n) => (0..n).filter(|i| !self.pieces.contains_key(i)).collect(),
        }
    }

    /// Mark a peer as having rejected metadata requests.
    ///
    /// The peer is added to the rejected set and removed from the active
    /// requested set. No further requests will be sent to this peer.
    pub fn mark_rejected(&mut self, peer: SocketAddr) {
        self.rejected_peers.insert(peer);
        self.requested_peers.remove(&peer);
    }

    /// Check whether a peer has been rejected.
    pub fn is_rejected(&self, peer: &SocketAddr) -> bool {
        self.rejected_peers.contains(peer)
    }

    /// Request all missing pieces from a peer (full redundancy).
    ///
    /// Returns the list of piece indices to request from this peer.
    /// Skips pieces we already have and rejects requests to blacklisted peers.
    /// Records the peer in `requested_peers` and updates `piece_request_times`.
    pub fn request_all_from_peer(&mut self, peer: SocketAddr) -> Vec<u32> {
        if self.rejected_peers.contains(&peer) {
            return Vec::new();
        }

        let missing = self.missing_pieces();
        if missing.is_empty() {
            return Vec::new();
        }

        let now = Instant::now();
        let peer_set = self
            .requested_peers
            .entry(peer)
            .or_default();
        for &piece in &missing {
            peer_set.insert(piece);
            // Only set request time if not already tracked (first requester sets the clock)
            self.piece_request_times.entry(piece).or_insert(now);
        }

        missing
    }

    /// Return piece indices whose last request time exceeds `timeout` and
    /// that have not yet been received.
    pub fn timed_out_pieces(&self, timeout: Duration) -> Vec<u32> {
        let now = Instant::now();
        self.piece_request_times
            .iter()
            .filter(|(piece, requested_at)| {
                !self.pieces.contains_key(piece)
                    && now.duration_since(**requested_at) >= timeout
            })
            .map(|(piece, _)| *piece)
            .collect()
    }

    /// Reset the request time for a piece (e.g., after re-requesting on timeout).
    pub fn reset_request_time(&mut self, piece: u32) {
        self.piece_request_times.insert(piece, Instant::now());
    }

    /// Whether any non-rejected peers have outstanding requests.
    pub fn has_active_peers(&self) -> bool {
        self.requested_peers
            .keys()
            .any(|peer| !self.rejected_peers.contains(peer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::Id20;

    #[test]
    fn new_empty() {
        let info_hash = Id20::ZERO;
        let dl = MetadataDownloader::new(info_hash);
        assert!(dl.total_size.is_none());
        assert!(dl.num_pieces.is_none());
        assert!(dl.pieces.is_empty());
        assert!(dl.requested_peers.is_empty());
        assert!(dl.rejected_peers.is_empty());
        assert!(dl.piece_request_times.is_empty());
    }

    #[test]
    fn set_total_size_calculates_num_pieces() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);

        dl.set_total_size(32768);
        assert_eq!(dl.num_pieces, Some(2));

        dl.set_total_size(16384);
        assert_eq!(dl.num_pieces, Some(1));

        dl.set_total_size(16385);
        assert_eq!(dl.num_pieces, Some(2));
    }

    #[test]
    fn single_piece_metadata() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(100);
        let complete = dl.piece_received(0, Bytes::from(vec![0u8; 100]));
        assert!(complete);
    }

    #[test]
    fn multi_piece_metadata() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(32768); // 2 pieces

        let complete = dl.piece_received(0, Bytes::from(vec![0u8; 16384]));
        assert!(!complete);

        let complete = dl.piece_received(1, Bytes::from(vec![0u8; 16384]));
        assert!(complete);
    }

    #[test]
    fn piece_received_returns_false_when_incomplete() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(32768); // 2 pieces

        let complete = dl.piece_received(0, Bytes::from(vec![0u8; 16384]));
        assert!(!complete);
    }

    #[test]
    fn assemble_and_verify_correct_hash() {
        // Create known test data that fits in a single piece.
        let data = b"hello world metadata test data!!";
        let info_hash = torrent_core::sha1(data);

        let mut dl = MetadataDownloader::new(info_hash);
        dl.set_total_size(data.len() as u64);
        dl.piece_received(0, Bytes::from(data.to_vec()));

        let result = dl.assemble_and_verify().unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn assemble_and_verify_wrong_hash() {
        let data = b"hello world metadata test data!!";
        // Use a wrong info_hash (all zeros will not match).
        let wrong_hash = Id20::ZERO;

        let mut dl = MetadataDownloader::new(wrong_hash);
        dl.set_total_size(data.len() as u64);
        dl.piece_received(0, Bytes::from(data.to_vec()));

        let result = dl.assemble_and_verify();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::Error::MetadataHashMismatch),
            "expected MetadataHashMismatch, got: {err:?}"
        );
    }

    // --- M107: New tests for parallel metadata fetch ---

    #[test]
    fn metadata_full_redundancy_all_pieces_to_each_peer() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(32768); // 2 pieces

        let peer_a: SocketAddr = "10.0.0.1:6881".parse().expect("valid addr");
        let peer_b: SocketAddr = "10.0.0.2:6881".parse().expect("valid addr");

        // Both peers should get all missing pieces (full redundancy).
        let pieces_a = dl.request_all_from_peer(peer_a);
        assert_eq!(pieces_a, vec![0, 1]);

        let pieces_b = dl.request_all_from_peer(peer_b);
        assert_eq!(pieces_b, vec![0, 1]);

        // Both peers recorded in requested_peers.
        assert!(dl.requested_peers.contains_key(&peer_a));
        assert!(dl.requested_peers.contains_key(&peer_b));
        assert_eq!(dl.requested_peers[&peer_a].len(), 2);
        assert_eq!(dl.requested_peers[&peer_b].len(), 2);

        // Request times set for both pieces.
        assert!(dl.piece_request_times.contains_key(&0));
        assert!(dl.piece_request_times.contains_key(&1));
    }

    #[test]
    fn metadata_reject_blacklists_peer() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(32768); // 2 pieces

        let peer_a: SocketAddr = "10.0.0.1:6881".parse().expect("valid addr");
        let peer_b: SocketAddr = "10.0.0.2:6881".parse().expect("valid addr");

        // Request from both peers.
        let _ = dl.request_all_from_peer(peer_a);
        let _ = dl.request_all_from_peer(peer_b);

        // Reject peer_a.
        dl.mark_rejected(peer_a);

        assert!(dl.is_rejected(&peer_a));
        assert!(!dl.is_rejected(&peer_b));

        // Rejected peer removed from requested_peers.
        assert!(!dl.requested_peers.contains_key(&peer_a));

        // No further requests to rejected peer.
        let pieces = dl.request_all_from_peer(peer_a);
        assert!(pieces.is_empty());

        // peer_b still works.
        let pieces = dl.request_all_from_peer(peer_b);
        assert_eq!(pieces, vec![0, 1]);
    }

    #[test]
    fn metadata_timeout_triggers_rerequest() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(32768); // 2 pieces

        let peer_a: SocketAddr = "10.0.0.1:6881".parse().expect("valid addr");
        let _ = dl.request_all_from_peer(peer_a);

        // Simulate time passing by backdating the request times.
        let old_time = Instant::now() - Duration::from_secs(10);
        dl.piece_request_times.insert(0, old_time);
        dl.piece_request_times.insert(1, old_time);

        // Both pieces should be timed out with a 5s threshold.
        let timed_out = dl.timed_out_pieces(Duration::from_secs(5));
        assert_eq!(timed_out.len(), 2);
        assert!(timed_out.contains(&0));
        assert!(timed_out.contains(&1));

        // After receiving piece 0, only piece 1 should time out.
        dl.piece_received(0, Bytes::from(vec![0u8; 16384]));
        let timed_out = dl.timed_out_pieces(Duration::from_secs(5));
        assert_eq!(timed_out, vec![1]);
    }

    #[test]
    fn metadata_parallel_fetch_from_multiple_peers() {
        // Two peers each provide different pieces — assembly succeeds.
        let data = b"hello world metadata test data!!";
        let info_hash = torrent_core::sha1(data);

        let mut dl = MetadataDownloader::new(info_hash);
        dl.set_total_size(data.len() as u64); // 1 piece (< 16 KiB)

        let peer_a: SocketAddr = "10.0.0.1:6881".parse().expect("valid addr");
        let peer_b: SocketAddr = "10.0.0.2:6881".parse().expect("valid addr");

        // Both peers get piece 0.
        let pieces_a = dl.request_all_from_peer(peer_a);
        let pieces_b = dl.request_all_from_peer(peer_b);
        assert_eq!(pieces_a, vec![0]);
        assert_eq!(pieces_b, vec![0]);

        // Peer A delivers first — completes the download.
        let complete = dl.piece_received(0, Bytes::from(data.to_vec()));
        assert!(complete);

        // Assembly works despite peer B never delivering.
        let result = dl.assemble_and_verify().unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn metadata_parallel_multi_piece_assembly() {
        // 3-piece metadata: peer A delivers piece 0, peer B delivers piece 1,
        // peer A delivers piece 2. Verifies cross-peer assembly.
        let data = vec![0xAA_u8; 16384 * 2 + 100]; // 2 full pieces + partial = 3 pieces
        let info_hash = torrent_core::sha1(&data);

        let mut dl = MetadataDownloader::new(info_hash);
        dl.set_total_size(data.len() as u64);
        assert_eq!(dl.num_pieces, Some(3));

        let peer_a: SocketAddr = "10.0.0.1:6881".parse().expect("valid addr");
        let peer_b: SocketAddr = "10.0.0.2:6881".parse().expect("valid addr");

        let _ = dl.request_all_from_peer(peer_a);
        let _ = dl.request_all_from_peer(peer_b);

        // Peer A delivers piece 0.
        assert!(!dl.piece_received(0, Bytes::from(data[..16384].to_vec())));
        // Peer B delivers piece 1.
        assert!(!dl.piece_received(1, Bytes::from(data[16384..32768].to_vec())));
        // Peer A delivers piece 2 — completes.
        assert!(dl.piece_received(2, Bytes::from(data[32768..].to_vec())));

        let result = dl.assemble_and_verify().unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn has_active_peers_reflects_state() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(16384);

        // No peers yet.
        assert!(!dl.has_active_peers());

        let peer_a: SocketAddr = "10.0.0.1:6881".parse().expect("valid addr");
        let _ = dl.request_all_from_peer(peer_a);
        assert!(dl.has_active_peers());

        // Reject the only active peer.
        dl.mark_rejected(peer_a);
        assert!(!dl.has_active_peers());
    }

    #[test]
    fn request_all_from_peer_skips_received_pieces() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(32768); // 2 pieces

        // Receive piece 0 first.
        dl.piece_received(0, Bytes::from(vec![0u8; 16384]));

        let peer: SocketAddr = "10.0.0.1:6881".parse().expect("valid addr");
        let pieces = dl.request_all_from_peer(peer);
        // Only piece 1 should be requested.
        assert_eq!(pieces, vec![1]);
    }

    #[test]
    fn reset_request_time_updates_timestamp() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(16384);

        let peer: SocketAddr = "10.0.0.1:6881".parse().expect("valid addr");
        let _ = dl.request_all_from_peer(peer);

        // Backdate the request time.
        let old_time = Instant::now() - Duration::from_secs(10);
        dl.piece_request_times.insert(0, old_time);

        // Piece should be timed out.
        assert_eq!(dl.timed_out_pieces(Duration::from_secs(5)).len(), 1);

        // Reset the time.
        dl.reset_request_time(0);

        // No longer timed out.
        assert!(dl.timed_out_pieces(Duration::from_secs(5)).is_empty());
    }
}
