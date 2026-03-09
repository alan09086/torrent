# Performance Parity v2 — Profile-Guided Optimization Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the 3x throughput gap with rqbit (33 MB/s → 77 MB/s) by optimizing the CPU hotspots identified by gperftools profiling.

**Architecture:** CPU profiling revealed that the original plan's memcpy elimination (Tasks 1-3, already done) only addressed ~0.3% of CPU. The real bottlenecks are: RC4 cipher (12.75%), HashMap/SipHash (27%), Vec allocations (17%), and piece arithmetic cascading recomputation (10%). All optimizations are internal implementation improvements — no protocol changes, MSE/PE encryption remains fully compliant.

**Tech Stack:** Rust, `rustc-hash` (FxHashMap), existing `bytes`/`tokio` stack

---

## Evidence

### Benchmark Data (post zero-copy patches)

| Client | Wall Time | User CPU | Avg MB/s |
|--------|-----------|----------|----------|
| rqbit 8.1.1 | 18.7s | 5.3s | 77.5 |
| torrent (encrypted) | 67.9s | 53s | 33.9 |
| torrent (no encrypt) | 59.4s | 33s | 26.9 |

### gperftools Profile (49.73s total, encrypted download)

| Function | Self | Cumulative | % |
|----------|------|------------|---|
| `Rc4::apply` | 3.58s | 6.34s | 12.75% |
| `Hasher<S>::write` (SipHash) | 1.87s | 3.85s | 7.74% |
| `BuildHasher::hash_one` | 0.51s | 4.77s | 9.59% |
| `RawTableInner::find_inner` | 0.40s | 5.89s | 11.84% |
| `Lengths::chunk_info` | 0.54s | 2.19s | 4.40% |
| `Lengths::piece_size` | 0.30s | 1.58s | 3.18% |
| `Vec::extend_desugared` | 0.52s | 3.78s | 7.60% |
| `SpecFromIterNested::from_iter` | 0.18s | 4.70s | 9.45% |

**Summary:** HashMap operations (~27%), RC4 (~13%), allocations (~17%), piece arithmetic (~8%) = ~65% of CPU time in 4 fixable areas.

---

## Task 1: Optimize RC4 PRGA with Loop Unrolling

**Impact:** 12.75% CPU → ~4% (estimated 3x speedup on cipher hot loop)

**Files:**
- Modify: `crates/torrent-wire/src/mse/cipher.rs:39-47` (the `apply` method)
- Test: existing tests in same file (roundtrip, consistency, different-keys)

**Context:** The RC4 cipher is required by MSE/PE (BEP 0-ish, de facto standard). We cannot disable it. But the current implementation processes one byte at a time with 4 dependent memory accesses per byte. We can unroll to process 8 bytes per iteration, and for long runs (>64 bytes), pre-generate keystream into a small buffer then XOR in bulk. This is still standard RC4 — just a faster implementation of the same algorithm.

**Step 1: Rewrite `apply` with bulk keystream generation**

Replace the byte-at-a-time loop with a two-phase approach:
1. For buffers ≥ 64 bytes: generate 64-byte keystream blocks, then XOR in bulk
2. For tail/small buffers: process byte-by-byte (same as current)

```rust
pub fn apply(&mut self, data: &mut [u8]) {
    let mut pos = 0;
    let len = data.len();

    // Bulk phase: generate keystream in 64-byte blocks, XOR in bulk
    while pos + 64 <= len {
        let mut keystream = [0u8; 64];
        for k in &mut keystream {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            *k = self.s[self.s[self.i as usize]
                .wrapping_add(self.s[self.j as usize]) as usize];
        }
        // XOR the 64-byte block — compiler can auto-vectorize this
        for (d, k) in data[pos..pos + 64].iter_mut().zip(keystream.iter()) {
            *d ^= k;
        }
        pos += 64;
    }

    // Tail: byte-at-a-time for remainder
    for byte in data[pos..].iter_mut() {
        self.i = self.i.wrapping_add(1);
        self.j = self.j.wrapping_add(self.s[self.i as usize]);
        self.s.swap(self.i as usize, self.j as usize);
        let k = self.s[self.s[self.i as usize]
            .wrapping_add(self.s[self.j as usize]) as usize];
        *byte ^= k;
    }
}
```

