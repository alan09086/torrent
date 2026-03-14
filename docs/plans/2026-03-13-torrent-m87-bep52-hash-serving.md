# M87: BEP 52 Hash Serving — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement BEP 52 hash serving so v2/hybrid seeders respond to Merkle hash requests from peers instead of always rejecting them.

**Architecture:** When seeding a v2/hybrid torrent, the piece-layer hashes are already in memory via `meta_v2.piece_layers`. On receiving a `HashRequest`, validate it, extract the requested piece-layer hashes, compute Merkle proof uncle hashes via `MerkleTree::proof_path()`, and send a `Message::Hashes` response. Also clean up placeholder fields in HashPicker and redundant closures in merkle.rs.

**Tech Stack:** Rust, torrent-session, torrent-core crates

---

### Task 1: Clean up HashPicker placeholder fields

**Files:**
- Modify: `crates/torrent-core/src/hash_picker.rs`

- [ ] **Step 1: Remove `_num_requests` from `PieceHashRequest`**

In `PieceHashRequest` struct (line ~42), remove:

```rust
// REMOVE:
_num_requests: u32,
```

Update any constructor that initializes this field (search for `PieceHashRequest {` and remove the `_num_requests: 0` initializer).

- [ ] **Step 2: Remove `_last_request` and `_num_requests` from `BlockHashRequest`**

In `BlockHashRequest` struct (lines ~49-50), remove:

```rust
// REMOVE:
_last_request: Option<Instant>,
_num_requests: u32,
```

Update any constructor that initializes these fields.

- [ ] **Step 3: Run tests to verify**

```bash
cargo test -p torrent-core
```

Expected: All pass. These fields were never read.

- [ ] **Step 4: Commit**

```bash
git add crates/torrent-core/src/hash_picker.rs
git commit -m "refactor: remove unused placeholder fields from HashPicker structs (M87)"
```

---

### Task 2: Fix redundant closures in merkle.rs tests

**Files:**
- Modify: `crates/torrent-core/src/merkle.rs`

- [ ] **Step 1: Replace 6 redundant closures**

At lines ~220, 240, 253, 263, 275, 289, replace each `|i| leaf(i)` with `leaf`:

```rust
// BEFORE:
let leaves: Vec<Id32> = (0..4).map(|i| leaf(i)).collect();

// AFTER:
let leaves: Vec<Id32> = (0..4).map(leaf).collect();
```

Apply this to all 6 occurrences.

- [ ] **Step 2: Run tests and clippy**

```bash
cargo test -p torrent-core && cargo clippy -p torrent-core -- -D warnings
```

Expected: All pass, no clippy warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-core/src/merkle.rs
git commit -m "refactor: fix redundant closures in merkle.rs tests (M87)"
```

---

### Task 3: Write failing test for hash serving

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs` (or a test module that can access `TorrentActor` internals)

Since `handle_incoming_hash_request` is a private method on `TorrentActor`, write a unit-level integration test that constructs the scenario. Alternatively, add tests to the existing test module in torrent.rs.

- [ ] **Step 1: Write test for serving piece-layer hashes from a v2 torrent**

The test should:
1. Create a v2 torrent with known data and piece layers
2. Set up a TorrentActor (or the relevant state) with `meta_v2` populated
3. Construct a `HashRequest` for the piece layer (base = piece_layer level, not block level)
4. Call the hash serving logic
5. Assert the response is `PeerCommand::SendHashes` with correct hashes, not `SendHashReject`

```rust
#[tokio::test]
async fn test_hash_serving_v2_piece_layer() {
    // Build a small v2 torrent with 4 pieces
    // Populate meta_v2 with piece_layers
    // Request piece-layer hashes for file_root at base=piece_layer, index=0, count=4
    // Verify response contains the correct 4 hashes + proof layers
}
```

- [ ] **Step 2: Write test for rejection on v1-only torrent**

```rust
#[tokio::test]
async fn test_hash_serving_rejects_v1_only() {
    // Set up a v1-only torrent (version = V1Only, meta_v2 = None)
    // Send a HashRequest
    // Assert response is SendHashReject
}
```

- [ ] **Step 3: Write test for rejection on out-of-bounds request**

