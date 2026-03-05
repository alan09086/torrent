# Architecture

Torrent is organized as a 12-crate Rust workspace. Each crate handles one layer of the BitTorrent stack, with dependencies flowing downward.

## Crate Dependency Graph

```
torrent-bencode          Serde bencode codec (leaf, no deps)
     │
torrent-core             Hashes, metainfo (v1+v2), magnets, piece arithmetic
     │
     ├──▶ torrent-wire       Peer wire protocol, handshake, MSE/PE encryption
     ├──▶ torrent-tracker    HTTP + UDP tracker announce/scrape
     ├──▶ torrent-dht        Kademlia DHT actor (BEP 5/24/42/44/51)
     │
torrent-storage          Piece verification, chunk tracking, mmap, ARC cache
     │
torrent-session          Session manager, torrent orchestration, disk I/O actor
     │
torrent-utp              uTP (BEP 29) micro transport protocol
     │
torrent-nat              PCP / NAT-PMP / UPnP automatic port mapping
     │
torrent                  Public facade: ClientBuilder + prelude + unified error
     │
torrent-sim              In-process network simulation (testing only)
```

## Actor Model

The session is built on tokio's async runtime using an actor model:

- **SessionActor** — Top-level coordinator. Manages torrent lifecycle, DHT, NAT, alert broadcasting. Receives commands via `mpsc` channel from `SessionHandle`.
- **TorrentActor** — One per torrent. Manages peer connections, piece picking, piece verification, tracker announces. Runs a `tokio::select!` loop over command channel, peer events, and timers.
- **DiskActor** — Shared across all torrents. Handles read/write/verify jobs via `spawn_blocking`. Uses an ARC (Adaptive Replacement Cache) for read caching and a write buffer for coalescing small writes.
- **DhtActor** — One per address family (IPv4, IPv6). Manages Kademlia routing table, query pipeline, and KRPC message dispatch.

Communication between actors uses `tokio::sync::mpsc` channels. The `TorrentActor` fires disk jobs into the `DiskActor` channel and receives results via dedicated response channels (non-blocking write errors, async verification results).

## Non-Blocking Transfer Pipeline

As of v0.65.0, the disk write path is non-blocking:

1. Wire codec delivers a `Piece` message as `Bytes` (zero-copy from network buffer)
2. `handle_piece_data()` calls `disk.enqueue_write()` which uses `try_send()` — if the disk channel is full, the write falls back to a blocking `.await`
3. Piece verification is dispatched asynchronously: the disk actor flushes the piece, hashes it, and sends the result back via a `verify_result_tx` channel
4. The TorrentActor's `select!` loop picks up verification results and either marks the piece as complete or triggers re-download

V2 (SHA-256 Merkle) and hybrid verification remain synchronous because they require mutable access to the `HashPicker` state machine.

## Crash Containment

Panics in `spawn_blocking` tasks (disk I/O, hashing) are caught by tokio's `JoinHandle` and surfaced as errors rather than crashing the entire session. Each torrent actor runs in its own `tokio::spawn` task, so a single torrent failure doesn't bring down the session.

## Pluggable Interfaces

- **DiskIoBackend** — trait for custom storage (POSIX, mmap, disabled/benchmark, or user-provided)
- **NetworkFactory** — trait for custom transport (real TCP/uTP, or SimNetwork for testing)
- **ExtensionPlugin** — trait for custom BEP 10 extension messages
- **ChokerStrategy** — trait for custom choking algorithms (FixedSlots, RateBased, or user-provided)
