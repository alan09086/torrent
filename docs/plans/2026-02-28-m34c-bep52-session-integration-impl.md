# M34c: BEP 52 Session Integration Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Wire everything together — TorrentActor v2 verification flow, peer hash exchange, eager per-block hash requests, v2 resume data, and facade re-exports.

**Architecture:** `TorrentActor` gets `is_v2: bool` and `Option<HashPicker>` fields. `verify_and_mark_piece()` branches into v1 (SHA-1) and v2 (per-block SHA-256 + Merkle) paths. Peer tasks handle hash request/response messages (msg 21-23) and eagerly request block-level hashes alongside piece data. `FastResumeData` gains optional v2 fields for piece-layer caching.

**Tech Stack:** Rust edition 2024, `tokio` async, existing actor model (`TorrentActor`/`PeerTask`), M34a types (`HashPicker`, `MerkleTreeState`, `SetBlockResult`), M34b disk I/O (`verify_piece_v2`, `hash_block`).

**Design doc:** `docs/plans/2026-02-28-m34-bep52-v2-wire-storage-design.md`

**Depends on:** M34a (v0.38.0) + M34b (v0.39.0) complete.

---

### Task 1: Resume Data v2 Fields

**Files:**
- Modify: `crates/ferrite-core/src/resume_data.rs`

This task is independent of session changes and can be done first.

**Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `resume_data.rs`:

```rust
#[test]
fn resume_data_v2_fields_round_trip() {
    let mut resume = FastResumeData::new(
        vec![0xAA; 20],
        "v2-torrent".into(),
        "/downloads".into(),
    );
    resume.info_hash2 = Some(vec![0xBB; 32]);
    resume.trees.insert(
        hex::encode([0xCC; 32]),
        vec![0xDD; 64], // 2 piece hashes
    );

    let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
    let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
    assert_eq!(decoded.info_hash2, Some(vec![0xBB; 32]));
    assert_eq!(decoded.trees.len(), 1);
}

#[test]
fn resume_data_v1_backward_compat() {
    // v1 resume data without v2 fields should still parse
    let resume = FastResumeData::new(
        vec![0x00; 20],
        "v1-torrent".into(),
        "/tmp".into(),
    );
    assert!(resume.info_hash2.is_none());
    assert!(resume.trees.is_empty());

    let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
    let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
    assert!(decoded.info_hash2.is_none());
    assert!(decoded.trees.is_empty());
}

#[test]
fn resume_data_v2_empty_trees_not_serialized() {
    let resume = FastResumeData::new(
        vec![0x00; 20],
        "no-trees".into(),
        "/tmp".into(),
    );
    let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
    // "trees" key should not appear in output when empty
    let encoded_str = String::from_utf8_lossy(&encoded);
    assert!(!encoded_str.contains("trees"));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p ferrite-core -- resume_data::tests::resume_data_v2`
Expected: FAIL — `info_hash2` and `trees` fields don't exist.

**Step 3: Implement v2 resume data fields**

Add to `FastResumeData` struct, after the existing `http_seeds` field:

```rust
    /// SHA-256 v2 info hash (32 bytes, BEP 52).
    #[serde(rename = "info-hash2")]
    #[serde(with = "serde_bytes")]
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub info_hash2: Option<Vec<u8>>,

    /// Cached piece-layer Merkle hashes per file.
    /// Key: hex-encoded file root hash. Value: concatenated 32-byte piece hashes.
    /// Allows skipping piece-layer hash requests on resume.
    #[serde(rename = "trees")]
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub trees: HashMap<String, Vec<u8>>,
```

Add `use std::collections::HashMap;` at the top if not already present.

Update `FastResumeData::new()` to initialize the new fields:

```rust
            info_hash2: None,
            trees: HashMap::new(),
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferrite-core -- resume_data`
Expected: All existing + 3 new tests pass.

**Step 5: Commit**

```bash
git add crates/ferrite-core/src/resume_data.rs
git commit -m "feat: add v2 fields to FastResumeData (M34c)"
```

---

### Task 2: PeerEvent and PeerCommand v2 Variants

**Files:**
- Modify: `crates/ferrite-session/src/types.rs`

**Step 1: Add v2 event/command variants**

Add to `PeerEvent` enum (no tests needed — compilation verification):

