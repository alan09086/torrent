# M6: ferrite-storage — Implementation Plan

## Context

M1-M5 complete (190 tests, zero clippy warnings). M6 is the storage layer between protocol crates and session manager — handles piece verification, chunk tracking, multi-file mapping, and disk I/O. No BEPs, pure infrastructure.

## Crate: `crates/ferrite-storage`

**Deps**: ferrite-core, thiserror, tracing
**No**: bytes, tokio, serde — this crate is fully synchronous

## Module Structure

```
ferrite-storage/src/
    lib.rs              -- pub exports
    error.rs            -- Error enum
    bitfield.rs         -- Compact bit-vector (wire-format compatible)
    file_map.rs         -- Piece/chunk coords → file segments
    chunk_tracker.rs    -- Per-piece chunk state + have-bitfield
    storage.rs          -- TorrentStorage trait
    memory.rs           -- In-memory backend (tests + magnet pre-metadata)
    filesystem.rs       -- Disk-backed backend (sync I/O)
```

## Key Types

### Bitfield (`bitfield.rs`)
```rust
pub struct Bitfield { data: Vec<u8>, len: u32 }
// MSB-first bit ordering (matches BEP 3 wire format)
// Methods: new, from_bytes, get, set, clear, count_ones, all_set,
//          as_bytes, ones(), zeros()
```

### FileMap (`file_map.rs`)
```rust
pub struct FileSegment { pub file_index: usize, pub file_offset: u64, pub len: u32 }
pub struct FileMap { file_offsets: Vec<u64>, file_lengths: Vec<u64>, lengths: Lengths }
// Pre-computed cumulative offsets, binary search for O(log n) file lookup
// Methods: new, byte_range_to_segments, chunk_segments, piece_segments
```

### ChunkTracker (`chunk_tracker.rs`)
```rust
pub struct ChunkTracker {
    have: Bitfield,                         // piece-level completion
    in_progress: HashMap<u32, Bitfield>,    // per-piece chunk state (only active pieces)
    lengths: Lengths,
}
// Methods: chunk_received -> bool (piece complete?), mark_verified, mark_failed,
//          has_chunk, has_piece, bitfield, missing_chunks, from_bitfield
```

### TorrentStorage trait (`storage.rs`)
```rust
pub trait TorrentStorage: Send + Sync {
    fn write_chunk(&self, piece: u32, begin: u32, data: &[u8]) -> Result<()>;
    fn read_chunk(&self, piece: u32, begin: u32, length: u32) -> Result<Vec<u8>>;
    fn read_piece(&self, piece: u32) -> Result<Vec<u8>>;
    fn verify_piece(&self, piece: u32, expected: &Id20) -> Result<bool> { /* default impl */ }
}
```
- `&self` not `&mut self` — shared via `Arc`, thread safety via interior mutability
- `(piece, begin, length)` matches wire protocol directly — no translation layer
- `verify_piece` has default impl using `read_piece` + `sha1()`

### FilesystemStorage (`filesystem.rs`)
- `files: Vec<Mutex<Option<File>>>` — lazy open, per-file locking
- Uses `FileMap` for piece→file mapping
- Creates sparse files with `File::set_len()`
- All sync I/O (no async FS overhead, enables pwritev later)

### MemoryStorage (`memory.rs`)
- `data: RwLock<Vec<u8>>` — flat buffer, concurrent reads
- Used for tests and magnet-link metadata downloads

## Improvements Over librqbit

1. **`from_bitfield` resume** — persist bitfield, skip re-verification on restart
2. **Binary search file lookup** — O(log n) vs linear scan
3. **Clean trait** — `verify_piece` default method, `(piece, begin, len)` wire-compatible API
4. **Lazy file handles** — reduced startup overhead
5. **Reusable Bitfield** — same type for internal tracking and wire protocol

## Implementation Order

| Step | File | Tests |
|------|------|-------|
| 1 | `Cargo.toml` + `error.rs` | 0 |
| 2 | `bitfield.rs` | ~12 |
| 3 | `file_map.rs` | ~8 |
| 4 | `chunk_tracker.rs` | ~8 |
| 5 | `storage.rs` (trait only) | 0 |
| 6 | `memory.rs` | ~5 |
| 7 | `filesystem.rs` | ~8 |
| 8 | `lib.rs` (exports) | 0 |

## Test Plan (~41 tests)

**bitfield.rs** (~12): new_all_clear, set_get, clear, count_ones, all_set, from_bytes_valid, from_bytes_trailing_rejected, round_trip, ones_iter, zeros_iter, empty, single_bit

**file_map.rs** (~8): single_file, multi_file_no_span, chunk_spans_boundary, piece_spans_three_files, last_piece_shorter, zero_length_file, byte_range_single, piece_segments

**chunk_tracker.rs** (~8): new_all_missing, chunk_received, piece_complete, mark_verified, mark_failed_resets, has_chunk, missing_chunks, from_bitfield

**memory.rs** (~5): write_read_chunk, read_piece, verify_correct, verify_wrong, zero_length

**filesystem.rs** (~8): single_file_write_read, multi_file_write_read, read_piece, verify_piece, creates_directories, creates_sparse_files, last_piece_shorter, concurrent_different_pieces

## Verification

```bash
cargo test --workspace           # 190 existing + ~41 new = ~231
cargo clippy --workspace -- -D warnings  # Zero warnings
```

## Key Files to Reference

- `ferrite-core/src/lengths.rs` — piece_offset, chunk_info, piece_size, file_pieces
- `ferrite-core/src/metainfo.rs` — FileInfo, InfoDict::files(), piece_hash()
- `ferrite-core/src/lib.rs` — sha1(), Id20, Lengths re-exports
- `ferrite-dht/src/error.rs` — error pattern to follow
