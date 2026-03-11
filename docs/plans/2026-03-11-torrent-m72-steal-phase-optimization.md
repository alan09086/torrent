# M72: Steal Phase Optimization — Zero-Alloc Block Stealing

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate 60% CPU overhead from the steal phase in `pick_partial` by replacing `missing_chunks` + `Vec::retain` with direct `assigned_blocks` iteration.

**Architecture:** The steal phase currently calls `missing_chunks_into()` (rebuilds full chunk list via `chunk_info()` × 32 blocks → `Vec::extend`) then `Vec::retain` (filters via `FxHashMap::get` per block). With 128 peers × 20 in-flight pieces × 32 blocks = 81,920 HashMap probes per batch fill cycle. Replace with: iterate `assigned_blocks` directly (only ~32 entries per piece), check peer rate, collect stealable blocks. O(assigned) instead of O(total_chunks).

**Tech Stack:** Rust, no new dependencies. Pure algorithmic improvement.

---

## Context & Profiling Data

v0.73.0 (M71) reduced CPU 23% (37.1s → 28.7s) by eliminating allocation churn from the normal pick path via ChunkMask. But profiling reveals the steal phase was untouched and now dominates:

| Function | % CPU | Location |
|----------|-------|----------|
| `Vec::retain::{{closure}}` | 16.77% | steal phase HashMap probe (SIMD vpcmpeqb) |
| `ring::sha1` | 15.75% | spawn_blocking (unavoidable) |
| `Vec::extend_desugared` | 13.09% | steal phase missing_chunks rebuild |
| `Lengths::chunk_info` | 11.64% | steal phase per-chunk arithmetic |
| `Vec::retain` | 9.09% | steal phase filter |
| `ChunkTracker::missing_chunks_into` | 8.03% | steal phase full chunk enumeration |
| `Rc4::apply` | 6.03% | peer threads (unavoidable) |
| `pick_blocks` | 3.92% | picker framework |

**Addressable CPU**: 58.62% (retain + extend + chunk_info + retain_closure + missing_chunks_into). Target: reduce to ~2%.

**Root cause**: The steal phase fires for every peer that finds no unassigned blocks in `pick_partial`. With 128 peers and ~640 block slots (20 pieces × 32 blocks), most peers exhaust unassigned blocks quickly. Each entering the steal phase then scans all 20 in-flight pieces with the expensive `missing_chunks` → `retain` pattern.

## Benchmark Baseline (v0.73.0)

| Client | Speed (MB/s) | CPU (s) | RSS (MiB) |
|--------|-------------|---------|-----------|
| torrent | 28.7 ± 1.3 | 28.7 ± 4.8 | 93.4 |
| rqbit | 75.0 ± 2.4 | 9.9 ± 0.5 | 41.4 |

## Target

| Metric | v0.73.0 | Target | Stretch |
|--------|---------|--------|---------|
| CPU time | 28.7s | <16s | <12s |
| Speed | 28.7 MB/s | 40+ MB/s | 60+ MB/s |
| RSS | 93.4 MiB | <95 MiB | <80 MiB |

## Files

| File | Tasks | Changes |
|------|-------|---------|
| `crates/torrent-session/src/piece_selector.rs` | 1, 2, 3 | Steal phase rewrite, early-out, tests |
| `Cargo.toml` | 4 | Version bump 0.73.0 → 0.74.0 |
| `README.md`, `CHANGELOG.md`, `CLAUDE.md` | 4 | Docs update |

---

## Task 1: Rewrite Steal Phase to Iterate assigned_blocks Directly

**Files:**
- Modify: `crates/torrent-session/src/piece_selector.rs:431-469`

**Design:** Instead of:
1. `missing_chunks(piece, scratch)` — rebuilds ALL chunks (allocated Vec)
2. `scratch.retain(|block| ifp.assigned_blocks.get(...).map(rate < threshold))` — filters

Replace with:
1. Iterate `ifp.assigned_blocks` directly (already has `(piece, begin) → peer` mapping)
2. Check if `peer != ctx.peer_addr` and `peer_rate < steal_threshold`
3. Compute block length from `begin` using simple arithmetic (no `chunk_info` needed)

This is O(assigned_blocks) per piece instead of O(total_chunks), and requires zero allocation for the scan phase. Only push to `scratch` for actual stealable blocks.

- [ ] **Step 1: Rewrite the steal phase**

