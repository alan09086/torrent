# Performance Parity with rqbit Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Achieve download throughput parity with rqbit (~77 MB/s on Arch Linux ISO) — currently ~22 MB/s, a 3.5x gap.

**Architecture:** The bottleneck is CPU-bound copying on the hot path. rqbit completes in 18.7s/5.3s CPU; we take 69s/56s CPU — 10x more CPU per byte. The root cause is `Bytes::copy_from_slice` in the wire codec for every 16KB block, plus a double-copy on the encode path. Fix: zero-copy message parsing by passing `BytesMut` ownership through the codec, and direct-write encoding that skips intermediate `Bytes` allocation.

**Tech Stack:** Rust, bytes crate (`Bytes`/`BytesMut`), tokio-util `FramedRead`/`FramedWrite`, tokio mpsc channels

---

## Evidence

### Benchmark Data (2026-03-05, Arch Linux ISO 1.52 GB, NVMe)

| Client | Wall Time | Avg MB/s | User CPU | Sys CPU |
|--------|-----------|----------|----------|---------|
| rqbit 8.1.1 | 18.7s | 77.5 | 5.3s | 2.6s |
| torrent 0.65.0 | 69s | 22.0 | 56s | 7s |
| torrent (tuned) | 81s | 18.8 | 70s | 8s |

The 10x CPU gap (5.3s vs 56s user time) means we're CPU-bound, not I/O-bound.

### Root Causes Identified

**P0: `Bytes::copy_from_slice` in Piece decode (line 319 of message.rs)**
- Every 16KB block is memcpy'd from codec's `BytesMut` into a fresh `Bytes`
- 1.52 GB download = ~96,000 copies × 16KB = 1.52 GB of unnecessary memcpy
- The codec already owns the `BytesMut` via `split_to()` — should freeze a sub-slice

**P1: Double-copy on encode path (codec.rs:79-81)**
- `msg.to_bytes()` allocates a new `Bytes` with full message contents
- `dst.extend_from_slice(&bytes)` copies it again into the write buffer
- Should write header + payload directly into `dst`

**P2: `from_payload` takes `&[u8]` not `BytesMut` (message.rs:286)**
- The codec calls `split_to(length)` to get a `BytesMut`, then passes `&payload` (a slice reference)
- `from_payload` can't take ownership because it only borrows — forcing the copy
- Must change signature to take `BytesMut` for zero-copy Piece/Bitfield/Extended

**P3: Bencode strict parsing rejecting peer messages (FIXED in this session)**
- Extension handshakes with unsorted dict keys were rejected
- Already fixed via `from_bytes_lenient()` — no further work needed

**P4: CLI config loading broken (FIXED in this session)**
- `--config` JSON only read `listen_port`, ignoring disk_io_threads etc.
- Already fixed via `ClientBuilder::from_settings()` — no further work needed

---

## Task 1: Zero-Copy Piece Decode

**Files:**
- Modify: `crates/torrent-wire/src/message.rs:286-320` (from_payload signature + Piece/Bitfield/Extended arms)
- Modify: `crates/torrent-wire/src/codec.rs:70-72` (pass BytesMut not &[u8])
- Test: existing tests in `crates/torrent-wire/src/message.rs` + `codec.rs`

**Step 1: Change `from_payload` to take `BytesMut`**

In `crates/torrent-wire/src/message.rs`, change the signature and the three hot-path arms:

