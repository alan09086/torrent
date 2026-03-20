//! Piece storage, verification, and disk I/O for BitTorrent.
//!
//! Re-exports from [`torrent_storage`].

pub use torrent_storage::{
    // ARC disk cache
    ArcCache,
    // Bit-vector for piece completion
    Bitfield,
    // Per-piece chunk tracking
    ChunkTracker,
    // Error types
    Error,
    // Piece-to-file mapping
    FileMap,
    FileSegment,
    FilesystemStorage,
    // Pre-allocation mode
    PreallocateMode,
    // Storage backends
    MemoryStorage,
    MmapStorage,
    Result,
    // Storage trait
    TorrentStorage,
};