The key optimization here is separating keystream generation from XOR application. When they're interleaved (current code), the compiler can't vectorize the XOR because of the data dependency on `self.s`. By generating the keystream into a local array first, the final XOR loop is a simple `data[i] ^= key[i]` that the compiler can auto-vectorize with SIMD.

**Step 2: Run existing tests to verify correctness**

```bash
cargo test -p torrent-wire -- mse::cipher 2>&1 | grep "test result"
```

Expected: All 3 tests pass (roundtrip, consistency, different-keys). These tests verify that encrypt(decrypt(data)) == data, so any PRGA bug would fail them.

**Step 3: Add a benchmark-style test for large data**

Add to the test module in `cipher.rs`:

```rust
#[test]
fn rc4_large_data_roundtrip() {
    let key = b"benchmark key for large data";
    let original: Vec<u8> = (0..65536).map(|i| (i % 256) as u8).collect();
    let mut encrypted = original.clone();
    Rc4::new(key).apply(&mut encrypted);
    assert_ne!(encrypted, original);
    let mut decrypted = encrypted;
    Rc4::new(key).apply(&mut decrypted);
    assert_eq!(decrypted, original);
}
```

**Step 4: Commit**

```bash
git add crates/torrent-wire/src/mse/cipher.rs
git commit -m "perf: bulk keystream RC4 enables auto-vectorized XOR

Separate keystream generation from XOR application in 64-byte blocks.
The XOR loop over local arrays lets the compiler auto-vectorize with SIMD.
Still standard RC4 PRGA — same algorithm, faster implementation.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Task 2: Cache Piece Arithmetic in Lengths

**Impact:** ~8% CPU → <1% (eliminate ~96,000 div_ceil calls per 1.5GB download)

**Files:**
- Modify: `crates/torrent-core/src/lengths.rs` (struct fields + constructor + methods)
- Test: existing 13 tests in same file

**Context:** `Lengths::chunk_info(piece, chunk)` is called once per 16KB block received. It calls `piece_size(piece)` which calls `num_pieces()` which does `div_ceil`. For 1.5GB at 16KB blocks = ~96,000 calls. The division result never changes — it's determined at construction time. Pre-compute `num_pieces` and `last_piece_size` once.

**Step 1: Add cached fields to the struct**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lengths {
    total_length: u64,
    piece_length: u64,
    chunk_size: u32,
    /// Pre-computed number of pieces (cached div_ceil result).
    num_pieces: u32,
    /// Pre-computed size of the last piece.
    last_piece_size: u32,
}
```

**Step 2: Compute cached values in constructor**

```rust
pub fn new(total_length: u64, piece_length: u64, chunk_size: u32) -> Self {
    assert!(piece_length > 0, "piece_length must be > 0");
    assert!(chunk_size > 0, "chunk_size must be > 0");

    let num_pieces = if total_length == 0 {
        0
    } else {
        total_length.div_ceil(piece_length) as u32
    };

    let last_piece_size = if num_pieces == 0 {
        0
    } else {
        let remainder = total_length % piece_length;
        if remainder == 0 {
            piece_length as u32
        } else {
            remainder as u32
        }
    };

    Lengths {
        total_length,
        piece_length,
        chunk_size,
        num_pieces,
        last_piece_size,
    }
}
```

**Step 3: Make hot methods use cached values**

```rust
pub fn num_pieces(&self) -> u32 {
    self.num_pieces
}

pub fn piece_size(&self, piece_index: u32) -> u32 {
    if piece_index >= self.num_pieces {
        0
    } else if piece_index == self.num_pieces - 1 {
        self.last_piece_size
    } else {
        self.piece_length as u32
    }
}
```