```rust
// OLD: pub fn from_payload(payload: &[u8]) -> Result<Self>
// NEW:
pub fn from_payload(payload: BytesMut) -> Result<Self> {
    if payload.is_empty() {
        return Ok(Message::KeepAlive);
    }

    let id = payload[0];
    // For messages that need the body as Bytes, freeze the tail
    // For messages that only read fixed fields, borrow the slice
    let body_offset = 1;

    match id {
        ID_CHOKE => Ok(Message::Choke),
        ID_UNCHOKE => Ok(Message::Unchoke),
        ID_INTERESTED => Ok(Message::Interested),
        ID_NOT_INTERESTED => Ok(Message::NotInterested),
        ID_HAVE => {
            ensure_len(&payload[body_offset..], 4, "Have")?;
            Ok(Message::Have {
                index: read_u32(&payload[body_offset..]),
            })
        }
        ID_BITFIELD => {
            // Zero-copy: freeze the BytesMut tail into Bytes
            let frozen = payload.freeze();
            Ok(Message::Bitfield(frozen.slice(body_offset..)))
        }
        ID_REQUEST => {
            let body = &payload[body_offset..];
            ensure_len(body, 12, "Request")?;
            Ok(Message::Request {
                index: read_u32(body),
                begin: read_u32(&body[4..]),
                length: read_u32(&body[8..]),
            })
        }
        ID_PIECE => {
            let body = &payload[body_offset..];
            ensure_len(body, 8, "Piece")?;
            let index = read_u32(body);
            let begin = read_u32(&body[4..]);
            // Zero-copy: freeze and slice off the 9-byte header (1 id + 4 index + 4 begin)
            let frozen = payload.freeze();
            let data = frozen.slice(body_offset + 8..);
            Ok(Message::Piece { index, begin, data })
        }
        ID_CANCEL => {
            let body = &payload[body_offset..];
            ensure_len(body, 12, "Cancel")?;
            Ok(Message::Cancel {
                index: read_u32(body),
                begin: read_u32(&body[4..]),
                length: read_u32(&body[8..]),
            })
        }
        ID_PORT => {
            let body = &payload[body_offset..];
            ensure_len(body, 2, "Port")?;
            Ok(Message::Port(u16::from_be_bytes([body[0], body[1]])))
        }
        ID_EXTENDED => {
            let body = &payload[body_offset..];
            ensure_len(body, 1, "Extended")?;
            let ext_id = body[0];
            // Zero-copy: freeze and slice
            let frozen = payload.freeze();
            let ext_payload = frozen.slice(body_offset + 1..);
            Ok(Message::Extended {
                ext_id,
                payload: ext_payload,
            })
        }
        // ... remaining arms use &payload[body_offset..] as before (small fixed-size messages)
```

Key insight: `BytesMut::freeze()` is O(1) — it converts the buffer to `Bytes` without copying. `Bytes::slice()` is also O(1) — it creates a view with a shared reference count.

**Step 2: Update codec decode to pass `BytesMut` ownership**

In `crates/torrent-wire/src/codec.rs:70-72`:

```rust
// OLD:
// let payload = src.split_to(length);
// Message::from_payload(&payload).map(Some)

// NEW:
let payload = src.split_to(length);
Message::from_payload(payload).map(Some)
```

This is a one-character change — remove the `&`.

**Step 3: Fix all callers of `from_payload`**

Search for `from_payload(` across the workspace. The main callers are:
- `codec.rs` (fixed above)
- Test code in `message.rs` and `peer.rs` — these pass `&[u8]` slices and need to wrap in `BytesMut::from(slice)`

For each test call site, change:
```rust
// OLD: Message::from_payload(&wire[4..])
// NEW: Message::from_payload(BytesMut::from(&wire[4..]))
```

**Step 4: Run tests**

