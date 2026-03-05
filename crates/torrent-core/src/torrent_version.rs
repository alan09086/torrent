//! Torrent protocol version indicator.

use serde::{Deserialize, Serialize};

/// Indicates which BitTorrent protocol version(s) a torrent supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TorrentVersion {
    /// BitTorrent v1 only (BEP 3). SHA-1 piece hashes.
    V1Only,
    /// BitTorrent v2 only (BEP 52). SHA-256 Merkle per-file trees.
    V2Only,
    /// Hybrid v1+v2 (BEP 52). Both hash types in a single info dict.
    Hybrid,
}

impl TorrentVersion {
    /// Whether this version includes v1 (SHA-1) hashes.
    pub fn has_v1(&self) -> bool {
        matches!(self, Self::V1Only | Self::Hybrid)
    }

    /// Whether this version includes v2 (SHA-256 Merkle) hashes.
    pub fn has_v2(&self) -> bool {
        matches!(self, Self::V2Only | Self::Hybrid)
    }

    /// Whether this is a hybrid torrent with both v1 and v2 hashes.
    pub fn is_hybrid(&self) -> bool {
        matches!(self, Self::Hybrid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_only_flags() {
        let v = TorrentVersion::V1Only;
        assert!(v.has_v1());
        assert!(!v.has_v2());
        assert!(!v.is_hybrid());
    }

    #[test]
    fn v2_only_flags() {
        let v = TorrentVersion::V2Only;
        assert!(!v.has_v1());
        assert!(v.has_v2());
        assert!(!v.is_hybrid());
    }

    #[test]
    fn hybrid_flags() {
        let v = TorrentVersion::Hybrid;
        assert!(v.has_v1());
        assert!(v.has_v2());
        assert!(v.is_hybrid());
    }
}
