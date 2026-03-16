#![warn(missing_docs)]
//! Core BitTorrent types: info hashes, metadata, magnets, piece arithmetic, and torrent creation.

mod create;
mod detect;
mod error;
mod file_priority;
mod file_selection;
mod file_tree;
mod hash;
mod hash_picker;
mod hash_request;
mod info_hashes;
mod lengths;
mod magnet;
mod merkle;
mod merkle_state;
mod metainfo;
mod metainfo_v2;
mod peer_id;
mod resume_data;
mod storage_mode;
mod torrent_version;

pub use create::{CreateTorrent, CreateTorrentResult};
pub use detect::{TorrentMeta, torrent_from_bytes_any};
pub use error::{Error, Result};
pub use file_priority::FilePriority;
pub use file_selection::FileSelection;
pub use file_tree::{FileTreeNode, V2FileAttr, V2FileInfo};
pub use hash::{Id20, Id32};
pub use hash_picker::{AddHashesResult, FileHashInfo, HashPicker};
pub use hash_request::{HashRequest, validate_hash_request};
pub use info_hashes::InfoHashes;
pub use lengths::{DEFAULT_CHUNK_SIZE, Lengths};
pub use magnet::Magnet;
pub use merkle::MerkleTree;
pub use merkle_state::{MerkleTreeState, SetBlockResult};
pub use metainfo::{FileEntry, FileInfo, InfoDict, TorrentMetaV1, torrent_from_bytes};
pub use metainfo_v2::{InfoDictV2, TorrentMetaV2, torrent_v2_from_bytes};
pub use peer_id::PeerId;
pub use resume_data::{FastResumeData, UnfinishedPiece};
pub use storage_mode::StorageMode;
pub use torrent_version::TorrentVersion;

// Re-export Sha1Hasher at crate root (defined below with crypto cfg blocks).

/// Network address family for dual-stack support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressFamily {
    /// IPv4.
    V4,
    /// IPv6.
    V6,
}

// --- Crypto backend: ring ---

/// Compute SHA1 hash of input bytes.
#[cfg(all(feature = "crypto-ring", not(feature = "crypto-openssl"), not(feature = "crypto-aws-lc")))]
pub fn sha1(data: &[u8]) -> Id20 {
    let hash = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data);
    let mut id = [0u8; 20];
    id.copy_from_slice(hash.as_ref());
    Id20(id)
}

/// Compute SHA1 hash of multiple chunks without concatenating them.
///
/// Avoids allocating a large buffer when piece data is stored as separate blocks.
#[cfg(all(feature = "crypto-ring", not(feature = "crypto-openssl"), not(feature = "crypto-aws-lc")))]
pub fn sha1_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Id20 {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    for chunk in chunks {
        ctx.update(chunk);
    }
    let hash = ctx.finish();
    let mut id = [0u8; 20];
    id.copy_from_slice(hash.as_ref());
    Id20(id)
}

/// Compute SHA-256 hash of input bytes (used by BitTorrent v2, BEP 52).
#[cfg(all(feature = "crypto-ring", not(feature = "crypto-openssl"), not(feature = "crypto-aws-lc")))]
pub fn sha256(data: &[u8]) -> Id32 {
    let hash = ring::digest::digest(&ring::digest::SHA256, data);
    let mut id = [0u8; 32];
    id.copy_from_slice(hash.as_ref());
    Id32(id)
}

/// Compute SHA-256 hash of multiple chunks without concatenating them.
#[cfg(all(feature = "crypto-ring", not(feature = "crypto-openssl"), not(feature = "crypto-aws-lc")))]
pub fn sha256_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Id32 {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for chunk in chunks {
        ctx.update(chunk);
    }
    let hash = ctx.finish();
    let mut id = [0u8; 32];
    id.copy_from_slice(hash.as_ref());
    Id32(id)
}

// --- Crypto backend: openssl ---

/// Compute SHA1 hash of input bytes.
#[cfg(feature = "crypto-openssl")]
pub fn sha1(data: &[u8]) -> Id20 {
    let hash = openssl::hash::hash(openssl::hash::MessageDigest::sha1(), data).unwrap();
    let mut id = [0u8; 20];
    id.copy_from_slice(&hash);
    Id20(id)
}

/// Compute SHA1 hash of multiple chunks without concatenating them.
///
/// Avoids allocating a large buffer when piece data is stored as separate blocks.
#[cfg(feature = "crypto-openssl")]
pub fn sha1_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Id20 {
    let mut hasher = openssl::hash::Hasher::new(openssl::hash::MessageDigest::sha1()).unwrap();
    for chunk in chunks {
        hasher.update(chunk).unwrap();
    }
    let hash = hasher.finish().unwrap();
    let mut id = [0u8; 20];
    id.copy_from_slice(&hash);
    Id20(id)
}

/// Compute SHA-256 hash of input bytes (used by BitTorrent v2, BEP 52).
#[cfg(feature = "crypto-openssl")]
pub fn sha256(data: &[u8]) -> Id32 {
    let hash = openssl::hash::hash(openssl::hash::MessageDigest::sha256(), data).unwrap();
    let mut id = [0u8; 32];
    id.copy_from_slice(&hash);
    Id32(id)
}

/// Compute SHA-256 hash of multiple chunks without concatenating them.
#[cfg(feature = "crypto-openssl")]
pub fn sha256_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Id32 {
    let mut hasher =
        openssl::hash::Hasher::new(openssl::hash::MessageDigest::sha256()).unwrap();
    for chunk in chunks {
        hasher.update(chunk).unwrap();
    }
    let hash = hasher.finish().unwrap();
    let mut id = [0u8; 32];
    id.copy_from_slice(&hash);
    Id32(id)
}

