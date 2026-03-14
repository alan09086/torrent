//! Pluggable disk I/O backend abstraction.
//!
//! The [`DiskIoBackend`] trait defines the interface for session-level disk I/O,
//! allowing custom storage backends (POSIX, mmap, disabled/null, etc.).
//! [`DisabledDiskIo`] is a no-op backend useful for network throughput benchmarking.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use bytes::Bytes;
use torrent_core::{Id20, Id32};
use torrent_storage::{ArcCache, TorrentStorage};

use crate::disk::DiskConfig;
use crate::write_buffer::WriteBuffer;

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

    fn cached_pieces(&self, _info_hash: Id20) -> Vec<u32> {
        vec![]
    }

    fn stats(&self) -> DiskIoStats {
        DiskIoStats::default()
    }
}

// ---------------------------------------------------------------------------
// PosixDiskIo — ARC cache + WriteBuffer backend
// ---------------------------------------------------------------------------

/// POSIX disk I/O backend with user-space ARC read cache and write buffer.
///
/// Extracts the caching/buffering logic from `DiskActor` into a standalone
/// `DiskIoBackend` implementation. All fields use interior mutability since
/// the trait methods take `&self`.
pub struct PosixDiskIo {
    storages: RwLock<HashMap<Id20, Arc<dyn TorrentStorage>>>,
    cache: Mutex<ArcCache<(Id20, u32, u32), Bytes>>,
    write_buffer: Mutex<WriteBuffer>,
    stats: Mutex<DiskIoStats>,
}

impl PosixDiskIo {
    /// Create a new POSIX disk I/O backend with the given configuration.
    pub fn new(config: &DiskConfig) -> Self {
        let write_max = (config.cache_size as f32 * config.write_cache_ratio) as usize;
        let cache_blocks = if config.cache_size > write_max {
            (config.cache_size - write_max) / 16384
        } else {
            64
        };
        PosixDiskIo {
            storages: RwLock::new(HashMap::new()),
            cache: Mutex::new(ArcCache::new(cache_blocks.max(1))),
            write_buffer: Mutex::new(WriteBuffer::new(write_max)),
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
        self.write_buffer.lock().unwrap().clear_torrent(info_hash);
        self.cache
            .lock()
            .unwrap()
            .remove_where(|k| k.0 == info_hash);
    }

    fn write_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: &[u8],
        flush: bool,
    ) -> crate::Result<()> {
        let len = data.len();

        if flush {
            let storage = self.get_storage(info_hash)?;
            storage.write_chunk(piece, begin, data)?;
            self.stats.lock().unwrap().write_bytes += len as u64;
            return Ok(());
        }

        // Buffer the write
        let needs_flush;
        let oldest;
        {
            let mut wb = self.write_buffer.lock().unwrap();
            wb.write(info_hash, piece, begin, Bytes::from(data.to_vec()));
            needs_flush = wb.needs_flush();
            oldest = if needs_flush { wb.oldest_piece() } else { None };
        }
        self.stats.lock().unwrap().write_bytes += len as u64;

        if let Some((ih, p)) = oldest
            && let Err(e) = self.flush_piece(ih, p)
        {
            tracing::warn!(%e, "background flush failed");
        }

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
        // 1. Check write buffer
        let wb_hit = {
            let wb = self.write_buffer.lock().unwrap();
            wb.read(info_hash, piece, begin, length)
        };
        if let Some(data) = wb_hit {
            self.stats.lock().unwrap().cache_hits += 1;
            return Ok(data);
        }

        // 2. Check ARC cache (unless volatile)
        if !volatile {
            let cache_key = (info_hash, piece, begin);
            let mut cache = self.cache.lock().unwrap();
            if let Some(data) = cache.get(&cache_key) {
                let cloned = data.clone();
                drop(cache);
                self.stats.lock().unwrap().cache_hits += 1;
                return Ok(cloned);
            }
        }
        self.stats.lock().unwrap().cache_misses += 1;

        // 3. Read from storage
        let storage = self.get_storage(info_hash)?;
        let data = storage.read_chunk(piece, begin, length)?;
        let bytes = Bytes::from(data);
        self.stats.lock().unwrap().read_bytes += bytes.len() as u64;

        // 4. Cache the result (unless volatile)
        if !volatile {
            let cache_key = (info_hash, piece, begin);
            self.cache.lock().unwrap().insert(cache_key, bytes.clone());
        }

        Ok(bytes)
    }

