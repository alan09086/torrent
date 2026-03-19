//! Hash request coordination for BEP 52 (Merkle tree hash downloading).
//!
//! `HashPicker` tracks which piece-layer and block-layer hashes are needed,
//! generates `HashRequest`s for peers, and validates received hashes.
//!
//! **Priority:** block hash requests (reactive, for failed pieces) take
//! precedence over piece-layer requests (proactive, 512-piece batches).
//!
//! **Limitation:** Currently assumes single-file v2 torrents (file_index=0).
//! Multi-file piece-to-file mapping will be added in a future milestone.

use std::time::Instant;

use crate::hash_request::HashRequest;
use crate::merkle_state::{MerkleTreeState, SetBlockResult};
use crate::{Id32, MerkleTree};

/// File info needed to initialize the hash picker.
#[derive(Debug, Clone)]
pub struct FileHashInfo {
    /// Merkle root hash for this file.
    pub root: Id32,
    /// Total number of 16 KiB blocks in the file.
    pub num_blocks: u32,
    /// Total number of pieces in the file.
    pub num_pieces: u32,
}

/// Result of adding received hashes to the picker.
#[derive(Debug)]
pub struct AddHashesResult {
    /// Whether the hashes passed Merkle proof validation.
    pub valid: bool,
    /// Piece indices that passed deferred verification.
    pub hash_passed: Vec<u32>,
    /// Piece indices that failed deferred verification.
    pub hash_failed: Vec<u32>,
}

struct PieceHashRequest {
    last_request: Option<Instant>,
    have: bool,
}

struct BlockHashRequest {
    file_index: usize,
    piece: u32,
}

/// Coordinates which Merkle hash requests to send to peers.
///
/// Priority: (1) block hashes for urgent/failed pieces,
/// (2) piece-layer hashes in 512-chunk batches.
pub struct HashPicker {
    pub(crate) trees: Vec<MerkleTreeState>,
    file_roots: Vec<Id32>,
    piece_requests: Vec<Vec<PieceHashRequest>>,
    block_requests: Vec<BlockHashRequest>,
    piece_layer: u32,
    blocks_per_piece: u32,
}

impl HashPicker {
    /// Create a new hash picker for the given files.
    ///
    /// `blocks_per_piece` is `piece_length / 16384`.
    pub fn new(files: &[FileHashInfo], blocks_per_piece: u32) -> Self {
        let piece_layer = blocks_per_piece.trailing_zeros();
        let trees: Vec<_> = files
            .iter()
            .map(|f| MerkleTreeState::new(f.root, f.num_blocks, f.num_pieces))
            .collect();

        let piece_requests: Vec<Vec<PieceHashRequest>> = files
            .iter()
            .map(|f| {
                let chunks = f.num_pieces.div_ceil(512) as usize;
                (0..chunks)
                    .map(|_| PieceHashRequest {
                        last_request: None,
                        have: false,
                    })
                    .collect()
            })
            .collect();

        let file_roots = files.iter().map(|f| f.root).collect();

        HashPicker {
            trees,
            file_roots,
            piece_requests,
            block_requests: Vec::new(),
            piece_layer,
            blocks_per_piece,
        }
    }

    /// Pick the next hash request to send.
    ///
    /// `has_piece` checks if the peer has a given piece index.
    /// Priority: block requests (for failed/urgent pieces) > piece-layer requests.
    pub fn pick_hashes(&self, has_piece: impl Fn(u32) -> bool) -> Option<HashRequest> {
        // Priority 1: block hash requests (reactive, for piece failures)
        if let Some(req) = self.block_requests.first() {
            let first_block = req.piece * self.blocks_per_piece;
            return Some(HashRequest {
                file_root: self.file_roots[req.file_index],
                base: 0,
                index: first_block,
                count: self.blocks_per_piece,
                proof_layers: self.piece_layer + 1,
            });
        }

        // Priority 2: piece-layer requests (512 pieces at a time)
        for (file_idx, requests) in self.piece_requests.iter().enumerate() {
            for (chunk_idx, req) in requests.iter().enumerate() {
                if req.have {
                    continue;
                }

                // Check if peer has any piece in this 512-chunk
                let first_piece = chunk_idx as u32 * 512;
                let has_relevant = (first_piece..)
                    .take(512)
                    .take_while(|&p| p < self.trees[file_idx].num_pieces())
                    .any(&has_piece);

                if !has_relevant {
                    continue;
                }

                let remaining = self.trees[file_idx]
                    .num_pieces()
                    .saturating_sub(first_piece);
                let count = 512.min(remaining.next_power_of_two());

                // Gap 4 fix: calculate proof_layers from tree depth
                let tree_depth = self.trees[file_idx]
                    .num_blocks()
                    .next_power_of_two()
                    .trailing_zeros();
                let proof_layers = tree_depth.saturating_sub(self.piece_layer);

                return Some(HashRequest {
                    file_root: self.file_roots[file_idx],
                    base: self.piece_layer,
                    index: first_piece,
                    count,
                    proof_layers,
                });
            }
        }

        None
    }