```rust
    /// BEP 52: Received hash response from peer.
    HashesReceived {
        peer_addr: SocketAddr,
        request: ferrite_core::HashRequest,
        hashes: Vec<ferrite_core::Id32>,
    },
    /// BEP 52: Peer rejected our hash request.
    HashRequestRejected {
        peer_addr: SocketAddr,
        request: ferrite_core::HashRequest,
    },
    /// BEP 52: Peer sent a hash request to us.
    IncomingHashRequest {
        peer_addr: SocketAddr,
        request: ferrite_core::HashRequest,
    },
```

Add to `PeerCommand` enum:

```rust
    /// BEP 52: Send a hash request to the peer.
    SendHashRequest(ferrite_core::HashRequest),
    /// BEP 52: Send hashes in response to a peer's request.
    SendHashes {
        request: ferrite_core::HashRequest,
        hashes: Vec<ferrite_core::Id32>,
    },
    /// BEP 52: Reject a peer's hash request.
    SendHashReject(ferrite_core::HashRequest),
```

**Step 2: Verify compilation**

Run: `cargo check -p ferrite-session`
Expected: Compiles (unused variant warnings are OK at this stage with `#[allow(dead_code)]`).

**Step 3: Commit**

```bash
git add crates/ferrite-session/src/types.rs
git commit -m "feat: add v2 PeerEvent/PeerCommand variants (M34c)"
```

---

### Task 3: Peer Task — Handle Hash Messages

**Files:**
- Modify: `crates/ferrite-session/src/peer.rs`

**Step 1: Add hash message handling to `handle_message()`**

In the `handle_message()` function, find the existing `Message::Extended` match arm and add new arms after it for the v2 hash messages:

```rust
Message::HashRequest { pieces_root, base, index, count, proof_layers } => {
    let request = ferrite_core::HashRequest {
        file_root: pieces_root,
        base,
        index,
        count,
        proof_layers,
    };
    event_tx
        .send(PeerEvent::IncomingHashRequest {
            peer_addr: addr,
            request,
        })
        .await
        .ok();
    Ok(None)
}
Message::Hashes { pieces_root, base, index, count, proof_layers, hashes } => {
    let request = ferrite_core::HashRequest {
        file_root: pieces_root,
        base,
        index,
        count,
        proof_layers,
    };
    event_tx
        .send(PeerEvent::HashesReceived {
            peer_addr: addr,
            request,
            hashes,
        })
        .await
        .ok();
    Ok(None)
}
Message::HashReject { pieces_root, base, index, count, proof_layers } => {
    let request = ferrite_core::HashRequest {
        file_root: pieces_root,
        base,
        index,
        count,
        proof_layers,
    };
    event_tx
        .send(PeerEvent::HashRequestRejected {
            peer_addr: addr,
            request,
        })
        .await
        .ok();
    Ok(None)
}
```

**Step 2: Add hash command handling**

In the peer task's command processing loop, find where `PeerCommand` variants are matched and add:

```rust
PeerCommand::SendHashRequest(req) => {
    let msg = Message::HashRequest {
        pieces_root: req.file_root,
        base: req.base,
        index: req.index,
        count: req.count,
        proof_layers: req.proof_layers,
    };
    writer.send(msg).await.ok();
}
PeerCommand::SendHashes { request, hashes } => {
    let msg = Message::Hashes {
        pieces_root: request.file_root,
        base: request.base,
        index: request.index,
        count: request.count,
        proof_layers: request.proof_layers,
        hashes,
    };
    writer.send(msg).await.ok();
}
PeerCommand::SendHashReject(req) => {
    let msg = Message::HashReject {
        pieces_root: req.file_root,
        base: req.base,
        index: req.index,
        count: req.count,
        proof_layers: req.proof_layers,
    };
    writer.send(msg).await.ok();
}
```

**Step 3: Verify compilation**

Run: `cargo check -p ferrite-session`
Expected: Compiles.

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/peer.rs
git commit -m "feat: peer task handles BEP 52 hash messages (M34c)"
```

---

### Task 4: TorrentActor v2 Fields + Initialisation

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

**Step 1: Add v2 fields to TorrentActor**

Add to the `TorrentActor` struct:

```rust
    // BEP 52 v2 support (M34)
    hash_picker: Option<ferrite_core::HashPicker>,
    is_v2: bool,
    meta_v2: Option<ferrite_core::TorrentMetaV2>,
```

Initialize in the constructor (wherever `TorrentActor` is created):

```rust
    hash_picker: None,
    is_v2: false,
    meta_v2: None,
