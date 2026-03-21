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
struct IoUringWriteState {
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
    uring_states: RwLock<HashMap<Id20, IoUringWriteState>>,
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
