//! Pluggable disk I/O backend abstraction.
//!
//! The [`DiskIoBackend`] trait defines the interface for session-level disk I/O,
//! allowing custom storage backends (POSIX, mmap, disabled/null, etc.).
//! [`DisabledDiskIo`] is a no-op backend useful for network throughput benchmarking.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use bytes::Bytes;
use torrent_core::{Id20, Id32};
use torrent_storage::TorrentStorage;

use crate::buffer_pool::BufferPool;
use crate::disk::DiskConfig;

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
    /// Current size of the read cache in bytes (M102).
    pub read_cache_bytes: usize,
    /// Total number of entries in the buffer pool (M102).
    pub pool_entries: usize,
    /// Number of prefetch insertions into the read cache (M102).
    pub prefetch_count: u64,
    /// Number of ARC evictions from the read cache (M102).
    pub eviction_count: u64,
    /// Number of Writing-to-Skeleton demotions (M102).
    pub skeleton_count: u64,
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
        data: Bytes,
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

    /// Read an entire piece from storage, returning its raw bytes.
    ///
    /// For backends with a write buffer, the piece is flushed before reading.
    fn read_piece(&self, info_hash: Id20, piece: u32) -> crate::Result<Vec<u8>>;

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
        _data: Bytes,
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

    fn read_piece(&self, _info_hash: Id20, _piece: u32) -> crate::Result<Vec<u8>> {
        Ok(Vec::new())
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

    fn cached_pieces(&self, _info_hash: Id20) -> Vec<u32> {
        vec![]
    }

    fn stats(&self) -> DiskIoStats {
        DiskIoStats::default()
    }
}

// ---------------------------------------------------------------------------
// PosixDiskIo — unified BufferPool backend
// ---------------------------------------------------------------------------

/// POSIX disk I/O backend with unified buffer pool (combined read cache +
/// write buffer under a single byte budget).
///
/// Replaces the previous separate `ArcCache` + `WriteBuffer` design with
/// a single [`BufferPool`] that uses a byte-budget ARC for read caching and
/// tracks in-flight writes as `Writing` entries.
pub struct PosixDiskIo {
    storages: RwLock<HashMap<Id20, Arc<dyn TorrentStorage>>>,
    pool: Mutex<BufferPool>,
    stats: Mutex<DiskIoStats>,
}

impl PosixDiskIo {
    /// Create a new POSIX disk I/O backend with the given configuration.
    pub fn new(config: &DiskConfig) -> Self {
        let mut pool = BufferPool::with_capacity(config.buffer_pool_capacity);
        pool.set_mlock(config.enable_mlock);
        PosixDiskIo {
            storages: RwLock::new(HashMap::new()),
            pool: Mutex::new(pool),
            stats: Mutex::new(DiskIoStats::default()),
        }
    }

    fn get_storage(&self, info_hash: Id20) -> crate::Result<Arc<dyn TorrentStorage>> {
        self.storages
            .read()
            .unwrap()
            .get(&info_hash)
            .cloned()
            .ok_or_else(|| {
                crate::Error::Storage(torrent_storage::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "torrent not registered",
                )))
            })
    }
}

impl DiskIoBackend for PosixDiskIo {
    fn name(&self) -> &str {
        "posix"
    }

    fn register(&self, info_hash: Id20, storage: Arc<dyn TorrentStorage>) {
        self.storages.write().unwrap().insert(info_hash, storage);
    }

    fn unregister(&self, info_hash: Id20) {
        self.storages.write().unwrap().remove(&info_hash);
        self.pool.lock().unwrap().clear_torrent(info_hash);
    }