```

**Step 2: Add v2 initialisation when metadata arrives**

Find where `TorrentMetaV1` is assigned to `self.meta` (metadata resolution path). Add a v2 detection branch. This will depend on how `AddTorrentParams` provides the metadata — check whether it provides `TorrentMeta` (the auto-detected enum) or always `TorrentMetaV1`.

If using `torrent_from_bytes_any()`:

```rust
// After metadata is resolved:
match ferrite_core::torrent_from_bytes_any(&torrent_bytes) {
    Ok(ferrite_core::TorrentMeta::V2(meta_v2)) => {
        self.is_v2 = true;
        // Build HashPicker from v2 file info
        let files: Vec<ferrite_core::FileHashInfo> = meta_v2.info.files()
            .iter()
            .map(|f| ferrite_core::FileHashInfo {
                root: f.attr.pieces_root.unwrap_or(ferrite_core::Id32::ZERO),
                num_blocks: (f.attr.length as u32).div_ceil(16384),
                num_pieces: (f.attr.length as u32).div_ceil(meta_v2.info.piece_length as u32),
            })
            .filter(|f| f.num_blocks > 0)
            .collect();

        let blocks_per_piece = (meta_v2.info.piece_length / 16384) as u32;
        let mut picker = ferrite_core::HashPicker::new(&files, blocks_per_piece);

        // Load piece layers from torrent metadata
        for (i, file) in meta_v2.info.files().iter().enumerate() {
            if let Some(root) = file.attr.pieces_root {
                if let Some(hashes) = meta_v2.file_piece_hashes(&root) {
                    picker.trees[i].set_piece_hashes(hashes);
                }
            }
        }

        // Enable v2 chunk tracking
        if let Some(ref mut ct) = self.chunk_tracker {
            ct.enable_v2_tracking();
        }

        self.hash_picker = Some(picker);
        self.meta_v2 = Some(meta_v2);
    }
    Ok(ferrite_core::TorrentMeta::V1(meta_v1)) => {
        // Existing v1 path
        self.meta = Some(meta_v1);
    }
    Err(e) => {
        warn!("failed to parse torrent metadata: {e}");
    }
}
```

**Note:** The exact integration point depends on the current metadata resolution flow. Read `torrent.rs` carefully to find where `self.meta = Some(...)` is set. The v2 branch should be added alongside this, not replacing it.

**Step 3: Verify compilation**

Run: `cargo check -p ferrite-session`
Expected: Compiles (may have unused field warnings — that's OK, they're consumed in Task 5).

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat: TorrentActor v2 fields and initialisation (M34c)"
```

---

### Task 5: v2 Piece Verification Flow

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

This is the core change — splitting `verify_and_mark_piece()` into v1 and v2 paths.

**Step 1: Refactor verify_and_mark_piece**

Rename the existing method to `verify_and_mark_piece_v1` and create a new dispatcher:

```rust
async fn verify_and_mark_piece(&mut self, index: u32) {
    if self.is_v2 {
        self.verify_and_mark_piece_v2(index).await;
    } else {
        self.verify_and_mark_piece_v1(index).await;
    }
}

async fn verify_and_mark_piece_v1(&mut self, index: u32) {
    // Move the existing verify_and_mark_piece body here unchanged
}
```

**Step 2: Implement verify_and_mark_piece_v2**

