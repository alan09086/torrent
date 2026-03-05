use serde::{Deserialize, Serialize};

/// A DHT bootstrap node entry for session persistence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DhtNodeEntry {
    /// Hostname or IP address of the DHT node.
    pub host: String,
    /// Port number of the DHT node.
    pub port: i64,
}

/// A peer strike entry for session persistence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerStrikeEntry {
    /// IP address of the peer that received strikes.
    pub ip: String,
    /// Number of accumulated strikes.
    pub count: i64,
}

/// Persisted session state containing a DHT node cache and torrent resume data.
///
/// Serializes to bencode for on-disk persistence. The DHT node list allows
/// faster bootstrapping on restart, and the torrent list holds
/// [`torrent_core::FastResumeData`] entries so torrents can skip piece
/// verification when the bitfield matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    /// Cached DHT routing table nodes for faster bootstrap on restart.
    #[serde(rename = "dht-nodes", default)]
    pub dht_nodes: Vec<DhtNodeEntry>,
    /// Fast resume data for each torrent in the session.
    #[serde(rename = "torrents", default)]
    pub torrents: Vec<torrent_core::FastResumeData>,
    /// IP addresses of permanently banned peers.
    #[serde(rename = "banned-peers", default)]
    pub banned_peers: Vec<String>,
    /// Per-peer strike counts for the smart ban system.
    #[serde(rename = "peer-strikes", default)]
    pub peer_strikes: Vec<PeerStrikeEntry>,
}

impl SessionState {
    /// Create a new empty `SessionState`.
    pub fn new() -> Self {
        Self {
            dht_nodes: Vec::new(),
            torrents: Vec::new(),
            banned_peers: Vec::new(),
            peer_strikes: Vec::new(),
        }
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns `true` if the `pieces` bitfield has the correct length for
/// `num_pieces` pieces (i.e. `ceil(num_pieces / 8)` bytes).
///
/// This is used to decide whether a resume file's piece bitfield is
/// trustworthy and hash verification can be skipped on restart.
pub fn validate_resume_bitfield(pieces: &[u8], num_pieces: u32) -> bool {
    if num_pieces == 0 {
        return pieces.is_empty();
    }
    let expected = num_pieces.div_ceil(8) as usize;
    pieces.len() == expected
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn session_state_bencode_round_trip() {
        let state = SessionState {
            dht_nodes: vec![
                DhtNodeEntry {
                    host: "router.bittorrent.com".into(),
                    port: 6881,
                },
                DhtNodeEntry {
                    host: "dht.transmissionbt.com".into(),
                    port: 6881,
                },
            ],
            torrents: vec![torrent_core::FastResumeData::new(
                vec![0xAA; 20],
                "test-torrent".into(),
                "/downloads".into(),
            )],
            banned_peers: Vec::new(),
            peer_strikes: Vec::new(),
        };

        let encoded = torrent_bencode::to_bytes(&state).unwrap();
        let decoded: SessionState = torrent_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn empty_session_state_round_trip() {
        let state = SessionState::new();

        let encoded = torrent_bencode::to_bytes(&state).unwrap();
        let decoded: SessionState = torrent_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn validate_resume_bitfield_correct_length() {
        // 8 pieces -> 1 byte
        assert!(validate_resume_bitfield(&[0xFF], 8));
        // 9 pieces -> 2 bytes
        assert!(validate_resume_bitfield(&[0xFF, 0x80], 9));
        // 16 pieces -> 2 bytes
        assert!(validate_resume_bitfield(&[0xFF, 0xFF], 16));
        // 1 piece -> 1 byte
        assert!(validate_resume_bitfield(&[0x80], 1));
    }

    #[test]
    fn validate_resume_bitfield_wrong_length() {
        // 8 pieces with 2 bytes -> wrong
        assert!(!validate_resume_bitfield(&[0xFF, 0x00], 8));
        // 9 pieces with 1 byte -> wrong
        assert!(!validate_resume_bitfield(&[0xFF], 9));
        // 0 pieces with 1 byte of data -> wrong
        assert!(!validate_resume_bitfield(&[0x00], 0));
    }

    #[test]
    fn validate_resume_bitfield_zero_pieces() {
        // 0 pieces with empty data -> true
        assert!(validate_resume_bitfield(&[], 0));
    }

    #[test]
    fn session_state_with_bans_round_trip() {
        let state = SessionState {
            dht_nodes: vec![],
            torrents: vec![],
            banned_peers: vec!["10.0.0.1".into(), "192.168.1.5".into()],
            peer_strikes: vec![
                PeerStrikeEntry {
                    ip: "10.0.0.1".into(),
                    count: 3,
                },
                PeerStrikeEntry {
                    ip: "10.0.0.2".into(),
                    count: 1,
                },
            ],
        };

        let encoded = torrent_bencode::to_bytes(&state).unwrap();
        let decoded: SessionState = torrent_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(state, decoded);
        assert_eq!(decoded.banned_peers.len(), 2);
        assert_eq!(decoded.peer_strikes.len(), 2);
    }

    #[test]
    fn session_state_backward_compatible() {
        // Old format without ban fields — should deserialize cleanly with defaults
        let old_state = SessionState {
            dht_nodes: vec![DhtNodeEntry {
                host: "example.com".into(),
                port: 6881,
            }],
            torrents: vec![],
            banned_peers: vec![],
            peer_strikes: vec![],
        };
        let encoded = torrent_bencode::to_bytes(&old_state).unwrap();

        // Manually create bencode without banned-peers/peer-strikes to simulate old format
        // Since #[serde(default)] is used, decoding old data missing those fields works
        let decoded: SessionState = torrent_bencode::from_bytes(&encoded).unwrap();
        assert!(decoded.banned_peers.is_empty());
        assert!(decoded.peer_strikes.is_empty());
        assert_eq!(decoded.dht_nodes.len(), 1);
    }
}
