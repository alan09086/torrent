//! Per-file Merkle tree verification state for BEP 52 downloads.
//!
//! Tracks which blocks have been verified against the file's Merkle tree.
//! Handles the case where block data arrives before piece-layer hashes
//! (deferred verification via `SetBlockResult::Unknown`).

use crate::{Id32, MerkleTree};

/// Result of setting a block hash in the Merkle tree state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetBlockResult {
    /// Block hash matches the Merkle tree — all blocks in the piece verified.
    Ok,
    /// Piece-layer hash not yet available, or not all sibling blocks present;
    /// block hash stored for deferred verification.
    Unknown,
    /// Block hash doesn't match the Merkle tree (bad peer).
    HashFailed,
}

/// Per-file Merkle tree verification state for BEP 52 downloads.
///
/// Tracks which blocks have been verified against the file's Merkle tree.
/// Handles the case where block data arrives before piece-layer hashes
/// (deferred verification via `SetBlockResult::Unknown`).
pub struct MerkleTreeState {
    /// File root hash (from torrent metadata, immutable).
    root: Id32,
    /// Total number of 16 KiB blocks in this file.
    num_blocks: u32,
    /// Total number of pieces in this file.
    num_pieces: u32,
    /// Piece-layer hashes (one per piece). None until received.
    piece_hashes: Option<Vec<Id32>>,
    /// Block-layer hashes (one per block). Populated during download.
    block_hashes: Vec<Option<Id32>>,
    /// Which blocks have been verified against the Merkle tree.
    /// Uses Vec<bool> to avoid circular dependency on ferrite-storage::Bitfield.
    block_verified: Vec<bool>,
}

impl MerkleTreeState {
    /// Create a new empty tree state for a file.
    pub fn new(root: Id32, num_blocks: u32, num_pieces: u32) -> Self {
        MerkleTreeState {
            root,
            num_blocks,
            num_pieces,
            piece_hashes: None,
            block_hashes: vec![None; num_blocks as usize],
            block_verified: vec![false; num_blocks as usize],
        }
    }

    /// The file's Merkle root hash.
    pub fn root(&self) -> Id32 {
        self.root
    }

    /// Total number of pieces.
    pub fn num_pieces(&self) -> u32 {
        self.num_pieces
    }

    /// Total number of blocks.
    pub fn num_blocks(&self) -> u32 {
        self.num_blocks
    }

    /// Whether the piece-layer hash is available for a given piece.
    pub fn has_piece_hash(&self, piece_index: u32) -> bool {
        self.piece_hashes
            .as_ref()
            .is_some_and(|ph| (piece_index as usize) < ph.len())
    }

    /// Set piece-layer hashes (from hash response).
    pub fn set_piece_hashes(&mut self, hashes: Vec<Id32>) {
        self.piece_hashes = Some(hashes);
    }

    /// Set piece-layer hashes and retroactively verify any stored block hashes.
    ///
    /// Returns a list of piece indices that were fully verified by this operation.
    pub fn set_piece_hashes_and_verify(
        &mut self,
        hashes: Vec<Id32>,
        blocks_per_piece: u32,
    ) -> Vec<u32> {
        self.piece_hashes = Some(hashes);

        let mut verified_pieces = Vec::new();

        for piece in 0..self.num_pieces {
            let first_block = piece * blocks_per_piece;
            let last_block = ((piece + 1) * blocks_per_piece).min(self.num_blocks);

            // Check if all blocks in this piece have stored hashes
            let all_stored = (first_block..last_block)
                .all(|b| self.block_hashes[b as usize].is_some());

            if !all_stored {
                continue;
            }

            // Collect stored block hashes and verify against piece hash
            let block_slice: Vec<Id32> = (first_block..last_block)
                .map(|b| self.block_hashes[b as usize].unwrap())
                .collect();

            let piece_hash = MerkleTree::root_from_hashes(&block_slice);
            if let Some(ref ph) = self.piece_hashes {
                if piece as usize >= ph.len() {
                    continue;
                }
                if piece_hash == ph[piece as usize] {
                    for b in first_block..last_block {
                        self.block_verified[b as usize] = true;
                    }
                    verified_pieces.push(piece);
                }
            }
        }

        verified_pieces
    }

    /// Record a computed block hash. Returns the verification result.
    ///
    /// - `Ok`: all blocks in the piece verified against piece-layer hash.
    /// - `Unknown`: piece-layer hash unavailable, or not all sibling blocks present yet.
    /// - `HashFailed`: block hash doesn't match the Merkle tree.
    pub fn set_block_hash(&mut self, block_index: u32, hash: Id32) -> SetBlockResult {
        if block_index >= self.num_blocks {
            return SetBlockResult::HashFailed;
        }

        // Store the block hash for potential deferred verification
        self.block_hashes[block_index as usize] = Some(hash);

        let piece_hashes = match &self.piece_hashes {
            Some(ph) => ph,
            None => return SetBlockResult::Unknown,
        };

        // Determine which piece this block belongs to
        let blocks_per_piece = if self.num_pieces > 0 {
            self.num_blocks.div_ceil(self.num_pieces)
        } else {
            return SetBlockResult::Unknown;
        };
        let piece_index = block_index / blocks_per_piece;

        if piece_index as usize >= piece_hashes.len() {
            return SetBlockResult::HashFailed;
        }

        // Check if all blocks for this piece have hashes
        let first_block = piece_index * blocks_per_piece;
        let last_block = ((piece_index + 1) * blocks_per_piece).min(self.num_blocks);

        let all_present = (first_block..last_block)
            .all(|b| self.block_hashes[b as usize].is_some());

        if !all_present {
            // Gap 3 fix: return Unknown, not Ok — block hash stored but can't
            // verify until all sibling blocks arrive for the full sub-tree.
            return SetBlockResult::Unknown;
        }

        // All blocks present — verify piece hash
        let block_slice: Vec<Id32> = (first_block..last_block)
            .map(|b| self.block_hashes[b as usize].unwrap())
            .collect();

        let computed = MerkleTree::root_from_hashes(&block_slice);
        if computed == piece_hashes[piece_index as usize] {
            for b in first_block..last_block {
                self.block_verified[b as usize] = true;
            }
            SetBlockResult::Ok
        } else {
            SetBlockResult::HashFailed
        }
    }

