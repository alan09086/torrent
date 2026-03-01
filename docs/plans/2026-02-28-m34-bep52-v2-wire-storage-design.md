# M34: BEP 52 v2 Wire Protocol + Storage Integration

## Context

M33 (v0.37.0, 843 tests) established BEP 52 core types: `Id32`, `InfoHashes`, `MerkleTree`, `FileTreeNode`, `TorrentMetaV2`, auto-detection, and v2 magnets. M34 bridges these types into the live protocol — wire messages, disk verification, hash downloading, and session orchestration.

**Goal:** Full BEP 52 wire protocol compliance, including proactive per-block hash requests that libtorrent left unimplemented (`#if 0` in `hash_picker.cpp`). Ferrite becomes the first BitTorrent client with real-time per-block Merkle verification.

**Reference:** [BEP 52 §7](https://www.bittorrent.org/beps/bep_0052.html) | libtorrent-rasterbar 2.0.11 source analysis

## Architecture Decision: Integrated Extension (Approach 1)

Extend existing types and flows with v2 branches. No new actors, no trait-based strategy pattern — simple if/else branching in `verify_and_mark_piece()`, matching libtorrent's proven approach but with full per-block support.

**Why not a separate MerkleVerifier actor?** Verification is tightly coupled to disk I/O and peer state. Extracting it creates artificial boundaries — the two paths share 90% of their logic.

## Sub-milestones

| Sub-milestone | Scope | Crates |
|---|---|---|
| **M34a** (v0.38.0) | Wire messages 21-23, `HashRequest` type, `HashPicker`, `MerkleTreeState` | ferrite-wire, ferrite-core |
| **M34b** (v0.39.0) | Storage v2 methods, `ChunkTracker` block verification, disk I/O v2 jobs | ferrite-storage, ferrite-session |
| **M34c** (v0.40.0) | Session v2 flow, peer hash exchange, eager block requests, resume data, facade | ferrite-session, ferrite-core, ferrite |

---

## M34a: Wire Protocol + Hash Picker

### Wire Messages (ferrite-wire)

BEP 52 defines three new **core protocol** messages (not extensions):

| ID | Name | Wire Format |
|----|------|-------------|
| 21 | HashRequest | `len(49) \| 21 \| pieces_root(32) \| base(4) \| index(4) \| count(4) \| proof_layers(4)` |
| 22 | Hashes | same 49-byte header + `(count + proof_count) * 32` bytes of hashes |
| 23 | HashReject | identical to HashRequest (echoes the request back) |

**New `Message` variants:**

```rust
/// BEP 52: Request hashes from a Merkle tree.
HashRequest {
    pieces_root: Id32,    // 32-byte file root hash
    base: u32,            // tree layer (0 = leaf/block layer)
    index: u32,           // first hash index at that layer
    count: u32,           // number of hashes requested
    proof_layers: u32,    // uncle hashes needed for verification
},
/// BEP 52: Response with hashes + uncle proof.
Hashes {
    pieces_root: Id32,
    base: u32,
    index: u32,
    count: u32,
    proof_layers: u32,
    hashes: Vec<Id32>,    // count base hashes + uncle proof hashes
},
/// BEP 52: Reject a hash request.
HashReject {
    pieces_root: Id32,
    base: u32,
    index: u32,
    count: u32,
    proof_layers: u32,
},
```

**Serialization** follows existing manual big-endian pattern in `message.rs`. HashRequest and HashReject are fixed-size (49 bytes). Hashes is variable-length.

**Handshake:** No structural change. The 68-byte handshake stays identical. v2 detection happens at the session layer by comparing the received `info_hash` (20 bytes) against the first 20 bytes of SHA-256(info_dict). This matches libtorrent's approach (see `bt_peer_connection.cpp:3459-3472`).

### HashRequest Type (ferrite-core)

Reusable across crates:

```rust
/// A request for Merkle tree hashes (BEP 52).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashRequest {
    pub file_root: Id32,
    pub base: u32,           // 0 = block layer, piece_layer = log2(blocks_per_piece)
    pub index: u32,          // first hash at specified layer
    pub count: u32,          // number of hashes
    pub proof_layers: u32,   // uncle hashes for proof verification
}

/// Validates a hash request against known file parameters.
pub fn validate_hash_request(
    req: &HashRequest,
    file_num_blocks: u32,
    file_num_pieces: u32,
) -> bool;
```

### MerkleTreeState (ferrite-core)

Per-file Merkle tree verification state. Simpler than libtorrent's 4 storage modes — we use explicit `Option` fields instead of enum-based mode transitions.

```rust
/// Per-file Merkle tree state for download verification.
pub struct MerkleTreeState {
    /// File root hash (immutable, from torrent metadata).
    root: Id32,
    /// Number of blocks in this file.
    num_blocks: u32,
    /// Number of pieces in this file.
    num_pieces: u32,
    /// Piece-layer hashes (populated from .torrent piece_layers or hash responses).
    piece_hashes: Option<Vec<Id32>>,
    /// Block-layer hashes (populated during download, sparse).
    block_hashes: Vec<Option<Id32>>,
    /// Which blocks have been verified against the tree.
    block_verified: Bitfield,
}

impl MerkleTreeState {
    pub fn new(root: Id32, num_blocks: u32, num_pieces: u32) -> Self;
    pub fn set_piece_hashes(&mut self, hashes: Vec<Id32>);
    pub fn has_piece_hash(&self, piece_index: u32) -> bool;
    pub fn set_block_hash(&mut self, block_index: u32, hash: Id32) -> SetBlockResult;
    pub fn add_hashes(&mut self, req: &HashRequest, hashes: &[Id32]) -> Result<AddHashesResult>;
    pub fn is_block_verified(&self, block_index: u32) -> bool;
    pub fn piece_verified(&self, piece_index: u32, blocks_per_piece: u32) -> bool;
}
```

**`SetBlockResult`** — the critical tri-state:
```rust
pub enum SetBlockResult {
    /// Block hash matches the Merkle tree.
    Ok,
    /// Piece-layer hash not yet available; block hash stored for later verification.
    Unknown,
    /// Block hash doesn't match the Merkle tree (bad peer).
    HashFailed,
}
```

### HashPicker (ferrite-core)

Coordinates which hash requests to send to peers. Unlike libtorrent, proactive per-block requesting is fully implemented.

```rust
pub struct HashPicker {
    trees: Vec<MerkleTreeState>,
    /// Per-file piece-layer request tracking (512-piece chunks).
    piece_requests: Vec<Vec<PieceHashRequest>>,
    /// Block-layer requests (proactive — libtorrent left this as #if 0).
    block_requests: Vec<BlockHashRequest>,
    /// log2(piece_length / 16384)
    piece_layer: u32,
    blocks_per_piece: u32,
}

impl HashPicker {
    pub fn new(files: &[V2FileInfo], piece_length: u64) -> Self;
    /// Pick next hash request. Priority: (1) block hashes for urgent pieces,
    /// (2) piece-layer hashes in 512-chunk batches.
    pub fn pick_hashes(&self, have: &Bitfield) -> Option<HashRequest>;
    /// Process received hashes, validate against tree.
    pub fn add_hashes(&mut self, req: &HashRequest, hashes: &[Id32]) -> Result<AddHashesResult>;
    /// Record a computed block hash. Returns Ok/Unknown/HashFailed.
    pub fn set_block_hash(&mut self, piece: u32, offset: u32, hash: Id32) -> SetBlockResult;
    /// Mark a hash request as rejected (retry with different peer).
    pub fn hashes_rejected(&mut self, req: &HashRequest);
    /// Request block hashes for a failed piece (reactive, for smart ban).
    pub fn verify_block_hashes(&mut self, piece: u32);
    /// Do we have the piece-layer hash for this piece?
    pub fn have_piece_hash(&self, piece: u32) -> bool;
    /// Are all blocks in this piece Merkle-verified?
    pub fn piece_verified(&self, piece: u32) -> bool;
}
```

**`AddHashesResult`:**
```rust
pub struct AddHashesResult {
    pub valid: bool,
    pub hash_passed: Vec<u32>,   // piece indices that passed verification
    pub hash_failed: Vec<u32>,   // piece indices that failed
}
```

### M34a Tests (~22)

Wire protocol:
1. HashRequest round-trip
2. HashReject round-trip
3. Hashes round-trip (with proof hashes)
4. Hashes variable-length parsing
5. Reject invalid HashRequest (too short)
6. Reject invalid Hashes (wrong hash count)
7. Unknown message ID still returns error (existing test unaffected)

HashRequest validation:
8. Valid request passes
9. Reject negative/overflow indices
10. Reject count > 8192

MerkleTreeState:
11. set_block_hash Ok (piece hash known, block matches)
12. set_block_hash Unknown (piece hash not yet available)
13. set_block_hash HashFailed (block doesn't match)
14. add_hashes with valid proof
15. add_hashes with invalid proof rejected
16. piece_verified after all blocks verified
17. Deferred verification: Unknown → later piece hash arrives → blocks retroactively verified

HashPicker:
18. pick_hashes returns piece-layer request first
19. pick_hashes returns block request for urgent piece
20. add_hashes populates tree state
21. hashes_rejected resets request state
22. have_piece_hash reports correctly

---

## M34b: Storage v2 + Disk I/O

### TorrentStorage Trait Extension (ferrite-storage)

```rust
pub trait TorrentStorage: Send + Sync {
    // ... existing v1 methods unchanged ...

    /// Verify a piece using SHA-256 (v2).
    fn verify_piece_v2(&self, piece: u32, expected: &Id32) -> Result<bool> {
        let data = self.read_piece(piece)?;
        Ok(ferrite_core::sha256(&data) == *expected)
    }

    /// Hash a single block with SHA-256 for Merkle verification.
    fn hash_block(&self, piece: u32, begin: u32, length: u32) -> Result<Id32> {
        let data = self.read_chunk(piece, begin, length)?;
        Ok(ferrite_core::sha256(&data))
    }
}
```

### ChunkTracker Block Verification (ferrite-storage)

```rust
pub struct ChunkTracker {
    have: Bitfield,
    in_progress: HashMap<u32, Bitfield>,
    lengths: Lengths,
    /// Per-block Merkle verification state (v2 only, None for v1).
    block_verified: Option<HashMap<u32, Bitfield>>,
}

impl ChunkTracker {
    /// Enable v2 block verification tracking.
    pub fn enable_v2_tracking(&mut self);
    /// Mark a specific block as Merkle-verified.
    pub fn mark_block_verified(&mut self, piece: u32, block_index: u32);
    /// Check if a specific block is Merkle-verified.
    pub fn is_block_verified(&self, piece: u32, block_index: u32) -> bool;
    /// Check if all blocks in a piece are Merkle-verified.
    pub fn all_blocks_verified(&self, piece: u32) -> bool;
}
```

### Disk I/O Extension (ferrite-session)

New `DiskJob` variants:

```rust
HashV2 {
    info_hash: Id20,
    piece: u32,
    expected: Id32,
    flags: DiskJobFlags,
    reply: oneshot::Sender<ferrite_storage::Result<bool>>,
},
BlockHash {
    info_hash: Id20,
    piece: u32,
    begin: u32,
    length: u32,
    flags: DiskJobFlags,
    reply: oneshot::Sender<ferrite_storage::Result<Id32>>,
},
```

New `DiskHandle` methods:

```rust
pub async fn verify_piece_v2(&self, piece: u32, expected: Id32, flags: DiskJobFlags)
    -> ferrite_storage::Result<bool>;
pub async fn hash_block(&self, piece: u32, begin: u32, length: u32, flags: DiskJobFlags)
    -> ferrite_storage::Result<Id32>;
```

### M34b Tests (~10)

Storage:
1. verify_piece_v2 correct hash passes
2. verify_piece_v2 wrong hash fails
3. hash_block returns correct SHA-256
4. hash_block with partial last block
5. MmapStorage verify_piece_v2
6. FilesystemStorage verify_piece_v2

ChunkTracker:
7. enable_v2_tracking initialises state
8. mark_block_verified + is_block_verified
9. all_blocks_verified returns false when incomplete
10. all_blocks_verified returns true when all done

---

## M34c: Session Integration + Resume

### TorrentActor v2 Support (ferrite-session)

New fields on `TorrentActor`:

```rust
hash_picker: Option<HashPicker>,        // None for v1 torrents
is_v2: bool,                            // quick check flag
```

**Verification flow modification:**

```rust
async fn verify_and_mark_piece(&mut self, index: u32) {
    if self.is_v2 {
        self.verify_and_mark_piece_v2(index).await;
    } else {
        self.verify_and_mark_piece_v1(index).await;
    }
}
```

**v2 verification flow (`verify_and_mark_piece_v2`):**

1. Flush write buffer for the piece
2. Read piece back block-by-block, compute SHA-256 of each 16 KiB block
3. Feed each block hash to `hash_picker.set_block_hash(piece, offset, hash)`
4. If all blocks return `Ok` → piece verified, `mark_verified()`
5. If any `HashFailed` → ban contributing peer, clear piece, re-download
6. If any `Unknown` → store block hashes, defer verification until piece-layer hash arrives

**Peer hash exchange (in peer task):**

- v2 connection detected: start requesting piece-layer hashes via `HashRequest`
- On receiving `Hashes`: pass to torrent actor → `hash_picker.add_hashes()`
- On receiving `HashReject`: mark request as rejected, retry with different peer
- **Eager block requesting (ferrite-specific):** when requesting a piece's data blocks, simultaneously send `HashRequest(base=0, index=first_block, count=blocks_per_piece)` to get block-level hashes for immediate per-block verification

**v2 torrent initialisation:**

- When metadata resolves and `TorrentMeta::V2` detected, set `is_v2 = true`
- Build `HashPicker` from `TorrentMetaV2.info.files()` and piece_length
- Load piece layers from `TorrentMetaV2.piece_layers` into tree state
- Enable `ChunkTracker.enable_v2_tracking()`

### Resume Data v2 (ferrite-core)

New optional fields on `FastResumeData`:

```rust
/// SHA-256 v2 info hash (32 bytes).
#[serde(rename = "info-hash2")]
#[serde(with = "serde_bytes")]
#[serde(skip_serializing_if = "Option::is_none", default)]
pub info_hash2: Option<Vec<u8>>,

/// Cached piece-layer hashes per file (key = hex file root, value = concatenated 32-byte hashes).
/// Allows skipping piece-layer hash requests on resume.
#[serde(rename = "trees")]
#[serde(skip_serializing_if = "HashMap::is_empty", default)]
pub trees: HashMap<String, Vec<u8>>,
```

### Facade Re-exports (ferrite)

Add to `core.rs`: `HashRequest`, `HashPicker`, `MerkleTreeState`, `SetBlockResult`, `AddHashesResult`, `validate_hash_request`

Add to `prelude.rs`: `HashPicker`, `SetBlockResult`

### M34c Tests (~11)

Session:
1. v2 torrent detected → hash_picker created
2. verify_and_mark_piece_v2 with all blocks Ok
3. verify_and_mark_piece_v2 with HashFailed → ban
4. verify_and_mark_piece_v2 with Unknown → deferred
5. Deferred blocks verified when piece-layer hash arrives
6. Peer hash request sent on v2 connection
7. Peer hash response processed correctly
8. Eager block hash request sent alongside piece requests

Resume:
9. v2 resume data round-trip (info_hash2 + trees)
10. v1 resume data backward compatibility (no v2 fields)

Facade:
11. Compilation verification (all re-exports resolve)

---

## Key Architectural Decisions

1. **Core protocol messages, not extensions.** BEP 52 defines msg IDs 21-23 as core protocol, confirmed by libtorrent source (`bt_peer_connection.hpp:152-154`). They sit alongside the existing 0-20 IDs.

2. **Proactive per-block hash requests.** libtorrent left this as `#if 0` in `hash_picker.cpp`. Ferrite implements it: when downloading a piece, simultaneously request block-level hashes from the same peer. This enables immediate per-block Merkle verification — a malicious peer's bad data is caught on the specific 16 KiB block, not after downloading an entire 4+ MiB piece.

3. **Tri-state block verification.** `SetBlockResult::Unknown` handles the race between block data arriving and piece-layer hashes being downloaded. Block hashes are stored and retroactively verified when piece-layer hashes arrive. This matches libtorrent's `merkle_tree::set_block()` return semantics.

4. **No v2-only handshake changes.** The 68-byte handshake format is unchanged. v2 detection is by hash comparison at the session layer. This avoids breaking v1 compatibility and matches libtorrent's approach.

5. **Simple `Option` fields over storage modes.** libtorrent's `merkle_tree` has 4 storage modes with complex transitions. Ferrite uses explicit `Option<Vec<Id32>>` for piece/block layers — clearer ownership, easier to reason about.

6. **`ChunkTracker` block_verified is `Option`.** v1 torrents pay zero cost. Only v2 torrents allocate per-block verification state.

## Breaking Changes

None. All changes are additive. v1 torrents are unaffected.

## Files Modified/Created

| File | Action | Sub-milestone |
|------|--------|---------------|
| `crates/ferrite-wire/src/message.rs` | Add HashRequest/Hashes/HashReject variants + serialization | M34a |
| `crates/ferrite-core/src/hash_request.rs` | **Create** — HashRequest struct, validate_hash_request | M34a |
| `crates/ferrite-core/src/merkle_state.rs` | **Create** — MerkleTreeState, SetBlockResult | M34a |
| `crates/ferrite-core/src/hash_picker.rs` | **Create** — HashPicker, AddHashesResult | M34a |
| `crates/ferrite-core/src/lib.rs` | Register new modules, exports | M34a |
| `crates/ferrite-storage/src/storage.rs` | Add verify_piece_v2(), hash_block() | M34b |
| `crates/ferrite-storage/src/chunk_tracker.rs` | Add block_verified field + methods | M34b |
| `crates/ferrite-session/src/disk.rs` | Add HashV2, BlockHash jobs + DiskHandle methods | M34b |
| `crates/ferrite-session/src/torrent.rs` | v2 verification flow, hash_picker integration | M34c |
| `crates/ferrite-session/src/peer.rs` | Hash request/response message handling | M34c |
| `crates/ferrite-core/src/resume_data.rs` | Add info_hash2, trees fields | M34c |
| `crates/ferrite/src/core.rs` | Re-export new types | M34c |
| `crates/ferrite/src/prelude.rs` | Add v2 types to prelude | M34c |
| `Cargo.toml` | Version bumps (0.38.0 → 0.39.0 → 0.40.0) | Each sub-milestone |

## What This Does NOT Cover (Deferred)

| Feature | Deferred To |
|---------|-------------|
| Hybrid v1+v2 torrents | M35 |
| BEP 53 magnet file selection (`so=`) | M35 |
| v2 torrent creation | M35 or separate |
| v2 DHT announce (32-byte info hash) | M35 |

## Verification

1. `cargo test --workspace` — all existing + ~43 new tests pass
2. `cargo clippy --workspace -- -D warnings` — zero warnings
3. HashRequest wire format matches libtorrent's 49-byte layout
4. MerkleTreeState correctly handles Ok/Unknown/HashFailed tri-state
5. HashPicker proactively requests block hashes (unlike libtorrent)
6. v1 torrents completely unaffected (no regressions)