// --- Crypto backend: aws-lc-rs ---

/// Compute SHA1 hash of input bytes.
#[cfg(all(feature = "crypto-aws-lc", not(feature = "crypto-openssl")))]
pub fn sha1(data: &[u8]) -> Id20 {
    let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, data);
    let mut id = [0u8; 20];
    id.copy_from_slice(hash.as_ref());
    Id20(id)
}

/// Compute SHA1 hash of multiple chunks without concatenating them.
///
/// Avoids allocating a large buffer when piece data is stored as separate blocks.
#[cfg(all(feature = "crypto-aws-lc", not(feature = "crypto-openssl")))]
pub fn sha1_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Id20 {
    let mut ctx = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY);
    for chunk in chunks {
        ctx.update(chunk);
    }
    let hash = ctx.finish();
    let mut id = [0u8; 20];
    id.copy_from_slice(hash.as_ref());
    Id20(id)
}

/// Compute SHA-256 hash of input bytes (used by BitTorrent v2, BEP 52).
#[cfg(all(feature = "crypto-aws-lc", not(feature = "crypto-openssl")))]
pub fn sha256(data: &[u8]) -> Id32 {
    let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data);
    let mut id = [0u8; 32];
    id.copy_from_slice(hash.as_ref());
    Id32(id)
}

/// Compute SHA-256 hash of multiple chunks without concatenating them.
#[cfg(all(feature = "crypto-aws-lc", not(feature = "crypto-openssl")))]
pub fn sha256_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Id32 {
    let mut ctx = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    for chunk in chunks {
        ctx.update(chunk);
    }
    let hash = ctx.finish();
    let mut id = [0u8; 32];
    id.copy_from_slice(hash.as_ref());
    Id32(id)
}

// --- Incremental SHA-1 hasher for streaming verification ---

/// Incremental SHA-1 hasher for streaming piece verification.
///
/// Eliminates per-piece allocation by allowing callers to feed data in
/// fixed-size chunks through a reusable buffer rather than reading the
/// entire piece into memory at once.
pub struct Sha1Hasher {
    #[cfg(all(feature = "crypto-ring", not(feature = "crypto-openssl"), not(feature = "crypto-aws-lc")))]
    ctx: ring::digest::Context,
    #[cfg(feature = "crypto-openssl")]
    ctx: openssl::hash::Hasher,
    #[cfg(all(feature = "crypto-aws-lc", not(feature = "crypto-openssl")))]
    ctx: aws_lc_rs::digest::Context,
}

impl Sha1Hasher {
    /// Create a new incremental SHA-1 hasher.
    pub fn new() -> Self {
        Sha1Hasher {
            #[cfg(all(
                feature = "crypto-ring",
                not(feature = "crypto-openssl"),
                not(feature = "crypto-aws-lc")
            ))]
            ctx: ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY),
            #[cfg(feature = "crypto-openssl")]
            ctx: openssl::hash::Hasher::new(openssl::hash::MessageDigest::sha1()).unwrap(),
            #[cfg(all(feature = "crypto-aws-lc", not(feature = "crypto-openssl")))]
            ctx: aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY),
        }
    }

    /// Feed data into the hasher.
    pub fn update(&mut self, data: &[u8]) {
        #[cfg(all(
            feature = "crypto-ring",
            not(feature = "crypto-openssl"),
            not(feature = "crypto-aws-lc")
        ))]
        self.ctx.update(data);

        #[cfg(feature = "crypto-openssl")]
        self.ctx.update(data).unwrap();

        #[cfg(all(feature = "crypto-aws-lc", not(feature = "crypto-openssl")))]
        self.ctx.update(data);
    }

    /// Finalize the hash and return the SHA-1 digest.
    #[cfg(all(
        feature = "crypto-ring",
        not(feature = "crypto-openssl"),
        not(feature = "crypto-aws-lc")
    ))]
    pub fn finish(self) -> Id20 {
        let hash = self.ctx.finish();
        let mut id = [0u8; 20];
        id.copy_from_slice(hash.as_ref());
        Id20(id)
    }

    /// Finalize the hash and return the SHA-1 digest.
    #[cfg(feature = "crypto-openssl")]
    pub fn finish(mut self) -> Id20 {
        let hash = self.ctx.finish().unwrap();
        let mut id = [0u8; 20];
        id.copy_from_slice(&hash);
        Id20(id)
    }

    /// Finalize the hash and return the SHA-1 digest.
    #[cfg(all(feature = "crypto-aws-lc", not(feature = "crypto-openssl")))]
    pub fn finish(self) -> Id20 {
        let hash = self.ctx.finish();
        let mut id = [0u8; 20];
        id.copy_from_slice(hash.as_ref());
        Id20(id)
    }
}

impl Default for Sha1Hasher {
    fn default() -> Self {
        Self::new()
    }
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

    #[test]
    fn sha1_hasher_matches_oneshot() {
        let data = b"hello world, this is a streaming hash test";
        let expected = sha1(data);

        let mut hasher = Sha1Hasher::new();
        hasher.update(&data[..12]);
        hasher.update(&data[12..]);
        assert_eq!(hasher.finish(), expected);
    }

    #[test]
    fn sha1_hasher_empty() {
        let expected = sha1(b"");
        let hasher = Sha1Hasher::new();
        assert_eq!(hasher.finish(), expected);
    }
}