`chunk_info` now calls `piece_size` which is just two comparisons and a field read — no division.

**Step 4: Run tests**

```bash
cargo test -p torrent-core 2>&1 | grep "test result"
```

Expected: All 13 tests pass unchanged (the behavior is identical, just faster).

**Step 5: Commit**

```bash
git add crates/torrent-core/src/lengths.rs
git commit -m "perf: cache num_pieces and last_piece_size in Lengths constructor

Eliminates div_ceil recomputation on every chunk_info/piece_size call.
For a 1.5GB download at 16KB blocks, this removes ~96,000 divisions.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Task 3: Replace SipHash with FxHash for Internal HashMaps

**Impact:** ~27% CPU → ~10% (FxHash is ~3x faster than SipHash for integer keys)

**Files:**
- Modify: `crates/torrent-session/Cargo.toml` (add `rustc-hash` dependency)
- Modify: `crates/torrent-session/src/piece_selector.rs` (InFlightPiece, PickContext)
- Modify: `crates/torrent-storage/Cargo.toml` (add `rustc-hash` dependency)
- Modify: `crates/torrent-storage/src/chunk_tracker.rs` (in_progress HashMap)
- Test: existing tests in both files

**Context:** The internal HashMaps use `HashMap<u32, _>` and `HashMap<(u32, u32), _>` for piece indices and block offsets. These are small, bounded integer keys with no untrusted input (piece indices come from our own protocol parsing after validation). SipHash's DoS protection is unnecessary here — FxHash (multiply-XOR) is ~3x faster for integer keys.

`rustc-hash` is the standard choice: it's the hasher used by the Rust compiler itself, zero dependencies, and provides `FxHashMap`/`FxHashSet` as drop-in replacements.

**Step 1: Add `rustc-hash` dependency**

In `crates/torrent-session/Cargo.toml` under `[dependencies]`:
```toml
rustc-hash = "2"
```

In `crates/torrent-storage/Cargo.toml` under `[dependencies]`:
```toml
rustc-hash = "2"
```

**Step 2: Replace HashMap in ChunkTracker**

In `crates/torrent-storage/src/chunk_tracker.rs`:

```rust
use rustc_hash::FxHashMap;

pub struct ChunkTracker {
    have: Bitfield,
    in_progress: FxHashMap<u32, Bitfield>,
    lengths: Lengths,
    block_verified: Option<FxHashMap<u32, Bitfield>>,
}
```

Update `new()`, `from_bitfield()`, `enable_v2_tracking()`, and `clear()` to use `FxHashMap::default()` instead of `HashMap::new()`.

**Step 3: Replace HashMap in InFlightPiece**

In `crates/torrent-session/src/piece_selector.rs`:

```rust
use rustc_hash::FxHashMap;

pub(crate) struct InFlightPiece {
    pub assigned_blocks: FxHashMap<(u32, u32), SocketAddr>,
    pub total_blocks: u32,
}

impl InFlightPiece {
    pub fn new(total_blocks: u32) -> Self {
        Self {
            assigned_blocks: FxHashMap::default(),
            total_blocks,
        }
    }
}
```

**Step 4: Replace HashMap in PickContext**

In the same file, update `PickContext`:

```rust
pub(crate) struct PickContext<'a> {
    // ...
    pub in_flight_pieces: &'a FxHashMap<u32, InFlightPiece>,
    pub peer_rates: &'a FxHashMap<SocketAddr, f64>,
    // ...
}
```

**Step 5: Update all call sites in torrent.rs**

Search for `HashMap<u32, InFlightPiece>` and `HashMap<SocketAddr, f64>` in `torrent.rs` and change to `FxHashMap`. The field declarations in `TorrentActor` need updating:

```bash
grep -n "HashMap.*InFlightPiece\|HashMap.*SocketAddr.*f64\|HashMap.*u32.*InFlightPiece" \
    crates/torrent-session/src/torrent.rs
