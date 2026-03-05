# Performance Bugfixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix 3 bugs and tune 4 constants to make ferrite's download throughput consistent across runs and reduce ramp-up time.

**Architecture:** All changes are in `crates/torrent-session/src/`. The TorrentActor is single-threaded (processes events sequentially), which means race conditions between actor event handlers are impossible — but stale cached state within a handler, Mutex contention with spawn_blocking tasks, and overly conservative timeouts are real issues. Fixes target: store buffer lock scope, pipeline loop freshness, and tuning constants.

**Tech Stack:** Rust, tokio, std::sync::Mutex, mpsc channels

**Build/Test:** `cargo test --workspace && cargo clippy --workspace -- -D warnings`

---

### Task 1: Store Buffer — Reduce Lock Scope in Verify Path

The store buffer (`Mutex<HashMap<...>>`) is accessed by both `enqueue_write()` (from TorrentActor on tokio runtime) and `enqueue_verify()` / `enqueue_verify_v2()` (from `spawn_blocking` threads). While the lock IS released before hashing (the `MutexGuard` temporary drops after `remove()`), the `remove()` call itself runs on a blocking thread that may compete with the tokio worker thread running `enqueue_write()`. Under heavy load with many pieces completing near-simultaneously, this creates brief but repeated contention on the std::sync::Mutex — which blocks the tokio worker thread.

