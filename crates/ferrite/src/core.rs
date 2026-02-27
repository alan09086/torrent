//! Core BitTorrent types: hashes, metainfo, magnets, piece arithmetic.
//!
//! Re-exports from [`ferrite_core`].

pub use ferrite_core::{
    // Hash types
    Id20,
    Id32,
    // Peer identity
    PeerId,
    // Magnet links (BEP 9)
    Magnet,
    // Torrent metainfo (BEP 3)
    TorrentMetaV1,
    torrent_from_bytes,
    // Piece/chunk arithmetic
    Lengths,
    DEFAULT_CHUNK_SIZE,
    // SHA1 utility
    sha1,
    // Error types
    Error,
    Result,
};

// Re-export info dict sub-types (needed to access TorrentMetaV1 fields)
pub use ferrite_core::{FileInfo, InfoDict, FileEntry};

// Resume data (M11)
pub use ferrite_core::{FastResumeData, UnfinishedPiece};

// File priority (M12)
pub use ferrite_core::FilePriority;

// Address family (M21 IPv6)
pub use ferrite_core::AddressFamily;
