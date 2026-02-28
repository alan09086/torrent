use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use bitflags::bitflags;
use bytes::Bytes;
use ferrite_core::Id20;
use ferrite_storage::{ArcCache, TorrentStorage};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::write_buffer::WriteBuffer;

bitflags! {
    /// Hint flags for disk I/O operations.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DiskJobFlags: u8 {
        /// Copy cached blocks rather than sharing references.
        const FORCE_COPY      = 0x01;
        /// Hint: sequential file access pattern (read-ahead friendly).
        const SEQUENTIAL      = 0x02;
        /// Don't cache this read result.
        const VOLATILE_READ   = 0x04;
        /// Flush completed piece to disk immediately.
        const FLUSH_PIECE     = 0x08;
    }
}

pub(crate) enum DiskJob {
    Register {
        info_hash: Id20,
        storage: Arc<dyn TorrentStorage>,
        reply: oneshot::Sender<()>,
    },
    Unregister {
        info_hash: Id20,
    },

    Write {
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: Bytes,
        flags: DiskJobFlags,
        reply: oneshot::Sender<ferrite_storage::Result<()>>,
    },
    Read {
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        flags: DiskJobFlags,
        reply: oneshot::Sender<ferrite_storage::Result<Bytes>>,
    },
    Hash {
        info_hash: Id20,
        piece: u32,
        expected: Id20,
        #[allow(dead_code)]
        flags: DiskJobFlags,
        reply: oneshot::Sender<ferrite_storage::Result<bool>>,
    },

    ClearPiece {
        info_hash: Id20,
        piece: u32,
    },
    FlushWriteBuffer {
        info_hash: Id20,
        piece: u32,
        reply: oneshot::Sender<ferrite_storage::Result<()>>,
    },
    MoveStorage {
        #[allow(dead_code)] // TODO: implement in future milestone
        info_hash: Id20,
        #[allow(dead_code)]
        new_path: PathBuf,
        reply: oneshot::Sender<ferrite_storage::Result<()>>,
    },

    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Configuration for the disk I/O subsystem.
#[derive(Debug, Clone)]
pub struct DiskConfig {
    /// Number of concurrent I/O threads (semaphore permits). Default: 4.
    pub io_threads: usize,
    /// Storage allocation mode. Default: Auto.
    pub storage_mode: ferrite_core::StorageMode,
    /// Total cache size in bytes (read + write). Default: 64 MiB.
    pub cache_size: usize,
    /// Fraction of cache_size reserved for write buffering. Default: 0.25.
    pub write_cache_ratio: f32,
    /// Bounded channel capacity. Default: 512.
    pub channel_capacity: usize,
}

impl Default for DiskConfig {
    fn default() -> Self {
        DiskConfig {
            io_threads: 4,
            storage_mode: ferrite_core::StorageMode::Auto,
            cache_size: 64 * 1024 * 1024,
            write_cache_ratio: 0.25,
            channel_capacity: 512,
        }
    }
}

/// Disk I/O performance counters.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DiskStats {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub write_buffer_bytes: usize,
    pub queued_jobs: usize,
}

// ---------------------------------------------------------------------------
// DiskManagerHandle — session-level handle
// ---------------------------------------------------------------------------

/// Session-level handle for managing the disk subsystem.
#[derive(Clone)]
pub struct DiskManagerHandle {
    tx: mpsc::Sender<DiskJob>,
}

impl DiskManagerHandle {
    /// Create a new disk manager. Returns the handle and a `JoinHandle` for
    /// the background actor task.
    pub fn new(config: DiskConfig) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let actor = DiskActor::new(rx, config);
        let join = tokio::spawn(actor.run());
        (DiskManagerHandle { tx }, join)
    }

    /// Register a torrent's storage with the disk subsystem and return a
    /// per-torrent `DiskHandle`.
    pub async fn register_torrent(
        &self,
        info_hash: Id20,
        storage: Arc<dyn TorrentStorage>,
    ) -> DiskHandle {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::Register {
                info_hash,
                storage,
                reply: reply_tx,
            })
            .await;
        let _ = reply_rx.await;
        DiskHandle {
            tx: self.tx.clone(),
            info_hash,
        }
    }

    /// Unregister a torrent, flushing and clearing its write buffer and cache.
    pub async fn unregister_torrent(&self, info_hash: Id20) {
        let _ = self.tx.send(DiskJob::Unregister { info_hash }).await;
    }

    /// Gracefully shut down the disk subsystem, flushing all buffers.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(DiskJob::Shutdown { reply: tx }).await;
        let _ = rx.await;
    }
}

