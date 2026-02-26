# M14: Bandwidth Limiter + Upload Slot Tuning — Design Document

**Version:** 0.15.0
**Date:** 2026-02-26
**Status:** Implemented

## Overview

M14 adds token-bucket rate limiting (global + per-torrent, upload + download) and automatic upload slot optimization to ferrite.

## Architecture

### Rate Limiting Approach: Actor-Level Gating

Rather than wrapping sockets, rate limiting is applied at the actor level:

- **Download throttling**: `request_pieces_from_peer()` checks the download bucket before sending Request messages. When budget is exhausted, no new requests are pipelined until tokens refill.
- **Upload throttling**: `serve_incoming_requests()` checks the upload bucket before dispatching each chunk. When budget is exhausted, remaining requests wait for the next refill cycle.
- **Global limiter**: `Arc<Mutex<TokenBucket>>` shared across all TorrentActors, consulted before per-torrent buckets.
- **Local network exemption**: Peers on RFC 1918 / loopback / link-local addresses skip the global limiter.

### TokenBucket (`rate_limiter.rs`)

- `rate` = bytes/sec (0 = unlimited)
- `capacity` = `rate` (1-second burst)
- `refill(elapsed)` adds tokens proportional to time, capped at capacity
- `try_consume(amount)` atomically checks and deducts tokens
- Refilled every 100ms via a dedicated timer arm in the select! loop

### SlotTuner (`slot_tuner.rs`)

Hill-climbing optimizer for unchoke slot count:

1. Every unchoke interval (10s), `observe(upload_bytes)` is called
2. If throughput improved from previous interval → continue direction (increase/decrease)
3. If throughput decreased → reverse direction
4. Clamp to `[min_slots, max_slots]`
5. Disabled mode returns a fixed slot count

### Wiring

- **TorrentActor** owns per-torrent `upload_bucket`, `download_bucket`, `slot_tuner`, and references to optional global buckets
- **SessionActor** owns the global buckets, creates them from `SessionConfig`, passes `Arc` clones to each torrent
- Global buckets are only shared when `rate_limit > 0` (avoids mutex overhead when unlimited)

## Design Decisions

1. **No peer class system** — full peer classes (M32) are premature without uTP. Simple `is_local_network()` check suffices.
2. **No socket-level wrapping** — request-queue throttling is simpler and matches libtorrent's approach.
3. **100ms refill granularity** — balances smoothness vs overhead.
4. **0 means unlimited** — all config defaults to 0, matching libtorrent convention.
5. **`pub(crate)` constructors** — `from_torrent`/`from_magnet` take private types (`TokenBucket`, `SlotTuner`), so they're crate-internal.

## Files Changed

| File | Change |
|------|--------|
| `types.rs` | Added `upload_rate_limit`, `download_rate_limit` to both configs; `auto_upload_slots*` to SessionConfig |
| `rate_limiter.rs` | **New** — `TokenBucket`, `is_local_network()` |
| `slot_tuner.rs` | **New** — `SlotTuner` hill-climbing optimizer |
| `choker.rs` | Added `set_unchoke_slots()` |
| `torrent.rs` | Per-torrent buckets, refill timer, gated upload/download, slot tuner wiring |
| `session.rs` | Global buckets, refill timer, bucket/tuner propagation to torrents |
| `lib.rs` | Module declarations |

## Test Coverage

| Module | Tests | Coverage |
|--------|-------|----------|
| `rate_limiter` | 7 | unlimited, limited, refill, cap, partial, set_rate, is_local_network |
| `slot_tuner` | 6 | initial, increase, decrease, bounds, disabled, stagnant |
| `choker` | 1 | set_unchoke_slots |
| `types` | 2 | TorrentConfig + SessionConfig bandwidth defaults |
| `torrent` (integration) | 3 | upload cap, download throttle, unlimited no-op |
