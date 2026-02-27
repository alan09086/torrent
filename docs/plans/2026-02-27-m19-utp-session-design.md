# M19: uTP Session Integration — Design Document

## Goal

Integrate the standalone `ferrite-utp` crate (M18) into `ferrite-session` so peers
can connect over uTP in addition to TCP. Outbound connections try uTP first with
TCP fallback. Inbound uTP connections are routed through the session to the correct
torrent.

## Architecture

```
SessionActor
├── UtpSocket (owned) ──→ cloned handle to each TorrentActor
├── UtpListener (owned) ──→ accept loop → identify info_hash → route
│
├── TorrentActor #1
│   ├── TcpListener (own)
│   ├── UtpSocket (cloned) ──→ try_connect_peers() uTP path
│   └── IncomingPeer cmd ←── from session's uTP accept loop
│
└── TorrentActor #2
    ├── TcpListener (own)
    ├── UtpSocket (cloned)
    └── IncomingPeer cmd ←──
```

## Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| UtpSocket ownership | Shared at session level | UDP uses connection IDs for multiplexing; one socket serves all torrents |
| Outbound strategy | Try uTP first (5s timeout), fall back to TCP | uTP is friendlier (LEDBAT), matches libtorrent behavior |
| Inbound routing | Session reads 48-byte BT preamble → extract info_hash → route | Must identify which torrent before dispatching |
| MSE + uTP inbound | Deferred (drop encrypted inbound uTP with debug log) | MSE multi-skey lookup is complex; plaintext inbound covers most cases |
| Pre-read bytes | `PrefixedStream<S>` wrapper | Replays consumed preamble bytes to `run_peer()` |

## Implementation

### Config Fields
- `TorrentConfig.enable_utp: bool` (default: `true`)
- `SessionConfig.enable_utp: bool` (default: `true`)

### PrefixedStream<S>
Generic stream wrapper implementing `AsyncRead + AsyncWrite + Unpin + Send`.
Serves prefix bytes (the consumed preamble) first, then delegates to inner stream.
Used to replay the 48-byte BT handshake preamble back to `run_peer()`.

### Outbound (TorrentActor)
In `try_connect_peers()`, if `enable_utp` and `utp_socket` is `Some`:
1. Try `socket.connect(addr)` with 5-second timeout
2. On success → `run_peer()` with uTP stream
3. On failure/timeout → fall back to TCP `TcpStream::connect(addr)`

### Inbound (SessionActor)
1. `accept_utp()` helper returns `pending` if no listener, matching `accept_incoming` pattern
2. `handle_utp_inbound()` spawns a routing task:
   - Calls `identify_plaintext_connection(stream)`
   - On plaintext: extracts info_hash, finds matching torrent, sends `TorrentCommand::IncomingPeer`
   - On encrypted (MSE): drops with debug log (deferred)
   - On error: drops with debug log

### Lifecycle
- `SessionHandle::start()`: binds UtpSocket if `enable_utp`
- `shutdown_all()`: calls `utp_socket.shutdown()` after torrent shutdown

## Deferred Work
- MSE-encrypted inbound uTP routing (requires `negotiate_inbound_multi()` with multi-skey lookup)
- uTP hole punching for NAT traversal (M20)