    /// Process received hashes from a peer.
    ///
    /// For piece-layer hashes, validates the uncle proof against the known file
    /// root before accepting (Gap 1 fix). For block-layer hashes, stores them
    /// directly in the tree state.
    pub fn add_hashes(
        &mut self,
        req: &HashRequest,
        hashes: &[Id32],
    ) -> Result<AddHashesResult, crate::Error> {
        let file_idx = self
            .file_roots
            .iter()
            .position(|r| *r == req.file_root)
            .ok_or_else(|| crate::Error::InvalidTorrent("unknown file root".into()))?;

        if req.base == self.piece_layer {
            // Piece-layer hashes — validate proof before accepting (Gap 1)
            let base_count = req.count as usize;

            if hashes.len() < base_count {
                return Ok(AddHashesResult {
                    valid: false,
                    hash_passed: Vec::new(),
                    hash_failed: Vec::new(),
                });
            }

            let base_hashes = &hashes[..base_count];
            let uncle_hashes = &hashes[base_count..];

            // Verify the received hashes against the file root via uncle proof
            if req.proof_layers > 0 && !uncle_hashes.is_empty() {
                let sub_root = MerkleTree::root_from_hashes(base_hashes);
                let leaf_index = req.index as usize / base_count;
                if !MerkleTree::verify_proof(
                    self.trees[file_idx].root(),
                    sub_root,
                    leaf_index,
                    uncle_hashes,
                ) {
                    return Ok(AddHashesResult {
                        valid: false,
                        hash_passed: Vec::new(),
                        hash_failed: Vec::new(),
                    });
                }
            }

            // Proof passed (or no proof needed) — accept the hashes
            let verified = self.trees[file_idx]
                .set_piece_hashes_and_verify(base_hashes.to_vec(), self.blocks_per_piece);

            // Mark the 512-chunk as received
            let chunk_idx = req.index as usize / 512;
            if chunk_idx < self.piece_requests[file_idx].len() {
                self.piece_requests[file_idx][chunk_idx].have = true;
            }

            Ok(AddHashesResult {
                valid: true,
                hash_passed: verified,
                hash_failed: Vec::new(),
            })
        } else {
            // Block-layer or other layer hashes — store directly
            let mut hash_passed = Vec::new();
            let mut hash_failed = Vec::new();

            for (i, hash) in hashes.iter().enumerate() {
                let block_index = req.index + i as u32;
                match self.trees[file_idx].set_block_hash(block_index, *hash) {
                    SetBlockResult::Ok => {
                        let piece = block_index / self.blocks_per_piece;
                        if !hash_passed.contains(&piece) {
                            hash_passed.push(piece);
                        }
                    }
                    SetBlockResult::HashFailed => {
                        let piece = block_index / self.blocks_per_piece;
                        if !hash_failed.contains(&piece) {
                            hash_failed.push(piece);
                        }
                    }
                    SetBlockResult::Unknown => {}
                }
            }

            // Remove matching block request
            self.block_requests.retain(|r| {
                r.file_index != file_idx || r.piece != req.index / self.blocks_per_piece
            });

            Ok(AddHashesResult {
                valid: true,
                hash_passed,
                hash_failed,
            })
        }
    }

    /// Record a computed block hash. Delegates to MerkleTreeState.
    ///
    /// Gap 10 fix: removed unused `offset` parameter from original plan.
    pub fn set_block_hash(
        &mut self,
        file_index: usize,
        block_index: u32,
        hash: Id32,
    ) -> SetBlockResult {
        if file_index >= self.trees.len() {
            return SetBlockResult::HashFailed;
        }
        self.trees[file_index].set_block_hash(block_index, hash)
    }

    /// Request block hashes for a piece that failed verification.
    pub fn verify_block_hashes(&mut self, piece: u32) {
        // Gap 6: single-file assumption — multi-file mapping deferred to M35
        let file_index = 0;
        if self.trees.is_empty() {
            return;
        }

        let existing = self
            .block_requests
            .iter()
            .any(|r| r.file_index == file_index && r.piece == piece);

        if !existing {
            self.block_requests
                .push(BlockHashRequest { file_index, piece });
        }
    }

