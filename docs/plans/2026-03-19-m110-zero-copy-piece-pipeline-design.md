# M110: Zero-Copy Piece Pipeline

## Context

After M109 (v0.109.0, 1631 tests), benchmarks show 42K page faults (target <10K,
rqbit achieves 7K). M109's ring buffer eliminated allocations for non-data messages
via inline decode, but Piece messages still allocate a Vec per block via
`consume_as_bytes` — 90K allocations per 1.4 GB download.

**M109 benchmark results (optimized, good trials):**

| Metric | M109 | rqbit | Target |
|--------|------|-------|--------|
| Speed | 60 MB/s | 74.6 MB/s | ≥65 |
| Page faults | 42K | 7K | <10K |
| Heap allocs | ~90K | ~4K | <5K |
| RSS | 55 MiB | — | <50 |

**Root cause**: `PeerReader::decode_from_ring()` calls `consume_as_bytes(n)` for every
Piece message, allocating a `Vec<u8>` and copying 16 KiB from the ring buffer into it.
rqbit avoids this entirely: `Message<'a>` borrows `&[u8]` slices directly from the ring
buffer, and `pwrite_all_vectored()` writes those slices to disk without any intermediate
allocation.

## Architecture

### rqbit's Zero-Copy Pipeline

```
socket → ReadBuf (32 KiB ring) → Message<'a> (borrowed &[u8]) → pwrite_vectored → disk
                                  ↑ zero alloc                    ↑ direct from ring
```

Key elements:
- `Message<'a>` with `ByteBuf<'a>(&'a [u8])` — borrowed from ring
- `Piece<B>` has two buffer fields (`block_0`, `block_1`) for ring-wrap
- `write_chunk()` uses `[IoSlice; 2]` for vectored pwrite directly from ring slices
- Synchronous pwrite from peer task — page cache makes it fast (~100µs)
- No channel, no background writer, no allocation for piece data

### Our Current Pipeline (Post-M109)

```
socket → ReadBuf → alloc Vec → copy from ring → Bytes → clone → mpsc → bg writer → pwrite
                   ↑ 90K allocs/download         ↑ refcount
```

### Proposed Pipeline (M110)

```
socket → ReadBuf → Message<&[u8]> (borrowed) → pwrite_vectored → disk
                   ↑ zero alloc                 ↑ direct from ring
```

## Design

### Change 1: `Message<B = Bytes>` — Generic Buffer Type

**File**: `crates/torrent-wire/src/message.rs`

Make the Message enum generic over the buffer type, with `Bytes` as default
for backward compatibility:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message<B = Bytes> {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have { index: u32 },
    Bitfield(B),
    Request { index: u32, begin: u32, length: u32 },
    Piece { index: u32, begin: u32, data_0: B, data_1: B },
    Cancel { index: u32, begin: u32, length: u32 },
    Port(u16),
    Extended { ext_id: u8, payload: B },
    SuggestPiece(u32),
    HaveAll,
    HaveNone,
    RejectRequest { index: u32, begin: u32, length: u32 },
    AllowedFast(u32),
    HashRequest { pieces_root: Id32, base: u32, index: u32, count: u32, proof_layers: u32 },
    Hashes { pieces_root: Id32, base: u32, index: u32, count: u32, proof_layers: u32, hashes: Vec<Id32> },
    HashReject { pieces_root: Id32, base: u32, index: u32, count: u32, proof_layers: u32 },
}
```

- **Default `B = Bytes`**: Existing code using `Message` unchanged (resolves to `Message<Bytes>`)
- **`Piece` two fields**: `data_0` and `data_1` handle ring-wrap; `data_1` is empty for contiguous data
- **`Hashes`**: Keeps `Vec<Id32>` — not generic (rare message, always allocates)
- **Trait bounds**: `encode_into` and `to_bytes` require `B: AsRef<[u8]>`

#### Piece Field Rename

The current `Message::Piece { index, begin, data }` becomes `{ index, begin, data_0, data_1 }`.
All pattern matches must be updated. In places that previously used `data`, the two halves
can be concatenated or handled via vectored I/O depending on the consumer.

#### from_payload

`from_payload(Bytes)` returns `Message<Bytes>` as before. For the Piece variant,
`data_0` gets the piece data and `data_1` is `Bytes::new()` (empty). This maintains
full backward compatibility for the codec path and tests.

#### encode_into

```rust
impl<B: AsRef<[u8]>> Message<B> {
    pub fn encode_into(&self, dst: &mut BytesMut) {
        // For Piece: write data_0 then data_1
        // For others: unchanged
    }

