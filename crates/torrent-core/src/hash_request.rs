//! BEP 52 hash request types and validation.

use crate::Id32;

/// A request for Merkle tree hashes (BEP 52).
///
/// Represents a range of hashes at a specific layer of a file's Merkle tree.
/// `base` = 0 is the leaf (block) layer, higher values are coarser layers.
/// The piece layer is at `log2(piece_length / 16384)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashRequest {
    /// SHA-256 root hash of the file's Merkle tree.
    pub file_root: Id32,
    /// Tree layer: 0 = block layer, piece_layer = log2(blocks_per_piece).
    pub base: u32,
    /// Index of the first hash at the specified layer.
    pub index: u32,
    /// Number of hashes requested.
    pub count: u32,
    /// Number of uncle proof layers needed for verification.
    pub proof_layers: u32,
}

/// Validate a hash request against file parameters.
///
/// Checks that the request is well-formed and within the file's bounds.
/// `file_num_blocks` is the total number of 16 KiB blocks in the file.
/// `file_num_pieces` is the total number of pieces in the file.
pub fn validate_hash_request(
    req: &HashRequest,
    file_num_blocks: u32,
    _file_num_pieces: u32,
) -> bool {
    if req.count == 0 || req.count > 8192 {
        return false;
    }

    let num_leafs = file_num_blocks.next_power_of_two();
    let num_layers = num_leafs.trailing_zeros() + 1;

    if req.base >= num_layers {
        return false;
    }

    // Number of nodes at the requested layer
    let layer_size = num_leafs >> req.base;

    if req.index >= layer_size || req.index + req.count > layer_size {
        return false;
    }

    if req.proof_layers >= num_layers - req.base {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Id32;

    #[test]
    fn valid_request_passes() {
        let req = HashRequest {
            file_root: Id32::ZERO,
            base: 0,
            index: 0,
            count: 512,
            proof_layers: 3,
        };
        assert!(validate_hash_request(&req, 8192, 512));
    }

    #[test]
    fn reject_count_too_large() {
        let req = HashRequest {
            file_root: Id32::ZERO,
            base: 0,
            index: 0,
            count: 8193, // max is 8192
            proof_layers: 0,
        };
        assert!(!validate_hash_request(&req, 16384, 8192));
    }

    #[test]
    fn reject_index_out_of_range() {
        let req = HashRequest {
            file_root: Id32::ZERO,
            base: 0,
            index: 100,
            count: 10,
            proof_layers: 0,
        };
        // file has 64 blocks, index 100 is out of range
        assert!(!validate_hash_request(&req, 64, 4));
    }

    #[test]
    fn reject_zero_count() {
        let req = HashRequest {
            file_root: Id32::ZERO,
            base: 0,
            index: 0,
            count: 0,
            proof_layers: 0,
        };
        assert!(!validate_hash_request(&req, 64, 4));
    }

    #[test]
    fn piece_layer_request_valid() {
        // piece_layer = log2(blocks_per_piece), e.g. 16 blocks/piece → base=4
        let req = HashRequest {
            file_root: Id32::ZERO,
            base: 4, // piece layer for 16 blocks/piece
            index: 0,
            count: 32,
            proof_layers: 2,
        };
        assert!(validate_hash_request(&req, 512, 32));
    }
}
