# M13: End-Game Mode Design

**Status:** Implemented in v0.14.0

## Overview

End-game mode prevents download stalls near completion by duplicate-requesting
blocks from multiple peers. When all remaining pieces have at least one request
in-flight, the `EndGame` struct takes over request scheduling.

## Architecture

- **EndGame struct** (`crates/ferrite-session/src/end_game.rs`): encapsulates all
  end-game state and logic, testable in isolation
- **TorrentActor** gains `end_game: EndGame` field
- Activation is O(1): `have + in_flight >= num_pieces`
- In end-game, each peer gets a single already-requested block (no pipelining)
- Cancel messages sent to other peers when a block arrives
- Deactivation on hash failure or download completion

## Key Design Decisions

1. **Separate struct** — keeps end-game logic testable without driving TorrentActor
2. **Block-level tracking only in end-game** — normal mode uses simple `HashSet<u32>`
   for in-flight pieces
3. **Single block per peer** — matches libtorrent behavior, keeps redundancy low
4. **strict_end_game (default: true)** — only duplicate-request when every remaining
   piece already has at least one outstanding request
5. **Full deactivation on hash failure** — rather than per-piece removal, deactivate
   entirely and let normal mode re-request (re-enters end-game when appropriate)

## Data Flow

```
request_pieces_from_peer()
  ├─ end_game.is_active()? → request_end_game_block()
  │   ├─ pick_block / pick_block_strict
  │   ├─ register_request
  │   └─ send PeerCommand::Request
  └─ normal pick path
      └─ pick() returns None → check_end_game_activation()

handle_piece_data()
  └─ end_game.is_active()? → block_received()
      └─ returns cancel targets → send PeerCommand::Cancel

verify_and_mark_piece()
  ├─ success + complete → end_game.deactivate()
  └─ hash failure → end_game.deactivate()

peer disconnected → end_game.peer_disconnected()
```

## Test Coverage

12 unit tests in `end_game.rs` covering:
- Activation/deactivation lifecycle
- Block picking (assigned, unassigned, peer lacks piece)
- Strict mode with uncovered pieces
- Cancel target generation on block receipt
- Peer disconnect cleanup
- Piece removal
- Request registration (idempotent)
