# M49: Pluggable Disk I/O Interface -- Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Introduce a `DiskIoBackend` trait that abstracts session-level disk I/O, allowing custom storage backends to be plugged in at session creation time. Refactor `DiskActor` to delegate all I/O through this trait instead of calling `TorrentStorage` directly.

**Architecture:** Today the `DiskActor` (in `ferrite-session/src/disk.rs`) holds a `HashMap<Id20, Arc<dyn TorrentStorage>>` and calls methods on `TorrentStorage` directly inside `spawn_blocking`. The refactoring introduces a `DiskIoBackend` trait at the `ferrite-session` level that wraps per-torrent storage creation, write buffering, caching, and hashing into a single pluggable unit. The three built-in backends (`PosixDiskIo`, `MmapDiskIo`, `DisabledDiskIo`) each implement this trait. `DiskActor` is refactored from an all-in-one implementation to a thin dispatcher that delegates to whatever `DiskIoBackend` the user provided. The existing `StorageMode` enum selects between `PosixDiskIo` and `MmapDiskIo`; `DisabledDiskIo` is only available via explicit `ClientBuilder::disk_io_backend()`.

**Tech Stack:** Rust edition 2024, tokio, bytes, ferrite-storage, ferrite-session

**Breaking changes:** None. The default code path is identical -- `StorageMode::Auto` continues to select between Mmap and Posix exactly as before. `DiskIoBackend` is additive.

---

## Task 1: Define the `DiskIoBackend` Trait

**Files:**
- Create: `crates/ferrite-session/src/disk_backend.rs`
- Modify: `crates/ferrite-session/src/lib.rs`

**Step 1: Write tests for the trait and `DisabledDiskIo`**

Create `crates/ferrite-session/src/disk_backend.rs` with the trait definition, `DisabledDiskIo`, and tests:

```rust
//! Pluggable disk I/O backend interface.
//!
//! The `DiskIoBackend` trait allows custom storage backends to be injected
//! at session creation time. The session's `DiskActor` delegates all I/O
//! operations through this trait.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use ferrite_core::{Id20, Id32};
use ferrite_storage::TorrentStorage;

/// Session-level abstraction for disk I/O operations.
///
/// Implementations handle write buffering, read caching, hashing, and
/// storage lifecycle for all torrents in a session. The `DiskActor` owns
/// one `Arc<dyn DiskIoBackend>` and delegates every job through it.
///
/// All methods receive an `info_hash` to identify the torrent. Backends
/// must handle concurrent calls from the `DiskActor`'s semaphore-limited
/// `spawn_blocking` pool.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync`. Internal state should use interior
/// mutability (e.g., `Mutex`, `RwLock`, atomics).
pub trait DiskIoBackend: Send + Sync {
    /// Human-readable name for this backend (e.g., "posix", "mmap", "disabled").
    fn name(&self) -> &str;

    /// Register a torrent's storage with this backend.
    fn register(&self, info_hash: Id20, storage: Arc<dyn TorrentStorage>);

    /// Unregister a torrent, releasing its storage and any cached state.
    fn unregister(&self, info_hash: Id20);

    /// Write a chunk of piece data. May buffer the write.
    ///
    /// If `flush` is true, the write bypasses any buffer and goes directly
    /// to the underlying storage.
    fn write_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: &[u8],
        flush: bool,
    ) -> ferrite_storage::Result<()>;

    /// Read a chunk of piece data. May serve from cache or write buffer.
    fn read_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        volatile: bool,
    ) -> ferrite_storage::Result<Bytes>;

    /// Verify a piece's SHA-1 hash (v1).
    ///
    /// The backend should flush any buffered writes for this piece first.
    fn hash_piece(
        &self,
        info_hash: Id20,
        piece: u32,
        expected: &Id20,
    ) -> ferrite_storage::Result<bool>;

    /// Verify a piece's SHA-256 hash (v2).
    ///
    /// The backend should flush any buffered writes for this piece first.
    fn hash_piece_v2(
        &self,
        info_hash: Id20,
        piece: u32,
        expected: &Id32,
    ) -> ferrite_storage::Result<bool>;

    /// Hash a single 16 KiB block with SHA-256 for Merkle verification.
    fn hash_block(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
    ) -> ferrite_storage::Result<Id32>;

    /// Clear a piece from any write buffer and cache (e.g., on hash failure).
    fn clear_piece(&self, info_hash: Id20, piece: u32);

    /// Flush buffered writes for a specific piece to the underlying storage.
    fn flush_piece(&self, info_hash: Id20, piece: u32) -> ferrite_storage::Result<()>;

    /// Flush all buffered writes across all torrents.
    fn flush_all(&self) -> ferrite_storage::Result<()>;

    /// Move a torrent's storage to a new directory.
    fn move_storage(
        &self,
        info_hash: Id20,
        new_path: &Path,
    ) -> ferrite_storage::Result<()>;

    /// Return performance counters for this backend.
    fn stats(&self) -> DiskIoStats;
}

/// Performance counters returned by `DiskIoBackend::stats()`.
#[derive(Debug, Clone, Default)]
pub struct DiskIoStats {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub write_buffer_bytes: usize,
}

