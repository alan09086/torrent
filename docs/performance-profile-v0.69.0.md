# Performance Profile — v0.69.0 (M66)

Date: 2026-03-10
Workload: archlinux-2026.03.01-x86_64.iso (1.4 GiB, 2867 pieces × 512 KiB)
Host: CachyOS (Arch Linux), Zen 5 CPU (SHA-NI capable), NVMe storage

## Benchmark Results (v0.69.0)

| Metric | v0.69.0 (avg of 3) | v0.68.0 (M65) | rqbit | Notes |
|--------|--------------------:|---------------:|------:|-------|
| Speed | swarm-dependent | 32.8 MB/s | 59.0 MB/s | Varies with seeder availability |
| User CPU | 18.1s | 22.0s | 10.6s | **18% reduction from M65** |
| System CPU | 6.96s | — | — | I/O + syscall overhead |
| Peak RSS | 79 MiB | 66 MiB | 42 MiB | +13 MiB vs M65 (see Memory) |
| Peak peers | 200 | 200 | 128 | We connect more peers |

**CPU gap vs rqbit: 1.71x** (was 2.08x at M65). Closing but still significant.

## CPU Profile (perf + flamegraph)

20,986 samples captured. Top functions by CPU time:

| Rank | Function | CPU % | Category | Notes |
|------|----------|------:|----------|-------|
| 1 | `sha1_compress` | **44.5%** | Piece verification | SHA1 hashing of all 1.4 GiB of payload |
| 2 | `Rc4::apply` (MSE cipher) | **11.8%** | Encryption | RC4 PRGA on every encrypted byte |
| 3 | libc (memcpy/memmove) | 2.2% | System | Data copying overhead |
| 4 | tokio scheduler (`Context::run`) | 2.1% | Runtime | Multi-thread scheduler overhead |
| 5 | `Bitfield::get` | 1.3% | Piece selection | Availability checks during picking |
| 6 | `num_bigint::montgomery` | 1.2% | Handshake | DH key exchange for MSE |
| 7 | tokio work-stealing (`Steal::steal_into`) | 1.1% | Runtime | Cross-thread task migration |
| 8 | `parking_lot::Condvar::wait_until_internal` | 1.0% | Sync | Lock contention (disk I/O?) |
| 9 | `futex::Mutex::lock_contended` | 0.8% | Sync | Mutex contention |
| 10 | `__vdso_clock_gettime` | 0.7% | Time | Instant::now() calls |

**Key insight: SHA1 + RC4 = 56.3% of all CPU time.** These two functions alone consume
more CPU than rqbit uses in total (10.6s). Everything else combined is ~8s.

### SHA1 Analysis (44.5%)

The `sha1_compress` function is the SHA1 block compression. Despite enabling the `asm`
feature on the `sha1` crate in M65, the profile shows a software implementation path.
**Possible explanations:**

1. **SHA-NI not dispatched at runtime** — The `sha1` crate's `asm` feature enables
   compile-time CPU detection, but the actual SHA-NI path may require specific target
   features (`+sha,+sse2,+ssse3,+sse4.1,+sse4.2`) to be set at compile time.
2. **Profile is accurate, SHA1 is just expensive** — Even with hardware acceleration,
   hashing 1.4 GiB of data requires ~2867 piece verifications × 512 KiB each. At
   hardware SHA1 speeds (~3 GB/s), this would be ~0.47s. At software speeds (~500 MB/s),
   it would be ~2.8s. Given 44.5% of 18.1s ≈ 8.05s, this suggests **software SHA1**.
3. **Incremental hashing overhead** — If `sha1_chunks()` creates/destroys hasher state
   per chunk rather than streaming, there's per-piece overhead.

**Verdict: SHA1 is almost certainly NOT using SHA-NI hardware acceleration.** 8.05s for
1.4 GiB = 174 MB/s throughput, consistent with pure Rust software SHA1.

### RC4 Analysis (11.8%)

The `torrent_wire::mse::cipher::Rc4::apply` function performs RC4 encryption/decryption
on every byte of payload for MSE/PE encrypted connections. 11.8% of 18.1s ≈ 2.14s.

