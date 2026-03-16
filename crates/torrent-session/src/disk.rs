use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bitflags::bitflags;
use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use torrent_core::{Id20, Id32};
use torrent_storage::TorrentStorage;
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

/// Error reported asynchronously from a non-blocking disk write.
#[derive(Debug)]
pub struct DiskWriteError {
    /// Piece index that failed to write.
    pub piece: u32,
    /// Byte offset within the piece.
    pub begin: u32,
    /// The underlying storage error.
    pub error: torrent_storage::Error,
}

/// Result of an asynchronous piece hash verification.
#[derive(Debug)]
pub struct VerifyResult {
    /// Piece index that was verified.
    pub piece: u32,
    /// Whether the piece hash matched the expected value.
    pub passed: bool,
}

/// A single block write job for the deferred writer task.
pub(crate) struct WriteJob {
    piece: u32,
    begin: u32,
    data: Bytes,
}

/// State for the per-torrent deferred write queue.
///
/// Peers enqueue writes via an MPSC channel; a dedicated writer task
/// drains the channel and calls `block_in_place(storage.write_chunk())`.
/// A per-piece pending counter + Notify allows callers to wait until
/// all writes for a piece are flushed before hash verification.
pub(crate) struct DiskWriteState {
    tx: mpsc::Sender<WriteJob>,
    /// Per-piece outstanding write count.
    pending: Mutex<HashMap<u32, u32>>,
    /// Signalled whenever any piece's pending count hits zero.
    notify: tokio::sync::Notify,
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
        reply: oneshot::Sender<torrent_storage::Result<()>>,
    },
    Read {
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        flags: DiskJobFlags,
        reply: oneshot::Sender<torrent_storage::Result<Bytes>>,
    },
    Hash {
        info_hash: Id20,
        piece: u32,
        expected: Id20,
        #[allow(dead_code)]
        flags: DiskJobFlags,
        reply: oneshot::Sender<torrent_storage::Result<bool>>,
    },
    HashV2 {
        info_hash: Id20,
        piece: u32,
        expected: Id32,
        #[allow(dead_code)]
        flags: DiskJobFlags,
        reply: oneshot::Sender<torrent_storage::Result<bool>>,
    },
    BlockHash {
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        #[allow(dead_code)]
        flags: DiskJobFlags,
        reply: oneshot::Sender<torrent_storage::Result<Id32>>,
    },

    ClearPiece {
        info_hash: Id20,
        piece: u32,
    },
    FlushWriteBuffer {
        info_hash: Id20,
        piece: u32,
        reply: oneshot::Sender<torrent_storage::Result<()>>,
    },

    CachedPieces {
        info_hash: Id20,
        reply: oneshot::Sender<Vec<u32>>,
    },

    /// Flush all buffered writes across all torrents.
    FlushAll {
        reply: oneshot::Sender<torrent_storage::Result<()>>,
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
    pub storage_mode: torrent_core::StorageMode,
    /// Total cache size in bytes (read + write). Default: 16 MiB.
    /// Deprecated: use `buffer_pool_capacity` instead.
    pub cache_size: usize,
    /// Fraction of cache_size reserved for write buffering. Default: 0.5.
    /// Deprecated: buffer pool handles write/read split implicitly.
    pub write_cache_ratio: f32,
    /// Bounded channel capacity. Default: 512.
    pub channel_capacity: usize,
    /// Unified buffer pool capacity in bytes. Default: 64 MiB.
    pub buffer_pool_capacity: usize,
    /// Lock cached piece data in physical memory. Default: true on Unix.
    pub enable_mlock: bool,
}

impl Default for DiskConfig {
    fn default() -> Self {
        DiskConfig {
            io_threads: 4,
            storage_mode: torrent_core::StorageMode::Auto,
            cache_size: 16 * 1024 * 1024,
            write_cache_ratio: 0.5,
            channel_capacity: 512,
            buffer_pool_capacity: 64 * 1024 * 1024,
            enable_mlock: cfg!(unix),
        }
    }
}

