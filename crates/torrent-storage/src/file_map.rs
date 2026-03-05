use torrent_core::Lengths;

/// A contiguous segment within a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSegment {
    /// Index into the file list.
    pub file_index: usize,
    /// Byte offset within the file.
    pub file_offset: u64,
    /// Length of this segment in bytes.
    pub len: u32,
}

/// Maps piece/chunk coordinates to file segments using pre-computed cumulative offsets.
///
/// Binary search gives O(log n) file lookup instead of linear scan.
#[derive(Debug, Clone)]
pub struct FileMap {
    /// Cumulative start offset of each file in the torrent's byte space.
    file_offsets: Vec<u64>,
    /// Length of each file.
    file_lengths: Vec<u64>,
    /// Piece/chunk arithmetic.
    lengths: Lengths,
}

impl FileMap {
    /// Create a new FileMap from file lengths and piece arithmetic.
    pub fn new(file_lengths: Vec<u64>, lengths: Lengths) -> Self {
        let mut file_offsets = Vec::with_capacity(file_lengths.len());
        let mut cumulative = 0u64;
        for &len in &file_lengths {
            file_offsets.push(cumulative);
            cumulative += len;
        }
        FileMap {
            file_offsets,
            file_lengths,
            lengths,
        }
    }

    /// Map an absolute byte range to file segments.
    pub fn byte_range_to_segments(&self, offset: u64, length: u32) -> Vec<FileSegment> {
        if length == 0 || self.file_lengths.is_empty() {
            return Vec::new();
        }

        let mut segments = Vec::new();
        let mut remaining = length as u64;
        let mut pos = offset;

        while remaining > 0 {
            // Binary search: find the file containing `pos`.
            let file_idx = match self.file_offsets.binary_search(&pos) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            };

            if file_idx >= self.file_lengths.len() {
                break;
            }

            let file_start = self.file_offsets[file_idx];
            let file_len = self.file_lengths[file_idx];
            let file_offset = pos - file_start;

            // How much of this file can we use from `file_offset`?
            let available = file_len - file_offset;
            let take = remaining.min(available);

            if take > 0 {
                segments.push(FileSegment {
                    file_index: file_idx,
                    file_offset,
                    len: take as u32,
                });
            }

            pos += take;
            remaining -= take;
        }

        segments
    }

    /// Map a chunk (piece, begin, length) to file segments.
    pub fn chunk_segments(&self, piece: u32, begin: u32, length: u32) -> Vec<FileSegment> {
        let abs_offset = self.lengths.piece_offset(piece) + begin as u64;
        self.byte_range_to_segments(abs_offset, length)
    }

    /// Map an entire piece to file segments.
    pub fn piece_segments(&self, piece: u32) -> Vec<FileSegment> {
        let abs_offset = self.lengths.piece_offset(piece);
        let piece_size = self.lengths.piece_size(piece);
        self.byte_range_to_segments(abs_offset, piece_size)
    }

    /// Number of files.
    pub fn num_files(&self) -> usize {
        self.file_lengths.len()
    }

    /// Length of a specific file.
    pub fn file_length(&self, index: usize) -> u64 {
        self.file_lengths[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::Lengths;

    #[test]
    fn single_file() {
        // 1 MiB single file, 256 KiB pieces, 16 KiB chunks
        let lengths = Lengths::new(1048576, 262144, 16384);
        let fm = FileMap::new(vec![1048576], lengths);

        let segs = fm.piece_segments(0);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].file_index, 0);
        assert_eq!(segs[0].file_offset, 0);
        assert_eq!(segs[0].len, 262144);
    }

    #[test]
    fn multi_file_no_span() {
        // Two files of 262144 each (exactly 1 piece each)
        let lengths = Lengths::new(524288, 262144, 16384);
        let fm = FileMap::new(vec![262144, 262144], lengths);

        let segs0 = fm.piece_segments(0);
        assert_eq!(segs0.len(), 1);
        assert_eq!(segs0[0].file_index, 0);

        let segs1 = fm.piece_segments(1);
        assert_eq!(segs1.len(), 1);
        assert_eq!(segs1[0].file_index, 1);
    }

    #[test]
    fn chunk_spans_boundary() {
        // Two files: 100 bytes and 200 bytes, piece size 300, chunk size 150
        let lengths = Lengths::new(300, 300, 150);
        let fm = FileMap::new(vec![100, 200], lengths);

        // Chunk at begin=0, length=150 → first 100 in file 0, next 50 in file 1
        let segs = fm.chunk_segments(0, 0, 150);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0], FileSegment { file_index: 0, file_offset: 0, len: 100 });
        assert_eq!(segs[1], FileSegment { file_index: 1, file_offset: 0, len: 50 });
    }

    #[test]
    fn piece_spans_three_files() {
        // Three files: 100, 50, 150 bytes. Piece size = 300 (one piece, spans all files)
        let lengths = Lengths::new(300, 300, 16384);
        let fm = FileMap::new(vec![100, 50, 150], lengths);

        let segs = fm.piece_segments(0);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0], FileSegment { file_index: 0, file_offset: 0, len: 100 });
        assert_eq!(segs[1], FileSegment { file_index: 1, file_offset: 0, len: 50 });
        assert_eq!(segs[2], FileSegment { file_index: 2, file_offset: 0, len: 150 });
    }

    #[test]
    fn last_piece_shorter() {
        // 500 bytes total, 300 byte pieces → piece 0 = 300, piece 1 = 200
        let lengths = Lengths::new(500, 300, 16384);
        let fm = FileMap::new(vec![500], lengths);

        let segs = fm.piece_segments(1);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].file_offset, 300);
        assert_eq!(segs[0].len, 200);
    }

    #[test]
    fn zero_length_file() {
        // Files: 0 bytes, 100 bytes. Total = 100, one piece.
        let lengths = Lengths::new(100, 100, 16384);
        let fm = FileMap::new(vec![0, 100], lengths);

        let segs = fm.piece_segments(0);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].file_index, 1);
        assert_eq!(segs[0].file_offset, 0);
        assert_eq!(segs[0].len, 100);
    }

    #[test]
    fn byte_range_single() {
        let lengths = Lengths::new(1000, 500, 16384);
        let fm = FileMap::new(vec![1000], lengths);

        let segs = fm.byte_range_to_segments(100, 50);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].file_offset, 100);
        assert_eq!(segs[0].len, 50);
    }

    #[test]
    fn piece_segments_second_piece_multi_file() {
        // Files: 400, 600. Piece size 500. Piece 1 starts at offset 500.
        // Piece 1: file 0 bytes 400..400 (0 bytes) → actually starts in file 1 offset 100
        let lengths = Lengths::new(1000, 500, 16384);
        let fm = FileMap::new(vec![400, 600], lengths);

        let segs = fm.piece_segments(1);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].file_index, 1);
        assert_eq!(segs[0].file_offset, 100);
        assert_eq!(segs[0].len, 500);
    }
}