    pub fn to_bytes(&self) -> Bytes { ... }
}
```

### Change 2: PeerReader Returns `Message<&[u8]>`

**File**: `crates/torrent-session/src/peer_codec.rs`

#### ReadBuf: New Method

```rust
impl ReadBuf {
    /// Return two borrowed slices covering `len` bytes starting at `offset`
    /// from the read cursor. Handles ring wrap by splitting into two slices.
    /// Does NOT consume — caller must call `consume()` separately.
    pub(crate) fn readable_slices_at(&self, offset: usize, len: usize) -> (&[u8], &[u8]) {
        let start = (self.start + offset) % BUF_LEN;
        let end = start + len;
        if end <= BUF_LEN {
            (&self.buf[start..end], &[])
        } else {
            (&self.buf[start..], &self.buf[..end - BUF_LEN])
        }
    }
}
```

#### next_message Signature

```rust
pub(crate) async fn next_message(&mut self) -> Result<Option<Message<&'_ [u8]>>, Error>
```

The lifetime `'_` ties to `&mut self`, meaning the returned message borrows the
ring buffer. The caller must drop the message before calling `next_message()` again.
This is naturally enforced by the select loop structure.

#### decode_from_ring Changes

Non-data messages (Choke, Have, Request, etc.) — unchanged from M109 inline decode.
No allocation, no buffer type involved.

Data-carrying messages change from allocating to borrowing:

```rust
// Piece: borrow two slices from ring
MSG_PIECE => {
    ensure_msg_len(length, 9)?;
    let index = self.buf.peek_u32_be_at(5);
    let begin = self.buf.peek_u32_be_at(9);
    let (s0, s1) = self.buf.readable_slices_at(13, length - 9);
    self.buf.consume(total);
    Ok(Some(Message::Piece { index, begin, data_0: s0, data_1: s1 }))
}

// Bitfield: borrow single slice
MSG_BITFIELD => {
    let (s0, s1) = self.buf.readable_slices_at(5, length - 1);
    self.buf.consume(total);
    // If wraps, concatenate into owned (rare, once per peer)
    if s1.is_empty() {
        Ok(Some(Message::Bitfield(s0)))
    } else {
        let mut v = Vec::with_capacity(length - 1);
        v.extend_from_slice(s0);
        v.extend_from_slice(s1);
        // Bitfield wrapping is very rare — acceptable allocation
        Ok(Some(Message::Bitfield(&[]))) // TODO: handle via Cow or leak
    }
}

// Extended: borrow payload
MSG_EXTENDED => {
    ensure_msg_len(length, 2)?;
    let ext_id = self.buf.peek_byte_at(5);
    let (s0, s1) = self.buf.readable_slices_at(6, length - 2);
    self.buf.consume(total);
    // Extended payloads that wrap need concatenation (rare)
    // ... similar handling
}
```

**Wrap handling for Bitfield/Extended**: When the payload wraps the ring boundary,
the data spans two non-contiguous slices. For Piece, this is handled naturally by
`data_0`/`data_1` and vectored pwrite. For Bitfield and Extended (which need
contiguous data for parsing), a copy is needed when wrapping occurs. This is rare
(depends on ring position) and these messages are infrequent (once per peer for
Bitfield, occasional for Extended).

#### Oversized Fallback

