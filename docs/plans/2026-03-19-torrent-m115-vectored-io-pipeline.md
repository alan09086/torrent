# M115: Vectored I/O Pipeline

## Context

After M110's zero-copy piece pipeline, the ring buffer read path still uses standard
`AsyncRead::read()` which fills one contiguous region at a time. When the ring buffer
wraps around its 32 KiB boundary, two separate `read()` calls are needed to fill both
sides — doubling the syscall count for wrap-around reads. Additionally, `PeerWriter`
uses `BytesMut` for message serialization, which may reallocate dynamically.

rqbit solves both problems:
1. A custom `AsyncReadVectored` trait that calls `readv()` with 2 `IoSliceMut`,
   filling both ring buffer regions in a single kernel syscall
2. Pre-allocated `Box<[u8; MAX_MSG_LEN]>` write buffers that are reused forever

**M110 benchmark context:**

| Metric       | M110    | rqbit  |
|------------- |---------|--------|
| Speed        | 58.8 MB/s | 74.6 MB/s |
| RSS          | 31 MiB  | --     |
| Heap allocs  | 148K    | ~4K    |

The `run_peer` spawning allocations (26K from heaptrack) include per-peer `BytesMut`
creation. Eliminating dynamic write buffers removes a significant portion of these.

## Architecture

### Current Read Path

```
socket.read(unfilled_contiguous())     ← one contiguous slice
  → fills [write_pos..BUF_LEN]        ← first half only if wrapping
  → must call read() again for [0..start)  ← second syscall
```

`PeerReader::fill_message()` in `peer_codec.rs` calls:
```rust
let n = self.reader.read(self.buf.unfilled_contiguous()).await?;
```

This returns a single `&mut [u8]` slice. When the write cursor is near the end of
the ring buffer, only the bytes from `write_pos` to `BUF_LEN` are fillable. The
remaining writable space at `[0..start)` requires a second `read()` call on the
next loop iteration.

### Proposed Read Path

```
socket.read_vectored([IoSliceMut(write_pos..BUF_LEN), IoSliceMut(0..start)])
  → single readv() syscall fills both regions
```

Uses the `AsyncReadVectored` trait with `unfilled_ioslices()` (already exists in
our `ReadBuf` but is currently unused) to pass both writable regions to a single
kernel `readv()` call.

### Current Write Path

```
PeerWriter {
    writer: W,
    buf: BytesMut,      ← dynamic allocation, may grow/shrink
}
```

`BytesMut` is cleared on each message but retains capacity. Under memory pressure
or after large messages, it may reallocate.

### Proposed Write Path

```
PeerWriter {
    writer: W,
    buf: Box<[u8; MAX_MSG_LEN]>,  ← fixed allocation, reused forever
}
```

`MAX_MSG_LEN = 4 + 9 + 16384 = 16397` bytes. One allocation at peer connection
time, zero reallocations for the lifetime of the connection.

## rqbit Reference

### `crates/librqbit/src/vectored_traits.rs` lines 5-70

Defines `AsyncReadVectored` trait:
```rust
pub trait AsyncReadVectored: AsyncRead + Unpin {
    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>>;
}
```

Implementations for:
- `tokio::net::tcp::OwnedReadHalf` — uses `try_read_vectored()` with WouldBlock retry
- `librqbit_utp::UtpStreamReadHalf` — delegates to uTP's vectored read
- `AsyncReadToVectoredCompat<T>` — fallback wrapper for non-vectored AsyncRead

### `crates/librqbit/src/read_buf.rs` line 124: `unfilled_ioslices()`

```rust
fn unfilled_ioslices(&mut self) -> [IoSliceMut<'_>; 2] {
    let write_start = (self.start.saturating_add(self.len)) % BUFLEN;
    let available_len = BUFLEN.saturating_sub(self.len);
    // ... splits into two IoSliceMut covering both writable regions
}
```