    /// Mark a hash request as rejected (peer couldn't serve it).
    pub fn hashes_rejected(&mut self, req: &HashRequest) {
        if req.base == self.piece_layer {
            let chunk_idx = req.index as usize / 512;
            let file_idx = self.file_roots.iter().position(|r| *r == req.file_root);
            if let Some(fi) = file_idx
                && chunk_idx < self.piece_requests[fi].len()
            {
                self.piece_requests[fi][chunk_idx].last_request = None;
            }
        }
    }

    /// Do we have the piece-layer hash for a specific piece?
    pub fn have_piece_hash(&self, file_index: usize, piece: u32) -> bool {
        self.trees
            .get(file_index)
            .is_some_and(|t| t.has_piece_hash(piece))
    }

    /// Are all blocks in a piece verified?
    pub fn piece_verified(&self, file_index: usize, piece: u32) -> bool {
        self.trees
            .get(file_index)
            .is_some_and(|t| t.piece_verified(piece, self.blocks_per_piece))
    }

    /// Tree depth for a file (number of layers from root to block leaves).
    pub fn tree_depth(&self, file_index: usize) -> Option<u32> {
        self.trees
            .get(file_index)
            .map(|t| t.num_blocks().next_power_of_two().trailing_zeros())
    }

    /// Pre-load piece-layer hashes from the `.torrent` file's `piece_layers` map.
    ///
    /// This is used when full metadata is available at torrent creation (not from
    /// a magnet link). Each entry maps a file's Merkle root to the concatenated
    /// piece-layer hashes (32 bytes each). After loading, any blocks that were
    /// already hashed are retroactively verified.
    ///
    /// Returns the list of piece indices that were fully verified during loading.
    pub fn load_piece_layers(
        &mut self,
        piece_layers: &std::collections::BTreeMap<Id32, Vec<u8>>,
    ) -> Vec<u32> {
        let mut verified = Vec::new();
        for (file_idx, root) in self.file_roots.iter().enumerate() {
            let Some(layer_bytes) = piece_layers.get(root) else {
                continue;
            };
            // Each hash is 32 bytes (SHA-256)
            if layer_bytes.len() % 32 != 0 {
                continue;
            }
            let hashes: Vec<Id32> = layer_bytes
                .chunks_exact(32)
                .map(|chunk| {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(chunk);
                    Id32(arr)
                })
                .collect();

            // Mark the corresponding piece-request chunks as received
            let num_chunks = self.piece_requests.get(file_idx).map_or(0, Vec::len);
            for chunk_idx in 0..num_chunks {
                self.piece_requests[file_idx][chunk_idx].have = true;
            }

            // Load into tree state and retroactively verify stored blocks
            if let Some(tree) = self.trees.get_mut(file_idx) {
                let v = tree.set_piece_hashes_and_verify(hashes, self.blocks_per_piece);
                verified.extend(v);
            }
        }
        verified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Id32, MerkleTree, sha256};

    fn make_file_info(root: Id32, num_blocks: u32, num_pieces: u32) -> FileHashInfo {
        FileHashInfo {
            root,
            num_blocks,
            num_pieces,
        }
    }

    fn make_block_hashes(n: usize) -> Vec<Id32> {
        (0..n).map(|i| sha256(&[i as u8])).collect()
    }

    #[test]
    fn pick_piece_layer_first() {
        let root = sha256(b"file1");
        let files = vec![make_file_info(root, 1024, 64)];
        let picker = HashPicker::new(&files, 16); // 16 blocks per piece

        let req = picker.pick_hashes(|p| p == 0);
        assert!(req.is_some());
        let req = req.unwrap();
        assert_eq!(req.file_root, root);
        // Should request piece-layer hashes (base = piece_layer)
        assert_eq!(req.base, 4); // log2(16) = 4
    }

    #[test]
    fn pick_returns_none_when_complete() {
        let block_hashes = make_block_hashes(4);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let root = tree.root();
        let files = vec![make_file_info(root, 4, 2)];
        let mut picker = HashPicker::new(&files, 2);

        // Simulate: we already have all piece hashes
        let pieces = tree.piece_layer(2).to_vec();
        picker.trees[0].set_piece_hashes(pieces);

        // Mark all blocks verified
        for (i, h) in block_hashes.iter().enumerate() {
            picker.trees[0].set_block_hash(i as u32, *h);
        }

        // All piece hashes received → chunk marked as have
        picker.piece_requests[0][0].have = true;

        assert!(picker.pick_hashes(|_| true).is_none());
    }