```

Replace each with the `FxHashMap` equivalent. Add `use rustc_hash::FxHashMap;` to the imports.

**Step 6: Run tests**

```bash
cargo test -p torrent-storage -p torrent-session 2>&1 | grep "test result"
```

Expected: All tests pass — FxHashMap is a drop-in replacement with identical API.

**Step 7: Commit**

```bash
git add crates/torrent-session/Cargo.toml crates/torrent-session/src/piece_selector.rs \
    crates/torrent-session/src/torrent.rs crates/torrent-storage/Cargo.toml \
    crates/torrent-storage/src/chunk_tracker.rs
git commit -m "perf: replace SipHash with FxHash for internal piece/block HashMaps

Internal HashMaps keyed by piece index (u32) and block offset ((u32,u32))
don't need DoS-resistant hashing — values come from validated protocol
messages. FxHash (multiply-XOR) is ~3x faster for integer keys.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Task 4: Eliminate Hot-Path Vec Allocations

**Impact:** ~17% CPU → ~5% (eliminate collect/from_iter in piece picker and chunk tracker)

**Files:**
- Modify: `crates/torrent-storage/src/chunk_tracker.rs` (missing_chunks method)
- Modify: `crates/torrent-session/src/piece_selector.rs` (pick_partial, unassigned_blocks, peer_set)
- Test: existing tests in both files

**Context:** The profiler shows `Vec::extend_desugared` (7.60%) and `SpecFromIterNested::from_iter` (9.45%) as major hotspots. These come from:
1. `ChunkTracker::missing_chunks()` — returns `Vec<(u32, u32)>` via `.collect()` on every call
2. `InFlightPiece::peer_set()` — collects into `HashSet<SocketAddr>` on every call
3. `PieceSelector::pick_partial()` — builds `best_blocks: Vec<(u32, u32)>` each iteration

### Sub-task 4a: Add `missing_chunks_into` to ChunkTracker

Add a method that appends to an existing Vec (caller reuses buffer):

```rust
/// Append missing chunk (offset, length) pairs to `out`.
/// Caller can reuse the Vec across calls to avoid allocation.
pub fn missing_chunks_into(&self, piece: u32, out: &mut Vec<(u32, u32)>) {
    out.clear();
    if self.have.get(piece) {
        return;
    }

    let num_chunks = self.lengths.chunks_in_piece(piece);

    match self.in_progress.get(&piece) {
        Some(bf) => {
            out.extend(
                bf.zeros()
                    .filter_map(|ci| self.lengths.chunk_info(piece, ci)),
            );
        }
        None => {
            out.extend(
                (0..num_chunks)
                    .filter_map(|ci| self.lengths.chunk_info(piece, ci)),
            );
        }
    }
}
```

Keep the existing `missing_chunks()` for compatibility (it can delegate to `missing_chunks_into`).

### Sub-task 4b: Replace `peer_set()` with `peer_count()` and direct iteration

In `InFlightPiece`, the `peer_set()` method collects all assigned peers into a `HashSet` just to get the count or check membership. Replace with:

```rust
pub fn peer_count(&self) -> usize {
    // Count unique peers without allocating a HashSet.
    // For typical piece sizes (16-128 blocks), a small sorted vec is faster.
    let mut peers: Vec<SocketAddr> = self.assigned_blocks.values().copied().collect();
    peers.sort_unstable();
    peers.dedup();
    peers.len()
}
```

Then update `pick_partial` to use `peer_count()` instead of `peer_set().len()`.

### Sub-task 4c: Update `pick_blocks` closure to reuse Vec

In `torrent.rs`, the `missing_chunks_fn` closure passed to `pick_blocks` currently returns a new `Vec` each call. Use a `RefCell<Vec<_>>` to reuse the buffer:

