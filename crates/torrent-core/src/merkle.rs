//! Merkle hash tree for BitTorrent v2 (BEP 52).
//!
//! Uses a binary heap layout stored as a flat `Vec<Id32>`. Leaves are padded
//! to a power of two with zero hashes. Internal nodes are `SHA-256(left || right)`.
//!
//! The piece layer is the tree layer whose nodes correspond to piece-sized chunks
//! of the file, used for initial verification. Below that, individual 16 KiB blocks
//! can be verified via Merkle proof paths.

use crate::hash::Id32;

/// A complete binary Merkle tree stored in a flat array (1-indexed heap layout).
///
/// Index 1 is the root. For a node at index `i`, its left child is at `2*i`
/// and its right child is at `2*i + 1`.
#[derive(Debug, Clone)]
pub struct MerkleTree {
    /// 1-indexed array: nodes[0] is unused, nodes[1] is root.
    nodes: Vec<Id32>,
    /// Number of actual (non-padding) leaves.
    leaf_count: usize,
    /// Total leaves including padding (always a power of 2).
    padded_leaf_count: usize,
}

impl MerkleTree {
    /// Build a Merkle tree from leaf hashes.
    ///
    /// Pads to the next power of two with zero hashes, then builds bottom-up
    /// by hashing pairs: `parent = SHA-256(left || right)`.
    pub fn from_leaves(leaves: &[Id32]) -> Self {
        assert!(
            !leaves.is_empty(),
            "cannot build Merkle tree from empty leaves"
        );

        let leaf_count = leaves.len();
        let padded = leaf_count.next_power_of_two();
        let total_nodes = 2 * padded; // 1-indexed: indices 1..total_nodes-1

        let mut nodes = vec![Id32::ZERO; total_nodes];

        // Place leaves at the bottom layer (indices padded..2*padded-1)
        for (i, leaf) in leaves.iter().enumerate() {
            nodes[padded + i] = *leaf;
        }
        // Padding leaves remain ZERO

        // Build bottom-up
        for i in (1..padded).rev() {
            let left = nodes[2 * i];
            let right = nodes[2 * i + 1];
            nodes[i] = hash_pair(left, right);
        }

        MerkleTree {
            nodes,
            leaf_count,
            padded_leaf_count: padded,
        }
    }

    /// The Merkle root hash.
    pub fn root(&self) -> Id32 {
        self.nodes[1]
    }

    /// Number of actual (non-padding) leaves.
    pub fn leaf_count(&self) -> usize {
        self.leaf_count
    }

    /// Tree depth (number of layers below the root). A single leaf has depth 0.
    pub fn depth(&self) -> usize {
        self.padded_leaf_count.trailing_zeros() as usize
    }

    /// Get all hashes at a given depth (0 = root, `depth()` = leaves).
    ///
    /// Returns a slice of the internal array at that tree level.
    pub fn layer(&self, depth: usize) -> &[Id32] {
        let layer_size = 1usize << depth;
        let start = layer_size; // 1-indexed: layer at depth d starts at index 2^d
        if start + layer_size > self.nodes.len() {
            return &[];
        }
        &self.nodes[start..start + layer_size]
    }

    /// Get the piece layer — the tree layer whose nodes correspond to pieces.
    ///
    /// `blocks_per_piece` is `piece_length / 16384` (number of 16 KiB blocks per piece).
    /// The piece layer is at depth `depth() - log2(blocks_per_piece)`.
    pub fn piece_layer(&self, blocks_per_piece: usize) -> &[Id32] {
        assert!(
            blocks_per_piece.is_power_of_two(),
            "blocks_per_piece must be a power of 2"
        );
        let levels_up = blocks_per_piece.trailing_zeros() as usize;
        let tree_depth = self.depth();
        if levels_up > tree_depth {
            // Piece is larger than entire file — root is the piece hash
            return self.layer(0);
        }
        self.layer(tree_depth - levels_up)
    }

    /// Get the leaf hashes.
    pub fn leaves(&self) -> &[Id32] {
        let start = self.padded_leaf_count;
        &self.nodes[start..start + self.leaf_count]
    }

    /// Compute a Merkle root from a list of hashes (e.g., verify piece layer → root).
    ///
    /// Pads to power of two and builds a temporary tree. Useful for validating
    /// that a received piece layer matches a known root.
    pub fn root_from_hashes(hashes: &[Id32]) -> Id32 {
        if hashes.is_empty() {
            return Id32::ZERO;
        }
        MerkleTree::from_leaves(hashes).root()
    }

    /// Extract the Merkle proof path for a specific leaf.
    ///
    /// Returns the sibling hashes from leaf to root needed to verify the leaf
    /// against the root. Used by M34's hash request/response (BEP 52 wire
    /// messages 21-23) for block-level verification.
    pub fn proof_path(&self, leaf_index: usize) -> Vec<Id32> {
        assert!(
            leaf_index < self.padded_leaf_count,
            "leaf index out of range"
        );
        let mut path = Vec::with_capacity(self.depth());
        let mut idx = self.padded_leaf_count + leaf_index;

        while idx > 1 {
            // Sibling is at idx ^ 1 (flip lowest bit)
            let sibling = idx ^ 1;
            path.push(self.nodes[sibling]);
            idx /= 2; // move to parent
        }

        path
    }