    fn write_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: Bytes,
        flush: bool,
    ) -> crate::Result<()> {
        let len = data.len();

        if flush {
            let storage = self.get_storage(info_hash)?;
            storage.write_chunk(piece, begin, &data)?;
            self.stats.lock().unwrap().write_bytes += len as u64;
            return Ok(());
        }

        // Buffer the write in the pool. Pass piece_size=0 since we don't know
        // it here — completion detection is handled by ChunkTracker in
        // torrent.rs, not by the buffer pool.
        let key = (info_hash, piece);
        self.pool.lock().unwrap().write_block(key, begin, data, 0);
        self.stats.lock().unwrap().write_bytes += len as u64;
        Ok(())
    }

    fn read_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        volatile: bool,
    ) -> crate::Result<Bytes> {
        let key = (info_hash, piece);

        // 1. Check buffer pool (Writing entries + Cached entries)
        if !volatile {
            let mut pool = self.pool.lock().unwrap();
            if let Some(data) = pool.read_block(key, begin, length as usize) {
                drop(pool);
                self.stats.lock().unwrap().cache_hits += 1;
                return Ok(data);
            }
        }
        self.stats.lock().unwrap().cache_misses += 1;

        // 2. Read full piece from storage and cache it (unless volatile)
        let storage = self.get_storage(info_hash)?;
        if !volatile {
            match storage.read_piece(piece) {
                Ok(piece_data) => {
                    let piece_bytes = Bytes::from(piece_data);
                    self.stats.lock().unwrap().read_bytes += piece_bytes.len() as u64;

                    let mut pool = self.pool.lock().unwrap();
                    pool.prefetch_piece(key, piece_bytes.clone());
                    drop(pool);

                    // Return the requested slice
                    let end = (begin as usize).saturating_add(length as usize);
                    if end <= piece_bytes.len() {
                        return Ok(piece_bytes.slice(begin as usize..end));
                    }
                    // Fall through to chunk read if slice is out of bounds
                    // (shouldn't happen in practice, but defensive)
                }
                Err(_) => {
                    // Fall through to chunk read on piece-level failure
                }
            }
        }

        // 3. Volatile path or fallback: read just the requested chunk
        let data = storage.read_chunk(piece, begin, length)?;
        let bytes = Bytes::from(data);
        self.stats.lock().unwrap().read_bytes += bytes.len() as u64;
        Ok(bytes)
    }

    fn read_piece(&self, info_hash: Id20, piece: u32) -> crate::Result<Vec<u8>> {
        self.flush_piece(info_hash, piece)?;
        let storage = self.get_storage(info_hash)?;
        let data = storage.read_piece(piece)?;
        self.stats.lock().unwrap().read_bytes += data.len() as u64;
        Ok(data)
    }

    fn hash_piece(&self, info_hash: Id20, piece: u32, expected: &Id20) -> crate::Result<bool> {
        let key = (info_hash, piece);

        // Try hash from cache: take all blocks from the Writing entry and hash
        // them in memory, avoiding a disk round-trip. This works because
        // hash_piece is only called after ChunkTracker confirms the piece is
        // complete, so all blocks should be present.
        let cached_data = {
            let mut pool = self.pool.lock().unwrap();
            pool.take_all_blocks(key)
        };

        if let Some(data) = cached_data {
            let hash = torrent_core::sha1(&data);
            if hash == *expected {
                // Hash pass: flush to disk + promote to read cache.
                let storage = self.get_storage(info_hash)?;
                storage.write_chunk(piece, 0, &data)?;
                self.stats.lock().unwrap().write_bytes += data.len() as u64;

                let mut pool = self.pool.lock().unwrap();
                pool.promote_to_cached(key, Bytes::from(data));
                return Ok(true);
            }
            // Hash fail: data already removed from pool by take_all_blocks.
            return Ok(false);
        }

        // No cached data — flush and verify from disk (legacy path).
        self.flush_piece(info_hash, piece)?;
        let storage = self.get_storage(info_hash)?;
        Ok(storage.verify_piece(piece, expected)?)
    }

    fn hash_piece_v2(&self, info_hash: Id20, piece: u32, expected: &Id32) -> crate::Result<bool> {
        let key = (info_hash, piece);

        // Try hash from cache (same pattern as hash_piece but SHA-256).
        let cached_data = {
            let mut pool = self.pool.lock().unwrap();
            pool.take_all_blocks(key)
        };

        if let Some(data) = cached_data {
            let hash = torrent_core::sha256(&data);
            if hash == *expected {
                let storage = self.get_storage(info_hash)?;
                storage.write_chunk(piece, 0, &data)?;
                self.stats.lock().unwrap().write_bytes += data.len() as u64;

                let mut pool = self.pool.lock().unwrap();
                pool.promote_to_cached(key, Bytes::from(data));
                return Ok(true);
            }
            return Ok(false);
        }

        self.flush_piece(info_hash, piece)?;
        let storage = self.get_storage(info_hash)?;
        Ok(storage.verify_piece_v2(piece, expected)?)
    }

    fn hash_block(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
    ) -> crate::Result<Id32> {
        self.flush_piece(info_hash, piece)?;
        let storage = self.get_storage(info_hash)?;
        Ok(storage.hash_block(piece, begin, length)?)
    }

    fn clear_piece(&self, info_hash: Id20, piece: u32) {
        self.pool.lock().unwrap().clear_piece((info_hash, piece));
    }

    fn flush_piece(&self, info_hash: Id20, piece: u32) -> crate::Result<()> {
        let blocks = {
            let mut pool = self.pool.lock().unwrap();
            match pool.flush_piece((info_hash, piece)) {
                Some(b) => b,
                None => return Ok(()),
            }
        };
        let storage = self.get_storage(info_hash)?;
        for (begin, data) in blocks {
            storage.write_chunk(piece, begin, &data)?;
        }
        Ok(())
    }

    fn flush_all(&self) -> crate::Result<()> {
        let all_blocks = {
            let mut pool = self.pool.lock().unwrap();
            pool.flush_all()
        };
        for ((info_hash, piece), blocks) in all_blocks {
            let storage = self.get_storage(info_hash)?;
            for (begin, data) in blocks {
                storage.write_chunk(piece, begin, &data)?;
            }
        }
        Ok(())
    }

    fn cached_pieces(&self, info_hash: Id20) -> Vec<u32> {
        let pool = self.pool.lock().unwrap();
        pool.hot_pieces(info_hash)
    }

    fn stats(&self) -> DiskIoStats {
        let mut s = self.stats.lock().unwrap().clone();
        let pool = self.pool.lock().unwrap();
        let ps = pool.stats();
        s.write_buffer_bytes = ps.write_buffer_bytes;
        s.read_cache_bytes = ps.read_cache_bytes;
        s.pool_entries = ps.total_entries;
        s.prefetch_count = ps.prefetch_count;
        s.eviction_count = ps.eviction_count;
        s.skeleton_count = ps.skeleton_count;
        s
    }
}