Messages > 32 KiB (rare metadata) still allocate a Vec. The Vec is stored as a
field on PeerReader and reused across oversized messages. The returned
`Message<&[u8]>` borrows from this field.

```rust
pub(crate) struct PeerReader<R> {
    reader: R,
    buf: ReadBuf,
    max_message_size: usize,
    oversized_buf: Vec<u8>,  // reused for >32K messages
}
```

#### Lifetime Safety

The borrow checker enforces correctness:

1. `next_message(&mut self)` returns `Message<&'_ [u8]>` borrowing from `self`
2. While `msg` exists, `self` (PeerReader) is borrowed — no further mutations
3. When `msg` is dropped (end of select loop body), PeerReader is released
4. Next iteration calls `next_message()` again — safe because msg is gone

In the `tokio::select!` loop, only one arm fires per iteration. The reader arm
produces `msg`, processes it (including pwrite), drops `msg`, then loops. Other
arms (cmd_rx, semaphore, flush_interval) don't touch the PeerReader.

### Change 3: Direct Vectored Pwrite

**Files**: `crates/torrent-session/src/peer.rs`, `crates/torrent-session/src/disk.rs`,
`crates/torrent-session/src/disk_backend.rs`

#### DiskIoBackend Trait

Add a synchronous vectored write method:

```rust
pub trait DiskIoBackend: Send + Sync + 'static {
    // Existing async methods...

    /// Write piece block data directly via vectored pwrite.
    /// The two slices handle ring buffer wrap — s1 is empty for contiguous data.
    fn write_block_vectored(
        &self,
        index: u32,
        begin: u32,
        s0: &[u8],
        s1: &[u8],
    ) -> io::Result<()>;
}
```

#### PosixDiskIo Implementation

```rust
fn write_block_vectored(&self, index: u32, begin: u32, s0: &[u8], s1: &[u8]) -> io::Result<()> {
    let absolute_offset = self.lengths.chunk_absolute_offset(index, begin);
    // Map to file(s) via FileMap
    // pwrite_all_vectored with [IoSlice::new(s0), IoSlice::new(s1)]
}
```

Uses `pwrite` (or `pwritev` for vectored) directly. Writes go to the OS page
cache — typically <100µs, proven at 74 MB/s by rqbit.

#### DiskHandle

Add a synchronous wrapper:

```rust
impl DiskHandle {
    pub fn write_block_vectored(&self, index: u32, begin: u32, s0: &[u8], s1: &[u8]) -> io::Result<()> {
        self.backend.write_block_vectored(index, begin, s0, s1)
    }
}
```

#### peer.rs Integration

Replace the deferred write with direct pwrite:

```rust
Message::Piece { index, begin, data_0, data_1 } => {
    // Block tracking (M103, unchanged)
    if let Some(ref bm) = peer_block_maps {
        let block_idx = *begin / block_length;
        bm.mark_received(*index, block_idx);
    }

    // M110: Direct pwrite from ring buffer slices
    if let Some((_, _, Some(ref disk), _)) = reservation_state {
        if let Err(e) = disk.write_block_vectored(*index, *begin, data_0, data_1) {
            // Handle write error — send via write_error_tx (existing path)
        }
    }

    // Batch tracking (M92, unchanged — metadata only, no data ownership)
}
```

#### What Gets Removed

- `disk.write_block_deferred(*index, *begin, data.clone())` call in peer.rs
- The `data.clone()` (Bytes refcount bump) — no longer needed
- The `consume_as_bytes()` allocation for Piece data in peer_codec.rs

The deferred write queue infrastructure in disk.rs may still be used for other
write types. If Piece was its only consumer, it can be removed in a follow-up.

### Change 4: handle_message Signature

**File**: `crates/torrent-session/src/peer.rs`

```rust
async fn handle_message<B: AsRef<[u8]>>(
    msg: Message<B>,
    // ... rest unchanged
) -> crate::Result<Option<Message>>
```

Or more specifically, since it's only called from the borrowed path:

