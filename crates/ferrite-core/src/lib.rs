//! Core types for BitTorrent: hashes, metainfo, magnets, and piece arithmetic.

mod error;
mod hash;
mod lengths;
mod magnet;
mod metainfo;
mod peer_id;
mod resume_data;
mod file_priority;
mod storage_mode;

pub use error::{Error, Result};
pub use file_priority::FilePriority;
pub use hash::{Id20, Id32};
pub use lengths::{Lengths, DEFAULT_CHUNK_SIZE};
pub use magnet::Magnet;
pub use metainfo::{FileEntry, FileInfo, InfoDict, TorrentMetaV1, torrent_from_bytes};
pub use peer_id::PeerId;
pub use resume_data::{FastResumeData, UnfinishedPiece};
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
    fn random_bytes_fills_buffer() {
        let mut buf = [0u8; 32];
        random_bytes(&mut buf);
        // At least some bytes should be non-zero (probability of all-zero is ~0)
        assert!(buf.iter().any(|&b| b != 0));
    }
}