// ---------------------------------------------------------------------------
// MmapDiskIo — direct I/O, no user-space cache
// ---------------------------------------------------------------------------

/// Memory-mapped disk I/O backend with no user-space cache.
///
/// Relies entirely on the kernel page cache via mmap. All reads and writes
/// go directly to storage with no write buffering.
pub struct MmapDiskIo {
    storages: RwLock<HashMap<Id20, Arc<dyn TorrentStorage>>>,
    stats: Mutex<DiskIoStats>,
}

impl MmapDiskIo {
    /// Create a new mmap disk I/O backend.
    pub fn new() -> Self {
        MmapDiskIo {
            storages: RwLock::new(HashMap::new()),
            stats: Mutex::new(DiskIoStats::default()),
        }
    }

    fn get_storage(&self, info_hash: Id20) -> crate::Result<Arc<dyn TorrentStorage>> {
        self.storages
            .read()
            .unwrap()
            .get(&info_hash)
            .cloned()
            .ok_or_else(|| {
                crate::Error::Storage(torrent_storage::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "torrent not registered",
                )))
            })
    }
}

impl Default for MmapDiskIo {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskIoBackend for MmapDiskIo {
    fn name(&self) -> &str {
        "mmap"
    }

    fn register(&self, info_hash: Id20, storage: Arc<dyn TorrentStorage>) {
        self.storages.write().unwrap().insert(info_hash, storage);
    }

