#![warn(missing_docs)]
//! Torrent storage backends, piece verification, and chunk tracking.

/// Compact bit array for tracking piece availability.
pub mod bitfield;
/// Adaptive Replacement Cache (ARC) for disk read caching.
pub mod cache;
/// Chunk-level download progress tracking.
pub mod chunk_tracker;
mod error;
/// Piece-to-file segment mapping.
pub mod file_map;
/// Disk-backed torrent storage using regular file I/O.
pub mod filesystem;
/// In-memory torrent storage for testing.
pub mod memory;
/// Memory-mapped torrent storage.
pub mod mmap;
/// Backend trait for reading and writing torrent piece data.
pub mod storage;

pub use bitfield::Bitfield;
pub use cache::ArcCache;
pub use chunk_tracker::ChunkTracker;
pub use error::{Error, Result};
pub use file_map::{FileMap, FileSegment};
pub use filesystem::FilesystemStorage;
pub use memory::MemoryStorage;
pub use mmap::MmapStorage;
pub use storage::TorrentStorage;