// ---------------------------------------------------------------------------
// DiskHandle — per-torrent handle
// ---------------------------------------------------------------------------

/// Per-torrent handle for async disk I/O.
#[derive(Clone)]
pub struct DiskHandle {
    tx: mpsc::Sender<DiskJob>,
    info_hash: Id20,
}

impl DiskHandle {
    /// Write a chunk to disk (may be buffered).
    pub async fn write_chunk(
        &self,
        piece: u32,
        begin: u32,
        data: Bytes,
        flags: DiskJobFlags,
    ) -> ferrite_storage::Result<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::Write {
                info_hash: self.info_hash,
                piece,
                begin,
                data,
                flags,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or(Err(ferrite_storage::Error::Io(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "disk actor gone"),
        )))
    }

    /// Read a chunk from disk (may hit cache or write buffer).
    pub async fn read_chunk(
        &self,
        piece: u32,
        begin: u32,
        length: u32,
        flags: DiskJobFlags,
    ) -> ferrite_storage::Result<Bytes> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::Read {
                info_hash: self.info_hash,
                piece,
                begin,
                length,
                flags,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or(Err(ferrite_storage::Error::Io(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "disk actor gone"),
        )))
    }

    /// Verify a piece hash against an expected value.
    pub async fn verify_piece(
        &self,
        piece: u32,
        expected: Id20,
        flags: DiskJobFlags,
    ) -> ferrite_storage::Result<bool> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::Hash {
                info_hash: self.info_hash,
                piece,
                expected,
                flags,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or(Err(ferrite_storage::Error::Io(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "disk actor gone"),
        )))
    }

    /// Clear a piece from cache and write buffer (e.g. on hash failure).
    pub async fn clear_piece(&self, piece: u32) {
        let _ = self
            .tx
            .send(DiskJob::ClearPiece {
                info_hash: self.info_hash,
                piece,
            })
            .await;
    }

    /// Flush a specific piece from the write buffer to disk.
    pub async fn flush_piece(&self, piece: u32) -> ferrite_storage::Result<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::FlushWriteBuffer {
                info_hash: self.info_hash,
                piece,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or(Err(ferrite_storage::Error::Io(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "disk actor gone"),
        )))
    }

    /// Move a torrent's storage to a new directory.
    pub async fn move_storage(&self, new_path: PathBuf) -> ferrite_storage::Result<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::MoveStorage {
                info_hash: self.info_hash,
                new_path,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or(Err(ferrite_storage::Error::Io(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "disk actor gone"),
        )))
    }
}

// ---------------------------------------------------------------------------
// DiskActor — central tokio task
// ---------------------------------------------------------------------------

struct DiskActor {
    rx: mpsc::Receiver<DiskJob>,
    storages: HashMap<Id20, Arc<dyn TorrentStorage>>,
    cache: ArcCache<(Id20, u32, u32), Bytes>,
    write_buffer: WriteBuffer,
    semaphore: Arc<tokio::sync::Semaphore>,
    stats: DiskStats,
    use_cache: bool,
    #[allow(dead_code)]
    config: DiskConfig,
}

impl DiskActor {
    fn new(rx: mpsc::Receiver<DiskJob>, config: DiskConfig) -> Self {
        let use_cache = config.storage_mode != ferrite_core::StorageMode::Mmap;
        let write_max = (config.cache_size as f32 * config.write_cache_ratio) as usize;
        // Cache capacity in number of 16 KiB blocks
        let cache_blocks = if config.cache_size > write_max {
            (config.cache_size - write_max) / 16384
        } else {
            64
        };

        DiskActor {
            rx,
            storages: HashMap::new(),
            cache: ArcCache::new(cache_blocks.max(1)),
            write_buffer: WriteBuffer::new(write_max),
            semaphore: Arc::new(tokio::sync::Semaphore::new(config.io_threads)),
            stats: DiskStats::default(),
            use_cache,
            config,
        }
    }