/// Disk I/O performance counters.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DiskStats {
    /// Total bytes read from disk.
    pub read_bytes: u64,
    /// Total bytes written to disk.
    pub write_bytes: u64,
    /// Number of read requests served from cache.
    pub cache_hits: u64,
    /// Number of read requests that required disk I/O.
    pub cache_misses: u64,
    /// Current size of the write buffer in bytes.
    pub write_buffer_bytes: usize,
    /// Number of pending disk I/O jobs in the queue.
    pub queued_jobs: usize,
    /// Current size of the read cache in bytes (M102).
    #[serde(default)]
    pub read_cache_bytes: usize,
    /// Total number of entries in the buffer pool (M102).
    #[serde(default)]
    pub pool_entries: usize,
    /// Number of prefetch insertions into the read cache (M102).
    #[serde(default)]
    pub prefetch_count: u64,
    /// Number of ARC evictions from the read cache (M102).
    #[serde(default)]
    pub eviction_count: u64,
    /// Number of Writing-to-Skeleton demotions (M102).
    #[serde(default)]
    pub skeleton_count: u64,
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
            read_cache_bytes: s.read_cache_bytes,
            pool_entries: s.pool_entries,
            prefetch_count: s.prefetch_count,
            eviction_count: s.eviction_count,
            skeleton_count: s.skeleton_count,
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
    /// Backend reference for per-torrent deferred writes (M100).
    backend: Arc<dyn crate::disk_backend::DiskIoBackend>,
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
        let backend_for_actor = Arc::clone(&backend);
        let actor = DiskActor::new(rx, config, backend_for_actor);
        let join = tokio::spawn(actor.run());
        (DiskManagerHandle { tx, backend }, join)
    }

    /// Register a torrent's storage with the disk subsystem and return a
    /// per-torrent `DiskHandle`.
    pub async fn register_torrent(
        &self,
        info_hash: Id20,
        storage: Arc<dyn TorrentStorage>,
    ) -> DiskHandle {
        // Clone storage: one for the DiskJob::Register, one for the DiskHandle.
        let storage_for_handle = Arc::clone(&storage);

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

        // Create the deferred write queue (M100).
        let (write_tx, mut write_rx) = mpsc::channel::<WriteJob>(512);
        let write_state = Arc::new(DiskWriteState {
            tx: write_tx,
            pending: Mutex::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
        });

        // Spawn the per-torrent writer task.
        let writer_storage = Arc::clone(&storage_for_handle);
        let writer_state = Arc::clone(&write_state);
        tokio::spawn(async move {
            while let Some(first) = write_rx.recv().await {
                // Drain up to 64 jobs into a batch, then execute all in a
                // single spawn_blocking call to reduce thread pool overhead.
                let mut batch = vec![first];
                while batch.len() < 64 {
                    match write_rx.try_recv() {
                        Ok(job) => batch.push(job),
                        Err(_) => break,
                    }
                }

                // Collect piece indices before moving batch into the closure.
                let pieces: Vec<u32> = batch.iter().map(|j| j.piece).collect();

                // Use spawn_blocking (not block_in_place) because this task may
                // run on a current_thread runtime in tests. The hot-path fallback
                // in write_block_deferred uses block_in_place on worker threads.
                let ws = Arc::clone(&writer_storage);
                let _ = tokio::task::spawn_blocking(move || {
                    for WriteJob { piece, begin, data } in &batch {
                        if let Err(e) = ws.write_chunk(*piece, *begin, data) {
                            tracing::warn!(piece, begin, %e, "deferred write failed");
                        }
                    }
                })
                .await;

                // Decrement pending counts for all jobs in batch, notify once.
                {
                    let mut pending = writer_state.pending.lock().expect("pending lock poisoned");
                    for piece in &pieces {
                        if let Some(count) = pending.get_mut(piece) {
                            *count = count.saturating_sub(1);
                            if *count == 0 {
                                pending.remove(piece);
                            }
                        }
                    }
                }
                writer_state.notify.notify_waiters();
            }
        });

        DiskHandle {
            tx: self.tx.clone(),
            info_hash,
            hash_pool: None,
            hash_result_tx: None,
            storage: Some(storage_for_handle),
            backend: Some(Arc::clone(&self.backend)),
            write_state: Some(write_state),
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
    /// Hash pool for parallel piece verification (M96).
    hash_pool: Option<std::sync::Arc<crate::hash_pool::HashPool>>,
    /// Per-torrent hash result sender (M96).
    hash_result_tx: Option<tokio::sync::mpsc::Sender<crate::hash_pool::HashResult>>,
    /// Direct storage reference for deferred writes (M100).
    storage: Option<Arc<dyn TorrentStorage>>,
    /// Backend reference for disk-based verify (M100).
    backend: Option<Arc<dyn crate::disk_backend::DiskIoBackend>>,
    /// Deferred write queue state (M100).
    write_state: Option<Arc<DiskWriteState>>,
}

impl std::fmt::Debug for DiskHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskHandle")
            .field("info_hash", &self.info_hash)
            .finish_non_exhaustive()
    }
}