    /// Check if a specific block has been verified.
    pub fn is_block_verified(&self, block_index: u32) -> bool {
        self.block_verified
            .get(block_index as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Check if all blocks in a piece have been verified.
    pub fn piece_verified(&self, piece_index: u32, blocks_per_piece: u32) -> bool {
        let first = piece_index * blocks_per_piece;
        let last = ((piece_index + 1) * blocks_per_piece).min(self.num_blocks);
        (first..last).all(|b| self.block_verified[b as usize])
    }

    /// Get a reference to the piece hashes, if available.
    pub fn piece_hashes(&self) -> Option<&[Id32]> {
        self.piece_hashes.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sha256, Id32, MerkleTree};

    fn make_block_hashes(n: usize) -> Vec<Id32> {
        (0..n).map(|i| sha256(&[i as u8])).collect()
    }

    #[test]
    fn new_state_has_no_piece_hashes() {
        let state = MerkleTreeState::new(Id32::ZERO, 16, 1);
        assert!(!state.has_piece_hash(0));
    }

    #[test]
    fn set_piece_hashes_and_query() {
        let block_hashes = make_block_hashes(4);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let pieces = tree.piece_layer(2).to_vec(); // 2 blocks per piece → 2 piece hashes

        let mut state = MerkleTreeState::new(tree.root(), 4, 2);
        state.set_piece_hashes(pieces);
        assert!(state.has_piece_hash(0));
        assert!(state.has_piece_hash(1));
        assert!(!state.has_piece_hash(2)); // out of range
    }

    #[test]
    fn set_block_hash_ok_when_all_siblings_present() {
        // Build a tree: 4 blocks, 2 per piece
        let block_hashes = make_block_hashes(4);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let pieces = tree.piece_layer(2).to_vec();

        let mut state = MerkleTreeState::new(tree.root(), 4, 2);
        state.set_piece_hashes(pieces);

        // First block → Unknown (sibling not present yet)
        let result = state.set_block_hash(0, block_hashes[0]);
        assert_eq!(result, SetBlockResult::Unknown);
        assert!(!state.is_block_verified(0));

        // Second block completes the piece → Ok
        let result = state.set_block_hash(1, block_hashes[1]);
        assert_eq!(result, SetBlockResult::Ok);
        assert!(state.is_block_verified(0));
        assert!(state.is_block_verified(1));
    }

    #[test]
    fn set_block_hash_unknown_no_piece_hashes() {
        // No piece hashes loaded → unknown
        let mut state = MerkleTreeState::new(Id32::ZERO, 4, 2);
        let hash = sha256(b"block0");
        let result = state.set_block_hash(0, hash);
        assert_eq!(result, SetBlockResult::Unknown);
        assert!(!state.is_block_verified(0));
    }

    #[test]
    fn set_block_hash_failed() {
        let block_hashes = make_block_hashes(4);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let pieces = tree.piece_layer(2).to_vec();

        let mut state = MerkleTreeState::new(tree.root(), 4, 2);
        state.set_piece_hashes(pieces);

        // Set correct first block
        state.set_block_hash(0, block_hashes[0]);
        // Set wrong second block → fails when piece hash is computed
        let wrong_hash = sha256(b"wrong");
        let result = state.set_block_hash(1, wrong_hash);
        assert_eq!(result, SetBlockResult::HashFailed);
    }

    #[test]
    fn piece_verified_after_all_blocks() {
        let block_hashes = make_block_hashes(4);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let pieces = tree.piece_layer(2).to_vec();

        let mut state = MerkleTreeState::new(tree.root(), 4, 2);
        state.set_piece_hashes(pieces);

        // Verify blocks 0,1 (piece 0)
        assert_eq!(state.set_block_hash(0, block_hashes[0]), SetBlockResult::Unknown);
        assert!(!state.piece_verified(0, 2)); // only 1 of 2 blocks
        assert_eq!(state.set_block_hash(1, block_hashes[1]), SetBlockResult::Ok);
        assert!(state.piece_verified(0, 2)); // both blocks done
    }

    #[test]
    fn deferred_verification_unknown_then_ok() {
        // Block hashes arrive before piece-layer hashes
        let block_hashes = make_block_hashes(4);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let pieces = tree.piece_layer(2).to_vec();

        let mut state = MerkleTreeState::new(tree.root(), 4, 2);

        // Blocks arrive → Unknown (no piece hashes yet)
        assert_eq!(state.set_block_hash(0, block_hashes[0]), SetBlockResult::Unknown);
        assert_eq!(state.set_block_hash(1, block_hashes[1]), SetBlockResult::Unknown);

        // Piece hashes arrive → retroactively verify stored blocks
        let verified = state.set_piece_hashes_and_verify(pieces, 2);
        assert_eq!(verified.len(), 1);
        assert!(verified.contains(&0u32)); // piece 0 now verified
        assert!(state.is_block_verified(0));
        assert!(state.is_block_verified(1));
    }
}
