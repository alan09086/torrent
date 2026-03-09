# M61: Performance Optimizations — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix the 6 bottlenecks identified in the v0.66.0 benchmark report and re-benchmark to measure improvement.

**Architecture:** Each bottleneck is a self-contained change to torrent-session internals (piece picker, peer state, disk I/O, end-game logic). Changes are ordered by implementation risk — data structure swaps first, then algorithmic changes, then I/O path changes. After all fixes, run the standardized 5-trial benchmark and generate a v0.67.0 report PDF.

**Tech Stack:** Rust (edition 2024), tokio, `rustc_hash::FxHashMap`, `bytes::Bytes`, pandoc (PDF generation)

**Baseline (v0.66.0):** 15.0 MB/s avg, 170.2 MiB RSS, 37.1s CPU (3.8x slower than rqbit)

**Target:** 40-60 MB/s avg, <80 MiB RSS

**Benchmark report template:** `docs/benchmark-report-v0.66.0.md` — every performance milestone produces a versioned copy with identical structure.

---

## Task 1: peer_count() — Replace O(n log n) Sort-Dedup with HashSet Counter

**Priority 5 from report. Lowest risk, pure data structure change.**

**Files:**
- Modify: `crates/torrent-session/src/piece_selector.rs` (lines 1-10 for imports, lines 73-81 for peer_count)
- Test: existing tests via `cargo test -p torrent-session`

**Context:**
- `InFlightPiece` has an `assigned_blocks: HashMap<(u32, u32), SocketAddr>` mapping `(piece_index, block_offset) -> peer_addr`
- `peer_count()` currently collects all values into a Vec, sorts, and deduplicates to count unique peers
- Called from piece scoring logic (line ~365) for snubbed peers during pick cycles
- With 128 assigned blocks per piece, this allocates and sorts a 128-element Vec on every call

**Step 1: Add a unique peer counter field to InFlightPiece**

In `piece_selector.rs`, add a `HashSet<SocketAddr>` field called `unique_peers` to `InFlightPiece`. Initialize it in the constructor.

```rust
use std::collections::HashSet;

pub(crate) struct InFlightPiece {
    pub assigned_blocks: HashMap<(u32, u32), SocketAddr>,
    unique_peers: HashSet<SocketAddr>,
    // ... other fields unchanged
}
```

Update the constructor to initialize `unique_peers: HashSet::new()`.

**Step 2: Maintain unique_peers on insert/remove**

Find every place where `assigned_blocks` is modified:
- `assign_block()` — insert into `unique_peers` when adding
- `block_completed()` / `block_cancelled()` — when removing from `assigned_blocks`, rebuild `unique_peers` only if the removed peer has no remaining blocks (scan values for the removed addr)

For the removal path, since rebuilding the full set on every removal would be wasteful, use a reference-counted approach:

```rust
pub fn assign_block(&mut self, block_offset: (u32, u32), peer: SocketAddr) {
    self.assigned_blocks.insert(block_offset, peer);
    self.unique_peers.insert(peer);
}

pub fn remove_block(&mut self, block_offset: &(u32, u32)) -> Option<SocketAddr> {
    if let Some(peer) = self.assigned_blocks.remove(block_offset) {
        // Check if this peer still has any other assigned blocks
        if !self.assigned_blocks.values().any(|&p| p == peer) {
            self.unique_peers.remove(&peer);
        }
        Some(peer)
    } else {
        None
    }
}
```

**Step 3: Replace peer_count() body**

```rust
pub fn peer_count(&self) -> usize {
    self.unique_peers.len()
}
```

**Step 4: Run tests**

```bash
cargo test -p torrent-session 2>&1 | tail -5
cargo test -p torrent-sim 2>&1 | tail -5
```

Expected: all tests pass. The swarm simulation tests exercise the piece picker end-to-end.