### `crates/librqbit/src/read_buf.rs` line 184: vectored fill

```rust
let size = with_timeout(
    "reading", timeout,
    conn.read_vectored(&mut self.unfilled_ioslices()).map_err(Error::Write),
).await?;
```

### `crates/librqbit/src/peer_connection.rs` line 113: pre-allocated write buffer

```rust
struct ManagePeerArgs {
    // ...
    write_buf: Box<[u8; MAX_MSG_LEN]>,
    // ...
}
```

Write buffer allocated once per peer connection:
```rust
let mut write_buf = Box::new([0u8; MAX_MSG_LEN]);
```

All outgoing messages serialize into this fixed buffer:
```rust
let len = msg.serialize(&mut *write_buf, ext_msg_ids)?;
write.write_all(&write_buf[..len]).await?;
```

## Changes

### 1. AsyncReadVectored trait (`torrent-session/src/vectored_io.rs`, ~90 lines)

New file with trait definition and implementations.

```rust
use std::{future::poll_fn, io::IoSliceMut, pin::Pin, task::Poll};
use tokio::io::AsyncRead;

/// Extension trait for vectored reads (readv syscall).
/// Fills multiple non-contiguous buffers in a single kernel call.
pub(crate) trait AsyncReadVectored: AsyncRead + Unpin {
    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>>;
}

/// Convenience extension for async callers.
pub(crate) trait AsyncReadVectoredExt {
    async fn read_vectored(&mut self, vec: &mut [IoSliceMut<'_>]) -> std::io::Result<usize>;
}

impl<T: AsyncReadVectored> AsyncReadVectoredExt for T {
    async fn read_vectored(&mut self, vec: &mut [IoSliceMut<'_>]) -> std::io::Result<usize> {
        poll_fn(|cx| Pin::new(&mut *self).poll_read_vectored(cx, vec)).await
    }
}
```

### 2. TCP implementation (~20 lines)

```rust
impl AsyncReadVectored for tokio::net::tcp::OwnedReadHalf {
    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        loop {
            match this.try_read_vectored(vec) {
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::task::ready!(this.as_ref().poll_read_ready(cx)?);
                    continue;
                }
                res => return Poll::Ready(res),
            }
        }
    }
}
```

### 3. Fallback compatibility wrapper (~30 lines)

For stream types that don't support native vectored reads (MseStream, test duplex):

```rust
/// Wraps any AsyncRead to provide a single-buffer vectored fallback.
pub(crate) struct VectoredCompat<T>(pub T);

impl<T: AsyncRead + Unpin> AsyncReadVectored for VectoredCompat<T> {
    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let first_non_empty = match vec.iter_mut().find(|s| !s.is_empty()) {
            Some(s) => &mut **s,
            None => return Poll::Ready(Ok(0)),
        };
        let mut rbuf = tokio::io::ReadBuf::new(first_non_empty);
        std::task::ready!(Pin::new(&mut self.get_mut().0).poll_read(cx, &mut rbuf)?);
        Poll::Ready(Ok(rbuf.filled().len()))
    }
}
```

The compatibility wrapper fills only the first non-empty `IoSliceMut`, which
preserves the current behavior for MseStream (RC4 encryption operates on a
single contiguous buffer). This is correct because MseStream decrypts in-place
and cannot handle split regions. For plaintext MseStream, the compat wrapper
is functionally equivalent to the current single-buffer read.

### 4. MseStream integration (~15 lines)

`MseStream<S>` wraps a stream with optional RC4 encryption. The vectored trait
is implemented via the compatibility wrapper:

```rust
impl<S: AsyncRead + Unpin + Send> AsyncReadVectored for torrent_wire::mse::MseStream<S> {
    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>> {
        // MseStream decrypts in-place; use single-buffer fallback
        // to ensure decryption operates on contiguous data.
        let first_non_empty = match vec.iter_mut().find(|s| !s.is_empty()) {
            Some(s) => &mut **s,
            None => return Poll::Ready(Ok(0)),
        };
        let mut rbuf = tokio::io::ReadBuf::new(first_non_empty);
        std::task::ready!(self.poll_read(cx, &mut rbuf)?);
        Poll::Ready(Ok(rbuf.filled().len()))
    }
}
```