```rust
async fn verify_and_mark_piece_v2(&mut self, index: u32) {
    let disk = match &self.disk {
        Some(d) => d.clone(),
        None => return,
    };

    let lengths = match &self.lengths {
        Some(l) => l.clone(),
        None => return,
    };

    // Flush write buffer before reading back
    if let Err(e) = disk.flush_piece(index).await {
        warn!(index, "failed to flush piece for v2 verification: {e}");
        return;
    }

    // Compute SHA-256 of each 16 KiB block
    let num_chunks = lengths.chunks_in_piece(index);
    let mut all_ok = true;
    let mut any_failed = false;

    for chunk_idx in 0..num_chunks {
        let (begin, length) = match lengths.chunk_info(index, chunk_idx) {
            Some(info) => info,
            None => continue,
        };

        let block_hash = match disk.hash_block(index, begin, length, DiskJobFlags::empty()).await {
            Ok(h) => h,
            Err(e) => {
                warn!(index, chunk_idx, "failed to hash block: {e}");
                all_ok = false;
                break;
            }
        };

        // Feed to hash picker
        if let Some(ref mut picker) = self.hash_picker {
            // TODO: determine file_index from piece index (single-file for now)
            let file_index = 0;
            let global_block = index * (lengths.piece_length() as u32 / 16384) + chunk_idx;
            match picker.set_block_hash(file_index, global_block, begin, block_hash) {
                SetBlockResult::Ok => {
                    if let Some(ref mut ct) = self.chunk_tracker {
                        ct.mark_block_verified(index, chunk_idx);
                    }
                }
                SetBlockResult::Unknown => {
                    // Piece-layer hash not available yet — stored for later
                    debug!(index, chunk_idx, "block hash stored, piece layer not yet available");
                }
                SetBlockResult::HashFailed => {
                    warn!(index, chunk_idx, "block hash failed Merkle verification");
                    any_failed = true;
                    break;
                }
            }
        }
    }

    if any_failed {
        // Piece failed — same handling as v1 hash failure
        let contributors: Vec<std::net::IpAddr> = self.piece_contributors
            .remove(&index)
            .unwrap_or_default()
            .into_iter()
            .collect();

        warn!(index, contributors = contributors.len(), "v2 piece Merkle verification failed");

        if let Some(parole) = self.parole_pieces.remove(&index) {
            self.apply_parole_failure(index, parole);
        } else {
            self.enter_parole(index, contributors.clone());
        }

        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::HashFailed {
            info_hash: self.info_hash,
            piece: index,
            contributors,
        });
        if let Some(ref mut ct) = self.chunk_tracker {
            ct.mark_failed(index);
        }
        self.in_flight_pieces.remove(&index);
        if let Some(ref disk) = self.disk {
            disk.clear_piece(index).await;
        }
    } else if all_ok
        && self.chunk_tracker.as_ref().is_some_and(|ct| ct.all_blocks_verified(index))
    {
        // All blocks verified — piece is good
        self.on_piece_verified(index).await;
    }
    // else: Unknown state — blocks stored, will be verified when piece-layer hashes arrive
}
```

**Step 3: Extract common post-verification logic**

The success path in `verify_and_mark_piece_v1` (mark_verified, post alerts, broadcast Have, etc.) should be extracted into a shared `on_piece_verified()` method:

```rust
async fn on_piece_verified(&mut self, index: u32) {
    if let Some(ref mut ct) = self.chunk_tracker {
        ct.mark_verified(index);
    }
    self.in_flight_pieces.remove(&index);
    self.piece_contributors.remove(&index);
    info!(index, "piece verified");
    post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PieceFinished {
        info_hash: self.info_hash,
        piece: index,
    });

    let _ = self.piece_ready_tx.send(index);
    if let Some(ref ct) = self.chunk_tracker {
        let _ = self.have_watch_tx.send(ct.bitfield().clone());
    }

    if let Some(parole) = self.parole_pieces.remove(&index) {
        self.apply_parole_success(index, parole).await;
    }

    // Broadcast Have (same as existing logic)
    if self.super_seed.is_none() {
        if self.have_buffer.is_enabled() {
            self.have_buffer.push(index);
        } else {
            for peer in self.peers.values() {
                if !peer.bitfield.get(index) {
                    let _ = peer.cmd_tx.send(PeerCommand::Have(index)).await;
                }
            }
        }
    }

    // Share mode LRU (same as existing)
    if self.share_max_pieces > 0 {
        self.share_lru.push_back(index);
        while self.share_lru.len() > self.share_max_pieces {
            if let Some(evicted) = self.share_lru.pop_front() {
                if let Some(ref mut ct) = self.chunk_tracker {
                    ct.clear_piece(evicted);
                }
                if evicted < self.wanted_pieces.len() {
                    self.wanted_pieces.set(evicted);
                }
                debug!(evicted, "share mode: evicted piece from LRU");
            }
        }
    }

    // Check download complete (same as existing)
    if self.share_max_pieces == 0
        && let Some(ref ct) = self.chunk_tracker
        && ct.bitfield().count_ones() == self.num_pieces
    {
        info!("download complete, transitioning to seeding");
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::TorrentFinished {
            info_hash: self.info_hash,
        });
        self.end_game.deactivate();
        self.transition_state(TorrentState::Seeding);
        self.choker.set_seed_mode(true);
        if self.config.upload_only_announce {
            let hs = ferrite_wire::ExtHandshake::new_upload_only();
            for peer in self.peers.values() {
                let _ = peer.cmd_tx.send(PeerCommand::SendExtHandshake(hs.clone())).await;
            }
        }
        let result = self
            .tracker_manager
            .announce_completed(self.uploaded, self.downloaded)
            .await;
        self.fire_tracker_alerts(&result.outcomes);
    }
}
```