```rust
#[tokio::test]
async fn test_hash_serving_rejects_out_of_bounds() {
    // Set up a v2 torrent with 4 pieces
    // Request index=100 (out of range)
    // Assert response is SendHashReject
}
```

- [ ] **Step 4: Run tests to verify they fail**

```bash
cargo test -p torrent-session -- hash_serving
```

Expected: FAIL — the current implementation always rejects.

---

### Task 4: Implement hash serving in `handle_incoming_hash_request`

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs`

- [ ] **Step 1: Replace the stub implementation**

Replace the current `handle_incoming_hash_request` method (lines ~4666-4679):

```rust
async fn handle_incoming_hash_request(
    &self,
    peer_addr: SocketAddr,
    request: torrent_core::HashRequest,
) {
    let peer = match self.peers.get(&peer_addr) {
        Some(p) => p,
        None => return,
    };

    // Reject if v1-only or no v2 metadata
    let meta_v2 = match self.meta_v2.as_ref() {
        Some(m) if self.version != torrent_core::TorrentVersion::V1Only => m,
        _ => {
            let _ = peer.cmd_tx.try_send(PeerCommand::SendHashReject(request));
            return;
        }
    };

    // Look up piece-layer hashes for the requested file root
    let piece_hashes = match meta_v2.file_piece_hashes(&request.file_root) {
        Some(hashes) => hashes,
        None => {
            let _ = peer.cmd_tx.try_send(PeerCommand::SendHashReject(request));
            return;
        }
    };

    // Validate request geometry
    let num_pieces = piece_hashes.len() as u32;
    let num_blocks = if let Some(lengths) = self.lengths.as_ref() {
        // Approximate block count from total length and chunk size
        lengths.total_chunks() as u32
    } else {
        let _ = peer.cmd_tx.try_send(PeerCommand::SendHashReject(request));
        return;
    };

    if !torrent_core::validate_hash_request(&request, num_blocks, num_pieces) {
        let _ = peer.cmd_tx.try_send(PeerCommand::SendHashReject(request));
        return;
    }

    // Extract requested hashes from the piece layer
    let start = request.index as usize;
    let end = (start + request.count as usize).min(piece_hashes.len());
    let mut hashes: Vec<torrent_core::Id32> = piece_hashes[start..end].to_vec();

    // Compute proof (uncle) hashes if requested
    if request.proof_layers > 0 && !piece_hashes.is_empty() {
        let tree = torrent_core::MerkleTree::from_leaves(&piece_hashes);
        for i in start..end {
            let proof = tree.proof_path(i);
            let proof_count = (request.proof_layers as usize).min(proof.len());
            hashes.extend_from_slice(&proof[..proof_count]);
        }
    }

    let _ = peer.cmd_tx.try_send(PeerCommand::SendHashes {
        request,
        hashes,
    });
}
```

**Note:** The exact implementation may need adjustment based on:
- How `PeerCommand::SendHashes` constructs the wire `Message::Hashes` (check the peer task's command handler)
- Whether `validate_hash_request` signature matches (it takes `&HashRequest, num_blocks, num_pieces`)
- The proof hash layout expected by BEP 52 (appended after the requested hashes)

The implementer should verify the `SendHashes` peer command handler in the peer task code to ensure the `request` fields map correctly to the `Message::Hashes` wire format fields (`pieces_root`, `base`, `index`, `count`, `proof_layers`).

- [ ] **Step 2: Run tests**

```bash
cargo test -p torrent-session -- hash_serving
```

Expected: All 3 tests pass.

- [ ] **Step 3: Run full test suite and clippy**

```bash
cargo test --workspace && cargo clippy --workspace -- -D warnings
```

Expected: All 1460+ tests pass, no clippy warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/torrent-session/src/torrent.rs
git commit -m "feat: implement BEP 52 hash serving for v2/hybrid seeders (M87)

Serve piece-layer Merkle hashes to peers requesting them via BEP 52
HashRequest (message ID 21). Uses piece_layers from v2 torrent
metadata and MerkleTree::proof_path() for uncle proofs. V1-only
torrents and invalid requests are still rejected."
```
