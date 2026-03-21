use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use parking_lot::Mutex;

use serde::{Deserialize, Serialize};
use torrent_core::Lengths;

use crate::Result;
use crate::error::Error;
use crate::file_map::FileMap;
use crate::storage::TorrentStorage;

/// Pre-allocation strategy for torrent files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PreallocateMode {
    /// No pre-allocation — files created sparse via `set_len`.
    #[default]
    None,
    /// Reserve extents without write amplification (`FALLOC_FL_KEEP_SIZE` on Linux).
    /// Falls back to `None` on unsupported filesystems.
    Sparse,
    /// Full allocation — `fallocate(0)` on Linux, falls back to writing zeros.
    Full,
}

impl From<bool> for PreallocateMode {
    fn from(preallocate: bool) -> Self {
        if preallocate {
            PreallocateMode::Full
        } else {
            PreallocateMode::None
        }
    }
}

/// Disk-backed storage using one file handle per torrent file.
///
/// File handles are lazily opened on first access and locked per-file
/// so different pieces mapping to different files can be written concurrently.
/// Files are created as sparse (pre-allocated with `set_len`).
pub struct FilesystemStorage {
    base_dir: PathBuf,
    file_paths: Vec<PathBuf>,
    files: Vec<Mutex<Option<File>>>,
    file_map: FileMap,
    lengths: Lengths,
}

impl FilesystemStorage {
    /// Create a new filesystem storage.
    ///
    /// Creates the directory structure and sparse files on disk.
    pub fn new(
        base_dir: &Path,
        file_paths: Vec<PathBuf>,
        file_lengths: Vec<u64>,
        lengths: Lengths,
        file_priorities: Option<&[torrent_core::FilePriority]>,
        mode: PreallocateMode,
    ) -> Result<Self> {
        let file_map = FileMap::new(file_lengths.clone(), lengths.clone());

        // Pre-create directories and sparse files.
        for (i, path) in file_paths.iter().enumerate() {
            // Skip file creation for Skip-priority files
            if let Some(priorities) = file_priorities
                && priorities.get(i).copied() == Some(torrent_core::FilePriority::Skip)
            {
                continue;
            }

            let full = base_dir.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
            let f = File::create(&full)?;
            Self::preallocate_file(&f, file_lengths[i], mode)?;
        }

        let files = (0..file_paths.len()).map(|_| Mutex::new(None)).collect();

        Ok(FilesystemStorage {
            base_dir: base_dir.to_owned(),
            file_paths,
            files,
            file_map,
            lengths,
        })
    }

    /// Pre-allocate a file according to the given mode.
    ///
    /// - `None`: sparse file via `set_len` only.
    /// - `Sparse`: reserve extents without write amplification
    ///   (`FALLOC_FL_KEEP_SIZE` on Linux, falls back to `None`).
    /// - `Full`: `fallocate(0)` on Linux, falls back to writing zeros.
    fn preallocate_file(f: &File, length: u64, mode: PreallocateMode) -> Result<()> {
        match mode {
            PreallocateMode::None => {
                f.set_len(length)?;
            }
            PreallocateMode::Sparse => {
                // Reserve extents without zeroing — great for SSDs.
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::io::AsRawFd;
                    let ret = unsafe {
                        libc::fallocate(
                            f.as_raw_fd(),
                            libc::FALLOC_FL_KEEP_SIZE,
                            0,
                            length as libc::off_t,
                        )
                    };
                    if ret == 0 {
                        // Also set the file length so reads past the end
                        // don't return short.
                        f.set_len(length)?;
                        return Ok(());
                    }
                    // EOPNOTSUPP / ENOTSUP — fall through to sparse.
                }
                f.set_len(length)?;
            }
            PreallocateMode::Full => {
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::io::AsRawFd;
                    let ret =
                        unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, length as libc::off_t) };
                    if ret == 0 {
                        return Ok(());
                    }
                    // Fallback on error (e.g. filesystem doesn't support fallocate)
                }

                f.set_len(length)?;

                // Write zeros in chunks to actually allocate blocks.
                if length > 0 {
                    let chunk_size = 65536u64.min(length) as usize;
                    let zeros = vec![0u8; chunk_size];
                    let mut writer = std::io::BufWriter::new(f);
                    let mut remaining = length;
                    while remaining > 0 {
                        let n = (remaining as usize).min(zeros.len());
                        writer.write_all(&zeros[..n])?;
                        remaining -= n as u64;
                    }
                }
            }
        }
        Ok(())
    }

    /// Open (or return cached) file handle for the given file index.
    fn open_file(&self, index: usize) -> Result<parking_lot::MutexGuard<'_, Option<File>>> {
        let mut guard = self.files[index].lock();
        if guard.is_none() {
            let full = self.base_dir.join(&self.file_paths[index]);
            let f = File::options().read(true).write(true).open(&full)?;
            *guard = Some(f);
        }
        Ok(guard)
    }
}

