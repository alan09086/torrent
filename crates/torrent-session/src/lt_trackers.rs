//! Tracker exchange between peers (lt_trackers).
//!
//! Allows peers to share tracker URLs they know about, enabling
//! discovery of additional trackers for a torrent.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// A tracker exchange message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct LtTrackersMessage {
    /// Tracker URLs being shared.
    #[serde(default)]
    pub added: Vec<String>,
}

#[allow(dead_code)]
impl LtTrackersMessage {
    /// Deserialize from bencoded bytes.
    pub fn from_bytes(data: &[u8]) -> crate::Result<Self> {
        torrent_bencode::from_bytes(data)
            .map_err(|e| crate::Error::Core(torrent_core::Error::from(e)))
    }

    /// Serialize to bencoded bytes.
    pub fn to_bytes(&self) -> crate::Result<Bytes> {
        let bytes = torrent_bencode::to_bytes(self)
            .map_err(|e| crate::Error::Core(torrent_core::Error::from(e)))?;
        Ok(Bytes::from(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let msg = LtTrackersMessage {
            added: vec![
                "http://tracker1.example.com/announce".into(),
                "udp://tracker2.example.com:6969/announce".into(),
            ],
        };
        let encoded = msg.to_bytes().expect("encode failed");
        let decoded = LtTrackersMessage::from_bytes(&encoded).expect("decode failed");
        assert_eq!(decoded.added.len(), 2);
        assert_eq!(decoded.added[0], "http://tracker1.example.com/announce");
        assert_eq!(decoded.added[1], "udp://tracker2.example.com:6969/announce");
    }

    #[test]
    fn empty_message() {
        let msg = LtTrackersMessage::default();
        let encoded = msg.to_bytes().expect("encode failed");
        let decoded = LtTrackersMessage::from_bytes(&encoded).expect("decode failed");
        assert!(decoded.added.is_empty());
    }

    #[test]
    fn single_tracker() {
        let msg = LtTrackersMessage {
            added: vec!["http://tracker.example.com/announce".into()],
        };
        let encoded = msg.to_bytes().expect("encode failed");
        let decoded = LtTrackersMessage::from_bytes(&encoded).expect("decode failed");
        assert_eq!(decoded.added.len(), 1);
        assert_eq!(decoded.added[0], "http://tracker.example.com/announce");
    }
}