/// Null disk I/O backend for benchmarking network throughput.
///
/// Accepts all writes (discards data), returns zeroes for reads,
/// and always reports piece hashes as valid. Useful for measuring
/// pure protocol overhead without filesystem involvement.
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
    ) -> ferrite_storage::Result<()> {
        Ok(())
    }

    fn read_chunk(
        &self,
        _info_hash: Id20,
        _piece: u32,
        _begin: u32,
        length: u32,
        _volatile: bool,
    ) -> ferrite_storage::Result<Bytes> {
        Ok(Bytes::from(vec![0u8; length as usize]))
    }

    fn hash_piece(
        &self,
        _info_hash: Id20,
        _piece: u32,
        _expected: &Id20,
    ) -> ferrite_storage::Result<bool> {
        Ok(true)
    }

    fn hash_piece_v2(
        &self,
        _info_hash: Id20,
        _piece: u32,
        _expected: &Id32,
    ) -> ferrite_storage::Result<bool> {
        Ok(true)
    }

    fn hash_block(
        &self,
        _info_hash: Id20,
        _piece: u32,
        _begin: u32,
        _length: u32,
    ) -> ferrite_storage::Result<Id32> {
        Ok(Id32::ZERO)
    }

    fn clear_piece(&self, _info_hash: Id20, _piece: u32) {}

    fn flush_piece(&self, _info_hash: Id20, _piece: u32) -> ferrite_storage::Result<()> {
        Ok(())
    }

    fn flush_all(&self) -> ferrite_storage::Result<()> {
        Ok(())
    }

    fn move_storage(
        &self,
        _info_hash: Id20,
        _new_path: &Path,
    ) -> ferrite_storage::Result<()> {
        Ok(())
    }

    fn stats(&self) -> DiskIoStats {
        DiskIoStats::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hash(n: u8) -> Id20 {
        let mut b = [0u8; 20];
        b[0] = n;
        Id20(b)
    }

    #[test]
    fn disabled_backend_name() {
        let backend = DisabledDiskIo;
        assert_eq!(backend.name(), "disabled");
    }

    #[test]
    fn disabled_write_succeeds() {
        let backend = DisabledDiskIo;
        let result = backend.write_chunk(make_hash(1), 0, 0, &[42u8; 16384], false);
        assert!(result.is_ok());
    }

    #[test]
    fn disabled_read_returns_zeroes() {
        let backend = DisabledDiskIo;
        let data = backend
            .read_chunk(make_hash(1), 0, 0, 16384, false)
            .unwrap();
        assert_eq!(data.len(), 16384);
        assert!(data.iter().all(|&b| b == 0));
    }

    #[test]
    fn disabled_hash_always_valid() {
        let backend = DisabledDiskIo;
        assert!(backend.hash_piece(make_hash(1), 0, &Id20::ZERO).unwrap());
        assert!(backend
            .hash_piece_v2(make_hash(1), 0, &Id32::ZERO)
            .unwrap());
    }

    #[test]
    fn disabled_hash_block_returns_zero() {
        let backend = DisabledDiskIo;
        let hash = backend.hash_block(make_hash(1), 0, 0, 16384).unwrap();
        assert_eq!(hash, Id32::ZERO);
    }

    #[test]
    fn disabled_move_storage_succeeds() {
        let backend = DisabledDiskIo;
        let result =
            backend.move_storage(make_hash(1), std::path::Path::new("/tmp/new"));
        assert!(result.is_ok());
    }

    #[test]
    fn disabled_stats_all_zero() {
        let backend = DisabledDiskIo;
        let stats = backend.stats();
        assert_eq!(stats.read_bytes, 0);
        assert_eq!(stats.write_bytes, 0);
        assert_eq!(stats.cache_hits, 0);
        assert_eq!(stats.cache_misses, 0);
        assert_eq!(stats.write_buffer_bytes, 0);
    }
}
```

**Step 2: Register the module in lib.rs**

In `crates/ferrite-session/src/lib.rs`, add after the `pub mod disk;` line:

```rust
pub mod disk_backend;
```

And add to the `pub use` section:

```rust
pub use disk_backend::{DiskIoBackend, DiskIoStats, DisabledDiskIo};
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session disk_backend -- --nocapture`
Expected: 7 tests PASS

---

## Task 2: Implement `PosixDiskIo` Backend

**Files:**
- Modify: `crates/ferrite-session/src/disk_backend.rs`

This backend wraps the current `DiskActor` logic: per-torrent `Arc<dyn TorrentStorage>` registry, user-space ARC cache, and `WriteBuffer`. It is the default backend when `StorageMode` is `Auto`, `Sparse`, or `Full`.

**Step 1: Write tests for PosixDiskIo**

Append to `crates/ferrite-session/src/disk_backend.rs`:

```rust
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::write_buffer::WriteBuffer;
use ferrite_storage::ArcCache;

/// POSIX pread/pwrite disk I/O backend with user-space ARC cache.
///
/// This is the default backend for `StorageMode::Auto`, `Sparse`, and `Full`.
/// It manages per-torrent storage instances, a write buffer, and an ARC cache
/// for read-path caching.
pub struct PosixDiskIo {
    storages: Mutex<HashMap<Id20, Arc<dyn TorrentStorage>>>,
    cache: Mutex<ArcCache<(Id20, u32, u32), Bytes>>,
    write_buffer: Mutex<WriteBuffer>,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
}

impl PosixDiskIo {
    /// Create a new POSIX disk I/O backend.
    ///
    /// - `cache_blocks`: number of 16 KiB blocks in the read cache
    /// - `write_buffer_max`: maximum bytes in the write buffer before auto-flush
    pub fn new(cache_blocks: usize, write_buffer_max: usize) -> Self {
        PosixDiskIo {
            storages: Mutex::new(HashMap::new()),
            cache: Mutex::new(ArcCache::new(cache_blocks.max(1))),
            write_buffer: Mutex::new(WriteBuffer::new(write_buffer_max)),
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }
    }

    /// Create from a `DiskConfig`.
    pub fn from_config(config: &crate::disk::DiskConfig) -> Self {
        let write_max = (config.cache_size as f32 * config.write_cache_ratio) as usize;
        let cache_blocks = if config.cache_size > write_max {
            (config.cache_size - write_max) / 16384
        } else {
            64
        };
        Self::new(cache_blocks, write_max)
    }