impl DiskHandle {
    /// Create a DiskHandle from raw parts (for internal/test use).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(tx: mpsc::Sender<DiskJob>, info_hash: Id20) -> Self {
        Self {
            tx,
            info_hash,
            hash_pool: None,
            hash_result_tx: None,
            storage: None,
            backend: None,
            write_state: None,
        }
    }

    /// Set the hash pool reference (M96).
    pub fn set_hash_pool(&mut self, pool: std::sync::Arc<crate::hash_pool::HashPool>) {
        self.hash_pool = Some(pool);
    }

    /// Set the per-torrent hash result sender (M96).
    pub fn set_hash_result_tx(
        &mut self,
        tx: tokio::sync::mpsc::Sender<crate::hash_pool::HashResult>,
    ) {
        self.hash_result_tx = Some(tx);
    }

    /// Write a chunk to disk (may be buffered).
    pub async fn write_chunk(
        &self,
        piece: u32,
        begin: u32,
        data: Bytes,
        flags: DiskJobFlags,
    ) -> torrent_storage::Result<()> {
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
        rx.await
            .unwrap_or(Err(torrent_storage::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "disk actor gone",
            ))))
    }

    /// Read a chunk from disk (may hit cache or write buffer).
    pub async fn read_chunk(
        &self,
        piece: u32,
        begin: u32,
        length: u32,
        flags: DiskJobFlags,
    ) -> torrent_storage::Result<Bytes> {
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
        rx.await
            .unwrap_or(Err(torrent_storage::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "disk actor gone",
            ))))
    }

    /// Verify a piece hash against an expected value.
    pub async fn verify_piece(
        &self,
        piece: u32,
        expected: Id20,
        flags: DiskJobFlags,
    ) -> torrent_storage::Result<bool> {
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
        rx.await
            .unwrap_or(Err(torrent_storage::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "disk actor gone",
            ))))
    }

    /// Verify a piece hash against an expected SHA-256 value (v2).
    pub async fn verify_piece_v2(
        &self,
        piece: u32,
        expected: Id32,
        flags: DiskJobFlags,
    ) -> torrent_storage::Result<bool> {
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
        rx.await
            .unwrap_or(Err(torrent_storage::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "disk actor gone",
            ))))
    }

    /// Hash a single block with SHA-256 for Merkle verification (v2).
    pub async fn hash_block(
        &self,
        piece: u32,
        begin: u32,
        length: u32,
        flags: DiskJobFlags,
    ) -> torrent_storage::Result<Id32> {
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
        rx.await
            .unwrap_or(Err(torrent_storage::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "disk actor gone",
            ))))
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
    pub async fn flush_piece(&self, piece: u32) -> torrent_storage::Result<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::FlushWriteBuffer {
                info_hash: self.info_hash,
                piece,
                reply: tx,
            })
            .await;
        rx.await
            .unwrap_or(Err(torrent_storage::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "disk actor gone",
            ))))
    }

    /// Query which pieces are currently in the read cache for this torrent.
    pub async fn cached_pieces(&self) -> Vec<u32> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::CachedPieces {
                info_hash: self.info_hash,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or_default()
    }

    /// Flush all buffered writes to persistent storage.
    pub async fn flush_cache(&self) -> torrent_storage::Result<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(DiskJob::FlushAll { reply: tx }).await;
        rx.await
            .unwrap_or(Err(torrent_storage::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "disk actor gone",
            ))))
    }

    /// Spawn a non-blocking v1 piece hash verification.
    ///
    /// M96: If a hash pool is configured, submits the job to the pool.
    /// M101: Uses `HashJob::Streaming` to delegate reading + hashing to the
    /// backend, eliminating the full-piece allocation on the caller side.
    ///
    /// The `generation` parameter enables staleness detection by the caller.
    pub fn enqueue_verify(
        &self,
        piece: u32,
        expected: Id20,
        generation: u64,
        result_tx: &mpsc::Sender<VerifyResult>,
    ) {
        // M96/M101: If hash pool is available, submit streaming job.
        if let (Some(pool), Some(hash_tx)) = (&self.hash_pool, &self.hash_result_tx) {
            if let Some(backend) = &self.backend {
                let pool = pool.clone();
                let hash_tx = hash_tx.clone();
                let backend = Arc::clone(backend);
                let info_hash = self.info_hash;
                let job = crate::hash_pool::HashJob::Streaming {
                    piece,
                    expected,
                    generation,
                    info_hash,
                    backend,
                    result_tx: hash_tx,
                };
                tokio::spawn(async move {
                    if pool.submit(job).await.is_err() {
                        tracing::warn!(piece, "hash pool shut down, treating as failed");
                    }
                });
                return;
            }

            // No backend — send failure.
            let hash_tx = hash_tx.clone();
            tokio::spawn(async move {
                tracing::warn!(piece, "verify: no backend (hash pool path)");
                let _ = hash_tx
                    .send(crate::hash_pool::HashResult {
                        piece,
                        passed: false,
                        generation,
                    })
                    .await;
            });
            return;
        }

        // Non-pool path: delegate to backend.hash_piece().
        if let Some(backend) = &self.backend {
            let backend = Arc::clone(backend);
            let info_hash = self.info_hash;
            let result_tx = result_tx.clone();
            tokio::spawn(async move {
                let passed = tokio::task::spawn_blocking(move || {
                    backend.hash_piece(info_hash, piece, &expected).unwrap_or_else(|e| {
                        warn!(piece, %e, "verify: hash_piece failed");
                        false
                    })
                })
                .await
                .expect("hash_piece task panicked");
                let _ = result_tx.send(VerifyResult { piece, passed }).await;
            });
            return;
        }

        // No data source at all — treat as failure.
        let result_tx = result_tx.clone();
        tokio::spawn(async move {
            warn!(piece, "verify: no data source, treating as failed");
            let _ = result_tx
                .send(VerifyResult {
                    piece,
                    passed: false,
                })
                .await;
        });
    }

    /// Spawn a non-blocking v2 piece hash verification (SHA-256).
    ///
    /// Reads the piece from disk via the backend's `read_piece()` method.
    pub fn enqueue_verify_v2(
        &self,
        piece: u32,
        expected: Id32,
        result_tx: &mpsc::Sender<VerifyResult>,
    ) {
        if let Some(backend) = &self.backend {
            let backend = Arc::clone(backend);
            let info_hash = self.info_hash;
            let result_tx = result_tx.clone();
            tokio::spawn(async move {
                let passed = tokio::task::spawn_blocking(move || {
                    match backend.read_piece(info_hash, piece) {
                        Ok(data) => {
                            let actual = torrent_core::sha256(&data);
                            actual == expected
                        }
                        Err(e) => {
                            warn!(piece, %e, "verify v2: read_piece failed");
                            false
                        }
                    }
                })
                .await
                .expect("read_piece v2 task panicked");
                let _ = result_tx.send(VerifyResult { piece, passed }).await;
            });
            return;
        }

        // No backend — treat as failure.
        let result_tx = result_tx.clone();
        tokio::spawn(async move {
            warn!(piece, "verify v2: no data source, treating as failed");
            let _ = result_tx
                .send(VerifyResult {
                    piece,
                    passed: false,
                })
                .await;
        });
    }

    /// Enqueue a block write via the deferred writer task (M100).
    ///
    /// The write is sent to a dedicated per-torrent writer task that calls
    /// `block_in_place(storage.write_chunk())`. If the channel is full, falls
    /// back to a synchronous `block_in_place` write from the calling task.
    ///
    /// Returns early (no-op) if write_state is `None` (pre-M100 code path).
    pub(crate) fn write_block_deferred(&self, piece: u32, begin: u32, data: Bytes) {
        let (write_state, storage) = match (&self.write_state, &self.storage) {
            (Some(ws), Some(s)) => (ws, s),
            _ => return, // pre-M100 path
        };

        // Increment pending count before sending.
        {
            let mut pending = write_state.pending.lock().expect("pending lock poisoned");
            *pending.entry(piece).or_insert(0) += 1;
        }

        match write_state.tx.try_send(WriteJob {
            piece,
            begin,
            data: data.clone(),
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Channel full: write synchronously to avoid unbounded backlog.
                let storage = Arc::clone(storage);
                tokio::task::block_in_place(|| {
                    if let Err(e) = storage.write_chunk(piece, begin, &data) {
                        tracing::warn!(piece, begin, %e, "deferred write fallback failed");
                    }
                });
                // Decrement pending + notify.
                let mut pending = write_state.pending.lock().expect("pending lock poisoned");
                if let Some(count) = pending.get_mut(&piece) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        pending.remove(&piece);
                        drop(pending);
                        write_state.notify.notify_waiters();
                    }
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer task gone — decrement pending to avoid stuck waiters.
                let mut pending = write_state.pending.lock().expect("pending lock poisoned");
                if let Some(count) = pending.get_mut(&piece) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        pending.remove(&piece);
                        drop(pending);
                        write_state.notify.notify_waiters();
                    }
                }
            }
        }
    }

    /// Wait until all deferred writes for `piece` have been flushed to storage.
    ///
    /// Returns immediately if write_state is `None` (pre-M100 path) or if
    /// there are no pending writes for the given piece.
    pub(crate) async fn flush_piece_writes(&self, piece: u32) {
        let write_state = match &self.write_state {
            Some(ws) => ws,
            None => return,
        };

        loop {
            {
                let pending = write_state.pending.lock().expect("pending lock poisoned");
                if !pending.contains_key(&piece) {
                    return;
                }
            }
            write_state.notify.notified().await;
        }
    }

    /// Direct storage reference (M100).
    #[allow(dead_code)]
    pub(crate) fn storage(&self) -> Option<Arc<dyn TorrentStorage>> {
        self.storage.clone()
    }
}

