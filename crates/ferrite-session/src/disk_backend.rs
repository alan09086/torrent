//! Pluggable disk I/O backend abstraction.
//!
//! The [`DiskIoBackend`] trait defines the interface for session-level disk I/O,
//! allowing custom storage backends (POSIX, mmap, disabled/null, etc.).
//! [`DisabledDiskIo`] is a no-op backend useful for network throughput benchmarking.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use ferrite_core::{Id20, Id32};
use ferrite_storage::TorrentStorage;

/// Aggregate I/O statistics for a disk backend.
#[derive(Debug, Clone, Default)]
pub struct DiskIoStats {
    /// Total bytes read from storage.
    pub read_bytes: u64,
    /// Total bytes written to storage.
    pub write_bytes: u64,
    /// Number of read requests satisfied from cache.
    pub cache_hits: u64,
    /// Number of read requests that required disk access.
    pub cache_misses: u64,
    /// Current size of the write buffer in bytes.
    pub write_buffer_bytes: usize,
}

/// Trait for pluggable disk I/O backends.
///
/// Implementations are shared across the session via `Arc<dyn DiskIoBackend>`.
/// All methods must be safe to call from multiple threads concurrently.
pub trait DiskIoBackend: Send + Sync {
    /// Human-readable backend name (e.g., "posix", "mmap", "disabled").
    fn name(&self) -> &str;

    /// Register a torrent's storage for I/O operations.
    fn register(&self, info_hash: Id20, storage: Arc<dyn TorrentStorage>);

    /// Unregister a torrent's storage.
    fn unregister(&self, info_hash: Id20);

    /// Write a chunk of piece data. If `flush` is true, persist immediately.
    fn write_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: &[u8],
        flush: bool,
    ) -> crate::Result<()>;

    /// Read a chunk of piece data. `volatile` hints the data won't be re-read soon.
    fn read_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        volatile: bool,
    ) -> crate::Result<Bytes>;

    /// Verify a piece against its expected SHA-1 hash.
    fn hash_piece(&self, info_hash: Id20, piece: u32, expected: &Id20) -> crate::Result<bool>;

    /// Verify a piece against its expected SHA-256 hash (BEP 52).
    fn hash_piece_v2(&self, info_hash: Id20, piece: u32, expected: &Id32) -> crate::Result<bool>;

    /// Compute SHA-256 hash of a single block within a piece (BEP 52 Merkle).
    fn hash_block(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
    ) -> crate::Result<Id32>;

    /// Discard buffered data for a piece (e.g., after hash failure).
    fn clear_piece(&self, info_hash: Id20, piece: u32);

    /// Flush buffered writes for a piece to persistent storage.
    fn flush_piece(&self, info_hash: Id20, piece: u32) -> crate::Result<()>;

    /// Flush all buffered writes across all torrents.
    fn flush_all(&self) -> crate::Result<()>;

    /// Move a torrent's data files to a new directory.
    fn move_storage(&self, info_hash: Id20, new_path: &Path) -> crate::Result<()>;

    /// Return piece indices currently held in cache (for SuggestPiece, M44).
    fn cached_pieces(&self, info_hash: Id20) -> Vec<u32>;

    /// Return current I/O statistics.
    fn stats(&self) -> DiskIoStats;
}

/// No-op disk I/O backend for network throughput benchmarking.
///
/// All writes succeed silently, reads return zeroed bytes, and hash
/// verifications always pass.
pub struct DisabledDiskIo;

impl DiskIoBackend for DisabledDiskIo {
    fn name(&self) -> &str {
        "disabled"
    }

    fn register(&self, _info_hash: Id20, _storage: Arc<dyn TorrentStorage>) {}

    fn unregister(&self, _info_hash: Id20) {}

    fn write_chunk(
        &self,
        _info_hash: Id20,
        _piece: u32,
        _begin: u32,
        _data: &[u8],
        _flush: bool,
    ) -> crate::Result<()> {
        Ok(())
    }

    fn read_chunk(
        &self,
        _info_hash: Id20,
        _piece: u32,
        _begin: u32,
        length: u32,
        _volatile: bool,
    ) -> crate::Result<Bytes> {
        Ok(Bytes::from(vec![0u8; length as usize]))
    }

    fn hash_piece(&self, _info_hash: Id20, _piece: u32, _expected: &Id20) -> crate::Result<bool> {
        Ok(true)
    }

    fn hash_piece_v2(
        &self,
        _info_hash: Id20,
        _piece: u32,
        _expected: &Id32,
    ) -> crate::Result<bool> {
        Ok(true)
    }

    fn hash_block(
        &self,
        _info_hash: Id20,
        _piece: u32,
        _begin: u32,
        _length: u32,
    ) -> crate::Result<Id32> {
        Ok(Id32([0u8; 32]))
    }

    fn clear_piece(&self, _info_hash: Id20, _piece: u32) {}

    fn flush_piece(&self, _info_hash: Id20, _piece: u32) -> crate::Result<()> {
        Ok(())
    }

    fn flush_all(&self) -> crate::Result<()> {
        Ok(())
    }

    fn move_storage(&self, _info_hash: Id20, _new_path: &Path) -> crate::Result<()> {
        Ok(())
    }

    fn cached_pieces(&self, _info_hash: Id20) -> Vec<u32> {
        vec![]
    }

    fn stats(&self) -> DiskIoStats {
        DiskIoStats::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hash() -> Id20 {
        Id20([0xAB; 20])
    }

    #[test]
    fn disabled_backend_name() {
        let backend = DisabledDiskIo;
        assert_eq!(backend.name(), "disabled");
    }

    #[test]
    fn disabled_backend_write_succeeds() {
        let backend = DisabledDiskIo;
        let result = backend.write_chunk(make_hash(), 0, 0, &[1, 2, 3, 4], false);
        assert!(result.is_ok());
    }

    #[test]
    fn disabled_backend_read_returns_zeroed() {
        let backend = DisabledDiskIo;
        let length = 16384u32;
        let data = backend
            .read_chunk(make_hash(), 0, 0, length, false)
            .unwrap();
        assert_eq!(data.len(), length as usize);
        assert!(data.iter().all(|&b| b == 0));
    }

    #[test]
    fn disabled_backend_hash_always_passes() {
        let backend = DisabledDiskIo;
        let expected = Id20([0xFF; 20]);
        let result = backend.hash_piece(make_hash(), 42, &expected).unwrap();
        assert!(result);
    }

    #[test]
    fn disabled_backend_hash_v2_always_passes() {
        let backend = DisabledDiskIo;
        let expected = Id32([0xFF; 32]);
        let result = backend.hash_piece_v2(make_hash(), 42, &expected).unwrap();
        assert!(result);
    }

    #[test]
    fn disabled_backend_stats_default() {
        let backend = DisabledDiskIo;
        let stats = backend.stats();
        assert_eq!(stats.read_bytes, 0);
        assert_eq!(stats.write_bytes, 0);
        assert_eq!(stats.cache_hits, 0);
        assert_eq!(stats.cache_misses, 0);
        assert_eq!(stats.write_buffer_bytes, 0);
    }

    #[test]
    fn disabled_backend_cached_pieces_empty() {
        let backend = DisabledDiskIo;
        let pieces = backend.cached_pieces(make_hash());
        assert!(pieces.is_empty());
    }
}