```bash
cargo test -p torrent-wire -p torrent-session 2>&1 | grep "test result"
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add crates/torrent-wire/src/message.rs crates/torrent-wire/src/codec.rs
git commit -m "perf: zero-copy Piece/Bitfield/Extended decode in wire codec

Bytes::copy_from_slice replaced with BytesMut::freeze() + Bytes::slice().
Eliminates ~1.5GB of memcpy per 1.5GB download (every 16KB block was copied).

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Task 2: Direct-Write Encoding (Eliminate Double-Copy on Outbound)

**Files:**
- Modify: `crates/torrent-wire/src/codec.rs:79-82` (Encoder impl)
- Modify: `crates/torrent-wire/src/message.rs:150-282` (add `encode_into` method)
- Test: existing codec encode tests

**Step 1: Add `encode_into(&self, dst: &mut BytesMut)` to Message**

This writes the length-prefixed message directly into the destination buffer, avoiding the intermediate `Bytes` allocation:

```rust
/// Encode this message (with length prefix) directly into a buffer.
/// Avoids the intermediate allocation of `to_bytes()`.
pub fn encode_into(&self, dst: &mut BytesMut) {
    match self {
        Message::KeepAlive => {
            dst.put_u32(0);
        }
        Message::Choke => encode_fixed(dst, ID_CHOKE),
        Message::Unchoke => encode_fixed(dst, ID_UNCHOKE),
        Message::Interested => encode_fixed(dst, ID_INTERESTED),
        Message::NotInterested => encode_fixed(dst, ID_NOT_INTERESTED),
        Message::Have { index } => {
            dst.put_u32(5);
            dst.put_u8(ID_HAVE);
            dst.put_u32(*index);
        }
        Message::Bitfield(bits) => {
            dst.put_u32(1 + bits.len() as u32);
            dst.put_u8(ID_BITFIELD);
            dst.put_slice(bits);
        }
        Message::Request { index, begin, length } => {
            encode_triple(dst, ID_REQUEST, *index, *begin, *length);
        }
        Message::Piece { index, begin, data } => {
            dst.reserve(13 + data.len());
            dst.put_u32(9 + data.len() as u32);
            dst.put_u8(ID_PIECE);
            dst.put_u32(*index);
            dst.put_u32(*begin);
            dst.put_slice(data);
        }
        // ... same pattern for remaining variants
    }
}
```

Add helpers:

```rust
fn encode_fixed(dst: &mut BytesMut, id: u8) {
    dst.put_u32(1);
    dst.put_u8(id);
}

fn encode_triple(dst: &mut BytesMut, id: u8, a: u32, b: u32, c: u32) {
    dst.put_u32(13);
    dst.put_u8(id);
    dst.put_u32(a);
    dst.put_u32(b);
    dst.put_u32(c);
}
```

**Step 2: Update Encoder to use `encode_into`**

In `crates/torrent-wire/src/codec.rs`:

```rust
impl Encoder<Message> for MessageCodec {
    type Error = Error;

    fn encode(&mut self, msg: Message, dst: &mut BytesMut) -> Result<(), Error> {
        msg.encode_into(dst);
        Ok(())
    }
}
```

**Step 3: Run tests**

```bash
cargo test -p torrent-wire 2>&1 | grep "test result"
```

**Step 4: Commit**

```bash
git add crates/torrent-wire/src/message.rs crates/torrent-wire/src/codec.rs
git commit -m "perf: direct-write encoding eliminates double-copy on outbound

Message::encode_into() writes length-prefixed message directly into the
FramedWrite buffer, removing the intermediate Bytes allocation from to_bytes().

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Task 3: Tune Default Settings for Modern Hardware

**Files:**
- Modify: `crates/torrent-session/src/settings.rs` (default functions)

**Step 1: Update defaults**

The current defaults (4 I/O threads, 2 hash threads) are conservative for 2026 hardware.
Auto-detect based on available cores:

```rust
fn default_disk_io_threads() -> usize {
    // Use half of available cores, minimum 4, maximum 16
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 2).clamp(4, 16)
}

fn default_hashing_threads() -> usize {
    // Use quarter of available cores, minimum 2, maximum 8
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 4).clamp(2, 8)
}
```

On the user's 12-core machine: 6 I/O threads, 3 hash threads (up from 4 and 2).

**Step 2: Fix tests that assert exact defaults**

Tests that assert `disk_io_threads == 4` or `hashing_threads == 2` need to match the
new computation or assert a range. Search:

