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
ferrite-utp          uTP (BEP 29) micro transport protocol
     |
ferrite              Public facade API
```

## Crates

| Crate | Description | Tests |
|-------|-------------|-------|
| `ferrite-bencode` | Serde-based bencode serialization | 64 |
| `ferrite-core` | Id20/Id32, TorrentMetaV1, Magnet, Lengths, PeerId, FastResumeData, FilePriority | 56 |
| `ferrite-wire` | Handshake, Message codec, BEP 6/9/10 extensions, MSE/PE encryption | 54 |
| `ferrite-tracker` | HTTP (reqwest) + UDP (BEP 15) tracker client, IPv6 compact peers | 24 |
| `ferrite-dht` | Kademlia DHT with actor model, KRPC, routing table, BEP 24 IPv6 | 55 |
| `ferrite-storage` | Bitfield, FileMap, ChunkTracker, TorrentStorage trait | 42 |
| `ferrite-session` | Session manager, peer tasks, torrent actor, BEP 6/14, seeding, persistence, selective download, bandwidth limiting, alerts, queue management, uTP integration, NAT port mapping, dual-stack IPv6 | 204 |
| `ferrite-utp` | uTP (BEP 29) micro transport protocol with LEDBAT congestion control | 21 |
| `ferrite-nat` | PCP (RFC 6887) / NAT-PMP (RFC 6886) / UPnP IGD automatic port mapping | 19 |
| `ferrite` | Public facade: full API + ClientBuilder + prelude + unified error | 27 |

**Total: 566 tests, zero clippy warnings.**

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

See [docs/plans/2026-02-26-ferrite-roadmap-v2.md](docs/plans/2026-02-26-ferrite-roadmap-v2.md) for the full 35-milestone roadmap to libtorrent-rasterbar parity.

| Phase | Milestones | Status |
|-------|-----------|--------|
| Foundation | M1-M10 (bencode → facade) | Done |
| Phase 1: Desktop Essentials | M11-M16 (resume, selective download, end-game, bandwidth + auto upload slots, alerts, queue) | Done |
| Phase 2: Transport & Security | M17-M20 (encryption, uTP, NAT traversal) | Done |
| Phase 3: Protocol Extensions | M21-M24 (IPv6, web seed, super seed + have batching, scrape) | M21 done |
| Phase 4: Performance | M25-M28 (smart ban, async disk + ARC cache, parallel hash, piece picker + streaming + dynamic request queue) | Planned |
| Phase 5: Network & Tools | M29-M32 (IP filter, torrent creation, settings, share mode + plugin interface) | Planned |
| Phase 6: BitTorrent v2 | M33-M35 (BEP 52, hybrid torrents, BEP 53) | Planned |

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
