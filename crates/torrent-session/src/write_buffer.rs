use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use torrent_core::Id20;

/// Buffers write blocks in memory, flushes sequentially on piece completion.
#[allow(dead_code)] // consumed by DiskActor in a later commit
pub(crate) struct WriteBuffer {
    /// (torrent, piece) → sorted blocks by offset.
    pending: HashMap<(Id20, u32), BTreeMap<u32, Bytes>>,
    total_bytes: usize,
    max_bytes: usize,
}

#[allow(dead_code)] // consumed by DiskActor in a later commit
impl WriteBuffer {
    pub fn new(max_bytes: usize) -> Self {
        WriteBuffer {
            pending: HashMap::new(),
            total_bytes: 0,
            max_bytes,
        }
    }

    /// Buffer a write block.
    pub fn write(&mut self, info_hash: Id20, piece: u32, begin: u32, data: Bytes) {
        self.total_bytes += data.len();
        self.pending
            .entry((info_hash, piece))
            .or_default()
            .insert(begin, data);
    }

    /// Take all buffered blocks for a piece, removing them from the buffer.
    pub fn take_piece(&mut self, info_hash: Id20, piece: u32) -> Option<Vec<(u32, Bytes)>> {
        let blocks = self.pending.remove(&(info_hash, piece))?;
        let result: Vec<(u32, Bytes)> = blocks.into_iter().collect();
        let freed: usize = result.iter().map(|(_, b)| b.len()).sum();
        self.total_bytes -= freed;
        Some(result)
    }

    /// Try to read a contiguous range from the buffer.
    ///
    /// Returns `Some` only if the exact range `[begin..begin+length)` is
    /// covered by a single buffered block.
    pub fn read(&self, info_hash: Id20, piece: u32, begin: u32, length: u32) -> Option<Bytes> {
        let blocks = self.pending.get(&(info_hash, piece))?;

        // Check if there's a single block that covers the entire range
        if let Some(data) = blocks.get(&begin)
            && data.len() >= length as usize
        {
            return Some(data.slice(..length as usize));
        }

        // Try to assemble from contiguous blocks
        let mut buf = Vec::with_capacity(length as usize);
        let mut pos = begin;
        let end = begin + length;

        for (&block_begin, block_data) in blocks.range(begin..) {
            if block_begin != pos {
                return None; // gap
            }
            let take = ((end - pos) as usize).min(block_data.len());
            buf.extend_from_slice(&block_data[..take]);
            pos += take as u32;
            if pos >= end {
                break;
            }
        }

        if buf.len() == length as usize {
            Some(Bytes::from(buf))
        } else {
            None
        }
    }

    /// Clear all buffered blocks for a piece.
    pub fn clear_piece(&mut self, info_hash: Id20, piece: u32) {
        if let Some(blocks) = self.pending.remove(&(info_hash, piece)) {
            let freed: usize = blocks.values().map(|b| b.len()).sum();
            self.total_bytes -= freed;
        }
    }

    /// Clear all buffered blocks for a torrent.
    pub fn clear_torrent(&mut self, info_hash: Id20) {
        let keys: Vec<(Id20, u32)> = self
            .pending
            .keys()
            .filter(|(ih, _)| *ih == info_hash)
            .copied()
            .collect();
        for key in keys {
            if let Some(blocks) = self.pending.remove(&key) {
                let freed: usize = blocks.values().map(|b| b.len()).sum();
                self.total_bytes -= freed;
            }
        }
    }

    /// Total bytes currently buffered.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Whether the buffer has exceeded its capacity.
    pub fn needs_flush(&self) -> bool {
        self.total_bytes >= self.max_bytes
    }

    /// Return the oldest buffered piece (first key in iteration order).
    pub fn oldest_piece(&self) -> Option<(Id20, u32)> {
        self.pending.keys().next().copied()
    }

    /// Iterate all pending (info_hash, piece) keys.
    pub fn pending_keys(&self) -> impl Iterator<Item = (Id20, u32)> + '_ {
        self.pending.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(n: u8) -> Id20 {
        let mut b = [0u8; 20];
        b[0] = n;
        Id20(b)
    }

    #[test]
    fn buffer_and_flush() {
        let mut wb = WriteBuffer::new(1024 * 1024);
        let ih = hash(1);
        wb.write(ih, 0, 0, Bytes::from(vec![1u8; 100]));
        wb.write(ih, 0, 100, Bytes::from(vec![2u8; 100]));

        assert_eq!(wb.total_bytes(), 200);

        let blocks = wb.take_piece(ih, 0).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(wb.total_bytes(), 0);
    }

    #[test]
    fn read_from_buffer() {
        let mut wb = WriteBuffer::new(1024 * 1024);
        let ih = hash(1);
        wb.write(ih, 0, 0, Bytes::from(vec![42u8; 50]));

        let data = wb.read(ih, 0, 0, 50);
        assert_eq!(data, Some(Bytes::from(vec![42u8; 50])));

        let data = wb.read(ih, 0, 0, 100);
        assert_eq!(data, None); // not fully buffered
    }

    #[test]
    fn read_contiguous_blocks() {
        let mut wb = WriteBuffer::new(1024 * 1024);
        let ih = hash(1);
        wb.write(ih, 0, 0, Bytes::from(vec![1u8; 25]));
        wb.write(ih, 0, 25, Bytes::from(vec![2u8; 25]));

        let data = wb.read(ih, 0, 0, 50).unwrap();
        assert_eq!(&data[..25], &[1u8; 25]);
        assert_eq!(&data[25..], &[2u8; 25]);
    }

    #[test]
    fn clear_piece() {
        let mut wb = WriteBuffer::new(1024 * 1024);
        let ih = hash(1);
        wb.write(ih, 0, 0, Bytes::from(vec![1u8; 100]));
        wb.write(ih, 1, 0, Bytes::from(vec![2u8; 100]));

        wb.clear_piece(ih, 0);
        assert_eq!(wb.total_bytes(), 100);
        assert!(wb.take_piece(ih, 0).is_none());
        assert!(wb.take_piece(ih, 1).is_some());
    }

    #[test]
    fn clear_torrent() {
        let mut wb = WriteBuffer::new(1024 * 1024);
        let ih1 = hash(1);
        let ih2 = hash(2);
        wb.write(ih1, 0, 0, Bytes::from(vec![1u8; 100]));
        wb.write(ih1, 1, 0, Bytes::from(vec![2u8; 100]));
        wb.write(ih2, 0, 0, Bytes::from(vec![3u8; 100]));

        wb.clear_torrent(ih1);
        assert_eq!(wb.total_bytes(), 100);
        assert!(wb.take_piece(ih2, 0).is_some());
    }

    #[test]
    fn needs_flush() {
        let mut wb = WriteBuffer::new(200);
        let ih = hash(1);
        wb.write(ih, 0, 0, Bytes::from(vec![1u8; 100]));
        assert!(!wb.needs_flush());
        wb.write(ih, 0, 100, Bytes::from(vec![2u8; 100]));
        assert!(wb.needs_flush());
    }
}