**Step 5: Run clippy**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -5
```

Expected: no warnings.

**Step 6: Commit**

```bash
git add -A
git commit -m "perf: replace peer_count() O(n log n) sort with HashSet counter (M61)"
```

---

## Task 2: Bitfield Clones — Pass by Reference in Picker

**Priority 6 from report. Low risk, signature change.**

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs` (lines ~5219, ~5375, ~5892 — bitfield clone sites)
- Modify: `crates/torrent-session/src/piece_selector.rs` (picker functions that accept bitfield)
- Modify: `crates/torrent-session/src/end_game.rs` (pick_block functions that accept bitfield)
- Test: existing tests via `cargo test -p torrent-session`

**Context:**
- `request_pieces_from_peer()` (line ~5219) clones the bitfield once before the picker loop — this is acceptable
- `request_end_game_block()` (line ~5375) clones the bitfield every call — called for every freed peer during reactive re-requesting
- The picker functions (`pick_piece()`, `pick_block()`, `pick_block_strict()`, `pick_block_streaming()`) receive the bitfield — check if they take ownership or just read it
- For a 1.5 GiB torrent, the bitfield is ~768 bytes. For a 100 GiB torrent, ~12.5 KB. Cloning 18,000 times/sec = significant churn.

**Step 1: Change picker functions to accept `&BitVec` instead of owned `BitVec`**

Find all picker/end-game functions that accept a bitfield parameter. Change their signatures from `bitfield: BitVec` (or whatever the type is) to `bitfield: &BitVec`. These functions only read the bitfield, never mutate it.

Update all call sites to pass `&peer_bitfield` instead of `peer_bitfield.clone()`.

**Step 2: Remove unnecessary clones in torrent.rs**

In `request_end_game_block()` (line ~5375), change:
```rust
// Before:
let peer_bitfield = match self.peers.get(&peer_addr) {
    Some(p) => p.bitfield.clone(),
    None => return,
};
```
to borrow directly (if borrow checker allows — since `self.peers` is borrowed immutably for the bitfield, but `self.end_game` is borrowed mutably for the pick call, this may require extracting the bitfield reference carefully):

```rust
// If the borrow checker fights back, clone once at the top (current behavior for
// request_pieces_from_peer is fine). The key win is removing clones in the
// reactive re-requesting loop where request_end_game_block is called per peer.
```

If the borrow checker prevents direct borrowing (likely, since `self` is used mutably in the same function), keep the single clone at the top of `request_end_game_block()` but ensure the reactive re-requesting loop in the block-received handler doesn't clone redundantly.

**Step 3: Remove clones in sequential/webseed paths**

Lines ~5892, ~6032, ~6213 — apply the same pattern. These are lower frequency but keep the code consistent.

**Step 4: Run tests and clippy**

```bash
cargo test -p torrent-session 2>&1 | tail -5
cargo test -p torrent-sim 2>&1 | tail -5
cargo clippy --workspace -- -D warnings 2>&1 | tail -5
```

Expected: all pass.

**Step 5: Commit**

```bash
git add -A
git commit -m "perf: pass bitfield by reference in picker functions (M61)"
```

---

## Task 3: pending_requests — Replace Vec with FxHashMap

**Priority 4 from report. Medium risk — data structure change across 4+ call sites.**

**Files:**
- Modify: `crates/torrent-session/src/peer_state.rs` (line ~55, type definition)
- Modify: `crates/torrent-session/src/torrent.rs` (lines ~3348, ~3515, ~3576, ~3614 — position() calls; plus all push/insert/len/clear/iter sites)
- Modify: `crates/torrent-session/Cargo.toml` (add `rustc-hash` dependency if not present)
- Test: existing tests + new unit test for pending request ops

**Context:**
- `pending_requests: Vec<(u32, u32, u32)>` stores `(piece_index, block_offset, block_length)` per peer
- Lookups use `.iter().position(|&(i, b, _)| i == index && b == begin)` — O(n) linear scan
- With pipeline depth 128, each lookup scans up to 128 entries
- Called on every block received, every reject, every cancel — ~6,000+ times/sec during active download
- `swap_remove` is used after `position()` — order doesn't matter

