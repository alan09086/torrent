use std::collections::HashMap;

use bytes::Bytes;
use ferrite_core::Id20;

/// BEP 9 metadata piece size: 16 KiB.
const METADATA_PIECE_SIZE: u64 = 16384;

/// State machine for downloading torrent metadata via BEP 9 (magnet links).
///
/// Metadata is split into 16 KiB pieces. Once all pieces are received,
/// they are assembled and verified against the info_hash.
#[allow(dead_code)]
pub(crate) struct MetadataDownloader {
    info_hash: Id20,
    total_size: Option<u64>,
    pieces: HashMap<u32, Bytes>,
    num_pieces: Option<u32>,
}

#[allow(dead_code)]
impl MetadataDownloader {
    /// Create a new downloader with no size known yet.
    pub fn new(info_hash: Id20) -> Self {
        Self {
            info_hash,
            total_size: None,
            pieces: HashMap::new(),
            num_pieces: None,
        }
    }

    /// Set the total metadata size and calculate the number of pieces.
    pub fn set_total_size(&mut self, size: u64) {
        self.total_size = Some(size);
        self.num_pieces = Some(size.div_ceil(METADATA_PIECE_SIZE) as u32);
    }

    /// Store a received piece. Returns `true` if all pieces have been received.
    pub fn piece_received(&mut self, piece: u32, data: Bytes) -> bool {
        self.pieces.insert(piece, data);
        match self.num_pieces {
            Some(n) => self.pieces.len() == n as usize,
            None => false,
        }
    }

    /// Concatenate all pieces in order and verify the SHA1 hash matches info_hash.
    ///
    /// Returns the assembled metadata bytes on success.
    pub fn assemble_and_verify(&self) -> crate::Result<Vec<u8>> {
        let num_pieces = self.num_pieces.ok_or_else(|| {
            crate::Error::Connection("metadata incomplete".to_string())
        })?;

        if self.pieces.len() != num_pieces as usize {
            return Err(crate::Error::Connection("metadata incomplete".to_string()));
        }

        let mut assembled = Vec::with_capacity(
            self.total_size.unwrap_or(0) as usize,
        );
        for i in 0..num_pieces {
            let piece = self.pieces.get(&i).ok_or_else(|| {
                crate::Error::Connection("metadata incomplete".to_string())
            })?;
            assembled.extend_from_slice(piece);
        }

        let hash = ferrite_core::sha1(&assembled);
        if hash != self.info_hash {
            return Err(crate::Error::MetadataHashMismatch);
        }

        Ok(assembled)
    }

    /// Return sorted list of piece indices we don't have yet.
    ///
    /// Returns an empty vec if `num_pieces` is not yet known.
    pub fn missing_pieces(&self) -> Vec<u32> {
        match self.num_pieces {
            None => Vec::new(),
            Some(n) => {
                (0..n).filter(|i| !self.pieces.contains_key(i)).collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_core::Id20;

    #[test]
    fn new_empty() {
        let info_hash = Id20::ZERO;
        let dl = MetadataDownloader::new(info_hash);
        assert!(dl.total_size.is_none());
        assert!(dl.num_pieces.is_none());
        assert!(dl.pieces.is_empty());
    }

    #[test]
    fn set_total_size_calculates_num_pieces() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);

        dl.set_total_size(32768);
        assert_eq!(dl.num_pieces, Some(2));

        dl.set_total_size(16384);
        assert_eq!(dl.num_pieces, Some(1));

        dl.set_total_size(16385);
        assert_eq!(dl.num_pieces, Some(2));
    }

    #[test]
    fn single_piece_metadata() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(100);
        let complete = dl.piece_received(0, Bytes::from(vec![0u8; 100]));
        assert!(complete);
    }

    #[test]
    fn multi_piece_metadata() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(32768); // 2 pieces

        let complete = dl.piece_received(0, Bytes::from(vec![0u8; 16384]));
        assert!(!complete);

        let complete = dl.piece_received(1, Bytes::from(vec![0u8; 16384]));
        assert!(complete);
    }

    #[test]
    fn piece_received_returns_false_when_incomplete() {
        let mut dl = MetadataDownloader::new(Id20::ZERO);
        dl.set_total_size(32768); // 2 pieces

        let complete = dl.piece_received(0, Bytes::from(vec![0u8; 16384]));
        assert!(!complete);
    }

    #[test]
    fn assemble_and_verify_correct_hash() {
        // Create known test data that fits in a single piece.
        let data = b"hello world metadata test data!!";
        let info_hash = ferrite_core::sha1(data);

        let mut dl = MetadataDownloader::new(info_hash);
        dl.set_total_size(data.len() as u64);
        dl.piece_received(0, Bytes::from(data.to_vec()));

        let result = dl.assemble_and_verify().unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn assemble_and_verify_wrong_hash() {
        let data = b"hello world metadata test data!!";
        // Use a wrong info_hash (all zeros will not match).
        let wrong_hash = Id20::ZERO;

        let mut dl = MetadataDownloader::new(wrong_hash);
        dl.set_total_size(data.len() as u64);
        dl.piece_received(0, Bytes::from(data.to_vec()));

        let result = dl.assemble_and_verify();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::Error::MetadataHashMismatch),
            "expected MetadataHashMismatch, got: {err:?}"
        );
    }
}
