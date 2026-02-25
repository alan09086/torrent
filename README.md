# Ferrite

A from-scratch Rust BitTorrent library targeting full libtorrent-rasterbar parity.

Ferrite is a modular workspace of focused crates, each handling one layer of the BitTorrent stack. The goal is a clean, well-tested engine that can replace librqbit as the backend for `rqbit-slint` and other applications.

## Architecture

```
ferrite-bencode      Serde bencode codec (leaf, no deps)
     |
ferrite-core         Hashes, metainfo, magnets, piece arithmetic
     |
     +---> ferrite-wire       Peer wire protocol, handshake, extensions
     +---> ferrite-tracker    HTTP + UDP tracker announce/scrape
     +---> ferrite-dht        Kademlia DHT (BEP 5, actor model)
     |
ferrite-storage      Piece verification, chunk tracking, disk I/O
     |
ferrite-session      Peer management, torrent orchestration
     |
ferrite              (planned) Public facade API
```

## Crates

| Crate | Description | Tests |
|-------|-------------|-------|
| `ferrite-bencode` | Serde-based bencode serialization | 66 |
| `ferrite-core` | Id20/Id32, TorrentMetaV1, Magnet, Lengths, PeerId | 42 |
| `ferrite-wire` | Handshake, Message codec, BEP 6/9/10 extensions | 35 |
| `ferrite-tracker` | HTTP (reqwest) + UDP (BEP 15) tracker client | 14 |
| `ferrite-dht` | Kademlia DHT with actor model, KRPC, routing table | 42 |
| `ferrite-storage` | Bitfield, FileMap, ChunkTracker, TorrentStorage trait | 41 |
| `ferrite-session` | Session manager, peer tasks, torrent actor, BEP 6/14, seeding | 102 |

**Total: 342 tests, zero clippy warnings.**

## Design Decisions

- **Modular crates** — each layer is independently testable and reusable
- **`thiserror` typed errors** per crate, no `anyhow`
- **Rust edition 2024** with workspace resolver 2
- **`bytes::Bytes`** for zero-copy buffer sharing across the wire protocol
- **Actor model** for DHT — single-owner event loop with cloneable handle, no `DashMap`
- **Sync I/O for storage** — no async filesystem overhead, enables future `pwritev` optimization
- **Wire-compatible coordinates** — `(piece, begin, length)` used throughout, matching BEP 3 directly
- **Binary search file lookup** — O(log n) piece-to-file mapping vs linear scan

## Building

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Requires Rust edition 2024 support (rustc 1.85+).

## Roadmap

See [docs/plans/2026-02-25-ferrite-roadmap.md](docs/plans/2026-02-25-ferrite-roadmap.md) for detailed milestone plans.

| Milestone | Crate | Status |
|-----------|-------|--------|
| M1 | ferrite-bencode | Done |
| M2 | ferrite-core | Done |
| M3 | ferrite-wire | Done |
| M4 | ferrite-tracker | Done |
| M5 | ferrite-dht | Done |
| M6 | ferrite-storage | Done |
| M7 | ferrite-session (peer + torrent) | Done |
| M8a | ferrite-session (tracker + DHT integration) | Done |
| M8b | ferrite-session (session manager) | Done |
| M8c | ferrite-session (magnet/LSD/AllowedFast/RejectRequest) | Done |
| M9 | ferrite-session (seeding & upload pipeline) | Done |
| M10a | ferrite (scaffold + bencode/core re-exports) | Planned |
| M10b | ferrite (wire + tracker re-exports) | Planned |
| M10c | ferrite (dht + storage re-exports) | Planned |
| M10d | ferrite (session + ClientBuilder) | Planned |
| M10e | ferrite (prelude + unified error + integration tests) | Planned |
| M11+ | BEP 52, uTP, DHT extensions, RSS | Future |

## License

MIT OR Apache-2.0