**Step 1: Write a unit test for the new data structure**

Create a test in `crates/torrent-session/src/peer_state.rs` (or a test module) that exercises insert, lookup, remove, len, iter, and clear on the new structure.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_requests_insert_remove() {
        let mut pending = PendingRequests::new();
        pending.insert(0, 0, 16384);
        pending.insert(0, 16384, 16384);
        pending.insert(1, 0, 16384);
        assert_eq!(pending.len(), 3);

        // Remove by (index, begin)
        let removed = pending.remove(0, 16384);
        assert_eq!(removed, Some(16384)); // returns length
        assert_eq!(pending.len(), 2);

        // Remove non-existent
        assert_eq!(pending.remove(99, 0), None);
        assert_eq!(pending.len(), 2);

        // Contains
        assert!(pending.contains(0, 0));
        assert!(!pending.contains(0, 16384)); // was removed

        // Clear
        pending.clear();
        assert_eq!(pending.len(), 0);
    }
}
```

**Step 2: Run the test to verify it fails**

```bash
cargo test -p torrent-session pending_requests 2>&1 | tail -5
```

Expected: FAIL — `PendingRequests` type doesn't exist yet.

**Step 3: Implement PendingRequests wrapper**

In `peer_state.rs`, add a newtype around `FxHashMap<(u32, u32), u32>` keyed on `(piece_index, block_offset)`, value is `block_length`:

```rust
use rustc_hash::FxHashMap;

#[derive(Debug, Clone, Default)]
pub(crate) struct PendingRequests {
    map: FxHashMap<(u32, u32), u32>,
}

impl PendingRequests {
    pub fn new() -> Self {
        Self { map: FxHashMap::default() }
    }

    pub fn insert(&mut self, index: u32, begin: u32, length: u32) {
        self.map.insert((index, begin), length);
    }

    pub fn remove(&mut self, index: u32, begin: u32) -> Option<u32> {
        self.map.remove(&(index, begin))
    }

    pub fn contains(&self, index: u32, begin: u32) -> bool {
        self.map.contains_key(&(index, begin))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Iterate all pending requests as (index, begin, length).
    pub fn iter(&self) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
        self.map.iter().map(|(&(i, b), &l)| (i, b, l))
    }
}
```

**Step 4: Run the unit test**

```bash
cargo test -p torrent-session pending_requests 2>&1 | tail -5
```

Expected: PASS.

**Step 5: Update PeerState to use PendingRequests**

Change `peer_state.rs`:
```rust
// Before:
pub pending_requests: Vec<(u32, u32, u32)>,

// After:
pub pending_requests: PendingRequests,
```

**Step 6: Update all call sites in torrent.rs**

Replace all 4 `.iter().position()` + `swap_remove()` patterns with `.remove()`:

```rust
// Before (4 sites):
if let Some(pos) = peer.pending_requests.iter().position(|&(i, b, _)| i == index && b == begin) {
    peer.pending_requests.swap_remove(pos);
}

// After:
peer.pending_requests.remove(index, begin);
```

Replace all `.push((index, begin, length))` with `.insert(index, begin, length)`.

Search for any other usages: `.iter()` for cancellation loops, `.len()` for pipeline depth checks, `.clear()` on disconnect. Update each to use the new API.

**Step 7: Run full test suite**

```bash
cargo test --workspace 2>&1 | tail -5
cargo clippy --workspace -- -D warnings 2>&1 | tail -5
```

Expected: all 1386+ tests pass, no clippy warnings.

**Step 8: Commit**

```bash
git add -A
git commit -m "perf: replace pending_requests Vec with FxHashMap for O(1) lookup (M61)"
```

---

## Task 4: End-Game Pipeline Depth — Raise to Match Normal Mode

**Priority 1 from report. Trivial change, highest impact.**

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs` (line ~5364, `END_GAME_DEPTH` constant)
- Test: existing tests + benchmark validation

