use std::path::PathBuf;
use std::sync::Arc;

use bitflags::bitflags;
use bytes::Bytes;
use ferrite_core::Id20;
use ferrite_storage::TorrentStorage;
use tokio::sync::oneshot;

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

#[allow(dead_code)] // consumed by DiskActor in a later commit
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
        info_hash: Id20,
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
#[derive(Debug, Clone, Default)]
pub struct DiskStats {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub write_buffer_bytes: usize,
    pub queued_jobs: usize,
}