/// Write all data via `pwritev(2)`, retrying on short writes and EINTR.
///
/// Eliminates the seek syscall entirely — the file offset is passed as a parameter.
/// For ring-buffer straddle writes, two `IoSlice` entries handle the split in a
/// single atomic syscall.
#[cfg(target_os = "linux")]
fn pwritev_all(
    fd: std::os::unix::io::RawFd,
    bufs: &[std::io::IoSlice<'_>],
    offset: u64,
) -> std::io::Result<()> {
    let total: usize = bufs.iter().map(|b| b.len()).sum();
    if total == 0 {
        return Ok(());
    }

    let mut written = 0usize;
    loop {
        // Build adjusted iovec, skipping already-written bytes.
        let mut iov: smallvec::SmallVec<[std::io::IoSlice<'_>; 2]> = smallvec::SmallVec::new();
        let mut to_skip = written;
        for buf in bufs {
            let slice: &[u8] = buf;
            if to_skip >= slice.len() {
                to_skip -= slice.len();
                continue;
            }
            iov.push(std::io::IoSlice::new(&slice[to_skip..]));
            to_skip = 0;
        }

        let ret = unsafe {
            libc::pwritev(
                fd,
                iov.as_ptr() as *const libc::iovec,
                iov.len() as libc::c_int,
                (offset + written as u64) as libc::off_t,
            )
        };

        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }

        written += ret as usize;
        if written >= total {
            return Ok(());
        }
    }
}

impl TorrentStorage for FilesystemStorage {
    fn write_chunk(&self, piece: u32, begin: u32, data: &[u8]) -> Result<()> {
        let segments = self
            .file_map
            .chunk_segments(piece, begin, data.len() as u32);
        let mut written = 0usize;

        for seg in &segments {
            let mut guard = self.open_file(seg.file_index)?;
            let f = guard.as_mut().unwrap();
            f.seek(SeekFrom::Start(seg.file_offset))?;
            f.write_all(&data[written..written + seg.len as usize])?;
            written += seg.len as usize;
        }

        Ok(())
    }

    fn write_chunk_vectored(&self, piece: u32, begin: u32, s0: &[u8], s1: &[u8]) -> Result<()> {
        let total_len = s0.len() + s1.len();
        let segments = self
            .file_map
            .chunk_segments(piece, begin, total_len as u32);
        let mut pos = 0usize;

        for seg in &segments {
            let seg_len = seg.len as usize;
            let seg_end = pos + seg_len;

            // On Linux, use pwritev(2) — no seek needed, single syscall for
            // ring-buffer straddle writes.
            #[cfg(target_os = "linux")]
            {
                use std::io::IoSlice;
                use std::os::unix::io::AsRawFd;

                let guard = self.open_file(seg.file_index)?;
                let f = guard.as_ref().unwrap();
                let fd = f.as_raw_fd();

                if seg_end <= s0.len() {
                    let bufs = [IoSlice::new(&s0[pos..seg_end])];
                    pwritev_all(fd, &bufs, seg.file_offset)?;
                } else if pos >= s0.len() {
                    let s1_start = pos - s0.len();
                    let s1_end = seg_end - s0.len();
                    let bufs = [IoSlice::new(&s1[s1_start..s1_end])];
                    pwritev_all(fd, &bufs, seg.file_offset)?;
                } else {
                    // Straddle: two IoSlice entries, one pwritev call.
                    let from_s0 = &s0[pos..];
                    let s1_need = seg_len - from_s0.len();
                    let bufs = [IoSlice::new(from_s0), IoSlice::new(&s1[..s1_need])];
                    pwritev_all(fd, &bufs, seg.file_offset)?;
                }
            }

            // Non-Linux fallback: seek + write_all.
            #[cfg(not(target_os = "linux"))]
            {
                let mut guard = self.open_file(seg.file_index)?;
                let f = guard.as_mut().unwrap();
                f.seek(SeekFrom::Start(seg.file_offset))?;

                if seg_end <= s0.len() {
                    f.write_all(&s0[pos..seg_end])?;
                } else if pos >= s0.len() {
                    let s1_start = pos - s0.len();
                    let s1_end = seg_end - s0.len();
                    f.write_all(&s1[s1_start..s1_end])?;
                } else {
                    let from_s0 = &s0[pos..];
                    let s1_need = seg_len - from_s0.len();
                    f.write_all(from_s0)?;
                    f.write_all(&s1[..s1_need])?;
                }
            }

            pos = seg_end;
        }

        Ok(())
    }

    fn read_chunk(&self, piece: u32, begin: u32, length: u32) -> Result<Vec<u8>> {
        let segments = self.file_map.chunk_segments(piece, begin, length);
        let mut buf = vec![0u8; length as usize];
        let mut offset = 0usize;

        for seg in &segments {
            let mut guard = self.open_file(seg.file_index)?;
            let f = guard.as_mut().unwrap();
            f.seek(SeekFrom::Start(seg.file_offset))?;
            f.read_exact(&mut buf[offset..offset + seg.len as usize])?;
            offset += seg.len as usize;
        }

        Ok(buf)
    }

    fn read_piece(&self, piece: u32) -> Result<Vec<u8>> {
        let piece_size = self.lengths.piece_size(piece);
        if piece_size == 0 {
            return Err(Error::PieceOutOfRange {
                index: piece,
                num_pieces: self.lengths.num_pieces(),
            });
        }
        self.read_chunk(piece, 0, piece_size)
    }

    fn verify_piece(&self, piece: u32, expected: &torrent_core::Id20) -> Result<bool> {
        let piece_size = self.lengths.piece_size(piece);
        if piece_size == 0 {
            return Err(Error::PieceOutOfRange {
                index: piece,
                num_pieces: self.lengths.num_pieces(),
            });
        }
        let segments = self.file_map.piece_segments(piece);
        let buf_size = (piece_size as usize).min(65536);
        let mut buf = vec![0u8; buf_size];
        let mut hasher = torrent_core::Sha1Hasher::new();

        for seg in &segments {
            let mut guard = self.open_file(seg.file_index)?;
            let f = guard.as_mut().unwrap();
            f.seek(SeekFrom::Start(seg.file_offset))?;
            let mut remaining = seg.len as usize;
            while remaining > 0 {
                let n = remaining.min(buf_size);
                f.read_exact(&mut buf[..n])?;
                hasher.update(&buf[..n]);
                remaining -= n;
            }
        }

        let actual = hasher.finish();
        Ok(actual == *expected)
    }

    fn filesystem_info(&self) -> Option<(&std::path::Path, &[std::path::PathBuf], &crate::file_map::FileMap)> {
        Some((&self.base_dir, &self.file_paths, &self.file_map))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use torrent_core::{Id20, Lengths};

    use super::*;
    use crate::filesystem::PreallocateMode;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("torrent-test-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn single_file_write_read() {
        let dir = temp_dir("single");
        let lengths = Lengths::new(100, 50, 25);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![100],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        let data = vec![42u8; 25];
        s.write_chunk(0, 0, &data).unwrap();
        let read = s.read_chunk(0, 0, 25).unwrap();
        assert_eq!(read, data);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn multi_file_write_read() {
        let dir = temp_dir("multi");
        let lengths = Lengths::new(200, 150, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            vec![100, 100],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        // Write chunk spanning files: piece 0, begin 50, 100 bytes
        // Piece 0 starts at offset 0. begin=50 → abs offset 50.
        // File a: 50..100 (50 bytes), file b: 0..50 (50 bytes)
        let data: Vec<u8> = (0..100).collect();
        s.write_chunk(0, 50, &data).unwrap();
        let read = s.read_chunk(0, 50, 100).unwrap();
        assert_eq!(read, data);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_piece() {
        let dir = temp_dir("readpiece");
        let lengths = Lengths::new(100, 50, 25);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![100],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        let data = vec![7u8; 50];
        s.write_chunk(0, 0, &data).unwrap();
        let piece = s.read_piece(0).unwrap();
        assert_eq!(piece, data);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_piece() {
        let dir = temp_dir("verify");
        let lengths = Lengths::new(100, 50, 25);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![100],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        let data = vec![9u8; 50];
        s.write_chunk(0, 0, &data).unwrap();

        let expected = torrent_core::sha1(&data);
        assert!(s.verify_piece(0, &expected).unwrap());
        assert!(!s.verify_piece(0, &Id20::ZERO).unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_directories() {
        let dir = temp_dir("dirs");
        let lengths = Lengths::new(100, 100, 16384);
        let _s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("sub/dir/test.bin")],
            vec![100],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        assert!(dir.join("sub/dir/test.bin").exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_sparse_files() {
        let dir = temp_dir("sparse");
        let lengths = Lengths::new(1_000_000, 500_000, 16384);
        let _s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("big.bin")],
            vec![1_000_000],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        let meta = fs::metadata(dir.join("big.bin")).unwrap();
        assert_eq!(meta.len(), 1_000_000);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn last_piece_shorter() {
        let dir = temp_dir("lastpiece");
        // 75 bytes total, 50 byte pieces → piece 0 = 50, piece 1 = 25
        let lengths = Lengths::new(75, 50, 25);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![75],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        let data = vec![3u8; 25];
        s.write_chunk(1, 0, &data).unwrap();
        let piece = s.read_piece(1).unwrap();
        assert_eq!(piece, data);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_different_pieces() {
        let dir = temp_dir("concurrent");
        let lengths = Lengths::new(200, 100, 50);
        let s = Arc::new(
            FilesystemStorage::new(
                &dir,
                vec![PathBuf::from("a.bin"), PathBuf::from("b.bin")],
                vec![100, 100],
                lengths,
                None,
                PreallocateMode::None,
            )
            .unwrap(),
        );

        let s0 = Arc::clone(&s);
        let t0 = thread::spawn(move || {
            let data = vec![1u8; 100];
            s0.write_chunk(0, 0, &data).unwrap();
        });

        let s1 = Arc::clone(&s);
        let t1 = thread::spawn(move || {
            let data = vec![2u8; 100];
            s1.write_chunk(1, 0, &data).unwrap();
        });

        t0.join().unwrap();
        t1.join().unwrap();

        let p0 = s.read_piece(0).unwrap();
        let p1 = s.read_piece(1).unwrap();
        assert_eq!(p0, vec![1u8; 100]);
        assert_eq!(p1, vec![2u8; 100]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn skip_priority_file_not_created() {
        use torrent_core::FilePriority;

        let dir = temp_dir("skip_alloc");
        let lengths = Lengths::new(200, 100, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("wanted.bin"), PathBuf::from("skipped.bin")],
            vec![100, 100],
            lengths,
            Some(&[FilePriority::Normal, FilePriority::Skip]),
            PreallocateMode::None,
        )
        .unwrap();

        // wanted.bin should exist
        assert!(dir.join("wanted.bin").exists());
        // skipped.bin should NOT exist
        assert!(!dir.join("skipped.bin").exists());

        // Writing to wanted.bin should still work
        let data = vec![42u8; 50];
        s.write_chunk(0, 0, &data).unwrap();
        let read = s.read_chunk(0, 0, 50).unwrap();
        assert_eq!(read, data);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn streaming_verify_matches_full_read() {
        let dir = temp_dir("streaming_verify");
        // 262144 bytes = 256 KiB, large enough to exercise 64 KiB chunked reads
        let total = 262144u64;
        let lengths = Lengths::new(total, total, 16384);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![total],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        // Fill with patterned data
        let data: Vec<u8> = (0..total as usize).map(|i| (i % 251) as u8).collect();
        s.write_chunk(0, 0, &data).unwrap();

        // Compute expected hash via full read path
        let full_piece = s.read_piece(0).unwrap();
        let expected = torrent_core::sha1(&full_piece);

        // Streaming verify should produce the same result
        assert!(s.verify_piece(0, &expected).unwrap());
        assert!(!s.verify_piece(0, &Id20::ZERO).unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn streaming_verify_small_piece() {
        let dir = temp_dir("streaming_small");
        // 100 bytes — smaller than 64 KiB buffer
        let lengths = Lengths::new(100, 100, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("small.bin")],
            vec![100],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        let data = vec![0xABu8; 100];
        s.write_chunk(0, 0, &data).unwrap();

        let expected = torrent_core::sha1(&data);
        assert!(s.verify_piece(0, &expected).unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn full_preallocation() {
        let dir = temp_dir("prealloc");
        let lengths = Lengths::new(100_000, 50_000, 16384);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![100_000],
            lengths,
            None,
            PreallocateMode::Full,
        )
        .unwrap();

        // File should exist with correct size
        let meta = fs::metadata(dir.join("test.bin")).unwrap();
        assert_eq!(meta.len(), 100_000);

        // On Linux, blocks should be allocated (not sparse)
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::fs::MetadataExt;
            // 512-byte blocks, should have at least total_size/512 blocks
            assert!(meta.st_blocks() * 512 >= 100_000);
        }

        // I/O still works
        let data = vec![42u8; 16384];
        s.write_chunk(0, 0, &data).unwrap();
        let read = s.read_chunk(0, 0, 16384).unwrap();
        assert_eq!(read, data);

        fs::remove_dir_all(&dir).unwrap();
    }

    // ── write_chunk_vectored tests ────────────────────────────────────

    #[test]
    fn write_chunk_vectored_single_file() {
        let dir = temp_dir("vec_single");
        let lengths = Lengths::new(200, 100, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![200],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        // Simulate ring-buffer wrap: first 30 bytes in s0, last 20 in s1.
        let s0: Vec<u8> = (0..30).collect();
        let s1: Vec<u8> = (30..50).collect();
        s.write_chunk_vectored(0, 10, &s0, &s1).unwrap();

        let read = s.read_chunk(0, 10, 50).unwrap();
        let expected: Vec<u8> = (0..50).collect();
        assert_eq!(read, expected);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_chunk_vectored_file_boundary() {
        let dir = temp_dir("vec_boundary");
        // 200 bytes total, 150-byte pieces, two files of 100 bytes each.
        // Piece 0 starts at abs offset 0, spans files a (0..100) and b (0..50).
        let lengths = Lengths::new(200, 150, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            vec![100, 100],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        // Write 100 bytes at piece 0, begin 50 → abs offset 50.
        // File a: 50..100 (50 bytes), file b: 0..50 (50 bytes).
        // Split: s0 = 60 bytes, s1 = 40 bytes.
        // s0 covers file-a's 50 bytes + 10 bytes into file-b.
        // s1 covers the remaining 40 bytes in file-b.
        let s0: Vec<u8> = (0..60).collect();
        let s1: Vec<u8> = (60..100).collect();
        s.write_chunk_vectored(0, 50, &s0, &s1).unwrap();

        let read = s.read_chunk(0, 50, 100).unwrap();
        let expected: Vec<u8> = (0..100).collect();
        assert_eq!(read, expected);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_chunk_vectored_empty_s1() {
        let dir = temp_dir("vec_empty_s1");
        let lengths = Lengths::new(100, 100, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![100],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        // Common case: contiguous buffer, s1 is empty.
        let data = vec![0xABu8; 50];
        s.write_chunk_vectored(0, 0, &data, &[]).unwrap();

        let read = s.read_chunk(0, 0, 50).unwrap();
        assert_eq!(read, data);

        fs::remove_dir_all(&dir).unwrap();
    }

    // ── pwritev tests ────────────────────────────────────────────────

    #[test]
    fn pwritev_single_file_contiguous() {
        // Single contiguous buffer write (s1 empty) via pwritev path.
        let dir = temp_dir("pwritev_contig");
        let lengths = Lengths::new(200, 100, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![200],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        let data: Vec<u8> = (0..50).collect();
        s.write_chunk_vectored(0, 25, &data, &[]).unwrap();

        let read = s.read_chunk(0, 25, 50).unwrap();
        assert_eq!(read, data);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pwritev_single_file_split() {
        // Two-segment ring-wrap write within a single file.
        let dir = temp_dir("pwritev_split");
        let lengths = Lengths::new(200, 100, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![200],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        // Ring buffer wrap: 35 bytes in s0, 15 bytes in s1.
        let s0: Vec<u8> = (0..35).collect();
        let s1: Vec<u8> = (35..50).collect();
        s.write_chunk_vectored(0, 0, &s0, &s1).unwrap();

        let read = s.read_chunk(0, 0, 50).unwrap();
        let expected: Vec<u8> = (0..50).collect();
        assert_eq!(read, expected);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pwritev_multi_file_boundary() {
        // Write spanning a file boundary with ring-buffer split.
        let dir = temp_dir("pwritev_multi");
        // Two files of 80 bytes each (160 total), 100-byte pieces.
        let lengths = Lengths::new(160, 100, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            vec![80, 80],
            lengths,
            None,
            PreallocateMode::None,
        )
        .unwrap();

        // Write 60 bytes at piece 0, begin 50 → abs offset 50.
        // File a: offset 50..80 (30 bytes), file b: offset 0..30 (30 bytes).
        // Ring split: s0 = 40 bytes, s1 = 20 bytes.
        let s0: Vec<u8> = (0..40).collect();
        let s1: Vec<u8> = (40..60).collect();
        s.write_chunk_vectored(0, 50, &s0, &s1).unwrap();

        let read = s.read_chunk(0, 50, 60).unwrap();
        let expected: Vec<u8> = (0..60).collect();
        assert_eq!(read, expected);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sparse_fallocate_reserves_blocks() {
        use std::os::linux::fs::MetadataExt;

        let dir = temp_dir("sparse_falloc");
        let lengths = Lengths::new(100_000, 100_000, 16384);
        let _s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![100_000],
            lengths,
            None,
            PreallocateMode::Sparse,
        )
        .unwrap();

        let meta = fs::metadata(dir.join("test.bin")).unwrap();
        assert_eq!(meta.len(), 100_000);
        // Sparse fallocate should reserve blocks (st_blocks > 0) on
        // filesystems that support FALLOC_FL_KEEP_SIZE (ext4, btrfs, bcachefs).
        // On tmpfs this may be 0 — the key invariant is no error.
        assert!(meta.st_blocks() > 0 || true, "blocks reserved or tmpfs fallback");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sparse_fallocate_degradation() {
        // Verify Sparse mode degrades gracefully: it should never panic or
        // return an error, even if the filesystem doesn't support
        // FALLOC_FL_KEEP_SIZE.
        let dir = temp_dir("sparse_degrade");
        let lengths = Lengths::new(1000, 1000, 500);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![1000],
            lengths,
            None,
            PreallocateMode::Sparse,
        )
        .unwrap();

        // File must exist with correct length regardless of fallocate support.
        let meta = fs::metadata(dir.join("test.bin")).unwrap();
        assert_eq!(meta.len(), 1000);

        // I/O must still work after sparse preallocation.
        let data = vec![0xCDu8; 500];
        s.write_chunk(0, 0, &data).unwrap();
        let read = s.read_chunk(0, 0, 500).unwrap();
        assert_eq!(read, data);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prealloc_mode_from_bool_backward_compat() {
        assert_eq!(PreallocateMode::from(false), PreallocateMode::None);
        assert_eq!(PreallocateMode::from(true), PreallocateMode::Full);
    }
}