    /// Verify a leaf hash against a root using a proof path.
    ///
    /// Recomputes the root from the leaf and its sibling hashes, then compares.
    pub fn verify_proof(root: Id32, leaf: Id32, leaf_index: usize, proof: &[Id32]) -> bool {
        let mut hash = leaf;
        let mut idx = leaf_index;

        for sibling in proof {
            hash = if idx.is_multiple_of(2) {
                // We're the left child
                hash_pair(hash, *sibling)
            } else {
                // We're the right child
                hash_pair(*sibling, hash)
            };
            idx /= 2;
        }

        hash == root
    }
}

/// Hash a pair of nodes: `SHA-256(left || right)`.
fn hash_pair(left: Id32, right: Id32) -> Id32 {
    crate::sha256(&[left.0, right.0].concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(byte: u8) -> Id32 {
        crate::sha256(&[byte])
    }

    #[test]
    fn single_leaf() {
        let l = leaf(0x01);
        let tree = MerkleTree::from_leaves(&[l]);
        // Single leaf: root = hash(leaf || ZERO) because we pad to 2 leaves
        assert_eq!(tree.leaf_count(), 1);
        assert_eq!(tree.depth(), 0);
        assert_eq!(tree.root(), l);
    }

    #[test]
    fn two_leaves() {
        let l0 = leaf(0x01);
        let l1 = leaf(0x02);
        let tree = MerkleTree::from_leaves(&[l0, l1]);
        assert_eq!(tree.leaf_count(), 2);
        assert_eq!(tree.depth(), 1);
        assert_eq!(tree.root(), hash_pair(l0, l1));
        assert_eq!(tree.leaves(), &[l0, l1]);
    }

    #[test]
    fn three_leaves_padded() {
        let l0 = leaf(0x01);
        let l1 = leaf(0x02);
        let l2 = leaf(0x03);
        let tree = MerkleTree::from_leaves(&[l0, l1, l2]);
        assert_eq!(tree.leaf_count(), 3);
        assert_eq!(tree.depth(), 2); // padded to 4 leaves
        // Bottom layer: l0, l1, l2, ZERO
        let left = hash_pair(l0, l1);
        let right = hash_pair(l2, Id32::ZERO);
        assert_eq!(tree.root(), hash_pair(left, right));
    }

    #[test]
    fn layer_extraction() {
        let leaves: Vec<Id32> = (0..4).map(leaf).collect();
        let tree = MerkleTree::from_leaves(&leaves);
        assert_eq!(tree.depth(), 2);

        // Layer 0 = root (1 node)
        assert_eq!(tree.layer(0).len(), 1);
        assert_eq!(tree.layer(0)[0], tree.root());

        // Layer 1 = 2 intermediate nodes
        assert_eq!(tree.layer(1).len(), 2);

        // Layer 2 = 4 leaves
        assert_eq!(tree.layer(2).len(), 4);
        assert_eq!(tree.layer(2)[0], leaves[0]);
        assert_eq!(tree.layer(2)[3], leaves[3]);
    }

    #[test]
    fn piece_layer_extraction() {
        // 8 block-level leaves, 2 blocks per piece → piece layer at depth 2 (4 nodes)
        let leaves: Vec<Id32> = (0..8).map(leaf).collect();
        let tree = MerkleTree::from_leaves(&leaves);
        assert_eq!(tree.depth(), 3);

        let pieces = tree.piece_layer(2); // 2 blocks per piece
        assert_eq!(pieces.len(), 4);
        // Each piece hash should be hash of 2 consecutive block hashes
        assert_eq!(pieces[0], hash_pair(leaves[0], leaves[1]));
        assert_eq!(pieces[1], hash_pair(leaves[2], leaves[3]));
    }

    #[test]
    fn root_from_piece_layer_round_trip() {
        let leaves: Vec<Id32> = (0..8).map(leaf).collect();
        let tree = MerkleTree::from_leaves(&leaves);
        let pieces = tree.piece_layer(2);
        // Rebuilding from piece layer should give the same root
        let rebuilt_root = MerkleTree::root_from_hashes(pieces);
        assert_eq!(rebuilt_root, tree.root());
    }

    #[test]
    fn proof_path_generation() {
        let leaves: Vec<Id32> = (0..4).map(leaf).collect();
        let tree = MerkleTree::from_leaves(&leaves);

        let proof = tree.proof_path(0);
        assert_eq!(proof.len(), 2); // depth = 2, so 2 siblings
        // First sibling is leaf[1], second is hash(leaf[2], leaf[3])
        assert_eq!(proof[0], leaves[1]);
        assert_eq!(proof[1], hash_pair(leaves[2], leaves[3]));
    }

    #[test]
    fn proof_verification_success() {
        let leaves: Vec<Id32> = (0..4).map(leaf).collect();
        let tree = MerkleTree::from_leaves(&leaves);

        for i in 0..4 {
            let proof = tree.proof_path(i);
            assert!(
                MerkleTree::verify_proof(tree.root(), leaves[i], i, &proof),
                "proof failed for leaf {i}"
            );
        }
    }

    #[test]
    fn proof_verification_failure() {
        let leaves: Vec<Id32> = (0..4).map(leaf).collect();
        let tree = MerkleTree::from_leaves(&leaves);

        let proof = tree.proof_path(0);
        let wrong_leaf = leaf(0xFF);
        assert!(!MerkleTree::verify_proof(
            tree.root(),
            wrong_leaf,
            0,
            &proof
        ));
    }
}
