# M9: Seeding & Upload Pipeline — Design Document

**Date:** 2026-02-25
**Status:** Complete
**Tests:** 342 total (102 in ferrite-session, +5 new)

## Goal

Add the upload/seeding pipeline so ferrite can serve pieces to peers,
transition to seeding state on completion, track upload rates, and
enforce seed ratio limits.

## Architecture

All changes are in the `ferrite-session` crate. The core design adds:

1. **PeerCommand::SendPiece** — wire-level command to push piece data to a peer
2. **serve_incoming_requests()** — reads from storage, dispatches SendPiece to unchoked peers
3. **TorrentState::Seeding** — new state after download completion
4. **Seed-mode choker** — ranks peers by upload rate instead of download rate
5. **Rolling window rates** — 10s window for meaningful rate calculation
6. **Seed ratio limits** — configurable `uploaded/downloaded >= limit` → stop
7. **Resume support** — verify existing pieces on startup, seed immediately if complete

## Key Design Decisions

### Seeding vs Complete state

The `Complete` state is retained but no longer the terminal download state.
Download completion transitions directly to `Seeding`. `Complete` remains
available for future use (e.g., post-processing hooks before seeding).
Resume also restores `Seeding` when all pieces are verified.

### Dual-mode choker

The choker supports two ranking modes via a `seed_mode` flag:
- **Leech mode** (default): rank by download rate (tit-for-tat)
- **Seed mode**: rank by upload rate (maximize distribution)

The flag is set when transitioning to Seeding and cleared on resume
to Downloading.

### Rolling window vs cumulative rates

The previous cumulative `download_rate` counter was replaced with a proper
rolling window. Bytes are accumulated in `download_bytes_window` and
`upload_bytes_window` fields on PeerState. Every 10 seconds (unchoke
interval), `update_peer_rates()` converts windows to bytes/sec and resets.

### Seed ratio check placement

`check_seed_ratio()` is called at the end of `serve_incoming_requests()`
rather than on a timer. This gives immediate response when the ratio is
reached, avoiding up to 10 seconds of unnecessary uploading.

### Resume verification

`verify_existing_pieces()` runs synchronously at the start of `run()`,
before any intervals or network activity. For large torrents with
filesystem storage, this could be slow — but it's correct and simple.
Async/incremental verification can be added in a future milestone if needed.

## Files Changed

| File | Changes |
|------|---------|
| `ferrite-session/src/types.rs` | `SendPiece` variant, `Seeding` state, `seed_ratio_limit` on configs |
| `ferrite-session/src/peer.rs` | Handle `SendPiece` → `Message::Piece` |
| `ferrite-session/src/peer_state.rs` | `upload_bytes_window`, `download_bytes_window` fields |
| `ferrite-session/src/choker.rs` | `upload_rate` in PeerInfo, `seed_mode` flag, dual-mode sorting |
| `ferrite-session/src/torrent.rs` | `serve_incoming_requests()`, `verify_existing_pieces()`, `update_peer_rates()`, `check_seed_ratio()`, state transitions |
| `ferrite-session/src/session.rs` | `seed_ratio_limit` passthrough in `make_torrent_config()` |

## Existing APIs Reused (no changes needed)

- `TorrentStorage::read_chunk(piece, begin, length)` — read piece data for upload
- `TorrentStorage::verify_piece(piece, expected)` — hash verification on resume
- `ChunkTracker::has_piece(piece)` — check if we can serve a request
- `ChunkTracker::mark_verified(piece)` — mark piece as verified on resume
- `Message::Piece { index, begin, data }` — wire message for piece data

## Deferred

- BEP 16 (superseeding) → M11+
- BEP 19 (webseed) → M11+
- Queue management / file relocation → independent feature
- Async/incremental piece verification → future optimization