    fn hash_piece(&self, info_hash: Id20, piece: u32, expected: &Id20) -> crate::Result<bool> {
        self.flush_piece(info_hash, piece)?;
        let storage = self.get_storage(info_hash)?;
        Ok(storage.verify_piece(piece, expected)?)
    }

    fn hash_piece_v2(&self, info_hash: Id20, piece: u32, expected: &Id32) -> crate::Result<bool> {
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
        self.write_buffer
            .lock()
            .unwrap()
            .clear_piece(info_hash, piece);
        self.cache
            .lock()
            .unwrap()
            .remove_where(|k| k.0 == info_hash && k.1 == piece);
    }

    fn flush_piece(&self, info_hash: Id20, piece: u32) -> crate::Result<()> {
        let blocks = {
            let mut wb = self.write_buffer.lock().unwrap();
            match wb.take_piece(info_hash, piece) {
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
        let pending: Vec<(Id20, u32)> = {
            let wb = self.write_buffer.lock().unwrap();
            wb.pending_keys().collect()
        };
        for (ih, piece) in pending {
            self.flush_piece(ih, piece)?;
        }
        Ok(())
    }

    fn cached_pieces(&self, info_hash: Id20) -> Vec<u32> {
        let cache = self.cache.lock().unwrap();
        cache
            .cached_keys()
            .filter(|(ih, _, _)| *ih == info_hash)
            .map(|(_, piece, _)| *piece)
            .collect::<HashSet<u32>>()
            .into_iter()
            .collect()
    }

    fn stats(&self) -> DiskIoStats {
        let mut s = self.stats.lock().unwrap().clone();
        s.write_buffer_bytes = self.write_buffer.lock().unwrap().total_bytes();
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
        data: &[u8],
        _flush: bool,
    ) -> crate::Result<()> {
        let len = data.len();
        let storage = self.get_storage(info_hash)?;
        storage.write_chunk(piece, begin, data)?;
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
/// returns `PosixDiskIo` with ARC cache and write buffer.
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
        backend.write_chunk(ih, 0, 0, &data, true).unwrap();
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
        // Write without flush — goes to write buffer
        backend.write_chunk(ih, 0, 0, &data, false).unwrap();

        // Read should find it in write buffer
        let read = backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        assert_eq!(&read[..], &data[..]);

        // Should be a cache hit (from write buffer)
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
        backend.write_chunk(ih, 0, 0, &data, true).unwrap();

        // First read: cache miss, reads from storage
        let r1 = backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        assert_eq!(&r1[..], &data[..]);
        let s1 = backend.stats();
        assert_eq!(s1.cache_misses, 1);

        // Second read: should be cache hit
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
        backend.write_chunk(ih, 0, 0, &data, true).unwrap();

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
        backend.write_chunk(ih, 0, 0, &data, true).unwrap();

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
        backend.write_chunk(ih, 0, 0, &data, true).unwrap();

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
        backend.write_chunk(ih, 0, 0, &data, false).unwrap();
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
        backend.write_chunk(ih, 0, 0, &data, true).unwrap();
        backend.write_chunk(ih, 1, 0, &data, true).unwrap();

        // Read piece 0 to populate cache (non-volatile)
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
        backend.write_chunk(ih, 0, 0, &data, false).unwrap();
        let read = backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        assert_eq!(&read[..], &data[..]);
    }

    #[test]
    fn mmap_cached_pieces_always_empty() {
        let backend = MmapDiskIo::new();
        let ih = make_hash_n(11);
        let storage = make_storage(100);
        backend.register(ih, storage);

        backend.write_chunk(ih, 0, 0, &[1u8; 50], false).unwrap();
        backend.read_chunk(ih, 0, 0, 50, false).unwrap();

        assert!(backend.cached_pieces(ih).is_empty());
    }

    #[test]
    fn mmap_stats_track_io() {
        let backend = MmapDiskIo::new();
        let ih = make_hash_n(12);
        let storage = make_storage(100);
        backend.register(ih, storage);

        backend.write_chunk(ih, 0, 0, &[1u8; 50], false).unwrap();
        let stats = backend.stats();
        assert_eq!(stats.write_bytes, 50);

        backend.read_chunk(ih, 0, 0, 50, false).unwrap();
        let stats = backend.stats();
        assert_eq!(stats.read_bytes, 50);
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