```rust
async fn handle_message(
    msg: Message<&[u8]>,
    // ... rest unchanged
) -> crate::Result<Option<Message>>
```

Inside handle_message:
- **Bitfield**: `Message::Bitfield(data)` where data is `&[u8]`.
  `Bitfield::from_bytes(data.to_vec(), ...)` — already copies, works unchanged.
- **Extended**: `Message::Extended { ext_id, payload }` where payload is `&[u8]`.
  Extension parsers already accept `&[u8]`. ExtHandshake::from_bytes takes `&[u8]`.
- **Piece**: Never reaches handle_message (skipped via `continue`). Dead code path
  for Piece can be removed or left with unreachable!().
- **Return type**: Still `Option<Message>` (= `Message<Bytes>`) for sending responses.
  Response messages are constructed from owned data (PeerCommand), not borrowed.

### Change 5: Codec Backward Compatibility

**File**: `crates/torrent-wire/src/codec.rs`

The `MessageCodec` Decoder impl explicitly specifies `Message<Bytes>`:

```rust
impl Decoder for MessageCodec {
    type Item = Message<Bytes>;  // explicit (was implicit via default)
    type Error = Error;
    // ... decode body unchanged
}
```

This keeps the codec tests and any code using `FramedRead` working.

## Files Changed

| File | Changes | Est. Lines |
|------|---------|------------|
| `torrent-wire/src/message.rs` | `Message<B>`, Piece two fields, trait bounds | ~+40/-20 |
| `torrent-wire/src/codec.rs` | Explicit `Message<Bytes>` in Decoder | ~+2/-1 |
| `torrent-session/src/peer_codec.rs` | Borrowed decode, `readable_slices_at`, oversized_buf | ~+30/-20 |
| `torrent-session/src/peer.rs` | Direct pwrite, handle_message signature, remove deferred | ~+20/-15 |
| `torrent-session/src/disk.rs` | `write_block_vectored` on DiskHandle | ~+10 |
| `torrent-session/src/disk_backend.rs` | Trait method + PosixDiskIo impl | ~+30 |
| Tests across both crates | Update Piece pattern matches (data → data_0/data_1) | ~+20/-20 |

## Tests

### New Tests
1. `message_piece_two_fields_round_trip` — encode/decode Piece with data_0 only
2. `message_piece_split_data_round_trip` — encode/decode Piece with data_0 + data_1
3. `message_generic_encode_borrowed` — encode_into works with Message<&[u8]>
4. `reader_decode_piece_borrowed` — PeerReader returns borrowed slices
5. `reader_decode_piece_wrapped_borrowed` — Piece data spanning ring wrap → data_0/data_1
6. `reader_decode_bitfield_borrowed` — Bitfield as borrowed slice
7. `reader_decode_extended_borrowed` — Extended as borrowed slice
8. `write_block_vectored_contiguous` — pwrite with empty data_1
9. `write_block_vectored_split` — pwrite with both slices populated
10. `write_block_vectored_file_boundary` — block spanning two files

### Existing Tests
All 1631 tests must continue passing. `Message` (= `Message<Bytes>`) is unchanged
via default type parameter. Piece pattern matches need `data` → `data_0` rename
(data_1 ignored or checked empty).

## Success Criteria

- Page faults <15K (stretch: <10K)
- Heap allocs <5K per download
- Speed ≥65 MB/s
- RSS <50 MiB
- 1641+ tests (1631 + 10 new)
- 10/10 benchmark reliability

## NOT in Scope

| Item | Why | Revisit |
|------|-----|---------|
| Buffer pool / slab allocator | Direct pwrite eliminates need | If pwrite latency is an issue |
| Memory-mapped ring buffer | Linux-specific, complex | If faults still >10K |
| Removing deferred write queue entirely | May be used by other write paths | Follow-up cleanup |
| `CloneToOwned` trait (rqbit) | Not needed — we convert at boundary | If generic conversion needed |
| Write coalescing | Removed in M100, pwrite replaces | N/A |
