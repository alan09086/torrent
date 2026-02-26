use serde::{Deserialize, Serialize};

/// A DHT bootstrap node entry for session persistence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DhtNodeEntry {
    pub host: String,
    pub port: i64,
}

/// Persisted session state containing a DHT node cache and torrent resume data.
///
/// Serializes to bencode for on-disk persistence. The DHT node list allows
/// faster bootstrapping on restart, and the torrent list holds
/// [`ferrite_core::FastResumeData`] entries so torrents can skip piece
/// verification when the bitfield matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(rename = "dht-nodes", default)]
    pub dht_nodes: Vec<DhtNodeEntry>,
    #[serde(rename = "torrents", default)]
    pub torrents: Vec<ferrite_core::FastResumeData>,
}

impl SessionState {
    /// Create a new empty `SessionState`.
    pub fn new() -> Self {
        Self {
            dht_nodes: Vec::new(),
            torrents: Vec::new(),
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
            torrents: vec![ferrite_core::FastResumeData::new(
                vec![0xAA; 20],
                "test-torrent".into(),
                "/downloads".into(),
            )],
        };

        let encoded = ferrite_bencode::to_bytes(&state).unwrap();
        let decoded: SessionState = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn empty_session_state_round_trip() {
        let state = SessionState::new();

        let encoded = ferrite_bencode::to_bytes(&state).unwrap();
        let decoded: SessionState = ferrite_bencode::from_bytes(&encoded).unwrap();
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
}
