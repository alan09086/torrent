# M128: I/O Performance Hardening

## Context

M122-M127 ship the platform-specific I/O backends (io_uring on Linux, IOCP on Windows,
F_NOCACHE on macOS). This milestone hardens the I/O subsystem with optimizations that
apply across platforms, plus Linux-specific io_uring tuning based on benchmark data.

### Optimization Candidates (from M122 design spec)

1. **Registered io_uring buffers** — pre-register buffer addresses with the kernel,
   eliminating per-SQE page pinning overhead
2. **Per-worker ring sharding** — one `IoUring` per tokio worker thread via
   `thread_local!`, eliminating `Mutex` contention on the shared ring
3. **Batch write accumulation** — collect multiple blocks before submitting to
   amortize ring overhead (currently one submit per write_block_direct call)

### When to Apply Each

| Optimization | Trigger | Expected Gain |
|-------------|---------|---------------|
| Registered buffers | Always beneficial | 5-10% write throughput |
| Per-worker sharding | Lock contention >5% in flamegraph | 10-20% at high peer counts |
| Batch accumulation | Small blocks dominate (16 KiB) | 5-15% from fewer submits |

## Design

### 1. Registered Buffers (Linux io_uring)

The `io_uring` kernel can pin buffer pages once at registration time instead of
per-SQE. This avoids repeated `get_user_pages_fast()` calls in the kernel.

```rust
impl IoUringDiskIo {
    fn register_buffers(&self) -> io::Result<()> {
        // Pre-allocate a pool of aligned buffers.
        let buf_size = 16384; // Standard block size
        let num_bufs = 64;    // Pool of 64 buffers
        let mut bufs: Vec<Vec<u8>> = (0..num_bufs)
            .map(|_| vec![0u8; buf_size])
            .collect();

        let iovecs: Vec<libc::iovec> = bufs.iter_mut()
            .map(|b| libc::iovec {
                iov_base: b.as_mut_ptr() as *mut _,
                iov_len: buf_size,
            })
            .collect();

        let mut ring = self.ring.lock();
        ring.submitter().register_buffers(&iovecs)?;
        Ok(())
    }
}
```

Then use `opcode::WriteFixed` instead of `opcode::Writev` — copies data into a
registered buffer, then submits with the buffer index. The copy is fast (L1 cache)
and the kernel skips page pinning.

### 2. Per-Worker Ring Sharding (Linux io_uring)

Replace `Mutex<IoUring>` with `thread_local!` storage:

```rust
thread_local! {
    static THREAD_RING: RefCell<Option<IoUring>> = RefCell::new(None);
}

impl IoUringDiskIo {
    fn with_ring<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut IoUring) -> R,
    {
        THREAD_RING.with(|cell| {
            let mut opt = cell.borrow_mut();
            let ring = opt.get_or_insert_with(|| {
                IoUring::new(self.config.sq_depth)
                    .expect("io_uring per-worker init")
            });
            f(ring)
        })
    }
}
```

This eliminates all Mutex contention — each tokio worker thread gets its own ring.
The tradeoff is more kernel resources (one ring per worker × sq_depth SQEs).

**Activation**: Behind a config flag `io_uring_per_worker: bool` (default: false).
Enable when flamegraph shows `Mutex::lock` in the io_uring write hot path.

### 3. Batch Write Accumulation

Currently each `write_block_direct()` call submits one batch of SQEs (usually 1
segment = 1 SQE for single-file torrents). For 16 KiB blocks at 60 MB/s, that's
~3,750 submits/sec.

Batch accumulation collects N blocks before submitting:

```rust
struct BatchAccumulator {
    pending: Vec<PendingWrite>,
    flush_threshold: usize,  // From IoUringConfig::batch_threshold
}

struct PendingWrite {
    segments: SmallVec<[FileSegment; 4]>,
    iovecs: Vec<SmallVec<[libc::iovec; 2]>>,
    // Lifetime managed by caller — data must live until flush
}
```

**Problem**: `write_block_direct()` borrows `s0`/`s1` — we can't defer the submit
past the method return because the slices would be dangling. Options:

1. **Copy into a staging buffer** — 16 KiB memcpy is fast (L1 cache), amortize
   ring overhead. Use registered buffers from optimization #1.
2. **Caller-side batching** — the peer task accumulates blocks and calls a new
   `write_blocks_batch()` method. Requires wire protocol changes.
3. **Timer-based flush** — background task flushes every N ms. Adds latency.

**Recommendation**: Option 1 (copy + registered buffers). No API changes needed.
`write_block_direct()` copies into a registered buffer, queues the SQE, and
submits when the batch threshold is reached or when the last SQE for a piece fires.

### 4. Cross-Platform Benchmark Suite

Add a structured benchmark comparing all backends:

```bash
./benchmarks/compare-backends.sh <magnet> -n 10
```

Runs baseline (posix) → io_uring → iocp (Windows) → direct-io variants.
Produces a comparison table with mean/median/stddev/min/max per backend.

## Files Modified

| File | Change |
|------|--------|
| `crates/torrent-session/src/io_uring_backend.rs` | Registered buffers, per-worker sharding, batch accumulation |
| `crates/torrent-storage/src/io_uring_backend.rs` | Buffer pool for registered buffers |
| `crates/torrent-session/src/settings.rs` | `io_uring_per_worker`, `io_uring_registered_buffers` settings |
| `benchmarks/compare-backends.sh` | **NEW** — cross-backend comparison script |

## Task Breakdown

### Task 1: Registered buffer pool + WriteFixed SQEs
### Task 2: Per-worker ring sharding (thread_local)
### Task 3: Batch accumulation with copy-into-registered-buffer
### Task 4: Settings + CLI for new options
### Task 5: Cross-platform benchmark script
### Task 6: Tests (8-10 new) — registered buffers, per-worker ring, batch flush

## Success Criteria

- Registered buffers reduce kernel overhead (measurable in flamegraph)
- Per-worker sharding eliminates Mutex contention
- Batch accumulation reduces submit rate by 4x+ (configurable)
- Benchmark suite produces clean comparison tables
- ≥65 MB/s with all optimizations enabled (target: 10% over M122 baseline)
- 8-10 new tests pass