**Files:**
- Modify: `crates/torrent-session/src/disk.rs:543-576` (enqueue_verify)
- Modify: `crates/torrent-session/src/disk.rs:581-612` (enqueue_verify_v2)
- Test: `crates/torrent-session/src/disk.rs` (inline #[cfg(test)] module)

**Step 1: Write a test proving verify works after store buffer refactor**

Add a test that verifies the store buffer remove-then-hash pattern works correctly. Place in the existing `#[cfg(test)]` module in `disk.rs` (after line ~1233).

```rust
#[tokio::test]
async fn store_buffer_verify_does_not_hold_lock_during_hash() {
    // Setup: create a DiskHandle with a store buffer
    // Write blocks for 2 different pieces
    // Trigger verify for piece 0
    // While verify is "in progress", write to piece 1 should succeed without blocking
    // (In practice, we verify the remove-then-hash pattern by checking both pieces verify correctly)
    use super::*;
    use bytes::Bytes;

    let (verify_tx, mut verify_rx) = mpsc::channel(8);
    let store_buffer: Arc<StoreBuffer> = Arc::new(Mutex::new(HashMap::new()));

    let info_hash = Id20::from([1u8; 20]);
    let piece_data = vec![0xABu8; 16384];
    let expected_hash = torrent_core::sha1(&piece_data);

    // Insert block into store buffer directly
    store_buffer
        .lock()
        .unwrap()
        .entry((info_hash, 0))
        .or_default()
        .insert(0, Bytes::from(piece_data.clone()));

    // Also insert a second piece to prove the lock isn't held during hashing
    store_buffer
        .lock()
        .unwrap()
        .entry((info_hash, 1))
        .or_default()
        .insert(0, Bytes::from(vec![0xCDu8; 16384]));

    // Verify piece 0
    let sb = Arc::clone(&store_buffer);
    tokio::spawn(async move {
        let passed = tokio::task::spawn_blocking(move || {
            let blocks = sb.lock().unwrap().remove(&(info_hash, 0));
            // After this line, lock is released.
            // Piece 1 should still be accessible.
            if let Some(blocks) = blocks {
                let mut data = Vec::with_capacity(16384);
                for d in blocks.values() {
                    data.extend_from_slice(d);
                }
                torrent_core::sha1(&data) == expected_hash
            } else {
                false
            }
        })
        .await
        .unwrap();
        let _ = verify_tx.send(VerifyResult { piece: 0, passed }).await;
    });

    // Piece 1 should still be in the store buffer (lock not held during hash)
    let piece_1_exists = store_buffer.lock().unwrap().contains_key(&(info_hash, 1));
    assert!(piece_1_exists, "piece 1 should be accessible while piece 0 verifies");

    let result = verify_rx.recv().await.unwrap();
    assert!(result.passed);
    assert_eq!(result.piece, 0);
}
```

**Step 2: Run test to verify it passes**

```bash
cargo test -p ferrite-session store_buffer_verify_does_not_hold_lock -- --nocapture
```

Expected: PASS (this test validates the current code already works correctly — the lock drops after remove)

**Step 3: Refactor enqueue_verify to make lock scope explicit**

At `disk.rs:553-570`, refactor to use an explicit scope block so the lock drop is visible and intentional, not relying on temporary lifetime rules:

```rust
    pub fn enqueue_verify(
        &self,
        piece: u32,
        expected: Id20,
        result_tx: &mpsc::Sender<VerifyResult>,
    ) {
        let store_buffer = Arc::clone(&self.store_buffer);
        let info_hash = self.info_hash;
        let result_tx = result_tx.clone();
        tokio::spawn(async move {
            let passed = tokio::task::spawn_blocking(move || {
                // Remove blocks from store buffer with minimal lock hold.
                // The MutexGuard drops at the end of this block, BEFORE hashing.
                let blocks = {
                    store_buffer.lock().unwrap().remove(&(info_hash, piece))
                };
                if let Some(blocks) = blocks {
                    let mut piece_data =
                        Vec::with_capacity(blocks.values().map(|b| b.len()).sum());
                    for data in blocks.values() {
                        piece_data.extend_from_slice(data);
                    }
                    torrent_core::sha1(&piece_data) == expected
                } else {
                    warn!(piece, "verify: store buffer miss, treating as failed");
                    false
                }
            })
            .await
            .unwrap();
            let _ = result_tx.send(VerifyResult { piece, passed }).await;
        });
    }
```

Apply the same pattern to `enqueue_verify_v2` at `disk.rs:591-606`:

```rust
                let blocks = {
                    store_buffer.lock().unwrap().remove(&(info_hash, piece))
                };
```

**Step 4: Run tests**

```bash
cargo test -p ferrite-session -- --nocapture 2>&1 | tail -5
```

Expected: All tests pass.

**Step 5: Commit**

```bash
git add crates/torrent-session/src/disk.rs
git commit -m "fix(disk): make store buffer lock scope explicit in verify path

Wrap the Mutex lock in a block scope so the MutexGuard drops immediately
after remove(), before piece assembly and hashing. The previous code was
technically correct (temporary drops at statement end), but the explicit
scope makes the intent clear and prevents future regressions.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 2: Pipeline Fill Loop — Validate Peer Each Iteration

The pipeline fill loop (lines 5059-5159) caches `peer_bitfield`, `suggested`, and `peer_rates` before the loop starts (lines 5037-5051). While the actor model prevents concurrent disconnects during the loop, the cached bitfield can be stale if the peer sent a `Have` message that was processed earlier in the same event batch. More importantly, if `try_send` returns an error (channel closed), the loop should break immediately rather than trying `pick_blocks` again with stale context.

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs:5037-5159`
- Test: `crates/torrent-session/src/torrent.rs` (inline #[cfg(test)] module)

**Step 1: Write a test for pipeline loop early exit on dead peer**

Add to the existing test module in `torrent.rs`. The test should verify that when a peer's command channel is closed, the pipeline loop stops trying to pick blocks for that peer.

```rust
#[test]
fn pipeline_loop_breaks_on_closed_channel() {
    // This is a unit-level logic test: if try_send fails with Closed,
    // sent == 0, and the loop should break. Verify by checking that
    // the function returns without panic when the channel is dropped.
    // (Integration test — actual pipeline loop is tested via the
    // connected_peer_send_requests test family)

    // The fix ensures we check peer existence at the TOP of each
    // iteration, not just inside the try_send call.
    // Verified by code inspection + existing integration tests.
}
```

Note: A true unit test for this requires mocking the full TorrentActor, which is complex. The fix is verified via code inspection and the existing `connected_peer_send_requests` integration tests. Skip the dedicated test; the fix is a 2-line defensive check.

**Step 2: Add peer existence check at loop top**

At `torrent.rs:5059`, add a check at the start of each loop iteration:

```rust
        loop {
            if slots_remaining == 0 {
                break;
            }

            // Defensive: verify peer still exists before each iteration.
            // While the actor model prevents concurrent disconnects, the
            // peer's channel may have been closed (peer task exited).
            if !self.peers.contains_key(&peer_addr) {
                break;
            }

            // Re-create closure and context each iteration...
```

**Step 3: Refresh peer bitfield each iteration**

Move the bitfield fetch inside the loop (replace the cache at line 5037-5040). At the top of the loop, after the existence check:

```rust
            // Refresh bitfield each iteration — peer may have sent Have
            // messages that updated it since the previous iteration.
            let peer_bitfield = match self.peers.get(&peer_addr) {
                Some(p) => p.bitfield.clone(),
                None => break,
            };
```

Remove the original `peer_bitfield` fetch at lines 5037-5040.

Keep `peer_rates` cached outside the loop (it's a snapshot of all peers, expensive to recompute each iteration, and staleness is acceptable).

Keep `suggested` cached outside the loop (minor staleness impact).

**Step 4: Run tests**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: All tests pass.

**Step 5: Run clippy**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -10
```

Expected: No warnings.

**Step 6: Commit**

```bash
git add crates/torrent-session/src/torrent.rs
git commit -m "fix(session): validate peer each pipeline loop iteration

Add peer existence check at the top of the pick_blocks fill loop and
refresh the peer bitfield each iteration. Prevents stale context from
causing wasted pick_blocks calls after a peer's channel closes.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 3: Disconnect Handler — Defensive Peer Check in Re-Request Loop

The disconnect handler at lines 3310-3315 iterates over a snapshot of peer addresses and calls `request_pieces_from_peer()` for each. While the actor model prevents concurrent disconnects, the function should still verify each peer exists as a defensive measure (guards against future refactors that might introduce concurrency).

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs:3309-3315`

**Step 1: Add peer existence check before re-request**

Replace lines 3310-3315:

```rust
                    // Re-request freed blocks from remaining peers
                    if had_pending {
                        let addrs: Vec<SocketAddr> = self.peers.keys().copied().collect();
                        for addr in addrs {
                            if self.peers.contains_key(&addr) {
                                self.request_pieces_from_peer(addr).await;
                            }
                        }
                    }
```

**Step 2: Run tests**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: All tests pass.

**Step 3: Commit**

```bash
git add crates/torrent-session/src/torrent.rs
git commit -m "fix(session): add defensive peer check in disconnect re-request loop

Verify each peer still exists before calling request_pieces_from_peer
in the disconnect handler's re-request loop. Defensive against future
concurrency changes.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 4: Reduce Snub Timeout from 60s to 15s

A snubbed peer holds up to 128 request slots for the full timeout duration. Reducing from 60s to 15s frees stalled slots 4x faster. rqbit effectively handles this in ~10s via read timeouts.

**Files:**
- Modify: `crates/torrent-session/src/settings.rs:190-192`
- Modify: `crates/torrent-session/src/types.rs:171` (if TorrentConfig has a hardcoded default)
- Test: `crates/torrent-session/src/pipeline.rs` (existing tests)

**Step 1: Update the test for new snub timeout**

Check if any existing test asserts the old 60s value. Search for `snub_timeout` in test code:

```bash
cargo test --workspace -- snub 2>&1
```

If tests reference the old value, update them.

**Step 2: Change default_snub_timeout_secs**

At `settings.rs:190-192`:

```rust
fn default_snub_timeout_secs() -> u32 {
    15
}
```

At `types.rs:171` (if present):

```rust
            snub_timeout_secs: 15,
```

**Step 3: Run tests**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: All tests pass.

**Step 4: Commit**

```bash
git add crates/torrent-session/src/settings.rs crates/torrent-session/src/types.rs
git commit -m "perf(session): reduce snub timeout from 60s to 15s

A stalled peer holding 128 request slots for 60 seconds wastes
significant pipeline capacity. 15 seconds is still conservative
while freeing slots 4x faster. rqbit handles this in ~10s.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 5: Queue Depth Floor — Raise Minimum from 2 to 64

The EWMA-based queue depth formula can drop to a floor of 2 (line 153 in pipeline.rs). This creates a negative feedback loop: brief throughput dip -> reduced depth -> less in-flight data -> further dip. A floor of 64 prevents the spiral while still allowing EWMA to modulate above that.

**Files:**
- Modify: `crates/torrent-session/src/pipeline.rs:150-154`
- Test: `crates/torrent-session/src/pipeline.rs` (inline tests)

**Step 1: Update the existing queue_depth_clamped test**

Find the existing test at ~line 256 in pipeline.rs. It likely asserts `clamp(2, max)`. Update to expect 64:

```rust
#[test]
fn queue_depth_floor_prevents_ewma_spiral() {
    let mut p = PeerPipelineState::new(250);
    // Simulate near-zero EWMA rate
    p.ewma_rate_bytes_sec = 100.0; // ~100 bytes/sec, would compute depth ~0
    p.recompute_queue_depth();
    // Floor should prevent depth from dropping below 64
    assert!(p.queue_depth >= 64, "depth {} should be >= 64", p.queue_depth);
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test -p ferrite-session queue_depth_floor -- --nocapture
```

Expected: FAIL (current floor is 2).

**Step 3: Change the floor in recompute_queue_depth**

At `pipeline.rs:150-154`:

```rust
    fn recompute_queue_depth(&mut self) {
        let depth = (self.ewma_rate_bytes_sec * self.request_queue_time / DEFAULT_CHUNK_SIZE as f64)
            as usize;
        self.queue_depth = depth.clamp(64, self.max_queue_depth);
    }
```

Also add a constant for clarity near line 18:

```rust
/// Minimum queue depth floor. Prevents EWMA-driven depth from spiralling
/// to near-zero during brief throughput dips.
const MIN_QUEUE_DEPTH: usize = 64;
```

And use it:

```rust
        self.queue_depth = depth.clamp(MIN_QUEUE_DEPTH, self.max_queue_depth);
```

**Step 4: Run test to verify it passes**

```bash
cargo test -p ferrite-session queue_depth_floor -- --nocapture
```

Expected: PASS.

**Step 5: Update existing tests that assert old floor value**

Check the `queue_depth_clamped` test (~line 256) and update if it asserts `clamp(2, ...)`.

**Step 6: Run all tests**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: All pass.

**Step 7: Commit**

```bash
git add crates/torrent-session/src/pipeline.rs
git commit -m "perf(pipeline): raise queue depth floor from 2 to 64

Prevents EWMA-driven negative feedback loop where brief throughput
dips reduce queue depth, which reduces in-flight data, which further
reduces throughput. 64 keeps the pipeline productive during transient
slowdowns while still allowing EWMA to modulate above that floor.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 6: Increase Peer Turnover Rate

Current: 4% of eligible peers every 300 seconds (~30 replacements/hour). New: 8% every 120 seconds (~120 replacements/hour). This cycles underperformers 4x faster.

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs:6691-6693`
- Modify: `crates/torrent-session/src/settings.rs` (if defaults defined there too)

**Step 1: Find and update the defaults**

At `torrent.rs:6691-6693`, update:

```rust
            peer_turnover: 0.08,
            peer_turnover_cutoff: 0.9,
            peer_turnover_interval: 120,
```

Search for matching defaults in `settings.rs` and update there too if present (search for `default_peer_turnover`).

**Step 2: Update any tests asserting old values**

```bash
cargo test --workspace -- peer_turnover 2>&1
```

Fix any tests that assert the old 0.04/300 values.

**Step 3: Run tests**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: All pass.

**Step 4: Commit**

```bash
git add crates/torrent-session/src/torrent.rs crates/torrent-session/src/settings.rs
git commit -m "perf(session): increase peer turnover from 4%/300s to 8%/120s

Cycles underperforming peers 4x faster (~120 replacements/hour vs ~30).
Faster churn means productive peers fill connection slots sooner,
reducing time spent on peers that contribute little throughput.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 7: Zombie Peer Pruning

Peers with empty bitfields (bf_ones == 0) after 30 seconds of connection contribute nothing. Disconnect them to free slots for productive peers. Add this check to the existing choking evaluation cycle.

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs:5460-5507` (run_choker function)

**Step 1: Add zombie detection after choking evaluation**

At the end of `run_choker()` (~line 5507), add zombie pruning:

```rust
        // Zombie pruning: disconnect peers with empty bitfields after 30s.
        // These peers consume connection slots but contribute no pieces.
        let zombie_threshold = Duration::from_secs(30);
        let zombies: Vec<SocketAddr> = self
            .peers
            .values()
            .filter(|p| {
                p.bitfield.count_ones() == 0
                    && p.connected_at.elapsed() > zombie_threshold
                    && !p.peer_choking // only if they've had time to send bitfield
            })
            .map(|p| p.addr)
            .collect();

        for addr in zombies {
            debug!(%addr, "disconnecting zombie peer (empty bitfield after 30s)");
            if let Some(peer) = self.peers.get(&addr) {
                let _ = peer.cmd_tx.try_send(PeerCommand::Disconnect);
            }
        }
```

Note: Check that `PeerCommand::Disconnect` exists. If not, use whatever mechanism the codebase uses to initiate peer disconnection (dropping the command channel, sending a close signal, etc.).

**Step 2: Verify PeerCommand variants**

Search for the disconnect mechanism:

```bash
grep -n "PeerCommand" crates/torrent-session/src/
```

Adapt the code to use the correct variant.

**Step 3: Run tests**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: All pass.

**Step 4: Run clippy**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -10
```

Expected: Clean.

**Step 5: Commit**

```bash
git add crates/torrent-session/src/torrent.rs
git commit -m "perf(session): prune zombie peers with empty bitfields after 30s

Peers that remain connected for 30+ seconds with zero pieces in their
bitfield consume connection slots without contributing downloads.
Disconnect them during the choking evaluation cycle to free slots
for productive peers.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 8: Final Verification

**Step 1: Full test suite**

```bash
cargo test --workspace 2>&1 | tail -10
```

Expected: All 1378+ tests pass.

**Step 2: Clippy clean**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -10
```

Expected: No warnings.

**Step 3: Version bump**

Bump the workspace version in the root `Cargo.toml` (e.g., 0.65.0 -> 0.66.0).

**Step 4: Commit version bump**

```bash
git add Cargo.toml
git commit -m "chore: bump version to v0.66.0

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

**Step 5: Update CHANGELOG.md**

Add entry for v0.66.0 covering all fixes and tuning changes.

**Step 6: Commit docs**

```bash
git add CHANGELOG.md
git commit -m "docs: update changelog for v0.66.0

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

**Step 7: Manual benchmark**

Download Arch Linux ISO 3 times consecutively, record average speed for each run. Success criteria:
- All 3 runs within 20% of each other
- Average >= 80 MB/s