Replace lines 437-469 in `pick_partial`:

```rust
for (&piece, ifp) in ctx.in_flight_pieces {
    if !ctx.peer_has.get(piece) || ctx.we_have.get(piece) || !ctx.wanted.get(piece) {
        continue;
    }

    scratch.clear();
    // Iterate assigned blocks directly — O(assigned) instead of O(total_chunks)
    for (&(p, begin), &assigned_peer) in &ifp.assigned_blocks {
        debug_assert_eq!(p, piece);
        if assigned_peer == ctx.peer_addr {
            continue; // don't steal from ourselves
        }
        let is_slow = ctx.peer_rates
            .get(&assigned_peer)
            .map(|&rate| rate < steal_threshold)
            .unwrap_or(false);
        if is_slow {
            // Compute block length: chunk_size unless it's the last chunk
            let length = ctx.chunk_size.min(ctx.piece_size.saturating_sub(begin));
            if length > 0 {
                scratch.push((begin, length));
            }
        }
    }
    if !scratch.is_empty() {
        return Some(PickResult {
            piece,
            blocks: std::mem::take(scratch),
            exclusive: false,
        });
    }
}
```

- [ ] **Step 2: Verify tests pass**

Run: `cargo test -p torrent-session`

- [ ] **Step 3: Commit**

```bash
git commit -m "perf: rewrite steal phase to iterate assigned_blocks directly (M72)"
```

---

## Task 2: Early-Out When No Slow Peers Exist

**Files:**
- Modify: `crates/torrent-session/src/piece_selector.rs`

**Design:** Before entering the steal phase O(pieces × blocks) loop, check if ANY peer in the download has a rate below the steal threshold. If not, skip the entire steal scan. This is O(peers) but uses the already-available `peer_rates` map.

- [ ] **Step 1: Add early-out check**

Before the steal phase loop, add:

```rust
// Early-out: check if any peer is slow enough to steal from
let any_slow_peer = ctx.peer_rates.values().any(|&rate| rate < steal_threshold);
if !any_slow_peer {
    // No slow peers — steal would never find anything
    return None; // fall through to pick_new_piece
}
```

Note: This must return `None` from `pick_partial` (not from `pick_blocks`), so the caller can try `pick_new_piece` next.

- [ ] **Step 2: Verify tests pass**

Run: `cargo test -p torrent-session`

- [ ] **Step 3: Commit**

```bash
git commit -m "perf: early-out steal phase when no slow peers exist (M72)"
```

---

## Task 3: Add Steal Phase Tests

**Files:**
- Modify: `crates/torrent-session/src/piece_selector.rs` (test module)

- [ ] **Step 1: Write tests for the rewritten steal phase**

Add tests verifying:
- Steal returns blocks from slow peers only
- Steal skips blocks assigned to self
- Steal skips blocks assigned to fast peers
- Early-out skips steal when no slow peers exist
- Steal with empty in-flight returns None

- [ ] **Step 2: Verify all tests pass**

Run: `cargo test --workspace`

- [ ] **Step 3: Commit**

```bash
git commit -m "test: add steal phase coverage (M72)"
```

---

## Task 4: Benchmark, Version Bump & Docs

- [ ] **Step 1: Benchmark** — 3-trial comparison
- [ ] **Step 2: Version bump** — 0.73.0 → 0.74.0
- [ ] **Step 3: Update README.md, CHANGELOG.md, CLAUDE.md**
- [ ] **Step 4: Commit docs**

---

## Commit Plan

1. `perf: rewrite steal phase to iterate assigned_blocks directly (M72)`
2. `perf: early-out steal phase when no slow peers exist (M72)`
3. `test: add steal phase coverage (M72)`
4. `chore: bump version to 0.74.0 (M72)`
5. `docs: update README and CHANGELOG for M72`

## Verification

1. `cargo test --workspace` — all tests pass after each commit
2. `cargo clippy --workspace -- -D warnings` — clean
3. 3-trial benchmark — CPU time <16s (target), speed ≥40 MB/s
4. Verify piece hash failures = 0, download completion = 100%

## What NOT to Do

- **Do NOT remove the steal phase entirely** — it's needed for endgame and slow-peer recovery
- **Do NOT change steal_threshold_ratio** — keep existing tuning
- **Do NOT call missing_chunks in the steal phase** — the whole point is to avoid that allocation
- **Do NOT modify InFlightPiece struct** — no new fields needed, just better iteration