    fn get_storage(&self, info_hash: Id20) -> ferrite_storage::Result<Arc<dyn TorrentStorage>> {
        self.storages
            .lock()
            .unwrap()
            .get(&info_hash)
            .cloned()
            .ok_or_else(|| {
                ferrite_storage::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "torrent not registered",
                ))
            })
    }
}
```

**Step 2: Implement `DiskIoBackend` for `PosixDiskIo`**

```rust
impl DiskIoBackend for PosixDiskIo {
    fn name(&self) -> &str {
        "posix"
    }

    fn register(&self, info_hash: Id20, storage: Arc<dyn TorrentStorage>) {
        self.storages.lock().unwrap().insert(info_hash, storage);
    }

    fn unregister(&self, info_hash: Id20) {
        self.storages.lock().unwrap().remove(&info_hash);
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
    ) -> ferrite_storage::Result<()> {
        let len = data.len();
        self.write_bytes.fetch_add(len as u64, Ordering::Relaxed);

        if flush {
            let storage = self.get_storage(info_hash)?;
            return storage.write_chunk(piece, begin, data);
        }

        let mut wb = self.write_buffer.lock().unwrap();
        wb.write(info_hash, piece, begin, Bytes::copy_from_slice(data));

        // Pressure flush
        if wb.needs_flush() {
            if let Some((ih, p)) = wb.oldest_piece() {
                if let Some(blocks) = wb.take_piece(ih, p) {
                    drop(wb); // release lock before I/O
                    if let Ok(storage) = self.get_storage(ih) {
                        for (b, d) in blocks {
                            storage.write_chunk(p, b, &d)?;
                        }
                    }
                }
            }
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
    ) -> ferrite_storage::Result<Bytes> {
        // 1. Check write buffer
        {
            let wb = self.write_buffer.lock().unwrap();
            if let Some(data) = wb.read(info_hash, piece, begin, length) {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(data);
            }
        }

        // 2. Check cache (unless volatile)
        if !volatile {
            let cache_key = (info_hash, piece, begin);
            let mut cache = self.cache.lock().unwrap();
            if let Some(data) = cache.get(&cache_key) {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(data.clone());
            }
        }
        self.cache_misses.fetch_add(1, Ordering::Relaxed);

        // 3. Read from storage
        let storage = self.get_storage(info_hash)?;
        let data = storage.read_chunk(piece, begin, length)?;
        let bytes = Bytes::from(data);
        self.read_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);

        // Cache (unless volatile)
        if !volatile {
            let cache_key = (info_hash, piece, begin);
            self.cache.lock().unwrap().insert(cache_key, bytes.clone());
        }

        Ok(bytes)
    }

    fn hash_piece(
        &self,
        info_hash: Id20,
        piece: u32,
        expected: &Id20,
    ) -> ferrite_storage::Result<bool> {
        self.flush_piece(info_hash, piece)?;
        let storage = self.get_storage(info_hash)?;
        storage.verify_piece(piece, expected)
    }

    fn hash_piece_v2(
        &self,
        info_hash: Id20,
        piece: u32,
        expected: &Id32,
    ) -> ferrite_storage::Result<bool> {
        self.flush_piece(info_hash, piece).ok();
        let storage = self.get_storage(info_hash)?;
        storage.verify_piece_v2(piece, expected)
    }