// ---------------------------------------------------------------------------
// DiskActor — dispatcher loop (all I/O runs on tokio's blocking thread pool)
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
                    let flush_result =
                        tokio::task::spawn_blocking(move || backend.flush_all()).await;
                    if let Ok(Err(e)) = flush_result {
                        warn!("flush_all on shutdown failed: {e}");
                    }
                    let _ = reply.send(());
                    return;
                }
                self.dispatch_job(job);
            }
        }
    }

    /// Dispatch a job for execution. Fast metadata ops run inline;
    /// all I/O ops are spawned as fire-and-forget tasks bounded by the
    /// semaphore, so the dispatcher loop never blocks on disk operations.
    fn dispatch_job(&self, job: DiskJob) {
        match job {
            // --- Fast metadata ops (inline) ---
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
            DiskJob::ClearPiece { info_hash, piece } => {
                self.backend.clear_piece(info_hash, piece);
            }
            DiskJob::CachedPieces { info_hash, reply } => {
                let pieces = self.backend.cached_pieces(info_hash);
                let _ = reply.send(pieces);
            }

            // --- Synchronous write (caller awaits reply) ---
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
                let semaphore = self.semaphore.clone();
                tokio::spawn(async move {
                    let permit = semaphore.acquire_owned().await.unwrap();
                    let result = tokio::task::spawn_blocking(move || {
                        backend.write_chunk(info_hash, piece, begin, data, flush)
                    })
                    .await
                    .unwrap();
                    drop(permit);
                    let _ = reply.send(to_storage_result(result));
                });
            }

            // --- Read ---
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
                let semaphore = self.semaphore.clone();
                tokio::spawn(async move {
                    let permit = semaphore.acquire_owned().await.unwrap();
                    let result = tokio::task::spawn_blocking(move || {
                        backend.read_chunk(info_hash, piece, begin, length, volatile)
                    })
                    .await
                    .unwrap();
                    drop(permit);
                    let _ = reply.send(to_storage_result(result));
                });
            }

            // --- Synchronous hash (v1) ---
            DiskJob::Hash {
                info_hash,
                piece,
                expected,
                reply,
                ..
            } => {
                let backend = Arc::clone(&self.backend);
                let semaphore = self.semaphore.clone();
                tokio::spawn(async move {
                    let permit = semaphore.acquire_owned().await.unwrap();
                    let result = tokio::task::spawn_blocking(move || {
                        backend.hash_piece(info_hash, piece, &expected)
                    })
                    .await
                    .unwrap();
                    drop(permit);
                    let _ = reply.send(to_storage_result(result));
                });
            }

            // --- Synchronous hash (v2) ---
            DiskJob::HashV2 {
                info_hash,
                piece,
                expected,
                reply,
                ..
            } => {
                let backend = Arc::clone(&self.backend);
                let semaphore = self.semaphore.clone();
                tokio::spawn(async move {
                    let permit = semaphore.acquire_owned().await.unwrap();
                    let result = tokio::task::spawn_blocking(move || {
                        backend.hash_piece_v2(info_hash, piece, &expected)
                    })
                    .await
                    .unwrap();
                    drop(permit);
                    let _ = reply.send(to_storage_result(result));
                });
            }

            // --- Block hash (v2 Merkle) ---
            DiskJob::BlockHash {
                info_hash,
                piece,
                begin,
                length,
                reply,
                ..
            } => {
                let backend = Arc::clone(&self.backend);
                let semaphore = self.semaphore.clone();
                tokio::spawn(async move {
                    let permit = semaphore.acquire_owned().await.unwrap();
                    let result = tokio::task::spawn_blocking(move || {
                        backend.hash_block(info_hash, piece, begin, length)
                    })
                    .await
                    .unwrap();
                    drop(permit);
                    let _ = reply.send(to_storage_result(result));
                });
            }

            // --- Flush piece ---
            DiskJob::FlushWriteBuffer {
                info_hash,
                piece,
                reply,
            } => {
                let backend = Arc::clone(&self.backend);
                let semaphore = self.semaphore.clone();
                tokio::spawn(async move {
                    let permit = semaphore.acquire_owned().await.unwrap();
                    let result =
                        tokio::task::spawn_blocking(move || backend.flush_piece(info_hash, piece))
                            .await
                            .unwrap();
                    drop(permit);
                    let _ = reply.send(to_storage_result(result));
                });
            }

            // --- Flush all ---
            DiskJob::FlushAll { reply } => {
                let backend = Arc::clone(&self.backend);
                let semaphore = self.semaphore.clone();
                tokio::spawn(async move {
                    let permit = semaphore.acquire_owned().await.unwrap();
                    let result = tokio::task::spawn_blocking(move || backend.flush_all())
                        .await
                        .unwrap();
                    drop(permit);
                    let _ = reply.send(to_storage_result(result));
                });
            }

            DiskJob::Shutdown { .. } => unreachable!(),
        }
    }
}