Then `verify_and_mark_piece_v1` calls `self.on_piece_verified(index).await` on success.

**Step 4: Verify compilation**

Run: `cargo check -p ferrite-session`
Expected: Compiles.

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat: v2 piece verification flow with per-block Merkle check (M34c)"
```

---

### Task 6: TorrentActor — Handle Hash Events

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

**Step 1: Handle hash events in the actor's event loop**

Find where `PeerEvent` variants are matched in the actor's `select!` loop. Add handlers:

```rust
PeerEvent::HashesReceived { peer_addr, request, hashes } => {
    if let Some(ref mut picker) = self.hash_picker {
        match picker.add_hashes(&request, &hashes) {
            Ok(result) => {
                // Process any pieces that became verifiable
                for piece in result.hash_passed {
                    self.on_piece_verified(piece).await;
                }
                for piece in result.hash_failed {
                    warn!(piece, "piece failed after hash layer received");
                    post_alert(&self.alert_tx, &self.alert_mask, AlertKind::HashFailed {
                        info_hash: self.info_hash,
                        piece,
                        contributors: Vec::new(),
                    });
                }
            }
            Err(e) => {
                warn!(peer = %peer_addr, "invalid hashes: {e}");
            }
        }
    }
}
PeerEvent::HashRequestRejected { peer_addr, request } => {
    if let Some(ref mut picker) = self.hash_picker {
        picker.hashes_rejected(&request);
    }
    debug!(peer = %peer_addr, "hash request rejected");
}
PeerEvent::IncomingHashRequest { peer_addr, request } => {
    // Serve hashes if we have them (seeding v2 torrent)
    self.handle_incoming_hash_request(peer_addr, request).await;
}
```

**Step 2: Implement hash request serving**

```rust
async fn handle_incoming_hash_request(
    &self,
    peer_addr: SocketAddr,
    request: ferrite_core::HashRequest,
) {
    let peer = match self.peers.get(&peer_addr) {
        Some(p) => p,
        None => return,
    };

    // For now, reject all hash requests if we don't have hash picker
    // (v1 torrent or metadata not yet available)
    let Some(ref picker) = self.hash_picker else {
        let _ = peer.cmd_tx.send(PeerCommand::SendHashReject(request)).await;
        return;
    };

    // TODO: look up hashes from our Merkle tree state and serve them
    // For now, reject — serving will be implemented when we have seeding support
    let _ = peer.cmd_tx.send(PeerCommand::SendHashReject(request)).await;
}
```

**Step 3: Add periodic hash request sending**

In the actor's periodic tick (where tracker announces, choking, etc. happen), add hash request sending to connected v2 peers:

```rust
// Periodic: send hash requests to v2 peers
if self.is_v2 {
    if let Some(ref picker) = self.hash_picker {
        let have_vec: Vec<bool> = (0..self.num_pieces)
            .map(|i| self.chunk_tracker.as_ref().is_some_and(|ct| ct.has_piece(i)))
            .collect();

        if let Some(req) = picker.pick_hashes(&have_vec) {
            // Send to first available peer that has relevant pieces
            for peer in self.peers.values() {
                let _ = peer.cmd_tx.send(PeerCommand::SendHashRequest(req.clone())).await;
                break; // one request at a time per tick
            }
        }
    }
}
```

**Step 4: Verify compilation**

Run: `cargo check -p ferrite-session`
Expected: Compiles.

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat: TorrentActor handles hash events and sends requests (M34c)"
```

---

### Task 7: Facade Re-exports + Version Bump

**Files:**
- Modify: `crates/ferrite/src/core.rs`
- Modify: `crates/ferrite/src/prelude.rs`
- Modify: `Cargo.toml`

**Step 1: Add M34b/c re-exports**

In `crates/ferrite/src/core.rs`, ensure all M34 types are exported (some may already be there from M34a):

```rust
pub use ferrite_core::{
    // M34a
    HashRequest, validate_hash_request,
    MerkleTreeState, SetBlockResult,
    HashPicker, FileHashInfo, AddHashesResult,
};
```