```bash
grep -rn "disk_io_threads.*4\|hashing_threads.*2" crates/torrent-session/src/settings.rs
```

Update test assertions to use the same formula or accept the computed value.

**Step 3: Run tests**

```bash
cargo test -p torrent-session 2>&1 | grep "test result"
```

**Step 4: Commit**

```bash
git add crates/torrent-session/src/settings.rs
git commit -m "perf: auto-detect disk I/O and hashing thread counts from CPU cores

Defaults now scale with available_parallelism(): io_threads = cores/2 (4-16),
hashing_threads = cores/4 (2-8). Previous fixed defaults (4/2) left NVMe
and multi-core systems underutilized.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Task 4: Benchmark and Verify

**Files:**
- None (benchmarking only)

**Step 1: Rebuild release binary**

```bash
cargo build --release -p torrent-cli
```

**Step 2: Run comparative benchmark**

```bash
# Clear caches and download with our client
rm -rf /mnt/TempNVME/bench/torrent_test && mkdir -p /mnt/TempNVME/bench/torrent_test
sync && echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
time target/release/torrent download "magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso" -o /mnt/TempNVME/bench/torrent_test -q -p 6881

# Compare with rqbit
rm -rf /mnt/TempNVME/bench/rqbit_test && mkdir -p /mnt/TempNVME/bench/rqbit_test
sync && echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
time rqbit -v warn download -o /mnt/TempNVME/bench/rqbit_test -e "magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso"
```

**Target:** Wall time within 2x of rqbit. User CPU within 3x.

**Step 3: If target not met, profile with `perf`**

```bash
perf record -g target/release/torrent download <magnet> -o /tmp/perf_test -q -p 6881
perf report --hierarchy
```

Look for remaining hot spots: SHA-1 hashing, channel contention, lock contention.

**Step 4: Run full test suite**

```bash
cargo test --workspace 2>&1 | grep "test result"
cargo clippy --workspace -- -D warnings
```

---

## Task 5: Commit Lenient Bencode + CLI Config Fixes

The lenient bencode parser and CLI config fix were done during benchmarking analysis.
They need to be committed.

**Step 1: Commit lenient bencode**

```bash
git add crates/torrent-bencode/src/de.rs crates/torrent-bencode/src/lib.rs \
    crates/torrent-wire/src/extended.rs crates/torrent-session/src/pex.rs \
    crates/torrent-session/src/lt_trackers.rs
git commit -m "fix: lenient bencode parsing for peer wire messages

Many real-world clients send extension handshakes with unsorted dictionary
keys. Add from_bytes_lenient() that accepts unsorted keys, use it in
ExtHandshake, MetadataMessage, PEX, and lt_trackers parsing.
Strict mode retained for .torrent files and info hash computation.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

**Step 2: Commit CLI config fix + version string**

```bash
git add crates/torrent/src/client.rs crates/torrent-cli/src/download.rs \
    crates/torrent-wire/src/extended.rs
git commit -m "fix: CLI --config now applies all settings, update version string

ClientBuilder::from_settings() passes the full Settings through instead
of only listen_port. ExtHandshake version updated to Torrent 0.65.0.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Summary

| Task | Impact | Effort |
|------|--------|--------|
| T1: Zero-copy decode | ~1.5GB memcpy eliminated per download | Medium — signature change ripples to tests |
| T2: Direct-write encode | Eliminates double-copy on upload | Medium — new method + encoder rewrite |
| T3: Auto-detect threads | Better utilization on modern hardware | Small — default function changes |
| T4: Benchmark | Validate results | Small |
| T5: Commit existing fixes | Bencode + CLI fixes already done | Small — git only |

**Expected outcome:** Tasks 1-2 should cut CPU time by 5-8x (eliminating the dominant memcpy). Task 3 improves disk throughput on multi-core systems. Combined, we should reach 50-80 MB/s, achieving parity with rqbit.