When encryption is `Disabled` (the default since M83), MseStream passes through
to the inner stream. A future optimization could detect plaintext mode and
delegate to the inner stream's native vectored read.

### 5. PeerReader changes (`torrent-session/src/peer_codec.rs`, ~20 lines changed)

Change the reader's generic parameter and fill method:

```rust
pub(crate) struct PeerReader<R> {
    reader: R,  // was: R: AsyncRead, now: R: AsyncReadVectored
    buf: ReadBuf,
    max_message_size: usize,
}

impl<R: AsyncReadVectored> PeerReader<R> {
    pub(crate) async fn fill_message(&mut self) -> Result<FillStatus, crate::Error> {
        // Use vectored read to fill both ring buffer regions in one syscall
        let (s0, s1) = self.buf.unfilled_ioslices();
        let mut iov = [IoSliceMut::new(s0), IoSliceMut::new(s1)];
        let n = self.reader.read_vectored(&mut iov).await
            .map_err(|e| crate::Error::Io(e))?;
        if n == 0 {
            return Ok(FillStatus::Eof);
        }
        self.buf.mark_filled(n);
        Ok(FillStatus::Ok)
    }
}
```

The `unfilled_ioslices()` method already exists on `ReadBuf` (added in M109, currently
marked `#[allow(dead_code)]`). This change activates it.

### 6. ReadBuf::unfilled_ioslices returns IoSliceMut (~10 lines changed)

The existing `unfilled_ioslices()` returns `(&mut [u8], &mut [u8])`. Change the
return type to `[IoSliceMut<'_>; 2]` to match the kernel readv interface directly:

```rust
pub(crate) fn unfilled_ioslices(&mut self) -> [IoSliceMut<'_>; 2] {
    let available = BUF_LEN - self.len;
    if available == 0 {
        return [IoSliceMut::new(&mut []), IoSliceMut::new(&mut [])];
    }
    let write_pos = (self.start + self.len) % BUF_LEN;
    if write_pos >= self.start {
        let (left, right) = self.buf.split_at_mut(write_pos);
        [
            IoSliceMut::new(right),
            IoSliceMut::new(&mut left[..self.start]),
        ]
    } else {
        [
            IoSliceMut::new(&mut self.buf[write_pos..self.start]),
            IoSliceMut::new(&mut []),
        ]
    }
}
```

### 7. PeerWriter changes (`torrent-session/src/peer_codec.rs`, ~30 lines changed)

Replace dynamic `BytesMut` with fixed pre-allocated buffer:

```rust
pub(crate) struct PeerWriter<W> {
    writer: W,
    buf: Box<[u8; MAX_MSG_LEN]>,  // was: BytesMut
}

/// Maximum peer wire message size:
/// 4 (length prefix) + 1 (msg ID) + 8 (piece header) + 16384 (chunk) = 16397
const MAX_MSG_LEN: usize = 4 + 1 + 8 + 16384;

impl<W: AsyncWrite + Unpin> PeerWriter<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            buf: Box::new([0u8; MAX_MSG_LEN]),
        }
    }

    pub(crate) async fn send(&mut self, msg: &Message<impl AsRef<[u8]>>) -> Result<(), crate::Error> {
        let len = msg.encode_into(&mut *self.buf)?;
        self.writer.write_all(&self.buf[..len]).await
            .map_err(|e| crate::Error::Io(e))?;
        Ok(())
    }
}
```

No reallocation, no capacity management. The buffer outlives all messages sent
on the connection.

### 8. run_peer signature update (`torrent-session/src/peer.rs`, ~10 lines)

