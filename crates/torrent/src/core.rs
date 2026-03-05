//! Core BitTorrent types: hashes, metainfo, magnets, piece arithmetic.
//!
//! Re-exports from [`torrent_core`].

pub use torrent_core::{
    DEFAULT_CHUNK_SIZE,
    // Error types
    Error,
    // Hash types
    Id20,
    Id32,
    // Piece/chunk arithmetic
    Lengths,
    // Magnet links (BEP 9)
    Magnet,
    // Peer identity
    PeerId,
    Result,
    // Torrent metainfo (BEP 3)
    TorrentMetaV1,
    // SHA1 utility
    sha1,
    torrent_from_bytes,
};

// Re-export info dict sub-types (needed to access TorrentMetaV1 fields)
pub use torrent_core::{FileEntry, FileInfo, InfoDict};

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
    FileTreeNode,
    InfoDictV2,
    InfoHashes,
    MerkleTree,
    TorrentMeta,
    TorrentMetaV2,
    // Hybrid v1+v2 (M35)
    TorrentVersion,
    V2FileAttr,
    V2FileInfo,
    sha256,
    torrent_from_bytes_any,
    torrent_v2_from_bytes,
};

// BEP 52 Wire + Hash Picker (M34a)
pub use torrent_core::{
    AddHashesResult, FileHashInfo, HashPicker, HashRequest, MerkleTreeState, SetBlockResult,
    validate_hash_request,
};