1.4 GiB at 2.14s = 653 MB/s — reasonable for a pure Rust byte-by-byte RC4 PRGA.

**Optimization opportunities:**
- RC4 is only needed for the first 1024 bytes of payload per connection (MSE/PE spec).
  After that, the "plaintext" crypto method should disable it. If we're encrypting ALL
  traffic, that's a protocol implementation issue.
- If full-stream encryption is required (negotiated), batch processing with wider
  registers could help (process 8/16/32 bytes at a time via SIMD).

## Memory Profile (heaptrack)

| Metric | Value |
|--------|------:|
| Peak heap consumption | 32.78 MiB |
| Peak RSS (including overhead) | 102 MiB (heaptrack inflated) |
| Benchmark RSS (no overhead) | 79 MiB |
| Total allocations | 3,781,138 |
| Allocation rate | 69,682/s |
| Temporary allocations | 2,040,157 (54%) |
| Memory leaked | 148 KiB |

### Peak Memory Breakdown

| Consumer | Peak | Calls | Description |
|----------|-----:|------:|-------------|
| DiskActor::dispatch_job | 16.86 MiB | 92,902 | Store buffer + piece verify buffers |
| Vec realloc (amortized growth) | 8.00 MiB | 140,296 | Vec resizing in hot paths |
| Aligned malloc (tokio internals) | 2.56 MiB | 195,509 | Runtime task allocations |
| Other | ~5.36 MiB | — | DH, channel buffers, peer state |

### Top Allocation Call Sites (by count)

| Function | Calls | Peak Memory | Type |
|----------|------:|-------------|------|
| PieceSelector::pick_partial | 436,301 | 0 B | **Temporary** — allocates and drops immediately |
| PieceSelector::pick_blocks (via pick_partial) | 429,525 | 0 B | **Temporary** |
| TorrentActor::handle_piece_data | 187,327 | 0 B | **Temporary** |
| TorrentActor::request_pieces_from_peer | ~107k | 0 B | **Temporary** |
| DiskActor::dispatch_job | 92,902 | 16.86 MiB | **Retained** — store buffer |
| MessageCodec::decode | ~varies | ~varies | Per-message buffer |

**Key insight: 54% of all allocations are temporary (allocate-then-immediately-free).**
The piece selector alone accounts for ~865K temporary allocations (23% of total). These
produce zero peak memory but cause allocator churn and potential cache pollution.

### RSS Gap vs rqbit (79 MiB vs 42 MiB = 37 MiB gap)

| Component | Our estimate | rqbit likely |
|-----------|------------:|-----------:|
| Store buffer (write cache) | ~16 MiB | ~0 (direct I/O?) |
| ARC read cache | ~8 MiB | ~0 (no cache?) |
| Vec realloc overhead | ~8 MiB | ~2 MiB |
| Tokio runtime | ~3 MiB | ~3 MiB |
| Per-peer state (200 peers) | ~8 MiB | ~5 MiB (128 peers) |
| Code + stack | ~15 MiB | ~15 MiB |
| Fragmentation | ~21 MiB | ~17 MiB |
| **Total** | **~79 MiB** | **~42 MiB** |

The store buffer (16 MiB) and ARC cache (8 MiB) account for most of the gap. rqbit
uses direct writes without a write-combining buffer and has no read-ahead cache.

## Network Analysis

From benchmark output and runtime statistics:

| Metric | Value | Notes |
|--------|------:|-------|
| Peak connected peers | 200 | Max configured |
| Peer limit | 200 | rqbit uses 128 |
| Pieces per second | ~47-96 | Depends on swarm speed |
| Blocks per second | ~6,000 | ~91k total blocks |

**Observation:** We connect 200 peers vs rqbit's 128, yet rqbit achieves higher speeds.
This suggests diminishing returns from additional peers — the overhead of maintaining 200
connections (channel buffers, state, select! branches) may outweigh the bandwidth gained
from peers 129-200.

## Optimization Recommendations (Ranked by Expected Impact)

### Tier 1: High Impact (estimated 2-3x CPU reduction)

#### 1. Fix SHA-NI Hardware Acceleration — **Critical**
**Expected impact:** 44.5% → ~5% CPU (8.05s → ~0.5s)
**Effort:** Small