**Context:**
- `END_GAME_DEPTH = 4` limits each peer to 4 in-flight requests during end-game
- Normal mode uses 128 in-flight requests per peer
- With 50 peers at 100ms RTT: end-game ceiling = 50 x 4 x 16KB / 0.1s = ~32 MB/s
- Raising to 128 removes the artificial throttle
- rqbit uses a flat 128-permit semaphore with no end-game reduction
- The download spends 5-10% of time in end-game but that translates to 40-60 seconds of wall time
- `final_speed=0.0 MB/s` on all 5 v0.66.0 trials — speed drops to near zero in the tail

**Step 1: Change the constant**

```rust
// Before:
const END_GAME_DEPTH: usize = 4;

// After:
const END_GAME_DEPTH: usize = 128;
```

**Step 2: Run tests**

```bash
cargo test -p torrent-session 2>&1 | tail -5
cargo test -p torrent-sim 2>&1 | tail -5
```

Expected: all pass. The swarm sim tests exercise end-game and should still complete.

**Step 3: Commit**

```bash
git add -A
git commit -m "perf: raise END_GAME_DEPTH from 4 to 128, eliminating tail stall (M61)"
```

**Why this is Task 4 (not Task 1 despite being Priority 1):**
The constant change is trivial but its effect depends on the cascade fix (Task 5). With depth=128 but the reactive cascade still active, we'd get 128x more cascade calls per block — potentially worse CPU. So we fix the data structures first (Tasks 1-3), then raise the depth, then fix the cascade.

---

## Task 5: Reactive Re-Requesting — Batch Into Next Tick

**Priority 2 from report. Medium risk, control flow change.**

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs` (lines ~3601-3628, end-game block received handler)
- Test: existing tests + swarm sim validation

**Context:**
- When a block is received in end-game, duplicates are cancelled on other peers
- For each cancelled peer, `request_end_game_block()` is called immediately — full picker cycle per peer
- With 50 peers and depth 128, this triggers 50+ picker cycles per block received
- The fix: instead of calling `request_end_game_block()` immediately for each freed peer, collect them and let the existing pipeline tick handle refilling
- The pipeline tick already runs on a regular interval and calls `request_end_game_block()` for all peers with available slots

**Step 1: Remove the reactive re-requesting loop**

In `torrent.rs`, find the end-game block received handler (lines ~3601-3628). Remove the `freed_peers` collection and the loop that calls `request_end_game_block()`:

```rust
// Before:
if self.end_game.is_active() {
    let cancels = self.end_game.block_received(index, begin, peer_addr);
    let mut freed_peers = Vec::new();
    for (cancel_addr, ci, cb, cl) in cancels {
        if let Some(cancel_peer) = self.peers.get_mut(&cancel_addr) {
            let _ = cancel_peer.cmd_tx.try_send(PeerCommand::Cancel {
                index: ci,
                begin: cb,
                length: cl,
            });
            if let Some(pos) = cancel_peer
                .pending_requests
                .iter()
                .position(|&(i, b, _)| i == ci && b == cb)
            {
                cancel_peer.pending_requests.swap_remove(pos);
            }
            freed_peers.push(cancel_addr);
        }
    }
    // Reactive scheduling: feed cancelled peers new blocks immediately
    for addr in freed_peers {
        self.request_end_game_block(addr).await;
    }
}

// After:
if self.end_game.is_active() {
    let cancels = self.end_game.block_received(index, begin, peer_addr);
    for (cancel_addr, ci, cb, cl) in cancels {
        if let Some(cancel_peer) = self.peers.get_mut(&cancel_addr) {
            let _ = cancel_peer.cmd_tx.try_send(PeerCommand::Cancel {
                index: ci,
                begin: cb,
                length: cl,
            });
            cancel_peer.pending_requests.remove(ci, cb);
        }
    }
    // Freed peers will be refilled by the next pipeline tick — no reactive cascade.
}
```

Note: the `pending_requests.remove(ci, cb)` call uses the new `PendingRequests` API from Task 3.

**Step 2: Verify the pipeline tick handles refilling**

Read the pipeline tick code (search for the interval-based refill logic). Confirm it iterates all peers and calls `request_end_game_block()` for those with available slots. This is the existing mechanism — we're just relying on it instead of eager refilling.

If the pipeline tick interval is too long (e.g., 1 second), consider reducing it to 100-250ms for end-game mode to maintain responsiveness. Check the current interval and adjust if needed.

**Step 3: Run tests**

```bash
cargo test -p torrent-session 2>&1 | tail -5
cargo test -p torrent-sim 2>&1 | tail -5
```

Expected: all pass. Swarm sim tests will validate end-game still completes.

**Step 4: Run clippy**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -5
```

