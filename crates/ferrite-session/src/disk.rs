use std::path::PathBuf;
use std::sync::Arc;

use bitflags::bitflags;
use bytes::Bytes;
use ferrite_core::{Id20, Id32};
use ferrite_storage::TorrentStorage;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

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
    HashV2 {
        info_hash: Id20,
        piece: u32,
        expected: Id32,
        #[allow(dead_code)]
        flags: DiskJobFlags,
        reply: oneshot::Sender<ferrite_storage::Result<bool>>,
    },
    BlockHash {
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        #[allow(dead_code)]
        flags: DiskJobFlags,
        reply: oneshot::Sender<ferrite_storage::Result<Id32>>,
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
        info_hash: Id20,
        new_path: PathBuf,
        reply: oneshot::Sender<ferrite_storage::Result<()>>,
    },

    CachedPieces {
        info_hash: Id20,
        reply: oneshot::Sender<Vec<u32>>,
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

// ---------------------------------------------------------------------------
// DiskManagerHandle — session-level handle
// ---------------------------------------------------------------------------

/// Session-level handle for managing the disk subsystem.
#[derive(Clone)]
pub struct DiskManagerHandle {
    tx: mpsc::Sender<DiskJob>,
}

impl DiskManagerHandle {
    /// Create a new disk manager with the default backend selected from config.
    /// Returns the handle and a `JoinHandle` for the background actor task.
    pub fn new(config: DiskConfig) -> (Self, tokio::task::JoinHandle<()>) {
        let backend = crate::disk_backend::create_backend_from_config(&config);
        Self::new_with_backend(config, backend)
    }

    /// Create a new disk manager with a custom disk I/O backend.
    /// Returns the handle and a `JoinHandle` for the background actor task.
    pub fn new_with_backend(
        config: DiskConfig,
        backend: Arc<dyn crate::disk_backend::DiskIoBackend>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let actor = DiskActor::new(rx, config, backend);
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
    /// Create a DiskHandle from raw parts (for internal/test use).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(tx: mpsc::Sender<DiskJob>, info_hash: Id20) -> Self {
        Self { tx, info_hash }
    }

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

    /// Verify a piece hash against an expected SHA-256 value (v2).
    pub async fn verify_piece_v2(
        &self,
        piece: u32,
        expected: Id32,
        flags: DiskJobFlags,
    ) -> ferrite_storage::Result<bool> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::HashV2 {
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

    /// Hash a single block with SHA-256 for Merkle verification (v2).
    pub async fn hash_block(
        &self,
        piece: u32,
        begin: u32,
        length: u32,
        flags: DiskJobFlags,
    ) -> ferrite_storage::Result<Id32> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::BlockHash {
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

    /// Query which pieces are currently in the read cache for this torrent.
    pub async fn cached_pieces(&self) -> Vec<u32> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(DiskJob::CachedPieces {
            info_hash: self.info_hash,
            reply: tx,
        }).await;
        rx.await.unwrap_or_default()
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
    backend: Arc<dyn crate::disk_backend::DiskIoBackend>,
    semaphore: Arc<tokio::sync::Semaphore>,
    #[allow(dead_code)]
    config: DiskConfig,
}

impl DiskActor {
    fn new(
        rx: mpsc::Receiver<DiskJob>,
        config: DiskConfig,
        backend: Arc<dyn crate::disk_backend::DiskIoBackend>,
    ) -> Self {
        DiskActor {
            rx,
            backend,
            semaphore: Arc::new(tokio::sync::Semaphore::new(config.io_threads)),
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
                    // Flush on blocking thread to avoid stalling tokio runtime.
                    let backend = Arc::clone(&self.backend);
                    let flush_result = tokio::task::spawn_blocking(move || {
                        backend.flush_all()
                    }).await;
                    if let Ok(Err(e)) = flush_result {
                        warn!("flush_all on shutdown failed: {e}");
                    }
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
                let flush = flags.contains(DiskJobFlags::FLUSH_PIECE);
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.write_chunk(info_hash, piece, begin, &data, flush)
                })
                .await
                .unwrap();
                drop(permit);
                let _ = reply.send(to_storage_result(result));
            }
            DiskJob::Read {
                info_hash,
                piece,
                begin,
                length,
                flags,
                reply,
            } => {
                let volatile = flags.contains(DiskJobFlags::VOLATILE_READ);
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.read_chunk(info_hash, piece, begin, length, volatile)
                })
                .await
                .unwrap();
                drop(permit);
                let _ = reply.send(to_storage_result(result));
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
                let _ = reply.send(to_storage_result(result));
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
                let _ = reply.send(to_storage_result(result));
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
                let _ = reply.send(to_storage_result(result));
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
                let _ = reply.send(to_storage_result(result));
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
                let _ = reply.send(to_storage_result(result));
            }
            DiskJob::CachedPieces { info_hash, reply } => {
                let pieces = self.backend.cached_pieces(info_hash);
                let _ = reply.send(pieces);
            }
            DiskJob::Shutdown { .. } => unreachable!(),
        }
    }
}

/// Convert `crate::Result<T>` to `ferrite_storage::Result<T>` for reply channels.
fn to_storage_result<T>(r: crate::Result<T>) -> ferrite_storage::Result<T> {
    r.map_err(|e| match e {
        crate::Error::Storage(se) => se,
        other => ferrite_storage::Error::Io(std::io::Error::other(other.to_string())),
    })
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
    async fn verify_piece_v2_via_disk_handle() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(11);
        let data = vec![0xABu8; 16384];
        let expected = ferrite_core::sha256(&data);
        let lengths = Lengths::new(16384, 16384, 16384);
        let storage = Arc::new(MemoryStorage::new(lengths));
        storage.write_chunk(0, 0, &data).unwrap();

        let disk = mgr.register_torrent(ih, storage).await;
        let result = disk
            .verify_piece_v2(0, expected, DiskJobFlags::empty())
            .await;
        assert!(result.unwrap());
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn hash_block_via_disk_handle() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(12);
        let data = vec![0xCDu8; 16384];
        let lengths = Lengths::new(16384, 16384, 16384);
        let storage = Arc::new(MemoryStorage::new(lengths));
        storage.write_chunk(0, 0, &data).unwrap();

        let disk = mgr.register_torrent(ih, storage).await;
        let hash = disk
            .hash_block(0, 0, 16384, DiskJobFlags::empty())
            .await;
        assert_eq!(hash.unwrap(), ferrite_core::sha256(&data));
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
