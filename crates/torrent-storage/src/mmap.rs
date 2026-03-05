use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use memmap2::MmapMut;

use torrent_core::Lengths;

use crate::Result;
use crate::error::Error;
use crate::file_map::FileMap;
use crate::storage::TorrentStorage;

/// Memory-mapped file storage backend.
///
/// Each file is lazily mmap'd on first access. Relies on the kernel page
/// cache for read caching (no user-space cache needed in mmap mode).
pub struct MmapStorage {
    base_dir: PathBuf,
    file_paths: Vec<PathBuf>,
    mmaps: Vec<Mutex<Option<MmapMut>>>,
    file_map: FileMap,
    lengths: Lengths,
}

impl MmapStorage {
    /// Create a new memory-mapped storage.
    ///
    /// Creates the directory structure and sparse files on disk.
    /// Files are mmap'd lazily on first read/write.
    pub fn new(
        base_dir: &Path,
        file_paths: Vec<PathBuf>,
        file_lengths: Vec<u64>,
        lengths: Lengths,
        file_priorities: Option<&[torrent_core::FilePriority]>,
    ) -> Result<Self> {
        let file_map = FileMap::new(file_lengths.clone(), lengths.clone());

        for (i, path) in file_paths.iter().enumerate() {
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
            f.set_len(file_lengths[i])?;
        }

        let mmaps = (0..file_paths.len()).map(|_| Mutex::new(None)).collect();

        Ok(MmapStorage {
            base_dir: base_dir.to_owned(),
            file_paths,
            mmaps,
            file_map,
            lengths,
        })
    }

    /// Open (or return cached) mmap for the given file index.
    fn open_mmap(&self, index: usize) -> Result<std::sync::MutexGuard<'_, Option<MmapMut>>> {
        let mut guard = self.mmaps[index].lock().unwrap();
        if guard.is_none() {
            let full = self.base_dir.join(&self.file_paths[index]);
            let f = File::options().read(true).write(true).open(&full)?;
            // Safety: we created the file with the correct length and hold
            // exclusive write access via the Mutex.
            let mmap = unsafe { MmapMut::map_mut(&f)? };
            *guard = Some(mmap);
        }
        Ok(guard)
    }
}

impl TorrentStorage for MmapStorage {
    fn write_chunk(&self, piece: u32, begin: u32, data: &[u8]) -> Result<()> {
        let segments = self
            .file_map
            .chunk_segments(piece, begin, data.len() as u32);
        let mut written = 0usize;

        for seg in &segments {
            let mut guard = self.open_mmap(seg.file_index)?;
            let mmap = guard.as_mut().unwrap();
            let start = seg.file_offset as usize;
            let end = start + seg.len as usize;
            mmap[start..end].copy_from_slice(&data[written..written + seg.len as usize]);
            written += seg.len as usize;
        }
        Ok(())
    }

    fn read_chunk(&self, piece: u32, begin: u32, length: u32) -> Result<Vec<u8>> {
        let segments = self.file_map.chunk_segments(piece, begin, length);
        let mut buf = vec![0u8; length as usize];
        let mut offset = 0usize;

        for seg in &segments {
            let guard = self.open_mmap(seg.file_index)?;
            let mmap = guard.as_ref().unwrap();
            let start = seg.file_offset as usize;
            let end = start + seg.len as usize;
            buf[offset..offset + seg.len as usize].copy_from_slice(&mmap[start..end]);
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
    use super::*;
    use std::path::PathBuf;
    use torrent_core::{Id20, Lengths};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("torrent-mmap-test-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn single_file_write_read() {
        let dir = temp_dir("single");
        let lengths = Lengths::new(100, 50, 25);
        let s = MmapStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![100],
            lengths,
            None,
        )
        .unwrap();

        let data = vec![42u8; 25];
        s.write_chunk(0, 0, &data).unwrap();
        let read = s.read_chunk(0, 0, 25).unwrap();
        assert_eq!(read, data);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn multi_file_spanning() {
        let dir = temp_dir("multi");
        let lengths = Lengths::new(200, 150, 50);
        let s = MmapStorage::new(
            &dir,
            vec![PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            vec![100, 100],
            lengths,
            None,
        )
        .unwrap();

        let data: Vec<u8> = (0..100).collect();
        s.write_chunk(0, 50, &data).unwrap();
        let read = s.read_chunk(0, 50, 100).unwrap();
        assert_eq!(read, data);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_piece() {
        let dir = temp_dir("verify");
        let lengths = Lengths::new(100, 50, 25);
        let s = MmapStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![100],
            lengths,
            None,
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
    fn last_piece_shorter() {
        let dir = temp_dir("lastpiece");
        let lengths = Lengths::new(75, 50, 25);
        let s = MmapStorage::new(
            &dir,
            vec![PathBuf::from("test.bin")],
            vec![75],
            lengths,
            None,
        )
        .unwrap();

        let data = vec![3u8; 25];
        s.write_chunk(1, 0, &data).unwrap();
        let piece = s.read_piece(1).unwrap();
        assert_eq!(piece, data);
        fs::remove_dir_all(&dir).unwrap();
    }
}