**Step 5: Commit**

```bash
git add -A
git commit -m "perf: batch end-game re-requesting into pipeline tick, eliminating cascade (M61)"
```

---

## Task 6: Store Buffer — Cap In-Flight Pieces and Apply Backpressure

**Priority 3 from report. Higher risk, touches disk I/O path.**

**Files:**
- Modify: `crates/torrent-session/src/disk.rs` (StoreBuffer usage, line ~47-52)
- Modify: `crates/torrent-session/src/write_buffer.rs` (WriteBuffer, enforce max_bytes)
- Modify: `crates/torrent-session/src/torrent.rs` (backpressure when buffer is full)
- Test: existing tests + new unit test for buffer cap

**Context:**
- `StoreBuffer = Mutex<HashMap<(Id20, u32), BTreeMap<u32, Bytes>>>` holds all in-flight blocks in RAM
- `WriteBuffer` already has a `max_bytes` field but it's not enforced as backpressure
- With 100+ pieces in-flight x 32 blocks/piece x 16 KB/block = ~50 MiB buffered
- rqbit writes blocks directly to disk via `pwritev()` — no buffer
- Full direct-write refactor is a larger change; for M61, cap the buffer and apply backpressure

**Step 1: Write a test for WriteBuffer capacity enforcement**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::Id20;
    use bytes::Bytes;

    #[test]
    fn write_buffer_reports_full_when_over_capacity() {
        let mut buf = WriteBuffer::new(32 * 1024); // 32 KB cap
        let hash = Id20::default();

        // Write 2 x 16KB blocks — should fit exactly
        buf.write(hash, 0, 0, Bytes::from(vec![0u8; 16384]));
        assert!(!buf.is_full());
        buf.write(hash, 0, 16384, Bytes::from(vec![0u8; 16384]));
        assert!(buf.is_full());

        // Flush piece 0 — should free space
        let blocks = buf.take_piece(hash, 0);
        assert!(blocks.is_some());
        assert!(!buf.is_full());
    }
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test -p torrent-session write_buffer 2>&1 | tail -5
```

Expected: FAIL — `is_full()` and `take_piece()` methods don't exist.

**Step 3: Add is_full() and take_piece() to WriteBuffer**

```rust
impl WriteBuffer {
    /// Returns true when buffered bytes >= max_bytes.
    pub fn is_full(&self) -> bool {
        self.total_bytes >= self.max_bytes
    }

    /// Remove and return all blocks for a completed piece.
    pub fn take_piece(&mut self, info_hash: Id20, piece: u32) -> Option<BTreeMap<u32, Bytes>> {
        if let Some(blocks) = self.pending.remove(&(info_hash, piece)) {
            let freed: usize = blocks.values().map(|b| b.len()).sum();
            self.total_bytes = self.total_bytes.saturating_sub(freed);
            Some(blocks)
        } else {
            None
        }
    }