    fn hash_block(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
    ) -> ferrite_storage::Result<Id32> {
        self.flush_piece(info_hash, piece).ok();
        let storage = self.get_storage(info_hash)?;
        storage.hash_block(piece, begin, length)
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

    fn flush_piece(&self, info_hash: Id20, piece: u32) -> ferrite_storage::Result<()> {
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

    fn flush_all(&self) -> ferrite_storage::Result<()> {
        let keys: Vec<(Id20, u32)> = {
            let wb = self.write_buffer.lock().unwrap();
            wb.pending_keys().collect()
        };
        for (ih, piece) in keys {
            self.flush_piece(ih, piece)?;
        }
        Ok(())
    }

    fn move_storage(
        &self,
        _info_hash: Id20,
        _new_path: &Path,
    ) -> ferrite_storage::Result<()> {
        // TODO: implement in a future milestone
        Ok(())
    }

    fn stats(&self) -> DiskIoStats {
        DiskIoStats {
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            write_buffer_bytes: self.write_buffer.lock().unwrap().total_bytes(),
        }
    }
}
```

**Step 3: Add tests for PosixDiskIo**

Append to the `#[cfg(test)] mod tests` block:

```rust
    use ferrite_core::Lengths;
    use ferrite_storage::MemoryStorage;

    fn make_posix() -> PosixDiskIo {
        PosixDiskIo::new(64, 1024 * 1024)
    }

    fn make_memory_storage() -> Arc<dyn TorrentStorage> {
        let lengths = Lengths::new(100, 50, 25);
        Arc::new(MemoryStorage::new(lengths))
    }

    #[test]
    fn posix_backend_name() {
        let backend = make_posix();
        assert_eq!(backend.name(), "posix");
    }

    #[test]
    fn posix_write_flush_read() {
        let backend = make_posix();
        let ih = make_hash(1);
        backend.register(ih, make_memory_storage());

        // Direct-flush write
        backend
            .write_chunk(ih, 0, 0, &[42u8; 25], true)
            .unwrap();
        let data = backend.read_chunk(ih, 0, 0, 25, false).unwrap();
        assert_eq!(&data[..], &[42u8; 25]);
    }

    #[test]
    fn posix_buffered_write_then_flush() {
        let backend = make_posix();
        let ih = make_hash(2);
        backend.register(ih, make_memory_storage());

        // Buffered write
        backend
            .write_chunk(ih, 0, 0, &[1u8; 25], false)
            .unwrap();
        backend
            .write_chunk(ih, 0, 25, &[2u8; 25], false)
            .unwrap();

        // Read from buffer (no flush yet)
        let data = backend.read_chunk(ih, 0, 0, 25, false).unwrap();
        assert_eq!(&data[..], &[1u8; 25]);

        // Explicit flush
        backend.flush_piece(ih, 0).unwrap();

        // Read from storage (cache)
        let data = backend.read_chunk(ih, 0, 0, 25, false).unwrap();
        assert_eq!(&data[..], &[1u8; 25]);
    }

    #[test]
    fn posix_hash_piece_v1() {
        let backend = make_posix();
        let ih = make_hash(3);
        backend.register(ih, make_memory_storage());

        let piece_data = vec![9u8; 50];
        backend
            .write_chunk(ih, 0, 0, &piece_data, true)
            .unwrap();

        let expected = ferrite_core::sha1(&piece_data);
        assert!(backend.hash_piece(ih, 0, &expected).unwrap());
        assert!(!backend.hash_piece(ih, 0, &Id20::ZERO).unwrap());
    }

    #[test]
    fn posix_hash_piece_v2() {
        let backend = make_posix();
        let ih = make_hash(4);
        let lengths = Lengths::new(16384, 16384, 16384);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        backend.register(ih, storage);

        let piece_data = vec![0xABu8; 16384];
        backend
            .write_chunk(ih, 0, 0, &piece_data, true)
            .unwrap();

        let expected = ferrite_core::sha256(&piece_data);
        assert!(backend.hash_piece_v2(ih, 0, &expected).unwrap());
    }

    #[test]
    fn posix_hash_block() {
        let backend = make_posix();
        let ih = make_hash(5);
        let lengths = Lengths::new(16384, 16384, 16384);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        backend.register(ih, storage);

        let block_data = vec![0xCDu8; 16384];
        backend
            .write_chunk(ih, 0, 0, &block_data, true)
            .unwrap();

        let hash = backend.hash_block(ih, 0, 0, 16384).unwrap();
        assert_eq!(hash, ferrite_core::sha256(&block_data));
    }

    #[test]
    fn posix_clear_piece() {
        let backend = make_posix();
        let ih = make_hash(6);
        backend.register(ih, make_memory_storage());

        backend
            .write_chunk(ih, 0, 0, &[5u8; 25], true)
            .unwrap();
        // Populate cache
        backend.read_chunk(ih, 0, 0, 25, false).unwrap();
        // Clear
        backend.clear_piece(ih, 0);
        // Still readable (from storage)
        let data = backend.read_chunk(ih, 0, 0, 25, false).unwrap();
        assert_eq!(&data[..], &[5u8; 25]);
    }

    #[test]
    fn posix_unregister_cleans_up() {
        let backend = make_posix();
        let ih = make_hash(7);
        backend.register(ih, make_memory_storage());

        backend
            .write_chunk(ih, 0, 0, &[1u8; 25], false)
            .unwrap();
        backend.unregister(ih);

        // Should fail (torrent not registered)
        assert!(backend.read_chunk(ih, 0, 0, 25, false).is_err());
    }

    #[test]
    fn posix_stats_track_io() {
        let backend = make_posix();
        let ih = make_hash(8);
        backend.register(ih, make_memory_storage());

        backend
            .write_chunk(ih, 0, 0, &[42u8; 25], true)
            .unwrap();
        let _ = backend.read_chunk(ih, 0, 0, 25, false);

        let stats = backend.stats();
        assert_eq!(stats.write_bytes, 25);
        assert!(stats.read_bytes > 0 || stats.cache_hits > 0);
    }

    #[test]
    fn posix_flush_all() {
        let backend = make_posix();
        let ih = make_hash(9);
        backend.register(ih, make_memory_storage());

        backend
            .write_chunk(ih, 0, 0, &[1u8; 25], false)
            .unwrap();
        backend
            .write_chunk(ih, 0, 25, &[2u8; 25], false)
            .unwrap();

        backend.flush_all().unwrap();

        // Verify data reached storage by re-reading
        let data = backend.read_chunk(ih, 0, 0, 50, true).unwrap(); // volatile to bypass cache
        assert_eq!(&data[..25], &[1u8; 25]);
        assert_eq!(&data[25..], &[2u8; 25]);
    }
```

**Step 4: Run tests**

Run: `cargo test -p ferrite-session disk_backend -- --nocapture`
Expected: 17 tests PASS (7 disabled + 10 posix)

---

## Task 3: Implement `MmapDiskIo` Backend

**Files:**
- Modify: `crates/ferrite-session/src/disk_backend.rs`

The mmap backend delegates to `TorrentStorage` directly (no user-space ARC cache, since the kernel page cache handles caching for mmap). It still uses a write buffer for piece assembly before hashing.

**Step 1: Add `MmapDiskIo` implementation**

```rust
/// Memory-mapped disk I/O backend.
///
/// Relies on the kernel page cache for read caching. No user-space ARC cache
/// is needed. Write buffering is still used for piece assembly before hashing.
pub struct MmapDiskIo {
    storages: Mutex<HashMap<Id20, Arc<dyn TorrentStorage>>>,
    write_buffer: Mutex<WriteBuffer>,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
}

impl MmapDiskIo {
    /// Create a new mmap disk I/O backend.
    ///
    /// - `write_buffer_max`: maximum bytes in the write buffer before auto-flush
    pub fn new(write_buffer_max: usize) -> Self {
        MmapDiskIo {
            storages: Mutex::new(HashMap::new()),
            write_buffer: Mutex::new(WriteBuffer::new(write_buffer_max)),
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
        }
    }

    /// Create from a `DiskConfig`.
    pub fn from_config(config: &crate::disk::DiskConfig) -> Self {
        let write_max = (config.cache_size as f32 * config.write_cache_ratio) as usize;
        Self::new(write_max)
    }

    fn get_storage(&self, info_hash: Id20) -> ferrite_storage::Result<Arc<dyn TorrentStorage>> {
        self.storages
            .lock()
            .unwrap()
            .get(&info_hash)
            .cloned()
            .ok_or_else(|| {
                ferrite_storage::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "torrent not registered",
                ))
            })
    }
}

impl DiskIoBackend for MmapDiskIo {
    fn name(&self) -> &str {
        "mmap"
    }

    fn register(&self, info_hash: Id20, storage: Arc<dyn TorrentStorage>) {
        self.storages.lock().unwrap().insert(info_hash, storage);
    }

    fn unregister(&self, info_hash: Id20) {
        self.storages.lock().unwrap().remove(&info_hash);
        self.write_buffer.lock().unwrap().clear_torrent(info_hash);
    }

    fn write_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: &[u8],
        flush: bool,
    ) -> ferrite_storage::Result<()> {
        let len = data.len();
        self.write_bytes.fetch_add(len as u64, Ordering::Relaxed);

        if flush {
            let storage = self.get_storage(info_hash)?;
            return storage.write_chunk(piece, begin, data);
        }

        let mut wb = self.write_buffer.lock().unwrap();
        wb.write(info_hash, piece, begin, Bytes::copy_from_slice(data));

        if wb.needs_flush() {
            if let Some((ih, p)) = wb.oldest_piece() {
                if let Some(blocks) = wb.take_piece(ih, p) {
                    drop(wb);
                    if let Ok(storage) = self.get_storage(ih) {
                        for (b, d) in blocks {
                            storage.write_chunk(p, b, &d)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn read_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        _volatile: bool,
    ) -> ferrite_storage::Result<Bytes> {
        // Check write buffer first
        {
            let wb = self.write_buffer.lock().unwrap();
            if let Some(data) = wb.read(info_hash, piece, begin, length) {
                return Ok(data);
            }
        }

        // Read directly from storage (kernel handles caching)
        let storage = self.get_storage(info_hash)?;
        let data = storage.read_chunk(piece, begin, length)?;
        let bytes = Bytes::from(data);
        self.read_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    fn hash_piece(
        &self,
        info_hash: Id20,
        piece: u32,
        expected: &Id20,
    ) -> ferrite_storage::Result<bool> {
        self.flush_piece(info_hash, piece)?;
        let storage = self.get_storage(info_hash)?;
        storage.verify_piece(piece, expected)
    }

    fn hash_piece_v2(
        &self,
        info_hash: Id20,
        piece: u32,
        expected: &Id32,
    ) -> ferrite_storage::Result<bool> {
        self.flush_piece(info_hash, piece).ok();
        let storage = self.get_storage(info_hash)?;
        storage.verify_piece_v2(piece, expected)
    }

    fn hash_block(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
    ) -> ferrite_storage::Result<Id32> {
        self.flush_piece(info_hash, piece).ok();
        let storage = self.get_storage(info_hash)?;
        storage.hash_block(piece, begin, length)
    }

    fn clear_piece(&self, info_hash: Id20, piece: u32) {
        self.write_buffer
            .lock()
            .unwrap()
            .clear_piece(info_hash, piece);
        // No ARC cache to clear in mmap mode
    }

    fn flush_piece(&self, info_hash: Id20, piece: u32) -> ferrite_storage::Result<()> {
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

    fn flush_all(&self) -> ferrite_storage::Result<()> {
        let keys: Vec<(Id20, u32)> = {
            let wb = self.write_buffer.lock().unwrap();
            wb.pending_keys().collect()
        };
        for (ih, piece) in keys {
            self.flush_piece(ih, piece)?;
        }
        Ok(())
    }

    fn move_storage(
        &self,
        _info_hash: Id20,
        _new_path: &Path,
    ) -> ferrite_storage::Result<()> {
        Ok(())
    }

    fn stats(&self) -> DiskIoStats {
        DiskIoStats {
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
            cache_hits: 0,
            cache_misses: 0,
            write_buffer_bytes: self.write_buffer.lock().unwrap().total_bytes(),
        }
    }
}
```

**Step 2: Add tests for MmapDiskIo**

Append to `#[cfg(test)] mod tests`:

```rust
    fn make_mmap() -> MmapDiskIo {
        MmapDiskIo::new(1024 * 1024)
    }

    #[test]
    fn mmap_backend_name() {
        let backend = make_mmap();
        assert_eq!(backend.name(), "mmap");
    }

    #[test]
    fn mmap_write_flush_read() {
        let backend = make_mmap();
        let ih = make_hash(10);
        backend.register(ih, make_memory_storage());

        backend
            .write_chunk(ih, 0, 0, &[42u8; 25], true)
            .unwrap();
        let data = backend.read_chunk(ih, 0, 0, 25, false).unwrap();
        assert_eq!(&data[..], &[42u8; 25]);
    }

    #[test]
    fn mmap_hash_piece_v1() {
        let backend = make_mmap();
        let ih = make_hash(11);
        backend.register(ih, make_memory_storage());

        let piece_data = vec![9u8; 50];
        backend
            .write_chunk(ih, 0, 0, &piece_data, true)
            .unwrap();

        let expected = ferrite_core::sha1(&piece_data);
        assert!(backend.hash_piece(ih, 0, &expected).unwrap());
    }

    #[test]
    fn mmap_no_cache_stats() {
        let backend = make_mmap();
        let ih = make_hash(12);
        backend.register(ih, make_memory_storage());

        backend
            .write_chunk(ih, 0, 0, &[1u8; 25], true)
            .unwrap();
        let _ = backend.read_chunk(ih, 0, 0, 25, false);

        let stats = backend.stats();
        // Mmap backend reports zero cache hits/misses (no user-space cache)
        assert_eq!(stats.cache_hits, 0);
        assert_eq!(stats.cache_misses, 0);
    }
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session disk_backend -- --nocapture`
Expected: 21 tests PASS

---

## Task 4: Add `create_backend_from_config()` Factory Function

**Files:**
- Modify: `crates/ferrite-session/src/disk_backend.rs`

This function creates the appropriate backend based on `StorageMode`, preserving backward compatibility. Add to the end of `disk_backend.rs` (before the `#[cfg(test)]` module):

**Step 1: Write the factory function**

```rust
/// Create a disk I/O backend from a `DiskConfig`.
///
/// - `StorageMode::Mmap` -> `MmapDiskIo`
/// - All others (`Auto`, `Sparse`, `Full`) -> `PosixDiskIo`
pub fn create_backend_from_config(config: &crate::disk::DiskConfig) -> Arc<dyn DiskIoBackend> {
    match config.storage_mode {
        ferrite_core::StorageMode::Mmap => Arc::new(MmapDiskIo::from_config(config)),
        _ => Arc::new(PosixDiskIo::from_config(config)),
    }
}
```

**Step 2: Add tests for the factory function**

```rust
    #[test]
    fn factory_auto_creates_posix() {
        let config = crate::disk::DiskConfig::default();
        let backend = create_backend_from_config(&config);
        assert_eq!(backend.name(), "posix");
    }

    #[test]
    fn factory_mmap_creates_mmap() {
        let config = crate::disk::DiskConfig {
            storage_mode: ferrite_core::StorageMode::Mmap,
            ..crate::disk::DiskConfig::default()
        };
        let backend = create_backend_from_config(&config);
        assert_eq!(backend.name(), "mmap");
    }

    #[test]
    fn factory_sparse_creates_posix() {
        let config = crate::disk::DiskConfig {
            storage_mode: ferrite_core::StorageMode::Sparse,
            ..crate::disk::DiskConfig::default()
        };
        let backend = create_backend_from_config(&config);
        assert_eq!(backend.name(), "posix");
    }
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session disk_backend -- --nocapture`
Expected: 24 tests PASS

---

## Task 5: Refactor `DiskActor` to Delegate to `DiskIoBackend`

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs`

This is the core refactoring. The `DiskActor` keeps its channel/semaphore/batch architecture but replaces all inline storage logic with calls to the backend trait.

**Step 1: Update `DiskManagerHandle::new` to accept an optional backend**

Replace the existing `DiskManagerHandle::new` signature and `DiskActor` struct. The changes:

1. `DiskActor` stores `Arc<dyn DiskIoBackend>` instead of `HashMap<Id20, Arc<dyn TorrentStorage>>`, `ArcCache`, `WriteBuffer`, and `DiskStats`.
2. `DiskManagerHandle::new` gains an overload `new_with_backend()`.
3. All `handle_*` methods delegate to the backend.

In `crates/ferrite-session/src/disk.rs`, replace the entire `DiskActor` struct and its `impl` block:

```rust
// Add import at top of file
use crate::disk_backend::{DiskIoBackend, create_backend_from_config};

// Updated DiskActor
struct DiskActor {
    rx: mpsc::Receiver<DiskJob>,
    backend: Arc<dyn DiskIoBackend>,
    semaphore: Arc<tokio::sync::Semaphore>,
    #[allow(dead_code)]
    config: DiskConfig,
}
```

Update `DiskManagerHandle`:

```rust
impl DiskManagerHandle {
    /// Create a new disk manager with a backend derived from config.
    pub fn new(config: DiskConfig) -> (Self, tokio::task::JoinHandle<()>) {
        let backend = create_backend_from_config(&config);
        Self::new_with_backend(config, backend)
    }

    /// Create a new disk manager with a custom backend.
    pub fn new_with_backend(
        config: DiskConfig,
        backend: Arc<dyn DiskIoBackend>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let actor = DiskActor::new(rx, config, backend);
        let join = tokio::spawn(actor.run());
        (DiskManagerHandle { tx }, join)
    }

    // ... (register_torrent, unregister_torrent, shutdown unchanged)
}
```

Update `DiskActor::new` and all `handle_*` methods:

```rust
impl DiskActor {
    fn new(
        rx: mpsc::Receiver<DiskJob>,
        config: DiskConfig,
        backend: Arc<dyn DiskIoBackend>,
    ) -> Self {
        DiskActor {
            rx,
            backend,
            semaphore: Arc::new(tokio::sync::Semaphore::new(config.io_threads)),
            config,
        }
    }

    async fn run(mut self) {
        // Same batch-processing loop, unchanged
        loop {
            let first = match self.rx.recv().await {
                Some(job) => job,
                None => break,
            };
            let mut batch = vec![first];
            while let Ok(job) = self.rx.try_recv() {
                batch.push(job);
            }
            for job in batch {
                if let DiskJob::Shutdown { reply } = job {
                    self.backend.flush_all().ok();
                    let _ = reply.send(());
                    return;
                }
                self.process_job(job).await;
            }
        }
    }

    async fn process_job(&mut self, job: DiskJob) {
        match job {
            DiskJob::Register {
                info_hash,
                storage,
                reply,
            } => {
                self.backend.register(info_hash, storage);
                let _ = reply.send(());
            }
            DiskJob::Unregister { info_hash } => {
                self.backend.unregister(info_hash);
            }
            DiskJob::Write {
                info_hash,
                piece,
                begin,
                data,
                flags,
                reply,
            } => {
                let backend = Arc::clone(&self.backend);
                let flush = flags.contains(DiskJobFlags::FLUSH_PIECE);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.write_chunk(info_hash, piece, begin, &data, flush)
                })
                .await
                .unwrap();
                drop(permit);
                let _ = reply.send(result);
            }
            DiskJob::Read {
                info_hash,
                piece,
                begin,
                length,
                flags,
                reply,
            } => {
                let backend = Arc::clone(&self.backend);
                let volatile = flags.contains(DiskJobFlags::VOLATILE_READ);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.read_chunk(info_hash, piece, begin, length, volatile)
                })
                .await
                .unwrap();
                drop(permit);
                let _ = reply.send(result);
            }
            DiskJob::Hash {
                info_hash,
                piece,
                expected,
                reply,
                ..
            } => {
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.hash_piece(info_hash, piece, &expected)
                })
                .await
                .unwrap();
                drop(permit);
                let _ = reply.send(result);
            }
            DiskJob::HashV2 {
                info_hash,
                piece,
                expected,
                reply,
                ..
            } => {
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.hash_piece_v2(info_hash, piece, &expected)
                })
                .await
                .unwrap();
                drop(permit);
                let _ = reply.send(result);
            }
            DiskJob::BlockHash {
                info_hash,
                piece,
                begin,
                length,
                reply,
                ..
            } => {
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.hash_block(info_hash, piece, begin, length)
                })
                .await
                .unwrap();
                drop(permit);
                let _ = reply.send(result);
            }
            DiskJob::ClearPiece { info_hash, piece } => {
                self.backend.clear_piece(info_hash, piece);
            }
            DiskJob::FlushWriteBuffer {
                info_hash,
                piece,
                reply,
            } => {
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.flush_piece(info_hash, piece)
                })
                .await
                .unwrap();
                drop(permit);
                let _ = reply.send(result);
            }
            DiskJob::MoveStorage {
                info_hash,
                new_path,
                reply,
            } => {
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.move_storage(info_hash, &new_path)
                })
                .await
                .unwrap();
                drop(permit);
                let _ = reply.send(result);
            }
            DiskJob::Shutdown { .. } => unreachable!(),
        }
    }
}
```

Remove the old `DiskActor` fields (`storages`, `cache`, `write_buffer`, `stats`, `use_cache`) and all the old `handle_write`, `handle_read`, `handle_hash`, `handle_hash_v2`, `handle_block_hash`, `flush_piece`, `flush_all_write_buffers`, `get_storage` methods. The `DiskStats` struct stays (it is a public type already used elsewhere) but its values now come from `backend.stats()`.

**Step 2: Update `DiskStats` conversion**

Add a `From<DiskIoStats>` impl for `DiskStats` so existing consumers are unaffected:

```rust
impl From<crate::disk_backend::DiskIoStats> for DiskStats {
    fn from(s: crate::disk_backend::DiskIoStats) -> Self {
        DiskStats {
            read_bytes: s.read_bytes,
            write_bytes: s.write_bytes,
            cache_hits: s.cache_hits,
            cache_misses: s.cache_misses,
            write_buffer_bytes: s.write_buffer_bytes,
            queued_jobs: 0,
        }
    }
}
```

**Step 3: Run ALL existing disk tests**

Run: `cargo test -p ferrite-session disk -- --nocapture`
Expected: All existing tests in `disk.rs` PASS (async_write_read, verify_through_handle, cache_hit_avoids_io, volatile_read_bypasses_cache, clear_piece_evicts_cache, write_buffer_flush, verify_piece_v2_via_disk_handle, hash_block_via_disk_handle, concurrent_verify_multiple_pieces). These tests use `DiskManagerHandle::new(test_config())` which now creates a `PosixDiskIo` backend internally.

**Step 4: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All 895+ tests PASS. No behavior change for existing consumers.

---

## Task 6: Refactor `SessionActor` to Accept Custom Backend

**Files:**
- Modify: `crates/ferrite-session/src/session.rs`

**Step 1: Add `start_with_backend` method to `SessionHandle`**

Add a new public method alongside the existing `start` and `start_with_plugins`:

```rust
impl SessionHandle {
    /// Start a session with a custom disk I/O backend.
    pub async fn start_with_backend(
        settings: Settings,
        backend: Arc<dyn crate::disk_backend::DiskIoBackend>,
    ) -> crate::Result<Self> {
        let plugins = Arc::new(Vec::new());
        Self::start_with_plugins_and_backend(settings, plugins, backend).await
    }
}
```

Refactor the existing `start_with_plugins` to call a shared internal method `start_with_plugins_and_backend`:

```rust
    pub async fn start_with_plugins(
        settings: Settings,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
    ) -> crate::Result<Self> {
        let disk_config = crate::disk::DiskConfig::from(&settings);
        let backend = crate::disk_backend::create_backend_from_config(&disk_config);
        Self::start_with_plugins_and_backend(settings, plugins, backend).await
    }

    async fn start_with_plugins_and_backend(
        settings: Settings,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
        backend: Arc<dyn crate::disk_backend::DiskIoBackend>,
    ) -> crate::Result<Self> {
        settings.validate()?;
        // ... (existing body, but DiskManagerHandle::new becomes DiskManagerHandle::new_with_backend)
```

In the body, replace:

```rust
let disk_config = crate::disk::DiskConfig::from(&settings);
let (disk_manager, disk_actor_handle) =
    crate::disk::DiskManagerHandle::new(disk_config);
```

With:

```rust
let disk_config = crate::disk::DiskConfig::from(&settings);
let (disk_manager, disk_actor_handle) =
    crate::disk::DiskManagerHandle::new_with_backend(disk_config, backend);
```

**Step 2: Export new types from lib.rs**

The `start_with_backend` method is already on the publicly exported `SessionHandle`. Ensure `DiskIoBackend`, `DiskIoStats`, `DisabledDiskIo`, `PosixDiskIo`, `MmapDiskIo`, `create_backend_from_config` are exported from `crates/ferrite-session/src/lib.rs`:

```rust
pub use disk_backend::{
    DiskIoBackend, DiskIoStats, DisabledDiskIo, PosixDiskIo, MmapDiskIo,
    create_backend_from_config,
};
```

**Step 3: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests PASS.

---

## Task 7: Add `ClientBuilder::disk_io_backend()` to the Facade

**Files:**
- Modify: `crates/ferrite/src/client.rs`
- Modify: `crates/ferrite/src/session.rs`
- Modify: `crates/ferrite/src/prelude.rs`

**Step 1: Store optional backend in ClientBuilder**

In `crates/ferrite/src/client.rs`, add a field:

```rust
pub struct ClientBuilder {
    settings: Settings,
    plugins: Vec<Box<dyn ExtensionPlugin>>,
    backend: Option<Arc<dyn ferrite_session::DiskIoBackend>>,
}
```

Update `new()`:

```rust
    pub fn new() -> Self {
        Self {
            settings: Settings::default(),
            plugins: Vec::new(),
            backend: None,
        }
    }
```

Add the builder method:

```rust
    /// Set a custom disk I/O backend.
    ///
    /// Overrides the default backend (which is selected by `StorageMode`).
    /// Use `DisabledDiskIo` for benchmarking network throughput without disk I/O.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn example() -> ferrite::session::Result<()> {
    /// use std::sync::Arc;
    /// use ferrite::session::DisabledDiskIo;
    ///
    /// let session = ferrite::ClientBuilder::new()
    ///     .download_dir("/tmp/downloads")
    ///     .disk_io_backend(Arc::new(DisabledDiskIo))
    ///     .start()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn disk_io_backend(
        mut self,
        backend: Arc<dyn ferrite_session::DiskIoBackend>,
    ) -> Self {
        self.backend = Some(backend);
        self
    }