/// Convert `crate::Result<T>` to `torrent_storage::Result<T>` for reply channels.
fn to_storage_result<T>(r: crate::Result<T>) -> torrent_storage::Result<T> {
    r.map_err(|e| match e {
        crate::Error::Storage(se) => se,
        other => torrent_storage::Error::Io(std::io::Error::other(other.to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::Lengths;
    use torrent_storage::MemoryStorage;

    // ── DiskActor integration tests ──────────────────────────────────

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
        disk.write_chunk(
            0,
            0,
            Bytes::from(piece_data.clone()),
            DiskJobFlags::FLUSH_PIECE,
        )
        .await
        .unwrap();
        disk.write_chunk(0, 25, Bytes::from(vec![9u8; 25]), DiskJobFlags::FLUSH_PIECE)
            .await
            .unwrap();

        let expected = torrent_core::sha1(&piece_data);
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
        let expected = torrent_core::sha256(&data);
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
        let hash = disk.hash_block(0, 0, 16384, DiskJobFlags::empty()).await;
        assert_eq!(hash.unwrap(), torrent_core::sha256(&data));
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
            expected_hashes.push(torrent_core::sha1(&data[offset..offset + size]));
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

    // ── Deferred write queue tests (M100) ────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_block_deferred_writes_to_storage() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(30);
        let lengths = Lengths::new(100, 50, 25);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, Arc::clone(&storage)).await;

        let block0 = Bytes::from(vec![0xAAu8; 25]);
        let block1 = Bytes::from(vec![0xBBu8; 25]);

        disk.write_block_deferred(0, 0, block0.clone());
        disk.write_block_deferred(0, 25, block1.clone());

        // Wait for all writes to piece 0 to flush.
        disk.flush_piece_writes(0).await;

        // Read back from storage to verify data landed on disk.
        let read0 = storage.read_chunk(0, 0, 25).unwrap();
        assert_eq!(&read0[..], &block0[..], "block 0 should match");
        let read1 = storage.read_chunk(0, 25, 25).unwrap();
        assert_eq!(&read1[..], &block1[..], "block 1 should match");

        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_piece_writes_waits_for_completion() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(31);
        let lengths = Lengths::new(200, 100, 25);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, Arc::clone(&storage)).await;

        // Enqueue 4 blocks for piece 0.
        for i in 0u32..4 {
            let data = Bytes::from(vec![(i as u8) + 1; 25]);
            disk.write_block_deferred(0, i * 25, data);
        }

        // flush_piece_writes must block until all 4 writes complete.
        disk.flush_piece_writes(0).await;

        // Verify all blocks are visible on storage.
        let piece = storage.read_piece(0).unwrap();
        assert_eq!(&piece[0..25], &[1u8; 25]);
        assert_eq!(&piece[25..50], &[2u8; 25]);
        assert_eq!(&piece[50..75], &[3u8; 25]);
        assert_eq!(&piece[75..100], &[4u8; 25]);

        mgr.shutdown().await;
    }

    // ── M101: Batch writer tests ──────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_writer_drains_multiple_jobs() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(50);
        // 10 blocks of 25 bytes each across a single piece of 250 bytes.
        let lengths = Lengths::new(250, 250, 25);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, Arc::clone(&storage)).await;

        // Enqueue 10 writes (well within the 64-job batch cap).
        for i in 0u32..10 {
            let data = Bytes::from(vec![i as u8 + 1; 25]);
            disk.write_block_deferred(0, i * 25, data);
        }

        // Wait for all writes to piece 0 to flush.
        disk.flush_piece_writes(0).await;

        // Verify all 10 blocks landed correctly.
        for i in 0u32..10 {
            let chunk = storage.read_chunk(0, i * 25, 25).unwrap();
            assert_eq!(
                &chunk[..],
                vec![i as u8 + 1; 25].as_slice(),
                "block {i} mismatch"
            );
        }

        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_writer_caps_at_64() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(51);
        // 100 blocks of 16 bytes each across a single piece of 1600 bytes.
        let lengths = Lengths::new(1600, 1600, 16);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, Arc::clone(&storage)).await;

        // Enqueue 100 writes — more than the 64-job batch cap, so at least
        // two batches are needed to drain the channel.
        for i in 0u32..100 {
            let data = Bytes::from(vec![i as u8; 16]);
            disk.write_block_deferred(0, i * 16, data);
        }

        // Wait for all 100 writes to complete (requires multiple batches).
        disk.flush_piece_writes(0).await;

        // Verify every block landed correctly.
        for i in 0u32..100 {
            let chunk = storage.read_chunk(0, i * 16, 16).unwrap();
            assert_eq!(
                &chunk[..],
                vec![i as u8; 16].as_slice(),
                "block {i} mismatch after overflow to next batch"
            );
        }

        mgr.shutdown().await;
    }

    // ── M100: Disk-based verify tests ────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_from_disk_after_deferred_write() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(40);
        let chunk_size = 16384u32;
        let piece_size = u64::from(chunk_size) * 2;
        let lengths = Lengths::new(piece_size, piece_size, chunk_size);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, Arc::clone(&storage)).await;

        // Write both chunks via deferred queue.
        let chunk0 = vec![0xAAu8; chunk_size as usize];
        let chunk1 = vec![0xBBu8; chunk_size as usize];
        disk.write_block_deferred(0, 0, Bytes::from(chunk0.clone()));
        disk.write_block_deferred(0, chunk_size, Bytes::from(chunk1.clone()));
        disk.flush_piece_writes(0).await;

        // Compute expected SHA-1 hash.
        let mut full_piece = Vec::with_capacity(piece_size as usize);
        full_piece.extend_from_slice(&chunk0);
        full_piece.extend_from_slice(&chunk1);
        let expected_hash = torrent_core::sha1(&full_piece);

        // Verify via disk-read path.
        let (result_tx, mut result_rx) = mpsc::channel(4);
        disk.enqueue_verify(0, expected_hash, 0, &result_tx);
        let result = result_rx
            .recv()
            .await
            .expect("should receive verify result");
        assert_eq!(result.piece, 0);
        assert!(result.passed, "disk-based SHA-1 verify should pass");

        // Wrong hash should fail.
        disk.write_block_deferred(0, 0, Bytes::from(chunk0));
        disk.write_block_deferred(0, chunk_size, Bytes::from(chunk1));
        disk.flush_piece_writes(0).await;
        disk.enqueue_verify(0, Id20::ZERO, 0, &result_tx);
        let result = result_rx
            .recv()
            .await
            .expect("should receive verify result");
        assert!(!result.passed, "wrong hash should fail disk-based verify");

        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_v2_from_disk_after_deferred_write() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(41);
        let chunk_size = 16384u32;
        let piece_size = u64::from(chunk_size) * 2;
        let lengths = Lengths::new(piece_size, piece_size, chunk_size);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let disk = mgr.register_torrent(ih, Arc::clone(&storage)).await;

        // Write both chunks via deferred queue.
        let chunk0 = vec![0xCCu8; chunk_size as usize];
        let chunk1 = vec![0xDDu8; chunk_size as usize];
        disk.write_block_deferred(0, 0, Bytes::from(chunk0.clone()));
        disk.write_block_deferred(0, chunk_size, Bytes::from(chunk1.clone()));
        disk.flush_piece_writes(0).await;

        // Compute expected SHA-256 hash.
        let mut full_piece = Vec::with_capacity(piece_size as usize);
        full_piece.extend_from_slice(&chunk0);
        full_piece.extend_from_slice(&chunk1);
        let expected_hash = torrent_core::sha256(&full_piece);

        // Verify via disk-read path.
        let (result_tx, mut result_rx) = mpsc::channel(4);
        disk.enqueue_verify_v2(0, expected_hash, &result_tx);
        let result = result_rx
            .recv()
            .await
            .expect("should receive v2 verify result");
        assert_eq!(result.piece, 0);
        assert!(result.passed, "disk-based SHA-256 verify should pass");

        // Wrong hash should fail.
        disk.write_block_deferred(0, 0, Bytes::from(chunk0));
        disk.write_block_deferred(0, chunk_size, Bytes::from(chunk1));
        disk.flush_piece_writes(0).await;
        disk.enqueue_verify_v2(0, Id32::ZERO, &result_tx);
        let result = result_rx
            .recv()
            .await
            .expect("should receive v2 verify result");
        assert!(
            !result.passed,
            "wrong hash should fail disk-based v2 verify"
        );

        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_with_hash_pool_from_disk() {
        let (mgr, _actor) = DiskManagerHandle::new(test_config());
        let ih = make_hash(42);
        let chunk_size = 16384u32;
        let piece_size = u64::from(chunk_size) * 2;
        let lengths = Lengths::new(piece_size, piece_size, chunk_size);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let mut disk = mgr.register_torrent(ih, Arc::clone(&storage)).await;

        // Configure hash pool.
        let (hash_result_tx, mut hash_result_rx) = mpsc::channel(4);
        disk.set_hash_result_tx(hash_result_tx);
        let hash_pool = std::sync::Arc::new(crate::hash_pool::HashPool::new(2, 16));
        disk.set_hash_pool(hash_pool);

        // Write both chunks via deferred queue.
        let chunk0 = vec![0xEEu8; chunk_size as usize];
        let chunk1 = vec![0xFFu8; chunk_size as usize];
        disk.write_block_deferred(0, 0, Bytes::from(chunk0.clone()));
        disk.write_block_deferred(0, chunk_size, Bytes::from(chunk1.clone()));
        disk.flush_piece_writes(0).await;

        // Compute expected SHA-1 hash.
        let mut full_piece = Vec::with_capacity(piece_size as usize);
        full_piece.extend_from_slice(&chunk0);
        full_piece.extend_from_slice(&chunk1);
        let expected_hash = torrent_core::sha1(&full_piece);

        // Verify via hash pool path (reads from disk, submits to pool).
        let (verify_result_tx, _) = mpsc::channel(4); // not used for pool path
        disk.enqueue_verify(0, expected_hash, 42, &verify_result_tx);
        let result = hash_result_rx
            .recv()
            .await
            .expect("should receive hash pool result");
        assert!(result.passed, "hash pool disk-based verify should pass");
        assert_eq!(result.piece, 0);
        assert_eq!(result.generation, 42);

        mgr.shutdown().await;
    }
}