Update the stream splitting to produce vectored-capable read halves:

```rust
// Current:
let (read_half, write_half) = tokio::io::split(stream);
let mut reader = PeerReader::new(read_half, max_message_size);
let mut writer = PeerWriter::new(write_half);

// Proposed:
let (read_half, write_half) = tokio::io::split(stream);
let mut reader = PeerReader::new(VectoredCompat(read_half), max_message_size);
let mut writer = PeerWriter::new(write_half);
```

For TCP connections where we have the raw `OwnedReadHalf`, use the native
vectored implementation directly. For MseStream-wrapped connections, use the
compatibility wrapper.

## Tests

### New Tests (6)

1. **`vectored_read_fills_both_ring_regions`** — Set ReadBuf start near end of
   buffer (e.g., `start = BUF_LEN - 100`, `len = 200`). Write 500 bytes via
   vectored read. Verify both `[BUF_LEN-100+200..BUF_LEN]` and `[0..N]` are
   filled in a single call.

2. **`vectored_read_single_region_no_wrap`** — ReadBuf with `start = 0`. Vectored
   read should fill `[len..BUF_LEN]` only (second IoSlice empty). Verify behavior
   matches non-vectored read.

3. **`vectored_read_with_mse_encryption`** — Create MseStream (plaintext mode).
   Verify vectored read falls back to single-buffer correctly and data integrity
   is maintained.

4. **`write_buffer_reused_across_messages`** — Send 100 messages through PeerWriter.
   Verify the buffer pointer (address) is the same for all messages (no reallocation).

5. **`write_buffer_handles_max_size_message`** — Send a Piece message with 16384
   bytes of data. Verify it serializes correctly into the fixed buffer without
   panic or truncation.

6. **`vectored_compat_wouldblock_retry`** — Mock an AsyncRead that returns
   WouldBlock on first poll, then data on second. Verify the TCP vectored
   implementation retries correctly via `poll_read_ready`.

### Existing Tests

All 1647+ existing tests must pass. PeerReader/PeerWriter API changes are
internal — the external run_peer interface is unchanged.

## Success Criteria

- Syscall count reduced: verify via `strace -c` that read syscalls decrease
  (fewer reads needed to fill wrapped ring buffers)
- PeerWriter allocations per peer = 1 (the Box) instead of dynamic BytesMut growth
- All existing tests pass
- 6 new tests pass
- No regression in download speed (>= 55 MB/s)

## Verification

```bash
# Run all tests
cargo test --workspace

# Run new vectored I/O tests
cargo test -p torrent-session vectored_
cargo test -p torrent-session write_buffer_

# Benchmark (10 trials)
./benchmarks/analyze.sh '<arch-iso-magnet>' -n 10

# Syscall comparison (before and after)
strace -c -e trace=read,readv ./target/release/torrent-cli download '<magnet>' 2>&1 | tail -20

# Heaptrack verification
heaptrack --record-only ./target/release/torrent-cli download '<magnet>'
heaptrack_print heaptrack.torrent-cli.*.zst | grep -i "BytesMut\|write_buf"
```

## NOT in Scope

| Item | Why | Revisit |
|------|-----|---------|
| Vectored writes (writev) | pwrite already handled by M110 direct path | If write coalescing returns |
| io_uring | Linux 5.1+ only, adds significant complexity, marginal gain for BitTorrent message sizes | If targeting high-throughput datacenter use cases |
| uTP native vectored read | uTP's `UtpStreamReadHalf` may or may not support vectored reads natively; use compat wrapper initially | If uTP performance needs improvement |
| MseStream vectored pass-through for plaintext | Optimization: detect plaintext MseStream and delegate to inner stream's native vectored read | Follow-up if MseStream is on hot path |
| Custom allocator for write buffer | Box<[u8; MAX_MSG_LEN]> is a single allocation; custom allocator would add complexity for zero gain | Never |