```

Update `start()`:

```rust
    pub async fn start(self) -> ferrite_session::Result<ferrite_session::SessionHandle> {
        let plugins = Arc::new(self.plugins);
        match self.backend {
            Some(backend) => {
                ferrite_session::SessionHandle::start_with_plugins_and_backend(
                    self.settings, plugins, backend,
                )
                .await
            }
            None => {
                ferrite_session::SessionHandle::start_with_plugins(self.settings, plugins).await
            }
        }
    }
```

Note: This requires `start_with_plugins_and_backend` to be `pub` (not `pub(crate)`). Update it from `async fn start_with_plugins_and_backend` to `pub async fn start_with_plugins_and_backend` in `session.rs`.

**Step 2: Re-export from facade**

In `crates/ferrite/src/session.rs`, add:

```rust
pub use ferrite_session::{
    // ... existing ...
    // Pluggable disk I/O (M49)
    DiskIoBackend,
    DiskIoStats,
    DisabledDiskIo,
    PosixDiskIo,
    MmapDiskIo,
    create_backend_from_config,
};
```

In `crates/ferrite/src/prelude.rs`, add:

```rust
// Pluggable disk I/O (M49)
pub use crate::session::{DiskIoBackend, DisabledDiskIo};
```

**Step 3: Add tests**

In `crates/ferrite/src/client.rs`, add to the `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn client_builder_disk_io_backend() {
        let backend = std::sync::Arc::new(ferrite_session::DisabledDiskIo);
        let builder = ClientBuilder::new()
            .disk_io_backend(backend)
            .listen_port(0)
            .download_dir("/tmp");

        // Verify builder chains correctly
        let _ = builder.into_settings();
    }

    #[test]
    fn client_builder_default_no_backend() {
        let builder = ClientBuilder::new();
        // backend field is None by default
        assert!(builder.backend.is_none());
    }
