//! io_uring disk I/O backend -- wraps [`PosixDiskIo`], overrides `write_block_direct()`.
//!
//! Uses a single shared `Mutex<IoUring>` ring per session. Writes are submitted
//! synchronously within `block_in_place` context -- NOT async-integrated with tokio.
//!
//! The entire module is gated with `#[cfg(all(target_os = "linux", feature = "io-uring"))]`
//! at the `mod` declaration site in `lib.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use io_uring::{opcode, types, IoUring};
use parking_lot::{Mutex, RwLock};
use smallvec::SmallVec;
use torrent_core::{Id20, Id32};
use torrent_storage::{FileMap, IoUringConfig, IoUringStorageState, TorrentStorage};
use tracing::warn;

use crate::disk::DiskConfig;
use crate::disk_backend::{DiskIoBackend, DiskIoStats, PosixDiskIo};

/// Per-torrent io_uring write state: pre-opened fds + file map.
pub(crate) struct IoUringWriteState {
    fds: IoUringStorageState,
    file_map: FileMap,
}

/// io_uring disk I/O backend.
///
/// Wraps [`PosixDiskIo`] for reads, cache, and hashing. Only overrides
/// `write_block_direct()` to submit writes via io_uring instead of pwritev.
///
/// Falls back to the inner [`PosixDiskIo`] when:
/// - Storage does not implement `filesystem_info()` (memory, mmap backends)
/// - io_uring fd opening fails at registration time
/// - An io_uring submission fails at write time
pub(crate) struct IoUringDiskIo {
    inner: PosixDiskIo,
    ring: Mutex<IoUring>,
    pub(crate) uring_states: RwLock<HashMap<Id20, IoUringWriteState>>,
    config: IoUringConfig,
    /// Cumulative bytes written via the io_uring path (lock-free stat tracking).
    uring_write_bytes: AtomicU64,
}

impl IoUringDiskIo {
    /// Create a new io_uring backend wrapping [`PosixDiskIo`].
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the kernel rejects io_uring setup (e.g. on
    /// older kernels or when the `io_uring_setup` syscall is disabled).
    pub fn new(disk_config: &DiskConfig, uring_config: IoUringConfig) -> std::io::Result<Self> {
        let ring = IoUring::new(uring_config.sq_depth)?;
        Ok(Self {
            inner: PosixDiskIo::new(disk_config),
            ring: Mutex::new(ring),
            uring_states: RwLock::new(HashMap::new()),
            config: uring_config,
            uring_write_bytes: AtomicU64::new(0),
        })
    }

    /// Submit vectored writes via io_uring for a single block.
    ///
    /// Maps (piece, begin, s0, s1) to file segments via [`FileMap`], builds
    /// `Writev` SQEs for each segment, submits, and reaps completions.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if ring submission fails, a CQE reports an error,
    /// or if fewer completions arrive than expected.
    fn uring_write(
        &self,
        state: &IoUringWriteState,
        piece: u32,
        begin: u32,
        s0: &[u8],
        s1: &[u8],
    ) -> crate::Result<()> {
        let total_len = s0.len().saturating_add(s1.len());
        // Avoid overflow when converting to u32 for chunk_segments.
        let total_u32 = u32::try_from(total_len).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "block length exceeds u32::MAX",
            )
        })?;
        let segments = state.file_map.chunk_segments(piece, begin, total_u32);
        let num_segments = segments.len();

        if num_segments == 0 {
            return Ok(());
        }

        // Build iovecs for each segment, respecting the s0/s1 straddle boundary.
        // Each segment gets its own SmallVec<[libc::iovec; 2]> -- at most 2 iovecs
        // when the segment straddles the s0/s1 boundary.
        //
        // All iovec arrays must remain alive until submit_and_wait completes.
        let mut all_iovecs: Vec<SmallVec<[libc::iovec; 2]>> = Vec::with_capacity(num_segments);

        let mut pos: usize = 0;
        for seg in &segments {
            let seg_len = seg.len as usize;
            let seg_end = pos.saturating_add(seg_len);
            let mut iovecs: SmallVec<[libc::iovec; 2]> = SmallVec::new();

            if seg_end <= s0.len() {
                // Entirely within s0.
                iovecs.push(libc::iovec {
                    iov_base: s0[pos..seg_end].as_ptr() as *mut libc::c_void,
                    iov_len: seg_len,
                });
            } else if pos >= s0.len() {
                // Entirely within s1.
                let s1_start = pos.saturating_sub(s0.len());
                let s1_end = seg_end.saturating_sub(s0.len());
                iovecs.push(libc::iovec {
                    iov_base: s1[s1_start..s1_end].as_ptr() as *mut libc::c_void,
                    iov_len: seg_len,
                });
            } else {
                // Straddle: part from s0, part from s1.
                let from_s0 = &s0[pos..];
                let s1_need = seg_len.saturating_sub(from_s0.len());
                iovecs.push(libc::iovec {
                    iov_base: from_s0.as_ptr() as *mut libc::c_void,
                    iov_len: from_s0.len(),
                });
                iovecs.push(libc::iovec {
                    iov_base: s1[..s1_need].as_ptr() as *mut libc::c_void,
                    iov_len: s1_need,
                });
            }

            pos = seg_end;
            all_iovecs.push(iovecs);
        }

        // Submit all SQEs under the ring lock.
        let mut ring = self.ring.lock();

        for (i, seg) in segments.iter().enumerate() {
            let iovecs = &all_iovecs[i];
            let fd = state.fds.fd(seg.file_index);

            let iov_count = u32::try_from(iovecs.len()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "iovec count overflow")
            })?;

            let writev = opcode::Writev::new(types::Fd(fd), iovecs.as_ptr().cast(), iov_count)
                .offset(seg.file_offset)
                .build()
                .user_data(i as u64);

            // SAFETY: The SQE references iovecs which point into s0/s1.
            // All three (SQE, iovecs, s0/s1) are alive for the entire method.
            // The ring lock is held from push through submit_and_wait, so no
            // other thread can submit concurrently. We have &mut access to the
            // submission queue via the Mutex.
            loop {
                match unsafe { ring.submission().push(&writev) } {
                    Ok(()) => break,
                    Err(_) => {
                        // SQ full -- submit current batch and retry.
                        ring.submit().map_err(crate::Error::Io)?;
                    }
                }
            }
        }

        // Submit and wait for all completions.
        ring.submit_and_wait(num_segments)
            .map_err(crate::Error::Io)?;

        // Reap CQEs and check for errors.
        let mut first_error: Option<std::io::Error> = None;
        let mut completed: usize = 0;
        for cqe in ring.completion() {
            completed = completed.saturating_add(1);
            let result = cqe.result();
            if result < 0 && first_error.is_none() {
                first_error = Some(std::io::Error::from_raw_os_error(-result));
            }
        }

        // If we didn't get enough completions, that's also an error.
        if completed < num_segments {
            return Err(crate::Error::Io(std::io::Error::other(format!(
                "io_uring: expected {num_segments} completions, got {completed}"
            ))));
        }

        if let Some(err) = first_error {
            return Err(crate::Error::Io(err));
        }

        Ok(())
    }
}