    /// Current buffered bytes.
    pub fn buffered_bytes(&self) -> usize {
        self.total_bytes
    }
}
```

**Step 4: Run unit test**

```bash
cargo test -p torrent-session write_buffer 2>&1 | tail -5
```

Expected: PASS.

**Step 5: Wire backpressure into DiskActor / TorrentActor**

This is the integration step. The approach depends on how `StoreBuffer` and `WriteBuffer` interact with the disk actor. Two strategies:

**Strategy A (simpler):** Set `max_bytes` to a reasonable cap (e.g., 16 MiB — enough for ~30 pieces) and have `write()` return a `bool` indicating whether the buffer accepted the write. When the buffer is full, the torrent actor skips requesting new pieces until space frees up.

**Strategy B (if WriteBuffer is already used by DiskActor):** Check if `WriteBuffer` is already wired into the disk path. If the `StoreBuffer` type in `disk.rs` is the active buffer, apply the cap there instead. The key is: find which buffer is actually live and cap it.

Read the disk actor code to determine the active buffer, then apply the cap with backpressure. The backpressure signal should be: when buffer is full, don't call `request_pieces_from_peer()` for any peer until a piece is verified and its blocks are freed.

**Step 6: Run full test suite**

```bash
cargo test --workspace 2>&1 | tail -5
cargo clippy --workspace -- -D warnings 2>&1 | tail -5
```

**Step 7: Commit**

```bash
git add -A
git commit -m "perf: cap store buffer at 16 MiB with backpressure (M61)"
```

---

## Task 7: Version Bump and Intermediate Validation

**Files:**
- Modify: `Cargo.toml` (root workspace version)
- Test: full workspace build + test

**Step 1: Bump version to 0.67.0**

In root `Cargo.toml`, change workspace version:
```toml
version = "0.67.0"
```

**Step 2: Run full validation**

```bash
cargo test --workspace 2>&1 | tail -5
cargo clippy --workspace -- -D warnings 2>&1 | tail -5
```

Expected: all tests pass, no warnings.

**Step 3: Commit**

```bash
git add -A
git commit -m "chore: bump version to 0.67.0 (M61)"
```

---

## Task 8: Standardized Benchmark Run and Report Generation

**Files:**
- Modify: `benchmarks/run_benchmark.sh` (add report generation step)
- Create: `benchmarks/generate_report.sh` (markdown report generator + pandoc PDF)
- Create: `benchmarks/report_template.md` (parameterized report template)
- Create: `docs/benchmark-report-v0.67.0.md` (generated output)
- Create: `docs/benchmark-report-v0.67.0.pdf` (generated PDF)
- Modify: `benchmarks/summarize.py` (add JSON output mode for report generation)

**Context:**
- The v0.66.0 report was generated manually — we want to automate this for every performance milestone
- The report structure is: Overview, Config, Results (summary + per-trial + relative), Observations, Bottlenecks, Architecture Comparison, Conclusion
- PDF is generated via pandoc with LaTeX (booktabs, longtable)

**Step 1: Add JSON output to summarize.py**

Add a `--json` flag to `summarize.py` that outputs structured data suitable for report generation:

```python
# Add to summarize.py argument parsing:
import argparse, json

parser = argparse.ArgumentParser()
parser.add_argument('csv_file')
parser.add_argument('--json', action='store_true', help='Output JSON for report generation')
parser.add_argument('--version', default='unknown', help='Version string for report')
args = parser.parse_args()

# After computing stats, if --json:
if args.json:
    output = {
        "version": args.version,
        "date": datetime.date.today().isoformat(),
        "summary": {client: {"time": mean_time, "time_std": std_time, ...} for client in clients},
        "trials": raw_trial_data,
    }
    print(json.dumps(output, indent=2))
    sys.exit(0)
```

**Step 2: Create generate_report.sh**

```bash
#!/bin/bash
set -euo pipefail

# Usage: ./benchmarks/generate_report.sh <version> [results.csv]
# Generates docs/benchmark-report-v<version>.md and .pdf

VERSION="${1:?Usage: $0 <version> [results.csv]}"
CSV="${2:-benchmarks/results.csv}"
REPORT="docs/benchmark-report-v${VERSION}.md"

# Get JSON stats
STATS=$(python benchmarks/summarize.py "$CSV" --json --version "$VERSION")

# Generate markdown report from template
python benchmarks/build_report.py "$STATS" "$VERSION" > "$REPORT"

