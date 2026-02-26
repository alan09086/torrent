//! Core types for BitTorrent: hashes, metainfo, magnets, and piece arithmetic.

mod error;
mod hash;
mod lengths;
mod magnet;
mod metainfo;
mod peer_id;
mod resume_data;
mod file_priority;

pub use error::{Error, Result};
pub use file_priority::FilePriority;
pub use hash::{Id20, Id32};
pub use lengths::{Lengths, DEFAULT_CHUNK_SIZE};
pub use magnet::Magnet;
pub use metainfo::{FileEntry, FileInfo, InfoDict, TorrentMetaV1, torrent_from_bytes};
pub use peer_id::PeerId;
pub use resume_data::{FastResumeData, UnfinishedPiece};

/// Compute SHA1 hash of input bytes.
pub fn sha1(data: &[u8]) -> Id20 {
    use sha1::Digest;
    let hash = sha1::Sha1::digest(data);
    Id20(hash.into())
}