    fn unregister(&self, info_hash: Id20) {
        self.storages.write().unwrap().remove(&info_hash);
    }

    fn write_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: Bytes,
        _flush: bool,
    ) -> crate::Result<()> {
        let len = data.len();
        let storage = self.get_storage(info_hash)?;
        storage.write_chunk(piece, begin, &data)?;
        self.stats.lock().unwrap().write_bytes += len as u64;
        Ok(())
    }

    fn read_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        _volatile: bool,
    ) -> crate::Result<Bytes> {
        let storage = self.get_storage(info_hash)?;
        let data = storage.read_chunk(piece, begin, length)?;
        let bytes = Bytes::from(data);
        self.stats.lock().unwrap().read_bytes += bytes.len() as u64;
        Ok(bytes)
    }

    fn read_piece(&self, info_hash: Id20, piece: u32) -> crate::Result<Vec<u8>> {
        let storage = self.get_storage(info_hash)?;
        let data = storage.read_piece(piece)?;
        self.stats.lock().unwrap().read_bytes += data.len() as u64;
        Ok(data)
    }

    fn hash_piece(&self, info_hash: Id20, piece: u32, expected: &Id20) -> crate::Result<bool> {
        let storage = self.get_storage(info_hash)?;
        Ok(storage.verify_piece(piece, expected)?)
    }

    fn hash_piece_v2(&self, info_hash: Id20, piece: u32, expected: &Id32) -> crate::Result<bool> {
        let storage = self.get_storage(info_hash)?;
        Ok(storage.verify_piece_v2(piece, expected)?)
    }

    fn hash_block(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
    ) -> crate::Result<Id32> {
        let storage = self.get_storage(info_hash)?;
        Ok(storage.hash_block(piece, begin, length)?)
    }

    fn clear_piece(&self, _info_hash: Id20, _piece: u32) {
        // No-op: no write buffer or cache
    }

    fn flush_piece(&self, _info_hash: Id20, _piece: u32) -> crate::Result<()> {
        // No-op: no write buffer
        Ok(())
    }

    fn flush_all(&self) -> crate::Result<()> {
        // No-op: no write buffer
        Ok(())
    }

    fn cached_pieces(&self, _info_hash: Id20) -> Vec<u32> {
        vec![]
    }

    fn stats(&self) -> DiskIoStats {
        self.stats.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create a disk I/O backend based on the storage mode in the configuration.
///
/// Returns `MmapDiskIo` when `config.storage_mode` is `Mmap`, otherwise
/// returns `PosixDiskIo` with unified buffer pool.
pub fn create_backend_from_config(config: &DiskConfig) -> Arc<dyn DiskIoBackend> {
    if config.storage_mode == torrent_core::StorageMode::Mmap {
        Arc::new(MmapDiskIo::new())
    } else {
        Arc::new(PosixDiskIo::new(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::Lengths;
    use torrent_storage::MemoryStorage;

    fn make_hash() -> Id20 {
        Id20([0xAB; 20])
    }

    fn make_hash_n(n: u8) -> Id20 {
        let mut b = [0u8; 20];
        b[0] = n;
        Id20(b)
    }

    fn test_config() -> DiskConfig {
        DiskConfig {
            io_threads: 2,
            cache_size: 1024 * 1024, // 1 MiB
            ..DiskConfig::default()
        }
    }

    /// Create a MemoryStorage with a single piece of the given size.
    fn make_storage(piece_size: u64) -> Arc<dyn TorrentStorage> {
        let chunk = piece_size.min(16384) as u32;
        let lengths = Lengths::new(piece_size, piece_size, chunk);
        Arc::new(MemoryStorage::new(lengths))
    }

    /// Create a MemoryStorage with the given total/piece/chunk sizes.
    fn make_storage_full(total: u64, piece_len: u64, chunk_size: u32) -> Arc<dyn TorrentStorage> {
        let lengths = Lengths::new(total, piece_len, chunk_size);
        Arc::new(MemoryStorage::new(lengths))
    }

    // -----------------------------------------------------------------------
    // DisabledDiskIo tests
    // -----------------------------------------------------------------------

    #[test]
    fn disabled_backend_name() {
        let backend = DisabledDiskIo;
        assert_eq!(backend.name(), "disabled");
    }

    #[test]
    fn disabled_backend_write_succeeds() {
        let backend = DisabledDiskIo;
        let result =
            backend.write_chunk(make_hash(), 0, 0, Bytes::from_static(&[1, 2, 3, 4]), false);
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

    // -----------------------------------------------------------------------
    // PosixDiskIo tests
    // -----------------------------------------------------------------------

    #[test]
    fn posix_backend_name() {
        let backend = PosixDiskIo::new(&test_config());
        assert_eq!(backend.name(), "posix");
    }

    #[test]
    fn posix_register_unregister() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(1);
        let storage = make_storage(100);
        backend.register(ih, storage);

        // Should be able to read (empty data)
        assert!(backend.read_chunk(ih, 0, 0, 10, false).is_ok());

        backend.unregister(ih);

        // Should fail after unregister
        assert!(backend.read_chunk(ih, 0, 0, 10, false).is_err());
    }

    #[test]
    fn posix_write_and_read_flush() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(2);
        let storage = make_storage(100);
        backend.register(ih, storage);

        let data = vec![42u8; 50];
        backend
            .write_chunk(ih, 0, 0, Bytes::from(data.clone()), true)
            .unwrap();
        let read = backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        assert_eq!(&read[..], &data[..]);
    }

    #[test]
    fn posix_write_buffered_then_read() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(3);
        let storage = make_storage(100);
        backend.register(ih, storage);

        let data = vec![99u8; 50];
        // Write without flush — goes to buffer pool
        backend
            .write_chunk(ih, 0, 0, Bytes::from(data.clone()), false)
            .unwrap();

        // Read should find it in buffer pool
        let read = backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        assert_eq!(&read[..], &data[..]);

        // Should be a cache hit (from buffer pool)
        let stats = backend.stats();
        assert!(stats.cache_hits >= 1);
    }

    #[test]
    fn posix_read_cache_hit() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(4);
        let storage = make_storage(100);
        backend.register(ih, storage);

        let data = vec![7u8; 50];
        backend
            .write_chunk(ih, 0, 0, Bytes::from(data.clone()), true)
            .unwrap();

        // First read: cache miss, reads from storage and prefetches
        let r1 = backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        assert_eq!(&r1[..], &data[..]);
        let s1 = backend.stats();
        assert_eq!(s1.cache_misses, 1);

        // Second read: should be cache hit (from prefetched Cached entry)
        let r2 = backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        assert_eq!(&r2[..], &data[..]);
        let s2 = backend.stats();
        assert_eq!(s2.cache_hits, 1);
    }

    #[test]
    fn posix_hash_piece_correct() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(5);
        let storage = make_storage(50);
        backend.register(ih, storage);

        let data = vec![9u8; 50];
        backend
            .write_chunk(ih, 0, 0, Bytes::from(data.clone()), true)
            .unwrap();

        let expected = torrent_core::sha1(&data);
        assert!(backend.hash_piece(ih, 0, &expected).unwrap());
    }

    #[test]
    fn posix_hash_piece_wrong() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(6);
        let storage = make_storage(50);
        backend.register(ih, storage);

        let data = vec![9u8; 50];
        backend
            .write_chunk(ih, 0, 0, Bytes::from(data.clone()), true)
            .unwrap();

        let wrong = Id20([0xFF; 20]);
        assert!(!backend.hash_piece(ih, 0, &wrong).unwrap());
    }

    #[test]
    fn posix_hash_piece_v2() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(7);
        let storage = make_storage(16384);
        backend.register(ih, storage);

        let data = vec![0xABu8; 16384];
        backend
            .write_chunk(ih, 0, 0, Bytes::from(data.clone()), true)
            .unwrap();

        let expected = torrent_core::sha256(&data);
        assert!(backend.hash_piece_v2(ih, 0, &expected).unwrap());
    }

    #[test]
    fn posix_clear_piece_drops_buffer() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(8);
        let storage = make_storage(100);
        backend.register(ih, storage);

        let data = vec![55u8; 50];
        // Write buffered (not flushed)
        backend
            .write_chunk(ih, 0, 0, Bytes::from(data), false)
            .unwrap();
        assert!(backend.stats().write_buffer_bytes > 0);

        backend.clear_piece(ih, 0);
        assert_eq!(backend.stats().write_buffer_bytes, 0);
    }

    #[test]
    fn posix_cached_pieces() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(9);
        // Two pieces: piece 0 and piece 1
        let storage = make_storage_full(100, 50, 25);
        backend.register(ih, storage);

        let data = vec![1u8; 25];
        backend
            .write_chunk(ih, 0, 0, Bytes::from(data.clone()), true)
            .unwrap();
        backend
            .write_chunk(ih, 1, 0, Bytes::from(data), true)
            .unwrap();

        // Read piece 0 twice to promote to T2 (hot_pieces requires ≥2 accesses)
        backend.read_chunk(ih, 0, 0, 25, false).unwrap();
        backend.read_chunk(ih, 0, 0, 25, false).unwrap();

        let cached = backend.cached_pieces(ih);
        assert!(cached.contains(&0));
        // piece 1 was not read, should not be cached
        assert!(!cached.contains(&1));
    }

    // -----------------------------------------------------------------------
    // MmapDiskIo tests
    // -----------------------------------------------------------------------

    #[test]
    fn mmap_backend_name() {
        let backend = MmapDiskIo::new();
        assert_eq!(backend.name(), "mmap");
    }

    #[test]
    fn mmap_write_and_read() {
        let backend = MmapDiskIo::new();
        let ih = make_hash_n(10);
        let storage = make_storage(100);
        backend.register(ih, storage);

        let data = vec![42u8; 50];
        backend
            .write_chunk(ih, 0, 0, Bytes::from(data.clone()), false)
            .unwrap();
        let read = backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        assert_eq!(&read[..], &data[..]);
    }

    #[test]
    fn mmap_cached_pieces_always_empty() {
        let backend = MmapDiskIo::new();
        let ih = make_hash_n(11);
        let storage = make_storage(100);
        backend.register(ih, storage);

        backend
            .write_chunk(ih, 0, 0, Bytes::from(vec![1u8; 50]), false)
            .unwrap();
        backend.read_chunk(ih, 0, 0, 50, false).unwrap();

        assert!(backend.cached_pieces(ih).is_empty());
    }

    #[test]
    fn mmap_stats_track_io() {
        let backend = MmapDiskIo::new();
        let ih = make_hash_n(12);
        let storage = make_storage(100);
        backend.register(ih, storage);

        backend
            .write_chunk(ih, 0, 0, Bytes::from(vec![1u8; 50]), false)
            .unwrap();
        let stats = backend.stats();
        assert_eq!(stats.write_bytes, 50);

        backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        let stats = backend.stats();
        assert_eq!(stats.read_bytes, 50);
    }

    // -----------------------------------------------------------------------
    // read_piece tests
    // -----------------------------------------------------------------------

    #[test]
    fn read_piece_returns_full_piece() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(20);
        let chunk_size = 16384u32;
        let piece_size = u64::from(chunk_size) * 2; // 32768 — two chunks
        let storage = make_storage_full(piece_size, piece_size, chunk_size);
        backend.register(ih, storage);

        // Write two chunks via the backend (buffered, not flushed)
        let chunk0 = vec![0xAAu8; chunk_size as usize];
        let chunk1 = vec![0xBBu8; chunk_size as usize];
        backend
            .write_chunk(ih, 0, 0, Bytes::from(chunk0.clone()), false)
            .expect("write chunk 0");
        backend
            .write_chunk(ih, 0, chunk_size, Bytes::from(chunk1.clone()), false)
            .expect("write chunk 1");

        // read_piece should flush the buffer pool and return the full piece
        let piece_data = backend.read_piece(ih, 0).expect("read_piece");
        assert_eq!(piece_data.len(), piece_size as usize);
        assert_eq!(&piece_data[..chunk_size as usize], &chunk0[..]);
        assert_eq!(&piece_data[chunk_size as usize..], &chunk1[..]);

        // Stats should reflect the read
        let stats = backend.stats();
        assert!(stats.read_bytes >= piece_size);
    }

    // -----------------------------------------------------------------------
    // Hash-from-cache tests
    // -----------------------------------------------------------------------

    #[test]
    fn posix_hash_from_cache_pass() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(30);
        let storage = make_storage(100);
        backend.register(ih, storage);

        // Write two blocks into the buffer pool (not flushed)
        let d1 = vec![0xAA_u8; 50];
        let d2 = vec![0xBB_u8; 50];
        backend
            .write_chunk(ih, 0, 0, Bytes::from(d1.clone()), false)
            .unwrap();
        backend
            .write_chunk(ih, 0, 50, Bytes::from(d2.clone()), false)
            .unwrap();

        // Compute expected hash of the full piece
        let mut full = d1;
        full.extend_from_slice(&d2);
        let expected = torrent_core::sha1(&full);

        // hash_piece should hash from cache, write to disk, and promote to Cached
        assert!(backend.hash_piece(ih, 0, &expected).unwrap());

        // The piece should now be readable from disk via read_piece
        let piece_data = backend.read_piece(ih, 0).unwrap();
        assert_eq!(piece_data.len(), 100);

        // Read again to promote from T1 to T2 (hot_pieces requires ≥2 accesses)
        backend.read_chunk(ih, 0, 0, 50, false).unwrap();

        // The piece should now be in the hot pieces list (T2)
        let cached = backend.cached_pieces(ih);
        assert!(cached.contains(&0));
    }

    #[test]
    fn posix_hash_from_cache_fail() {
        let backend = PosixDiskIo::new(&test_config());
        let ih = make_hash_n(31);
        let storage = make_storage(50);
        backend.register(ih, storage);

        // Write one block into buffer pool
        backend
            .write_chunk(ih, 0, 0, Bytes::from(vec![0xCC_u8; 50]), false)
            .unwrap();

        // Wrong hash — should fail, data discarded from pool
        let wrong = Id20([0xFF; 20]);
        assert!(!backend.hash_piece(ih, 0, &wrong).unwrap());

        // Buffer pool should no longer hold the data
        assert_eq!(backend.stats().write_buffer_bytes, 0);
    }

    // -----------------------------------------------------------------------
    // Factory tests
    // -----------------------------------------------------------------------

    #[test]
    fn factory_creates_mmap_for_mmap_mode() {
        let config = DiskConfig {
            storage_mode: torrent_core::StorageMode::Mmap,
            ..DiskConfig::default()
        };
        let backend = create_backend_from_config(&config);
        assert_eq!(backend.name(), "mmap");
    }

    #[test]
    fn factory_creates_posix_for_auto_mode() {
        let config = DiskConfig {
            storage_mode: torrent_core::StorageMode::Auto,
            ..DiskConfig::default()
        };
        let backend = create_backend_from_config(&config);
        assert_eq!(backend.name(), "posix");
    }
}