```rust
let missing_buf = std::cell::RefCell::new(Vec::with_capacity(64));
let missing_chunks_fn = |piece: u32| -> Vec<(u32, u32)> {
    let mut buf = missing_buf.borrow_mut();
    self.chunk_tracker
        .as_ref()
        .map(|ct| {
            ct.missing_chunks_into(piece, &mut buf);
            buf.clone()
        })
        .unwrap_or_default()
};
```

Note: The closure signature returns `Vec<(u32, u32)>` which is consumed by the caller. A more invasive change would be to change the signature to accept `&mut Vec`, but that would ripple through the entire picker API. The RefCell approach at least reuses the allocation for capacity.

**Step 1: Implement sub-tasks 4a, 4b, 4c**

**Step 2: Run tests**

```bash
cargo test -p torrent-storage -p torrent-session 2>&1 | grep "test result"
```

**Step 3: Commit**

```bash
git add crates/torrent-storage/src/chunk_tracker.rs \
    crates/torrent-session/src/piece_selector.rs \
    crates/torrent-session/src/torrent.rs
git commit -m "perf: reduce hot-path allocations in chunk tracker and piece picker

Add missing_chunks_into() for buffer reuse, replace peer_set() HashSet
allocation with peer_count() using sorted dedup, reuse Vec capacity
in pick_blocks closure via RefCell.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Task 5: Benchmark and Verify Parity

**Files:** None (benchmarking only)

**Step 1: Rebuild**

```bash
cargo build --release -p torrent-cli
```

**Step 2: Kill any lingering torrent processes**

```bash
pkill -f "target/release/torrent" || true
pkill -f rqbit || true
sleep 2
```

**Step 3: Benchmark torrent**

```bash
rm -rf /mnt/TempNVME/bench/torrent_test && mkdir -p /mnt/TempNVME/bench/torrent_test
sync && echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
time target/release/torrent download \
    "magnet:?xt=urn:btih:ab6ad7e3c4c54cf93305fede316e0221840585a1&dn=archlinux-2025.03.01-x86_64.iso" \
    -o /mnt/TempNVME/bench/torrent_test -q -p 6881 \
    --config /mnt/TempNVME/bench/highperf.json
```

**Step 4: Benchmark rqbit**

```bash
rm -rf /mnt/TempNVME/bench/rqbit_test && mkdir -p /mnt/TempNVME/bench/rqbit_test
sync && echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
time rqbit -v warn download -o /mnt/TempNVME/bench/rqbit_test -e \
    "magnet:?xt=urn:btih:ab6ad7e3c4c54cf93305fede316e0221840585a1&dn=archlinux-2025.03.01-x86_64.iso"
```

**Step 5: Compare results**

**Target:** Wall time within 1.5x of rqbit. User CPU within 2x.

If not met, profile again with gperftools to identify the next bottleneck tier.

**Step 6: Full workspace verification**

```bash
cargo test --workspace 2>&1 | grep "test result"
cargo clippy --workspace -- -D warnings
```

---

## Summary

| Task | Target | CPU Reduction | Risk |
|------|--------|---------------|------|
| T1: RC4 bulk keystream | 12.75% → ~4% | ~8.75% | Low — same algorithm, tests verify correctness |
| T2: Cache piece arithmetic | ~8% → <1% | ~7% | Low — pure computation caching |
| T3: FxHash for internals | ~27% → ~10% | ~17% | Low — drop-in replacement, no external input |
| T4: Reduce allocations | ~17% → ~5% | ~12% | Medium — API additions, closure plumbing |
| T5: Benchmark | Validation | — | None |

**Expected total CPU reduction:** ~45% of the 53s user CPU → ~29s. Combined with the already-committed zero-copy patches, this should bring us to ~25-30s user CPU, within 2x of rqbit's 5.3s. The remaining gap would be architectural (actor message passing, channel overhead) which is a larger refactor for a future milestone.

**Protocol compliance:** All optimizations are internal. MSE/PE RC4 encryption is still applied to every byte. Piece hashing, chunk tracking, and piece selection logic are unchanged — only their data structure implementations are faster.
