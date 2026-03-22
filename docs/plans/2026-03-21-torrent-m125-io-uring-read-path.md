# M125: io_uring Async Read Path

## Context

M122 shipped the io_uring write backend — `IoUringDiskIo` wraps `PosixDiskIo` and
overrides `write_block_direct()` with `Writev` SQEs. Reads still go through the inner
`PosixDiskIo` which uses standard `read_chunk()` → `pread`/`seek+read` via the
`FilesystemStorage` layer.

This milestone adds `Readv` SQEs for the read hot path, completing the io_uring story.
Reads are important for seeding (uploading pieces to peers) and for hash verification
after a piece completes (read-back-and-verify pattern from M100).

### Read Paths in DiskIoBackend

| Method | Used For | Hot Path? |
|--------|----------|-----------|
| `read_chunk()` | Peer upload requests | Yes (seeding) |
| `read_piece()` | Hash verification after write | Yes (downloading) |
| `hash_piece()` | Piece verification (read + SHA-1) | Medium |
| `hash_piece_v2()` | V2 verification (read + SHA-256) | Medium |
| `hash_block()` | Merkle block hash | Low |

### Design Decision: Override read_chunk() and read_piece() Only

`hash_piece()` and `hash_piece_v2()` in `PosixDiskIo` have a cache-aware code path
(hash-from-cache via `BufferPool::take_all_blocks()`). Overriding them would bypass the
cache optimization. Instead, we only override the raw read methods — the hash methods
call `read_piece()` internally when the cache path misses, so they benefit transitively.

## Design

### IoUringDiskIo Changes

Add a `uring_read()` method mirroring `uring_write()`:

```rust
fn uring_read(
    &self,
    state: &IoUringWriteState,  // rename to IoUringTorrentState
    piece: u32,
    begin: u32,
    length: u32,
) -> crate::Result<Vec<u8>> {
    let segments = state.file_map.chunk_segments(piece, begin, length);
    let num_segments = segments.len();
    let mut buf = vec![0u8; length as usize];

    // Build Readv SQEs — one per segment, iovecs point into buf.
    let mut all_iovecs: Vec<SmallVec<[libc::iovec; 1]>> = Vec::with_capacity(num_segments);
    let mut pos = 0usize;

    for seg in &segments {
        let seg_len = seg.len as usize;
        let iovecs = smallvec![libc::iovec {
            iov_base: buf[pos..pos + seg_len].as_mut_ptr() as *mut libc::c_void,
            iov_len: seg_len,
        }];
        pos += seg_len;
        all_iovecs.push(iovecs);
    }

    // Submit under ring lock.
    let mut ring = self.ring.lock();
    for (i, seg) in segments.iter().enumerate() {
        let iovecs = &all_iovecs[i];
        let fd = state.fds.fd(seg.file_index);
        let readv = opcode::Readv::new(
            types::Fd(fd),
            iovecs.as_ptr().cast(),
            iovecs.len() as u32,
        )
        .offset(seg.file_offset)
        .build()
        .user_data(i as u64);

        // SQ-full retry loop (same as uring_write).
        loop {
            match unsafe { ring.submission().push(&readv) } {
                Ok(()) => break,
                Err(_) => { ring.submit().map_err(crate::Error::Io)?; }
            }
        }
    }

    ring.submit_and_wait(num_segments).map_err(crate::Error::Io)?;

    // Reap + error check (same pattern as uring_write).
    // ...

    Ok(buf)
}
```

### File Descriptor Flags

Currently `IoUringStorageState::open_files()` opens with `O_WRONLY | O_CREAT`.
For reads, we need `O_RDWR`. Update the flags in `register()`:

```rust
let mut flags = libc::O_RDWR | libc::O_CREAT;  // was O_WRONLY
```

This is the only change to the existing code — everything else is additive.

### Rename IoUringWriteState → IoUringTorrentState

Since it now handles both reads and writes, rename for clarity.

### Stats Tracking

Add `uring_read_bytes: AtomicU64` alongside existing `uring_write_bytes`.
Merge into `stats()`:
```rust
fn stats(&self) -> DiskIoStats {
    let mut s = self.inner.stats();
    s.write_bytes += self.uring_write_bytes.load(Relaxed);
    s.read_bytes += self.uring_read_bytes.load(Relaxed);
    s
}
```

### DiskIoBackend Overrides

```rust
fn read_chunk(&self, info_hash: Id20, piece: u32, begin: u32, length: u32, volatile: bool) -> crate::Result<Bytes> {
    // Non-volatile reads go through inner (cache-aware path).
    if !volatile {
        return self.inner.read_chunk(info_hash, piece, begin, length, volatile);
    }
    // Volatile reads (won't be re-read) use io_uring directly.
    let uring_states = self.uring_states.read();
    if let Some(state) = uring_states.get(&info_hash) {
        let data = self.uring_read(state, piece, begin, length)?;
        drop(uring_states);
        self.uring_read_bytes.fetch_add(length as u64, Relaxed);
        return Ok(Bytes::from(data));
    }
    drop(uring_states);
    self.inner.read_chunk(info_hash, piece, begin, length, volatile)
}

fn read_piece(&self, info_hash: Id20, piece: u32) -> crate::Result<Vec<u8>> {
    // read_piece is called for hash verification — always from disk.
    // Flush inner first (pending write buffer), then read via uring.
    self.inner.flush_piece(info_hash, piece)?;
    let uring_states = self.uring_states.read();
    if let Some(state) = uring_states.get(&info_hash) {
        let piece_size = state.file_map.piece_size(piece);
        let data = self.uring_read(state, piece, 0, piece_size)?;
        drop(uring_states);
        self.uring_read_bytes.fetch_add(piece_size as u64, Relaxed);
        return Ok(data);
    }
    drop(uring_states);
    self.inner.read_piece(info_hash, piece)
}
```

**Design note**: Non-volatile `read_chunk()` still goes through inner's buffer pool
for cache hits. Only volatile reads (peer upload requests marked as won't-be-reread)
bypass the cache via io_uring. `read_piece()` always uses io_uring when available
since it's reading the full piece for verification (no cache benefit).

## Files Modified

| File | Change |
|------|--------|
| `crates/torrent-session/src/io_uring_backend.rs` | Add `uring_read()`, override `read_chunk()`/`read_piece()`, rename state type, add read stats |
| `crates/torrent-storage/src/io_uring_backend.rs` | Update `open_files()` default flags comment |

## Task Breakdown

### Task 1: Rename IoUringWriteState → IoUringTorrentState, update fd flags to O_RDWR
### Task 2: Implement uring_read() method
### Task 3: Override read_chunk() (volatile path) and read_piece()
### Task 4: Stats tracking (uring_read_bytes)
### Task 5: Tests (6-8 new) — single-file read, multi-file read, volatile vs non-volatile, read-after-write

## Success Criteria

- Volatile reads go through io_uring when uring state exists
- read_piece() uses io_uring for hash verification path
- Non-volatile reads still use buffer pool cache
- uring_read_bytes tracked in stats
- 6-8 new tests pass
- Benchmark shows improvement on seeding workload