    #[test]
    fn add_hashes_populates_tree() {
        let block_hashes = make_block_hashes(4);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let root = tree.root();
        let pieces = tree.piece_layer(2).to_vec();
        let files = vec![make_file_info(root, 4, 2)];
        let mut picker = HashPicker::new(&files, 2);

        assert!(!picker.have_piece_hash(0, 0));

        let req = HashRequest {
            file_root: root,
            base: 1, // piece layer for 2 blocks/piece
            index: 0,
            count: 2,
            proof_layers: 0,
        };
        let result = picker.add_hashes(&req, &pieces);
        assert!(result.is_ok());
        assert!(result.unwrap().valid);
        assert!(picker.have_piece_hash(0, 0));
        assert!(picker.have_piece_hash(0, 1));
    }

    #[test]
    fn set_block_hash_delegates_to_tree() {
        let block_hashes = make_block_hashes(4);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let root = tree.root();
        let pieces = tree.piece_layer(2).to_vec();
        let files = vec![make_file_info(root, 4, 2)];
        let mut picker = HashPicker::new(&files, 2);

        // Set piece hashes first
        picker.trees[0].set_piece_hashes(pieces);

        // Set block hashes — first returns Unknown, second completes piece → Ok
        let r0 = picker.set_block_hash(0, 0, block_hashes[0]);
        assert_eq!(r0, SetBlockResult::Unknown);

        let r1 = picker.set_block_hash(0, 1, block_hashes[1]);
        assert_eq!(r1, SetBlockResult::Ok);

        assert!(picker.piece_verified(0, 0));
    }

    #[test]
    fn verify_block_hashes_adds_request() {
        let root = sha256(b"file1");
        let files = vec![make_file_info(root, 1024, 64)];
        let mut picker = HashPicker::new(&files, 16);

        // Request block hashes for a failed piece
        picker.verify_block_hashes(5);

        let req = picker.pick_hashes(|_| true);
        assert!(req.is_some());
        let req = req.unwrap();
        // Should be a block-layer request (base=0)
        assert_eq!(req.base, 0);
    }

    #[test]
    fn add_hashes_with_valid_proof_accepted() {
        // Build a tree with 8 blocks, 2 per piece = 4 piece hashes
        let block_hashes = make_block_hashes(8);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let root = tree.root();
        let piece_hashes = tree.piece_layer(2).to_vec();

        let files = vec![make_file_info(root, 8, 4)];
        let mut picker = HashPicker::new(&files, 2);

        // Request first 2 piece hashes with uncle proof.
        // Sub-root = hash(piece[0] || piece[1]) — one level above piece layer.
        // Uncle = hash(piece[2] || piece[3]) — the sibling subtree root.
        // verify_proof(root, sub_root, leaf_index=0, [uncle]) should pass.
        let uncle = MerkleTree::root_from_hashes(&piece_hashes[2..]);

        let mut hashes_with_proof = piece_hashes[0..2].to_vec();
        hashes_with_proof.push(uncle);

        let req = HashRequest {
            file_root: root,
            base: 1, // piece layer
            index: 0,
            count: 2,
            proof_layers: 1,
        };
        let result = picker.add_hashes(&req, &hashes_with_proof).unwrap();
        assert!(result.valid);
        assert!(picker.have_piece_hash(0, 0));
    }

    #[test]
    fn add_hashes_with_invalid_proof_rejected() {
        let block_hashes = make_block_hashes(8);
        let tree = MerkleTree::from_leaves(&block_hashes);
        let root = tree.root();
        let piece_hashes = tree.piece_layer(2).to_vec();

        let files = vec![make_file_info(root, 8, 4)];
        let mut picker = HashPicker::new(&files, 2);

        // Send piece hashes with a wrong uncle hash
        let wrong_uncle = sha256(b"wrong");
        let mut hashes_with_bad_proof = piece_hashes[0..2].to_vec();
        hashes_with_bad_proof.push(wrong_uncle);

        let req = HashRequest {
            file_root: root,
            base: 1,
            index: 0,
            count: 2,
            proof_layers: 1,
        };
        let result = picker.add_hashes(&req, &hashes_with_bad_proof).unwrap();
        assert!(!result.valid);
        // Piece hashes should NOT be set
        assert!(!picker.have_piece_hash(0, 0));
    }

    #[test]
    fn pick_hashes_with_callback() {
        let root = sha256(b"file1");
        let files = vec![make_file_info(root, 1024, 64)];
        let picker = HashPicker::new(&files, 16);

        // No pieces → no request
        assert!(picker.pick_hashes(|_| false).is_none());

        // Has piece 0 → request
        assert!(picker.pick_hashes(|p| p == 0).is_some());
    }
}
