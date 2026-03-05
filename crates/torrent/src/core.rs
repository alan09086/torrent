//! Core BitTorrent types: hashes, metainfo, magnets, piece arithmetic.
//!
//! Re-exports from [`torrent_core`].

pub use torrent_core::{
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
pub use torrent_core::{FileInfo, InfoDict, FileEntry};

// Resume data (M11)
pub use torrent_core::{FastResumeData, UnfinishedPiece};

// File priority (M12)
pub use torrent_core::FilePriority;

// BEP 53 file selection (M36)
pub use torrent_core::FileSelection;

// Address family (M21 IPv6)
pub use torrent_core::AddressFamily;

// Storage allocation mode (M26)
pub use torrent_core::StorageMode;

// Torrent creation (M30)
pub use torrent_core::{CreateTorrent, CreateTorrentResult};

// BitTorrent v2 (M33, BEP 52)
pub use torrent_core::{
    InfoHashes,
    InfoDictV2, TorrentMetaV2, torrent_v2_from_bytes,
    MerkleTree,
    FileTreeNode, V2FileAttr, V2FileInfo,
    TorrentMeta, torrent_from_bytes_any,
    sha256,
    // Hybrid v1+v2 (M35)
    TorrentVersion,
};

// BEP 52 Wire + Hash Picker (M34a)
pub use torrent_core::{
    HashRequest, validate_hash_request,
    MerkleTreeState, SetBlockResult,
    HashPicker, FileHashInfo, AddHashesResult,
};