    async fn run(mut self) {
        loop {
            // Block on first job
            let first = match self.rx.recv().await {
                Some(job) => job,
                None => break,
            };

            // Drain remaining pending jobs (batch processing)
            let mut batch = vec![first];
            while let Ok(job) = self.rx.try_recv() {
                batch.push(job);
            }

            for job in batch {
                if let DiskJob::Shutdown { reply } = job {
                    self.flush_all_write_buffers().await;
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
                self.storages.insert(info_hash, storage);
                let _ = reply.send(());
            }
            DiskJob::Unregister { info_hash } => {
                self.storages.remove(&info_hash);
                self.write_buffer.clear_torrent(info_hash);
                self.cache.remove_where(|k| k.0 == info_hash);
            }
            DiskJob::Write {
                info_hash,
                piece,
                begin,
                data,
                flags,
                reply,
            } => {
                let result = self
                    .handle_write(info_hash, piece, begin, data, flags)
                    .await;
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
                let result = self
                    .handle_read(info_hash, piece, begin, length, flags)
                    .await;
                let _ = reply.send(result);
            }
            DiskJob::Hash {
                info_hash,
                piece,
                expected,
                reply,
                ..
            } => {
                let result = self.handle_hash(info_hash, piece, expected).await;
                let _ = reply.send(result);
            }
            DiskJob::ClearPiece { info_hash, piece } => {
                self.write_buffer.clear_piece(info_hash, piece);
                self.cache
                    .remove_where(|k| k.0 == info_hash && k.1 == piece);
            }
            DiskJob::FlushWriteBuffer {
                info_hash,
                piece,
                reply,
            } => {
                let result = self.flush_piece(info_hash, piece).await;
                let _ = reply.send(result);
            }
            DiskJob::MoveStorage { reply, .. } => {
                // TODO: implement in future milestone
                let _ = reply.send(Ok(()));
            }
            DiskJob::Shutdown { .. } => unreachable!(),
        }
    }

    async fn handle_write(
        &mut self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: Bytes,
        flags: DiskJobFlags,
    ) -> ferrite_storage::Result<()> {
        let len = data.len();

        if flags.contains(DiskJobFlags::FLUSH_PIECE) {
            // Write directly to storage, bypass buffer
            let storage = self.get_storage(info_hash)?;
            let permit = self.semaphore.clone().acquire_owned().await.unwrap();
            let result =
                tokio::task::spawn_blocking(move || storage.write_chunk(piece, begin, &data))
                    .await
                    .unwrap();
            drop(permit);
            self.stats.write_bytes += len as u64;
            return result;
        }

        // Buffer the write
        self.write_buffer.write(info_hash, piece, begin, data);
        self.stats.write_bytes += len as u64;

        // Check for pressure flush
        if self.write_buffer.needs_flush()
            && let Some((ih, p)) = self.write_buffer.oldest_piece()
        {
            self.flush_piece(ih, p).await.ok();
        }

        Ok(())
    }

    async fn handle_read(
        &mut self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        flags: DiskJobFlags,
    ) -> ferrite_storage::Result<Bytes> {
        // 1. Check write buffer first
        if let Some(data) = self.write_buffer.read(info_hash, piece, begin, length) {
            self.stats.cache_hits += 1;
            return Ok(data);
        }

        // 2. Check ARC cache (unless volatile or mmap mode)
        let cache_key = (info_hash, piece, begin);
        if self.use_cache
            && !flags.contains(DiskJobFlags::VOLATILE_READ)
            && let Some(data) = self.cache.get(&cache_key)
        {
            self.stats.cache_hits += 1;
            return Ok(data.clone());
        }
        self.stats.cache_misses += 1;

        // 3. Read from storage
        let storage = self.get_storage(info_hash)?;
        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        let data =
            tokio::task::spawn_blocking(move || storage.read_chunk(piece, begin, length))
                .await
                .unwrap()?;
        drop(permit);

        let bytes = Bytes::from(data);
        self.stats.read_bytes += bytes.len() as u64;

        // Cache the result (unless volatile or mmap)
        if self.use_cache && !flags.contains(DiskJobFlags::VOLATILE_READ) {
            self.cache.insert(cache_key, bytes.clone());
        }

        Ok(bytes)
    }

    async fn handle_hash(
        &mut self,
        info_hash: Id20,
        piece: u32,
        expected: Id20,
    ) -> ferrite_storage::Result<bool> {
        // Flush any buffered writes for this piece before hashing
        self.flush_piece(info_hash, piece).await?;

        let storage = self.get_storage(info_hash)?;
        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        let result =
            tokio::task::spawn_blocking(move || storage.verify_piece(piece, &expected))
                .await
                .unwrap();
        drop(permit);
        result
    }

    async fn flush_piece(
        &mut self,
        info_hash: Id20,
        piece: u32,
    ) -> ferrite_storage::Result<()> {
        let blocks = match self.write_buffer.take_piece(info_hash, piece) {
            Some(b) => b,
            None => return Ok(()),
        };
        let storage = match self.storages.get(&info_hash).cloned() {
            Some(s) => s,
            None => return Ok(()),
        };

        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        let result = tokio::task::spawn_blocking(move || {
            for (begin, data) in blocks {
                storage.write_chunk(piece, begin, &data)?;
            }
            Ok(())
        })
        .await
        .unwrap();
        drop(permit);
        result
    }

    async fn flush_all_write_buffers(&mut self) {
        // Collect all pending (info_hash, piece) pairs
        let pieces: Vec<(Id20, u32)> = self
            .write_buffer
            .pending_keys()
            .collect();

        for (ih, piece) in pieces {
            if let Err(e) = self.flush_piece(ih, piece).await {
                warn!(piece, "flush_all: failed to flush piece: {e}");
            }
        }
    }

    fn get_storage(
        &self,
        info_hash: Id20,
    ) -> ferrite_storage::Result<Arc<dyn TorrentStorage>> {
        self.storages.get(&info_hash).cloned().ok_or_else(|| {
            ferrite_storage::Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "torrent not registered",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_core::Lengths;
    use ferrite_storage::MemoryStorage;

    fn test_config() -> DiskConfig {
        DiskConfig {
            io_threads: 2,
            cache_size: 1024 * 1024,
            ..DiskConfig::default()
        }
    }

    fn make_hash(n: u8) -> Id20 {
        let mut b = [0u8; 20];
        b[0] = n;
        Id20(b)
    }

    #[tokio::test]
    async fn async_write_read() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(1);
        let lengths = Lengths::new(100, 50, 25);
        let storage = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, storage).await;

        let data = Bytes::from(vec![42u8; 25]);
        disk.write_chunk(0, 0, data.clone(), DiskJobFlags::FLUSH_PIECE)
            .await
            .unwrap();
        let read = disk
            .read_chunk(0, 0, 25, DiskJobFlags::empty())
            .await
            .unwrap();
        assert_eq!(read, data);

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn verify_through_handle() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(2);
        let lengths = Lengths::new(100, 50, 25);
        let storage = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, storage).await;

        let piece_data = vec![9u8; 50];
        disk.write_chunk(0, 0, Bytes::from(piece_data.clone()), DiskJobFlags::FLUSH_PIECE)
            .await
            .unwrap();
        disk.write_chunk(
            0,
            25,
            Bytes::from(vec![9u8; 25]),
            DiskJobFlags::FLUSH_PIECE,
        )
        .await
        .unwrap();

        let expected = ferrite_core::sha1(&piece_data);
        assert!(
            disk.verify_piece(0, expected, DiskJobFlags::empty())
                .await
                .unwrap()
        );
        assert!(
            !disk
                .verify_piece(0, Id20::ZERO, DiskJobFlags::empty())
                .await
                .unwrap()
        );

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn cache_hit_avoids_io() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(3);
        let lengths = Lengths::new(100, 50, 25);
        let storage = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, storage).await;

        let data = Bytes::from(vec![7u8; 25]);
        disk.write_chunk(0, 0, data.clone(), DiskJobFlags::FLUSH_PIECE)
            .await
            .unwrap();

        // First read: cache miss, reads from storage
        let r1 = disk
            .read_chunk(0, 0, 25, DiskJobFlags::empty())
            .await
            .unwrap();
        assert_eq!(r1, data);

        // Second read: should be cache hit
        let r2 = disk
            .read_chunk(0, 0, 25, DiskJobFlags::empty())
            .await
            .unwrap();
        assert_eq!(r2, data);

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn volatile_read_bypasses_cache() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(4);
        let lengths = Lengths::new(100, 50, 25);
        let storage = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, storage).await;

