//! Piece storage, verification, and disk I/O for BitTorrent.
//!
//! Re-exports from [`ferrite_storage`].

pub use ferrite_storage::{
    // Storage trait
    TorrentStorage,
    // Bit-vector for piece completion
    Bitfield,
    // Per-piece chunk tracking
    ChunkTracker,
    // Piece-to-file mapping
    FileMap,
    FileSegment,
    // Storage backends
    MemoryStorage,
    FilesystemStorage,
    MmapStorage,
    // ARC disk cache
    ArcCache,
    // Error types
    Error,
    Result,
};
