# M68: Memory & Allocation Optimization — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate 865K+ temporary allocations per download in the piece selector hot path, reduce RSS by ~20 MiB through configuration tuning and cache optimization.

**Architecture:** Replace `.collect()` patterns with reusable scratch buffers, eliminate `buf.clone()` in the missing chunks closure, replace temporary HashSet with direct iteration in peer counting. Configuration changes for peer count and memory budgets.

**Tech Stack:** Rust, `FxHashMap`, `RefCell`-based buffer reuse

**Profiling baseline (v0.69.0):**
- 3.78M total allocations (69,682/s), 2.04M temporary (54%)
- `PieceSelector::pick_partial`: 436,301 temporary allocations
- `PieceSelector::pick_blocks` (via `pick_partial`): 429,525 temporary allocations
- Peak heap: 32.78 MiB, RSS: 79 MiB (vs rqbit's 42 MiB)
- Store buffer: 16.86 MiB, Vec realloc: 8.00 MiB

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/torrent-session/src/piece_selector.rs` | Eliminate temp allocs in picker |
| Modify | `crates/torrent-session/src/torrent.rs` | Fix `missing_chunks_fn` clone |
| Modify | `crates/torrent-session/src/settings.rs` | Memory-related config defaults |

---

## Task 1: Eliminate `buf.clone()` in `missing_chunks_fn` Closure

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs` (~line 5248-5256)

### Problem

The `missing_chunks_fn` closure (called by `pick_blocks()`) uses a `RefCell<Vec<(u32, u32)>>`
buffer that it fills via `ct.missing_chunks_into(piece, &mut buf)`, then **clones** the
buffer to return it. This clone happens on every call — up to 5 times per `pick_blocks()`
invocation across all picker layers (streaming, time-critical, suggested, partial, steal).

```rust
// Current (allocates on every call):
let missing_chunks_fn = |piece: u32| -> Vec<(u32, u32)> {
    self.chunk_tracker.as_ref().map(|ct| {
        let mut buf = missing_buf.borrow_mut();
        ct.missing_chunks_into(piece, &mut buf);
        buf.clone()  // ← CLONE ALLOCATION
    }).unwrap_or_default()
};
```

### Fix

Change the closure to return the buffer directly and have callers not own the result.
The simplest approach: have `missing_chunks_fn` write into a caller-provided buffer
instead of returning a Vec. This requires changing `unassigned_blocks()` to accept a
buffer parameter.

However, if the `missing_chunks_fn` signature is used as a trait/callback type that
can't easily change, an alternative is to **swap** the buffer out of the RefCell instead
of cloning:

```rust
let missing_chunks_fn = |piece: u32| -> Vec<(u32, u32)> {
    self.chunk_tracker.as_ref().map(|ct| {
        let mut buf = missing_buf.borrow_mut();
        ct.missing_chunks_into(piece, &mut buf);
        std::mem::take(&mut *buf)  // ← ZERO-COST take, moves buffer out
    }).unwrap_or_default()
};
```

With `std::mem::take`, the buffer's allocation is moved to the caller. The RefCell is
left with an empty Vec. On the next call, `missing_chunks_into` will re-grow the Vec
from scratch. To preserve the allocation across calls, the callers must return the Vec
back after use — but that requires more invasive changes.

**Better approach: pass buffer through to unassigned_blocks.**

Read the actual code to determine which approach is cleanest. The key constraint is:
`pick_blocks()` calls `unassigned_blocks()` which calls the `missing_chunks_fn` closure.
If `unassigned_blocks()` can accept a `&mut Vec<(u32, u32)>` scratch buffer and write
results into it (instead of returning a new Vec), the entire chain becomes allocation-free.

- [ ] **Step 1: Read the current `pick_blocks()` and `unassigned_blocks()` signatures**

Read `crates/torrent-session/src/piece_selector.rs` lines 240-350 and
`crates/torrent-session/src/torrent.rs` lines 5220-5260.

Understand the full call chain:
- `request_pieces_from_peer()` creates `missing_chunks_fn` closure
- Passes it to `pick_blocks(ctx, missing_chunks_fn)`
- `pick_blocks()` calls `unassigned_blocks(piece, missing_chunks_fn, ifp)`
- `unassigned_blocks()` calls `missing_chunks_fn(piece)` → Vec, then filters → `.collect()`

- [ ] **Step 2: Refactor `unassigned_blocks()` to write into a scratch buffer**

Change signature from:
```rust
fn unassigned_blocks(piece: u32, missing_fn: &impl Fn(u32) -> Vec<(u32, u32)>, ifp: &InFlightPiece) -> Vec<(u32, u32)>
```

To:
```rust
fn unassigned_blocks(
    piece: u32,
    missing_fn: &mut impl FnMut(u32, &mut Vec<(u32, u32)>),
    ifp: &InFlightPiece,
    out: &mut Vec<(u32, u32)>,
)
```

The `missing_fn` now writes directly into a buffer, and `out` receives the filtered results.
Instead of `.collect()`, use `out.extend(...)`.

- [ ] **Step 3: Update `missing_chunks_fn` closure in `torrent.rs`**

Change to write into provided buffer:
```rust
let missing_chunks_fn = |piece: u32, buf: &mut Vec<(u32, u32)>| {
    buf.clear();
    if let Some(ct) = self.chunk_tracker.as_ref() {
        ct.missing_chunks_into(piece, buf);
    }
};
```

No allocation at all — the buffer is pre-allocated and reused across calls.

- [ ] **Step 4: Update all callers of `unassigned_blocks()` in `pick_blocks()`**

Each call site currently does:
```rust
let blocks = Self::unassigned_blocks(piece, &missing_fn, ifp);
```

Change to:
```rust
Self::unassigned_blocks(piece, &mut missing_fn, ifp, &mut scratch);
// results are now in scratch
```

Create a `scratch: Vec<(u32, u32)>` at the top of `pick_blocks()` with capacity 32
(typical piece has 32 blocks of 16 KiB in a 512 KiB piece). Reuse across all layers.

- [ ] **Step 5: Run tests**

Run: `cargo test -p torrent-session 2>&1 | tail -10`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/torrent-session/src/piece_selector.rs crates/torrent-session/src/torrent.rs
git commit -m "perf: eliminate temp allocations in pick_blocks/unassigned_blocks (M68)"
```

---

## Task 2: Eliminate `best_blocks` Vec Churn in `pick_partial()`

**Files:**
- Modify: `crates/torrent-session/src/piece_selector.rs` (~lines 351-435)

### Problem

`pick_partial()` creates a `best_blocks: Vec<(u32, u32)>` (line 356), then in a loop
over in-flight pieces, it calls `unassigned_blocks()` and replaces `best_blocks = blocks`
(line 378) each time a better candidate is found. Each replacement drops the old Vec
and moves a new one in.

### Fix

After Task 1's refactor, `unassigned_blocks()` writes into a scratch buffer. The
`pick_partial()` loop should use two buffers: one for the current candidate and one
for the best-so-far. When a better candidate is found, swap the buffers instead of
dropping/moving.

- [ ] **Step 1: Refactor `pick_partial()` to use two scratch buffers**

```rust
let mut candidate_buf: Vec<(u32, u32)> = Vec::with_capacity(32);
let mut best_buf: Vec<(u32, u32)> = Vec::with_capacity(32);
let mut best_piece = None;
let mut best_score = ...;

for (piece, ifp) in ctx.in_flight_pieces.iter() {
    Self::unassigned_blocks(*piece, &mut missing_fn, ifp, &mut candidate_buf);
    if candidate_buf.is_empty() { continue; }

    let score = ...;
    if score > best_score {
        std::mem::swap(&mut best_buf, &mut candidate_buf);
        best_piece = Some(*piece);
        best_score = score;
    }
}
```

`std::mem::swap` is O(1) — just swaps the Vec pointers. No allocation.

- [ ] **Step 2: Run tests**

Run: `cargo test -p torrent-session -- piece_selector 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-session/src/piece_selector.rs
git commit -m "perf: eliminate Vec churn in pick_partial via buffer swap (M68)"
```

---

## Task 3: Replace `peer_count()` HashSet with Direct Iteration

**Files:**
- Modify: `crates/torrent-session/src/piece_selector.rs` (~line 73-78)

### Problem

```rust
fn peer_count(&self) -> usize {
    self.assigned_blocks.values().collect::<HashSet<_>>().len()
}
```

This allocates a HashSet on every call just to count unique peers. Called from
`pick_partial()` scoring (line 370) for every in-flight piece candidate.

### Fix

Count unique peers without allocation. Since peer IDs are `SocketAddr` (which is Copy),
a small sorted Vec or a bitset would work, but the simplest zero-alloc approach for
small N is:

```rust
fn peer_count(&self) -> usize {
    // Typically 1-5 unique peers per piece — O(n²) is fine
    let mut count = 0;
    let values: Vec<_> = self.assigned_blocks.values().collect();
    for (i, v) in values.iter().enumerate() {
        if !values[..i].contains(v) {
            count += 1;
        }
    }
    count
}
```

Wait — that still allocates a Vec. Better:

```rust
fn peer_count(&self) -> usize {
    let mut seen = [None; 8]; // Stack-allocated, fits typical case
    let mut count = 0;
    for peer in self.assigned_blocks.values() {
        if !seen[..count].iter().any(|s| s == &Some(peer)) {
            if count < seen.len() {
                seen[count] = Some(peer);
            }
            count += 1;
        }
    }
    count
}
```

Actually, read the type of `assigned_blocks` values first. They might be `SocketAddr`
or an index. The exact solution depends on the value type. Read the code and implement
the minimal-allocation approach.

- [ ] **Step 1: Read `InFlightPiece` struct and `assigned_blocks` type**

Read `crates/torrent-session/src/piece_selector.rs` lines 60-80.

- [ ] **Step 2: Implement allocation-free `peer_count()`**

Use a small fixed-size array on the stack (8 slots covers >99% of cases — pieces rarely
have more than 8 unique peers). Fall back to counting via sort for edge cases.

- [ ] **Step 3: Run tests**

Run: `cargo test -p torrent-session -- piece_selector 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 4: Commit**

```bash
git add crates/torrent-session/src/piece_selector.rs
git commit -m "perf: replace HashSet allocation in peer_count with stack array (M68)"
```

---

## Task 4: Cache `preferred_extent()` Result

**Files:**
- Modify: `crates/torrent-session/src/piece_selector.rs` (~lines 472-500)

### Problem

`preferred_extent()` builds a `FxHashMap<u32, u32>` on every call by iterating all
in-flight pieces. Called from the rarest-first path in `pick_blocks()`.

### Fix

Compute the preferred extent once at the top of `pick_blocks()` and pass it through
as a parameter, rather than recomputing inside `pick_new_piece()` / rarest-first.

- [ ] **Step 1: Read how `preferred_extent()` is called**

Read `crates/torrent-session/src/piece_selector.rs` lines 470-500. Identify all callers.

- [ ] **Step 2: Hoist computation to `pick_blocks()` and pass as parameter**

Compute `preferred_extent` once in `pick_blocks()`:
```rust
let preferred_ext = Self::preferred_extent(ctx);
```

Pass it to `pick_new_piece()` as a parameter instead of having it call
`preferred_extent()` internally.

- [ ] **Step 3: Run tests**

Run: `cargo test -p torrent-session 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 4: Commit**

```bash
git add crates/torrent-session/src/piece_selector.rs
git commit -m "perf: cache preferred_extent computation in pick_blocks (M68)"
```

---

## Task 5: Reduce Store Buffer and ARC Cache Defaults

**Files:**
- Modify: `crates/torrent-session/src/settings.rs`

### Store Buffer

The `max_in_flight_pieces` default is 32, capping the store buffer at ~16 MiB (32 × 512 KiB).
Reducing to 20 would cap at ~10 MiB with negligible speed impact (the store buffer only
needs to absorb write latency, not buffer large amounts of data).

### ARC Cache

The ARC read cache is useful for streaming but wastes memory for download-to-completion.
If there's an `arc_cache_size` or similar setting, reduce the default.

- [ ] **Step 1: Read current defaults**

Search: `grep -n "max_in_flight\|arc_cache\|read_cache\|cache_size" crates/torrent-session/src/settings.rs`

- [ ] **Step 2: Reduce `max_in_flight_pieces` default from 32 to 20**

Update the default function and any related documentation.

- [ ] **Step 3: Investigate ARC cache configuration**

Read the ARC cache setup in `crates/torrent-session/src/disk.rs`. If there's a
configurable size, reduce the default. If it's hardcoded, make it configurable via
Settings and set a smaller default.

- [ ] **Step 4: Update low-memory preset**

Ensure `Settings::min_memory()` uses an even smaller `max_in_flight_pieces` (e.g., 8).

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/torrent-session/src/settings.rs crates/torrent-session/src/disk.rs
git commit -m "perf: reduce store buffer and cache defaults for lower RSS (M68)"
```

---

## Task 6: Benchmark and Verify

- [ ] **Step 1: Run heaptrack to verify allocation reduction**

```bash
heaptrack target/release/torrent download \
    "magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso" \
    -o /tmp/torrent-bench/profile -q
heaptrack_print heaptrack.torrent.*.zst 2>&1 | grep "temporary\|calls to\|peak heap"
```

Expected:
- Temporary allocations: <500K (down from 2.04M)
- Total allocations: <2M (down from 3.78M)
- Peak heap: <25 MiB (down from 32.78 MiB)

- [ ] **Step 2: Run 3-trial benchmark**

```bash
for i in 1 2 3; do
    rm -rf /tmp/torrent-bench/trial$i && mkdir -p /tmp/torrent-bench/trial$i
    /usr/bin/time -v target/release/torrent download \
        "magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso" \
        -o /tmp/torrent-bench/trial$i -q 2>&1 | grep -E "peak_peers|final_speed|Maximum resident|User time"
done
```

Expected: RSS ~55-65 MiB (down from 79 MiB)

- [ ] **Step 3: Save results and commit**

Update benchmark reports and commit artifacts.

---

## Verification Checklist

1. `cargo test --workspace` — all tests pass
2. `cargo clippy --workspace -- -D warnings` — clean
3. Full download test (Arch ISO) — completes without stall
4. Heaptrack: temporary allocations reduced by >50%
5. RSS reduced by >15 MiB from v0.69.0 baseline

## Commit Plan

1. `perf: eliminate temp allocations in pick_blocks/unassigned_blocks (M68)`
2. `perf: eliminate Vec churn in pick_partial via buffer swap (M68)`
3. `perf: replace HashSet allocation in peer_count with stack array (M68)`
4. `perf: cache preferred_extent computation in pick_blocks (M68)`
5. `perf: reduce store buffer and cache defaults for lower RSS (M68)`
6. `chore: bump version to 0.71.0 (M68)`
7. `docs: update README and CHANGELOG for M68`