impl DiskIoBackend for IoUringDiskIo {
    fn name(&self) -> &str {
        "io_uring"
    }

    fn register(&self, info_hash: Id20, storage: Arc<dyn TorrentStorage>) {
        // Register with inner backend first (for reads/cache).
        self.inner.register(info_hash, Arc::clone(&storage));

        // Try to open io_uring fds via filesystem_info().
        if let Some((base_dir, file_paths, file_map)) = storage.filesystem_info() {
            let mut flags = libc::O_WRONLY | libc::O_CREAT;
            if self.config.enable_direct_io {
                flags |= libc::O_DIRECT;
            }

            match IoUringStorageState::open_files(base_dir, file_paths, flags) {
                Ok(fds) => {
                    self.uring_states.write().insert(
                        info_hash,
                        IoUringWriteState {
                            fds,
                            file_map: file_map.clone(),
                        },
                    );
                }
                Err(e) => {
                    warn!(
                        %info_hash,
                        error = %e,
                        "io_uring: failed to open fds, falling back to pwritev"
                    );
                }
            }
        }
    }

    fn unregister(&self, info_hash: Id20) {
        // Remove uring state first (Drop closes fds).
        self.uring_states.write().remove(&info_hash);
        self.inner.unregister(info_hash);
    }

    fn write_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: Bytes,
        flush: bool,
    ) -> crate::Result<()> {
        self.inner.write_chunk(info_hash, piece, begin, data, flush)
    }

    fn read_chunk(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        volatile: bool,
    ) -> crate::Result<Bytes> {
        self.inner
            .read_chunk(info_hash, piece, begin, length, volatile)
    }

    fn read_piece(&self, info_hash: Id20, piece: u32) -> crate::Result<Vec<u8>> {
        self.inner.read_piece(info_hash, piece)
    }

    fn hash_piece(&self, info_hash: Id20, piece: u32, expected: &Id20) -> crate::Result<bool> {
        self.inner.hash_piece(info_hash, piece, expected)
    }

    fn hash_piece_v2(&self, info_hash: Id20, piece: u32, expected: &Id32) -> crate::Result<bool> {
        self.inner.hash_piece_v2(info_hash, piece, expected)
    }

    fn hash_block(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
    ) -> crate::Result<Id32> {
        self.inner.hash_block(info_hash, piece, begin, length)
    }

    fn clear_piece(&self, info_hash: Id20, piece: u32) {
        self.inner.clear_piece(info_hash, piece)
    }

    fn flush_piece(&self, info_hash: Id20, piece: u32) -> crate::Result<()> {
        self.inner.flush_piece(info_hash, piece)
    }

    fn flush_all(&self) -> crate::Result<()> {
        self.inner.flush_all()
    }

    fn cached_pieces(&self, info_hash: Id20) -> Vec<u32> {
        self.inner.cached_pieces(info_hash)
    }

    fn stats(&self) -> DiskIoStats {
        let mut s = self.inner.stats();
        s.write_bytes = s
            .write_bytes
            .saturating_add(self.uring_write_bytes.load(Ordering::Relaxed));
        s
    }

    fn write_block_direct(
        &self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        s0: &[u8],
        s1: &[u8],
    ) -> crate::Result<()> {
        // If we have uring state for this torrent, use io_uring.
        let uring_states = self.uring_states.read();
        if let Some(state) = uring_states.get(&info_hash) {
            let result = self.uring_write(state, piece, begin, s0, s1);
            drop(uring_states);

            if result.is_ok() {
                let total = (s0.len().saturating_add(s1.len())) as u64;
                self.uring_write_bytes.fetch_add(total, Ordering::Relaxed);
                return Ok(());
            }

            // Fall through to inner on failure.
            warn!(
                %info_hash,
                piece,
                begin,
                error = %result.as_ref().unwrap_err(),
                "io_uring write failed, falling back to pwritev"
            );
        } else {
            drop(uring_states);
        }

        self.inner
            .write_block_direct(info_hash, piece, begin, s0, s1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::Lengths;
    use torrent_storage::{FilesystemStorage, IoUringConfig, MemoryStorage, PreallocateMode};

    use std::path::PathBuf;
    use std::sync::Arc;

    fn make_hash() -> Id20 {
        Id20([0xAB; 20])
    }

    fn make_hash_n(n: u8) -> Id20 {
        let mut b = [0u8; 20];
        b[0] = n;
        Id20(b)
    }

    fn test_config() -> crate::disk::DiskConfig {
        crate::disk::DiskConfig {
            io_threads: 2,
            cache_size: 1024 * 1024,
            ..crate::disk::DiskConfig::default()
        }
    }

    fn test_uring_config() -> IoUringConfig {
        IoUringConfig::default()
    }

    /// Create a [`FilesystemStorage`] in a temp directory with a single file.
    fn make_fs_storage(dir: &std::path::Path, total: u64) -> Arc<dyn TorrentStorage> {
        let chunk = total.min(16384) as u32;
        let lengths = Lengths::new(total, total, chunk);
        Arc::new(
            FilesystemStorage::new(
                dir,
                vec![PathBuf::from("test.bin")],
                vec![total],
                lengths,
                None,
                PreallocateMode::None,
            )
            .expect("FilesystemStorage::new should succeed"),
        )
    }

    /// Create a multi-file [`FilesystemStorage`].
    fn make_fs_storage_multi(
        dir: &std::path::Path,
        file_sizes: &[u64],
        piece_len: u64,
    ) -> Arc<dyn TorrentStorage> {
        let total: u64 = file_sizes.iter().sum();
        let chunk = piece_len.min(16384) as u32;
        let lengths = Lengths::new(total, piece_len, chunk);
        let paths: Vec<PathBuf> = file_sizes
            .iter()
            .enumerate()
            .map(|(i, _)| PathBuf::from(format!("file{i}.bin")))
            .collect();
        Arc::new(
            FilesystemStorage::new(
                dir,
                paths,
                file_sizes.to_vec(),
                lengths,
                None,
                PreallocateMode::None,
            )
            .expect("multi-file FilesystemStorage::new should succeed"),
        )
    }

    // -------------------------------------------------------------------
    // 1. Config defaults
    // -------------------------------------------------------------------

    #[test]
    fn uring_config_default() {
        let cfg = IoUringConfig::default();
        assert_eq!(cfg.sq_depth, 256);
        assert!(!cfg.enable_direct_io);
        assert_eq!(cfg.batch_threshold, 4);
    }

    // -------------------------------------------------------------------
    // 2. Ring creation (success)
    // -------------------------------------------------------------------

    #[test]
    fn uring_ring_creation() {
        let result = IoUringDiskIo::new(&test_config(), test_uring_config());
        assert!(result.is_ok(), "IoUringDiskIo::new() should succeed with default config");
    }

    // -------------------------------------------------------------------
    // 3. Ring creation (invalid depth)
    // -------------------------------------------------------------------

    #[test]
    fn uring_ring_creation_invalid_depth() {
        let cfg = IoUringConfig {
            sq_depth: 0,
            ..IoUringConfig::default()
        };
        let result = IoUringDiskIo::new(&test_config(), cfg);
        assert!(result.is_err(), "sq_depth=0 should fail io_uring setup");
    }

    // -------------------------------------------------------------------
    // 4. Register filesystem storage (uring_states populated)
    // -------------------------------------------------------------------

    #[test]
    fn uring_register_filesystem_storage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        let ih = make_hash();
        let storage = make_fs_storage(dir.path(), 16384);

        backend.register(ih, storage);

        let states = backend.uring_states.read();
        assert!(
            states.contains_key(&ih),
            "uring_states should contain the registered info_hash"
        );
    }

    // -------------------------------------------------------------------
    // 5. Register memory storage (uring_states NOT populated)
    // -------------------------------------------------------------------

    #[test]
    fn uring_register_memory_storage() {
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        let ih = make_hash();
        let lengths = Lengths::new(16384, 16384, 16384);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));

        backend.register(ih, storage);

        let states = backend.uring_states.read();
        assert!(
            !states.contains_key(&ih),
            "uring_states should NOT contain memory storage (no filesystem_info)"
        );
    }

    // -------------------------------------------------------------------
    // 6. Write single block via io_uring, read back
    // -------------------------------------------------------------------

    #[test]
    fn uring_write_single_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        let ih = make_hash_n(6);
        let storage = make_fs_storage(dir.path(), 16384);

        backend.register(ih, Arc::clone(&storage));

        let data = vec![0xBEu8; 16384];
        backend
            .write_block_direct(ih, 0, 0, &data, &[])
            .expect("write_block_direct should succeed");

        let readback = storage
            .read_chunk(0, 0, 16384)
            .expect("read_chunk should succeed");
        assert_eq!(readback, data);
    }

    // -------------------------------------------------------------------
    // 7. Write vectored split (s0 + s1), read back
    // -------------------------------------------------------------------

    #[test]
    fn uring_write_vectored_split() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        let ih = make_hash_n(7);
        let total = 16384_u64;
        let storage = make_fs_storage(dir.path(), total);

        backend.register(ih, Arc::clone(&storage));

        // Simulate ring buffer straddle: 10000 bytes in s0, 6384 bytes in s1.
        let s0: Vec<u8> = (0..10000_u32)
            .map(|i| (i % 251) as u8)
            .collect();
        let s1: Vec<u8> = (0..6384_u32)
            .map(|i| ((i + 10000) % 251) as u8)
            .collect();

        backend
            .write_block_direct(ih, 0, 0, &s0, &s1)
            .expect("vectored write should succeed");

        let readback = storage
            .read_chunk(0, 0, 16384)
            .expect("read_chunk should succeed");

        let mut expected = s0.clone();
        expected.extend_from_slice(&s1);
        assert_eq!(readback, expected);
    }

    // -------------------------------------------------------------------
    // 8. Write spanning two files (multi-file boundary)
    // -------------------------------------------------------------------

    #[test]
    fn uring_write_multi_file_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        let ih = make_hash_n(8);

        // Two files of 8192 bytes each, one piece of 16384 spanning both.
        let storage = make_fs_storage_multi(dir.path(), &[8192, 8192], 16384);
        backend.register(ih, Arc::clone(&storage));

        let data = vec![0xCDu8; 16384];
        backend
            .write_block_direct(ih, 0, 0, &data, &[])
            .expect("multi-file write should succeed");

        let readback = storage
            .read_chunk(0, 0, 16384)
            .expect("read_chunk should succeed");
        assert_eq!(readback, data);
    }

    // -------------------------------------------------------------------
    // 9. Read delegates to inner PosixDiskIo
    // -------------------------------------------------------------------

    #[test]
    fn uring_read_delegates_to_inner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        let ih = make_hash_n(9);
        let storage = make_fs_storage(dir.path(), 16384);

        backend.register(ih, Arc::clone(&storage));

        // Write data via the uring path.
        let data = vec![0xAAu8; 16384];
        backend
            .write_block_direct(ih, 0, 0, &data, &[])
            .expect("write should succeed");

        // Read via the backend's read_chunk (delegates to inner PosixDiskIo).
        let readback = backend
            .read_chunk(ih, 0, 0, 16384, true)
            .expect("read_chunk via backend should succeed");
        assert_eq!(&readback[..], &data[..]);
    }

    // -------------------------------------------------------------------
    // 10. Hash piece delegates to inner
    // -------------------------------------------------------------------

    #[test]
    fn uring_hash_piece_delegates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        let ih = make_hash_n(10);
        let storage = make_fs_storage(dir.path(), 16384);

        backend.register(ih, Arc::clone(&storage));

        let data = vec![0x42u8; 16384];
        backend
            .write_block_direct(ih, 0, 0, &data, &[])
            .expect("write should succeed");

        // Flush so the inner backend can read from disk.
        backend
            .flush_piece(ih, 0)
            .expect("flush should succeed");

        let expected_hash = torrent_core::sha1(&data);
        let matches = backend
            .hash_piece(ih, 0, &expected_hash)
            .expect("hash_piece should succeed");
        assert!(matches, "hash should match the written data");
    }

    // -------------------------------------------------------------------
    // 11. Unregister removes uring state
    // -------------------------------------------------------------------

    #[test]
    fn uring_unregister_closes_fds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        let ih = make_hash_n(11);
        let storage = make_fs_storage(dir.path(), 16384);

        backend.register(ih, Arc::clone(&storage));
        assert!(backend.uring_states.read().contains_key(&ih));

        backend.unregister(ih);
        assert!(
            !backend.uring_states.read().contains_key(&ih),
            "uring_states should be empty after unregister"
        );
    }

    // -------------------------------------------------------------------
    // 12. Backend name
    // -------------------------------------------------------------------

    #[test]
    fn uring_backend_name() {
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        assert_eq!(backend.name(), "io_uring");
    }

    // -------------------------------------------------------------------
    // 13. Stats track uring writes
    // -------------------------------------------------------------------

    #[test]
    fn uring_stats_track_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");
        let ih = make_hash_n(13);
        let storage = make_fs_storage(dir.path(), 16384);

        backend.register(ih, storage);

        let data = vec![0xFFu8; 16384];
        backend
            .write_block_direct(ih, 0, 0, &data, &[])
            .expect("write should succeed");

        let stats = backend.stats();
        assert!(
            stats.write_bytes >= 16384,
            "stats.write_bytes ({}) should include the 16384-byte uring write",
            stats.write_bytes
        );
    }

    // -------------------------------------------------------------------
    // 14. Concurrent torrents — isolation
    // -------------------------------------------------------------------

    #[test]
    fn uring_concurrent_torrents() {
        let dir1 = tempfile::tempdir().expect("tempdir 1");
        let dir2 = tempfile::tempdir().expect("tempdir 2");
        let backend =
            IoUringDiskIo::new(&test_config(), test_uring_config()).expect("backend creation");

        let ih1 = make_hash_n(141);
        let ih2 = make_hash_n(142);

        let storage1 = make_fs_storage(dir1.path(), 16384);
        let storage2 = make_fs_storage(dir2.path(), 16384);

        backend.register(ih1, Arc::clone(&storage1));
        backend.register(ih2, Arc::clone(&storage2));

        // Write different data to each torrent.
        let data1 = vec![0x11u8; 16384];
        let data2 = vec![0x22u8; 16384];

        backend
            .write_block_direct(ih1, 0, 0, &data1, &[])
            .expect("write to torrent 1");
        backend
            .write_block_direct(ih2, 0, 0, &data2, &[])
            .expect("write to torrent 2");

        // Read back and verify isolation.
        let read1 = storage1
            .read_chunk(0, 0, 16384)
            .expect("read torrent 1");
        let read2 = storage2
            .read_chunk(0, 0, 16384)
            .expect("read torrent 2");

        assert_eq!(read1, data1, "torrent 1 data should be 0x11");
        assert_eq!(read2, data2, "torrent 2 data should be 0x22");
    }

    // -------------------------------------------------------------------
    // 15. Factory fallback on io_uring init failure
    // -------------------------------------------------------------------

    #[test]
    fn uring_factory_fallback() {
        use crate::disk_backend::create_backend_from_config;

        let config = crate::disk::DiskConfig {
            storage_mode: torrent_core::StorageMode::IoUring,
            // sq_depth=0 forces IoUring::new() to fail.
            io_uring_sq_depth: 0,
            ..crate::disk::DiskConfig::default()
        };

        let backend = create_backend_from_config(&config);
        assert_eq!(
            backend.name(),
            "posix",
            "should fall back to posix when io_uring init fails"
        );
    }
}