# Generate PDF via pandoc
if command -v pandoc &>/dev/null; then
    pandoc "$REPORT" -o "${REPORT%.md}.pdf" \
        --pdf-engine=xelatex \
        -V geometry:margin=1in \
        -V fontsize=11pt
    echo "PDF: ${REPORT%.md}.pdf"
else
    echo "Warning: pandoc not installed, skipping PDF generation"
fi

echo "Report: $REPORT"
```

**Step 3: Create build_report.py**

A Python script that takes the JSON stats and version, and outputs the standardized markdown report following the exact structure of `docs/benchmark-report-v0.66.0.md`. Include:
- YAML front matter (title, date, geometry, fontsize, LaTeX packages)
- Overview section with version comparison
- Benchmark configuration table
- Summary table (mean +/- stddev)
- Per-trial data table
- Relative performance tables (torrent vs each competitor)
- Key observations (auto-generated from the data — e.g., detect speed drops, memory outliers)
- Placeholder sections for bottleneck analysis (to be filled manually after profiling)

This script should be ~150 lines and produce markdown identical in structure to the v0.66.0 report.

**Step 4: Run the standardized benchmark**

```bash
# Get current Arch ISO magnet (or use the same one from v0.66.0)
./benchmarks/run_benchmark.sh "magnet:?xt=urn:btih:..." 5
```

Wait for all 15 trials (5 per client) to complete. This takes ~15-30 minutes.

**Step 5: Generate the v0.67.0 report**

```bash
./benchmarks/generate_report.sh 0.67.0 benchmarks/results.csv
```

**Step 6: Review the report**

Open `docs/benchmark-report-v0.67.0.md` and verify:
- Summary table shows improved metrics vs v0.66.0 baseline
- Per-trial data is complete
- Relative performance ratios are calculated correctly
- Add manual observations about what changed (reference specific fixes from M61)

**Step 7: Add a "Changes Since v0.66.0" section**

Manually add a section to the report listing the 6 optimizations and their expected vs actual impact:

```markdown
# Changes Since v0.66.0

| Fix | Expected Impact | Actual Impact |
|:----|:----------------|:--------------|
| peer_count() HashSet | Reduce CPU in picker | ... |
| Bitfield pass-by-ref | Reduce alloc churn | ... |
| pending_requests FxHashMap | O(1) block lookup | ... |
| END_GAME_DEPTH 4→128 | Eliminate tail stall | ... |
| Batch re-requesting | Eliminate cascade | ... |
| Store buffer cap 16 MiB | Reduce RSS ~50 MiB | ... |
```

**Step 8: Commit**

```bash
git add -A
git commit -m "bench: add standardized benchmark report generation (M61)"
```

---

## Task 9: Documentation Updates

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `README.md`

**Step 1: Update CHANGELOG.md**

Add v0.67.0 entry:

```markdown
## v0.67.0 — M61: Performance Optimizations

### Changed
- **peer_count()**: Replaced O(n log n) sort-dedup with HashSet counter in piece selector
- **Bitfield passing**: Changed picker functions to accept bitfield by reference, eliminating clone churn
- **pending_requests**: Replaced Vec with FxHashMap for O(1) block lookup (was O(n) linear scan)
- **End-game pipeline**: Raised END_GAME_DEPTH from 4 to 128, eliminating tail stall
- **End-game re-requesting**: Batched freed peers into pipeline tick instead of reactive cascade
- **Store buffer**: Capped at 16 MiB with backpressure to reduce peak RSS

### Performance
- v0.66.0: 15.0 MB/s, 170 MiB RSS
- v0.67.0: [fill after benchmark]
```

**Step 2: Update README.md**

Update the performance section / badge with new benchmark results. Update test count if changed. Update version references.

**Step 3: Commit**

```bash
git add -A
git commit -m "docs: update README and CHANGELOG for M61 v0.67.0"
```

**Step 4: Push**

```bash
git push origin main && git push github main
```
