# TODOS

## rqbit-style per-block pwrite architecture (M100+)

The current write pipeline uses two layers of buffering:

1. **WriteCoalescer** — per-peer `BytesMut` that accumulates 16 KiB blocks into full
   pieces (~512 KiB) before issuing a single `pwrite()`.
2. **StoreBuffer** — a `BTreeMap<offset, Bytes>` that holds blocks in memory until
   piece hash verification completes.

This works correctly but uses ~58 MiB RSS:
- PieceBufferPool: 32 × 512 KiB = 16 MiB (bounded)
- StoreBuffer: up to 32 MiB (bounded, but ≤16 MiB in practice)
- Other: ~10 MiB

### Proposed change

Replace both layers with **direct per-block pwrite at the correct file offset**,
verifying from disk after all blocks arrive. This is the approach used by
[rqbit](https://github.com/ikatson/rqbit):

- Source: `~/.cargo/registry/src/index.crates.io-*/librqbit-8.1.1/src/file_ops.rs`
  - `write_chunk()`: computes file segments from piece/offset, writes each segment
    directly via `pwrite()` at the correct position
  - `check_piece()`: reads the full piece back from disk and hashes it

### Benefits

- **RSS reduction**: ~58 MiB → ~38 MiB (eliminates both coalescer and store buffer)
- **Code deletion**: ~500 lines removed (WriteCoalescer, PieceBufferPool, StoreBuffer)
- **Simpler mental model**: blocks go directly to disk, verification reads from disk

### Why not done in M99

The failed first M99 attempt tried to partially move in this direction (replacing the
store buffer with a pool) but introduced three fatal bugs:

1. **Out-of-order blocks**: Coalescer assumes sequential block delivery; without it,
   blocks must be written at correct offsets
2. **Partial write corruption**: Flushing incomplete pieces on disconnect left garbage
   on disk
3. **Verify-before-write race**: Hash verification read from disk before all blocks
   were written

All three bugs stem from removing the store buffer while keeping the coalescer. The
correct fix is to remove *both* — write blocks directly to their file positions (like
rqbit) and verify by reading from disk. This is a larger architectural change that
warrants its own milestone.

### Prerequisite

The `TorrentStorage` trait's `write_chunk()` already supports writing at arbitrary
(piece, offset) positions, so the storage layer is ready. The change is entirely in
`torrent-session`: remove the coalescer, remove the store buffer, and change
`enqueue_verify` to read from disk instead of memory.
