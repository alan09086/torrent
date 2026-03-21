//! Unified info-hash container for v1 (SHA-1) and v2 (SHA-256) hashes.
//!
//! Mirrors libtorrent's `info_hash_t` — every component that needs an info hash
//! uses `InfoHashes`, which gracefully handles v1-only, v2-only, and hybrid torrents.

use crate::hash::{Id20, Id32};
use serde::Serialize;

/// Holds optional v1 (SHA-1) and v2 (SHA-256) info hashes.
///
/// At least one hash must be present. Used throughout the stack as the canonical
/// way to identify a torrent regardless of protocol version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct InfoHashes {
    /// v1 info hash (SHA-1 of the v1 info dict).
    pub v1: Option<Id20>,
    /// v2 info hash (SHA-256 of the v2 info dict).
    pub v2: Option<Id32>,
}

impl InfoHashes {
    /// Create with only a v1 (SHA-1) hash.
    pub fn v1_only(hash: Id20) -> Self {
        Self {
            v1: Some(hash),
            v2: None,
        }
    }

    /// Create with only a v2 (SHA-256) hash.
    pub fn v2_only(hash: Id32) -> Self {
        Self {
            v1: None,
            v2: Some(hash),
        }
    }

    /// Create with both v1 and v2 hashes (hybrid torrent).
    pub fn hybrid(v1: Id20, v2: Id32) -> Self {
        Self {
            v1: Some(v1),
            v2: Some(v2),
        }
    }

    /// Whether a v1 hash is present.
    pub fn has_v1(&self) -> bool {
        self.v1.is_some()
    }

    /// Whether a v2 hash is present.
    pub fn has_v2(&self) -> bool {
        self.v2.is_some()
    }

    /// Whether both v1 and v2 hashes are present (hybrid torrent).
    pub fn is_hybrid(&self) -> bool {
        self.v1.is_some() && self.v2.is_some()
    }

    /// Get the best available v1 hash for tracker/DHT compatibility.
    ///
    /// Returns the v1 hash if present, otherwise truncates the v2 SHA-256
    /// hash to 20 bytes (as specified by BEP 52 for DHT/tracker fallback).
    pub fn best_v1(&self) -> Id20 {
        if let Some(v1) = self.v1 {
            v1
        } else if let Some(v2) = self.v2 {
            let mut truncated = [0u8; 20];
            truncated.copy_from_slice(&v2.0[..20]);
            Id20(truncated)
        } else {
            Id20::ZERO
        }
    }
}

impl std::fmt::Display for InfoHashes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.v1, &self.v2) {
            (Some(v1), Some(v2)) => write!(f, "v1:{v1} v2:{v2}"),
            (Some(v1), None) => write!(f, "v1:{v1}"),
            (None, Some(v2)) => write!(f, "v2:{v2}"),
            (None, None) => write!(f, "<no hash>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_only_construction() {
        let hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let ih = InfoHashes::v1_only(hash);
        assert!(ih.has_v1());
        assert!(!ih.has_v2());
        assert!(!ih.is_hybrid());
        assert_eq!(ih.best_v1(), hash);
    }

    #[test]
    fn v2_only_construction() {
        let hash =
            Id32::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap();
        let ih = InfoHashes::v2_only(hash);
        assert!(!ih.has_v1());
        assert!(ih.has_v2());
        assert!(!ih.is_hybrid());
        assert_eq!(ih.v2, Some(hash));
    }

    #[test]
    fn hybrid_construction() {
        let v1 = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let v2 = Id32::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
            .unwrap();
        let ih = InfoHashes::hybrid(v1, v2);
        assert!(ih.has_v1());
        assert!(ih.has_v2());
        assert!(ih.is_hybrid());
        assert_eq!(ih.best_v1(), v1);
    }

    #[test]
    fn best_v1_truncation_from_v2() {
        let v2 = Id32::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
            .unwrap();
        let ih = InfoHashes::v2_only(v2);
        let truncated = ih.best_v1();
        // First 20 bytes of the SHA-256 hash
        assert_eq!(
            truncated.to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4"
        );
    }
}