The SHA1 hashing is using software fallback despite `asm` being enabled. Two approaches:
- **Option A:** Compile with `RUSTFLAGS="-C target-cpu=native"` to enable SHA-NI codegen
- **Option B:** Switch to `ring` crate which has hand-optimized asm for SHA1/SHA256
- **Verification:** After fix, `sha1_compress` should drop from 44.5% to <5% in flamegraph

This single fix would cut total CPU from ~18s to ~10s — immediately matching rqbit.

#### 2. RC4 Cipher Optimization — **Important**
**Expected impact:** 11.8% → ~1% CPU (2.14s → ~0.2s)
**Effort:** Medium

Two sub-approaches:
- **2a. Protocol fix:** Verify we're not encrypting full payload when plaintext was
  negotiated. MSE/PE allows "plaintext" after handshake — if we're RC4-encrypting all
  data when we don't need to, that's a bug worth ~2s of CPU.
- **2b. SIMD RC4:** If full-stream encryption IS required, process multiple bytes at
  once using SIMD intrinsics (4-8x speedup on the PRGA loop).

### Tier 2: Medium Impact (estimated 10-30% further reduction)

#### 3. Reduce Temporary Allocations in Piece Selector
**Expected impact:** Reduce 865K temp allocs → near zero
**Effort:** Medium

`pick_partial` and `pick_blocks` create 865K temporary allocations per download.
Pre-allocate scratch buffers on the `PieceSelector` and reuse them across calls.
This reduces allocator pressure and improves cache behaviour.

#### 4. Reduce Peer Count to 128
**Expected impact:** ~12% less per-peer overhead, lower RSS
**Effort:** Trivial (configuration change)

rqbit uses 128 peers and achieves 1.8x our speed. Reducing from 200 to 128 saves:
- ~3 MiB per-peer state
- ~72 fewer select! branches per event loop iteration
- ~72 fewer channel buffers

#### 5. Lock Contention Reduction (1.8% combined)
**Expected impact:** ~1.8% CPU reduction
**Effort:** Medium-Hard

`parking_lot::Condvar` (1.0%) + `futex::Mutex` (0.8%) = 1.8% in lock contention.
Likely the store buffer Mutex and/or ARC cache RwLock. Profile lock holders to confirm,
then consider lock-free alternatives or finer-grained locking.

### Tier 3: Low Impact (memory optimization)

#### 6. Reduce Store Buffer Size
**Expected impact:** -8 MiB RSS
**Effort:** Trivial (configuration change)

The store buffer accounts for ~16 MiB. Reducing `max_in_flight_pieces` from 32 to 16
would halve this to ~8 MiB with minimal speed impact (verify with benchmark).

#### 7. Disable or Shrink ARC Cache
**Expected impact:** -8 MiB RSS
**Effort:** Small

The ARC read cache is mainly useful for streaming. For download-to-completion workloads,
it's wasted memory. Make it optional or reduce default size.

## Profiling Artifacts

- `benchmarks/flamegraph-v0.69.0.svg` — Interactive CPU flamegraph
- `benchmarks/heaptrack-v0.69.0.txt` — Full heap allocation analysis
- `benchmarks/heaptrack.torrent.728818.zst` — Raw heaptrack data

## Summary

| Finding | Current | Target | Fix |
|---------|---------|--------|-----|
| SHA1 not hardware-accelerated | 44.5% CPU | <5% CPU | target-cpu=native or ring crate |
| RC4 encrypting all payload | 11.8% CPU | <2% CPU | Plaintext after handshake / SIMD |
| Temporary allocations | 2M/download | <100K | Pre-allocated scratch buffers |
| Peer count overhead | 200 peers | 128 peers | Config change |
| Store buffer size | 16 MiB | 8 MiB | Reduce in-flight cap |
| ARC cache for non-streaming | 8 MiB | 0 MiB | Optional/disabled |

**Projected v0.70.0 after Tier 1 fixes:** ~10s user CPU (matching rqbit), ~65 MiB RSS.
**Projected after all tiers:** ~8s user CPU (beating rqbit), ~50 MiB RSS.
