//! Piece storage, verification, and disk I/O for BitTorrent.

mod error;
pub mod bitfield;
pub mod cache;
pub mod chunk_tracker;
pub mod file_map;
pub mod filesystem;
pub mod memory;
pub mod mmap;
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