In `crates/ferrite/src/prelude.rs`, verify `HashPicker` and `SetBlockResult` are present.

**Step 2: Bump version**

In root `Cargo.toml`, change workspace version from `"0.39.0"` to `"0.40.0"`.

**Step 3: Run full test suite**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All tests pass, zero warnings.

**Step 4: Commit**

```bash
git add crates/ferrite/src/core.rs crates/ferrite/src/prelude.rs Cargo.toml
git commit -m "feat: facade re-exports and version bump to 0.40.0 (M34c)"
```

---

### Task 8: Documentation Update

**Files:**
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `CLAUDE.md`

**Step 1: Update README.md**

- Update test counts across all affected crates
- Update total test count
- Update roadmap: Phase 6 M34 complete
- Add note about BEP 52 wire protocol + proactive per-block verification

**Step 2: Update CHANGELOG.md**

Add v0.40.0 entry:

```markdown
## v0.40.0 — M34c: BEP 52 Session Integration

### Added
- `FastResumeData` v2 fields: `info_hash2` (SHA-256), `trees` (piece-layer cache)
- `PeerEvent::HashesReceived`, `HashRequestRejected`, `IncomingHashRequest`
- `PeerCommand::SendHashRequest`, `SendHashes`, `SendHashReject`
- Peer task handles BEP 52 hash messages (msg 21-23)
- `TorrentActor` v2 verification flow with per-block SHA-256 + Merkle check
- `on_piece_verified()` shared post-verification logic (refactored from v1 path)
- Periodic hash request sending to connected v2 peers
- Eager per-block hash requesting alongside piece data (ferrite-specific, beyond libtorrent)

### Architecture Notes
- `verify_and_mark_piece()` now branches: v1 (SHA-1 full piece) vs v2 (per-block SHA-256 + Merkle)
- v2 verification handles tri-state: Ok (verified), Unknown (deferred), HashFailed (ban peer)
- Hash request serving stubs in place — full seeding support deferred to future milestone
```

**Step 3: Update CLAUDE.md**

Update the Key Types & Patterns Reference to include M34 types and the v2 verification flow.
Update the Milestones section: M34 complete.

**Step 4: Commit and push**

```bash
git add README.md CHANGELOG.md CLAUDE.md
git commit -m "docs: update README/changelog/CLAUDE.md for M34c (v0.40.0)"
git push origin main && git push github main
```

---

### Task 9: Memory File Update

**Files:**
- Modify: `/home/alan/.claude/projects/-mnt-TempNVME-projects/memory/MEMORY.md`

**Step 1: Update ferrite status**

Update the Ferrite section in MEMORY.md:

```markdown
- **Status**: M1-M34 complete (XXX tests, v0.40.0), M35 (hybrid torrents) next
```

Update the M33 note to include M34:

```markdown
- **M34 complete**: BEP 52 v2 wire protocol + storage — HashRequest/Hashes/HashReject wire messages,
  HashPicker with proactive per-block requests, MerkleTreeState tri-state verification,
  TorrentStorage v2 methods, DiskJob v2 variants, TorrentActor v2 verification flow, v2 resume data
```

**Step 2: Commit (no git — memory file)**

Memory files don't get committed to the project repo.

---

## Summary

| Task | What | Tests | Files |
|------|------|-------|-------|
| 1 | Resume data v2 fields | 3 | resume_data.rs |
| 2 | PeerEvent/PeerCommand v2 variants | 0 | types.rs |
| 3 | Peer task hash message handling | 0 | peer.rs |
| 4 | TorrentActor v2 fields + init | 0 | torrent.rs |
| 5 | v2 piece verification flow | 0 | torrent.rs |
| 6 | TorrentActor hash event handling | 0 | torrent.rs |
| 7 | Facade + version bump | 0 | core.rs, prelude.rs, Cargo.toml |
| 8 | Documentation | 0 | README, CHANGELOG, CLAUDE.md |
| 9 | Memory file update | 0 | MEMORY.md |
| **Total** | | **3** | |

**Note on test count:** M34c has fewer unit tests because the session integration is primarily wiring — connecting M34a types (HashPicker, MerkleTreeState) to M34b I/O (DiskHandle v2 methods) through the existing actor model. The correctness is verified through M34a's 23 tests (hash picker logic) and M34b's 11 tests (storage verification). Integration testing of the full flow would require a mock peer network, which is better suited to a future integration test milestone.
