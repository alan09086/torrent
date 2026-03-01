//! Core types for BitTorrent: hashes, metainfo, magnets, and piece arithmetic.

mod create;
mod detect;
mod error;
mod file_tree;
mod hash;
mod info_hashes;
mod lengths;
mod merkle;
mod magnet;
mod metainfo;
mod metainfo_v2;
mod peer_id;
mod resume_data;
mod file_priority;
mod hash_picker;
mod hash_request;
mod merkle_state;
mod storage_mode;
mod torrent_version;

pub use create::{CreateTorrent, CreateTorrentResult};
pub use detect::{TorrentMeta, torrent_from_bytes_any};
pub use torrent_version::TorrentVersion;
pub use error::{Error, Result};
pub use file_tree::{FileTreeNode, V2FileAttr, V2FileInfo};
pub use file_priority::FilePriority;
pub use hash::{Id20, Id32};
pub use info_hashes::InfoHashes;
pub use merkle::MerkleTree;
pub use lengths::{Lengths, DEFAULT_CHUNK_SIZE};
pub use magnet::Magnet;
pub use metainfo::{FileEntry, FileInfo, InfoDict, TorrentMetaV1, torrent_from_bytes};
pub use metainfo_v2::{InfoDictV2, TorrentMetaV2, torrent_v2_from_bytes};
pub use peer_id::PeerId;
pub use resume_data::{FastResumeData, UnfinishedPiece};
pub use hash_picker::{AddHashesResult, FileHashInfo, HashPicker};
pub use hash_request::{HashRequest, validate_hash_request};
pub use merkle_state::{MerkleTreeState, SetBlockResult};
pub use storage_mode::StorageMode;

/// Network address family for dual-stack support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressFamily {
    V4,
    V6,
}

/// Compute SHA1 hash of input bytes.
pub fn sha1(data: &[u8]) -> Id20 {
    use sha1::Digest;
    let hash = sha1::Sha1::digest(data);
    Id20(hash.into())
}

/// Compute SHA-256 hash of input bytes (used by BitTorrent v2, BEP 52).
pub fn sha256(data: &[u8]) -> Id32 {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(data);
    Id32(hash.into())
}

/// Fill a buffer with pseudo-random bytes (xorshift64, not cryptographic).
pub fn random_bytes(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = peer_id::random_byte();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty_string() {
        let hash = sha256(b"");
        assert_eq!(
            hash.to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hello() {
        let hash = sha256(b"hello");
        assert_eq!(
            hash.to_hex(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn random_bytes_fills_buffer() {
        let mut buf = [0u8; 32];
        random_bytes(&mut buf);
        // At least some bytes should be non-zero (probability of all-zero is ~0)
        assert!(buf.iter().any(|&b| b != 0));
    }
}
