use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use torrent_core::Lengths;

use crate::error::Error;
use crate::file_map::FileMap;
use crate::storage::TorrentStorage;
use crate::Result;

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
        preallocate: bool,
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
            if preallocate {
                Self::preallocate_file(&f, file_lengths[i])?;
            } else {
                // Create sparse file with correct length.
                f.set_len(file_lengths[i])?;
            }
        }

        let files = (0..file_paths.len())
            .map(|_| Mutex::new(None))
            .collect();

        Ok(FilesystemStorage {
            base_dir: base_dir.to_owned(),
            file_paths,
            files,
            file_map,
            lengths,
        })
    }

    /// Pre-allocate a file by writing actual blocks (not sparse).
    ///
    /// Uses `fallocate` on Linux, falls back to writing zeros on other platforms.
    fn preallocate_file(f: &File, length: u64) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let ret = unsafe {
                libc::fallocate(f.as_raw_fd(), 0, 0, length as libc::off_t)
            };
            if ret == 0 {
                return Ok(());
            }
            // Fallback on error (e.g. filesystem doesn't support fallocate)
        }

        f.set_len(length)?;

        // Write zeros in chunks to actually allocate blocks
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

        Ok(())
    }

    /// Open (or return cached) file handle for the given file index.
    fn open_file(&self, index: usize) -> Result<std::sync::MutexGuard<'_, Option<File>>> {
        let mut guard = self.files[index].lock().unwrap();
        if guard.is_none() {
            let full = self.base_dir.join(&self.file_paths[index]);
            let f = File::options().read(true).write(true).open(&full)?;
            *guard = Some(f);
        }
        Ok(guard)
    }
}

impl TorrentStorage for FilesystemStorage {
    fn write_chunk(&self, piece: u32, begin: u32, data: &[u8]) -> Result<()> {
        let segments = self.file_map.chunk_segments(piece, begin, data.len() as u32);
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use torrent_core::{Id20, Lengths};

    use super::*;

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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
                false,
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
            false,
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
    fn full_preallocation() {
        let dir = temp_dir("prealloc");
        let lengths = Lengths::new(100_000, 50_000, 16384);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![100_000],
            lengths,
            None,
            true,
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
}