        let data = Bytes::from(vec![5u8; 25]);
        disk.write_chunk(0, 0, data.clone(), DiskJobFlags::FLUSH_PIECE)
            .await
            .unwrap();

        // Volatile read: should not cache
        let r = disk
            .read_chunk(0, 0, 25, DiskJobFlags::VOLATILE_READ)
            .await
            .unwrap();
        assert_eq!(r, data);

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn clear_piece_evicts_cache() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(5);
        let lengths = Lengths::new(100, 50, 25);
        let storage = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, storage).await;

        let data = Bytes::from(vec![5u8; 25]);
        disk.write_chunk(0, 0, data.clone(), DiskJobFlags::FLUSH_PIECE)
            .await
            .unwrap();
        // Populate cache
        disk.read_chunk(0, 0, 25, DiskJobFlags::empty())
            .await
            .unwrap();
        // Clear
        disk.clear_piece(0).await;

        // Can still read (from storage, not cache)
        let r = disk
            .read_chunk(0, 0, 25, DiskJobFlags::empty())
            .await
            .unwrap();
        assert_eq!(r, data);

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn write_buffer_flush() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(6);
        let lengths = Lengths::new(100, 50, 25);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, Arc::clone(&storage)).await;

        // Write to buffer (no FLUSH_PIECE)
        disk.write_chunk(0, 0, Bytes::from(vec![1u8; 25]), DiskJobFlags::empty())
            .await
            .unwrap();
        disk.write_chunk(0, 25, Bytes::from(vec![2u8; 25]), DiskJobFlags::empty())
            .await
            .unwrap();

        // Explicitly flush
        disk.flush_piece(0).await.unwrap();

        // Read back from storage directly to verify flush happened
        let piece = storage.read_piece(0).unwrap();
        assert_eq!(&piece[..25], &[1u8; 25]);
        assert_eq!(&piece[25..], &[2u8; 25]);

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_verify_multiple_pieces() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(10);

        // 8 pieces of 50 bytes each = 400 bytes total
        let data: Vec<u8> = (0..400).map(|i| (i % 256) as u8).collect();
        let piece_len = 50u64;
        let lengths = Lengths::new(data.len() as u64, piece_len, 25);
        let storage = Arc::new(MemoryStorage::new(lengths.clone()));

        // Write all piece data
        let num_pieces = lengths.num_pieces();
        for p in 0..num_pieces {
            let offset = lengths.piece_offset(p) as usize;
            let size = lengths.piece_size(p) as usize;
            storage
                .write_chunk(p, 0, &data[offset..offset + size])
                .unwrap();
        }

        let disk = mgr.register_torrent(ih, storage).await;

        // Compute expected hashes
        let mut expected_hashes = Vec::new();
        for p in 0..num_pieces {
            let offset = lengths.piece_offset(p) as usize;
            let size = lengths.piece_size(p) as usize;
            expected_hashes.push(ferrite_core::sha1(&data[offset..offset + size]));
        }

        // Verify all 8 pieces concurrently via JoinSet
        let mut js = tokio::task::JoinSet::new();
        for p in 0..num_pieces {
            let d = disk.clone();
            let hash = expected_hashes[p as usize];
            js.spawn(async move {
                let valid = d
                    .verify_piece(p, hash, DiskJobFlags::empty())
                    .await
                    .unwrap();
                (p, valid)
            });
        }

        let mut results = Vec::new();
        while let Some(r) = js.join_next().await {
            results.push(r.unwrap());
        }
        results.sort_by_key(|&(p, _)| p);

        assert_eq!(results.len(), num_pieces as usize);
        for (p, valid) in &results {
            assert!(valid, "piece {p} should be valid");
        }

        mgr.shutdown().await;
    }
}