```

In `crates/ferrite/src/lib.rs`, add an integration test:

```rust
    #[tokio::test]
    async fn session_with_disabled_disk_io() {
        let backend = std::sync::Arc::new(session::DisabledDiskIo);
        let session = crate::ClientBuilder::new()
            .listen_port(0)
            .download_dir("/tmp")
            .enable_dht(false)
            .enable_lsd(false)
            .enable_utp(false)
            .disk_io_backend(backend)
            .start()
            .await
            .unwrap();

        // Add a torrent and verify it works with disabled I/O
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent_facade(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(make_storage_facade(&data, 16384)))
            .await
            .unwrap();

        let list = session.list_torrents().await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&info_hash));

        session.shutdown().await.unwrap();
    }
```

**Step 4: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests PASS.

---

## Task 8: Run Clippy and Final Verification

**Step 1: Clippy check**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

**Step 2: Full test suite**

Run: `cargo test --workspace`
Expected: All tests PASS (~900+ tests).

**Step 3: Verify backward compatibility**

Confirm these invariants:
1. `ClientBuilder::new().start().await` uses `PosixDiskIo` by default (Auto mode)
2. `StorageMode::Mmap` creates `MmapDiskIo`
3. `DisabledDiskIo` only activable via explicit `disk_io_backend()`
4. All existing `DiskHandle` tests pass without modification
5. `DiskStats` values are populated from backend stats

---

## Summary

| Task | Files | Tests Added | Description |
|------|-------|-------------|-------------|
| 1 | `disk_backend.rs`, `lib.rs` | 7 | `DiskIoBackend` trait + `DisabledDiskIo` |
| 2 | `disk_backend.rs` | 10 | `PosixDiskIo` (wraps current pread/pwrite + ARC cache logic) |
| 3 | `disk_backend.rs` | 4 | `MmapDiskIo` (kernel page cache, no user-space cache) |
| 4 | `disk_backend.rs` | 3 | `create_backend_from_config()` factory |
| 5 | `disk.rs` | 0 (existing pass) | Refactor `DiskActor` to delegate to backend |
| 6 | `session.rs`, `lib.rs` | 0 | `SessionHandle::start_with_backend()` |
| 7 | `client.rs`, `session.rs`, `prelude.rs`, `lib.rs` | 3 | `ClientBuilder::disk_io_backend()` + integration test |
| 8 | — | 0 | Clippy + full verification |
| **Total** | **7 files** | **~27 new tests** | |

**Version bump:** `0.41.0` -> `0.42.0`
**Commit message:** `feat: pluggable disk I/O backend interface (M49)`
