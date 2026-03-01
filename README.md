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
| `ferrite-bencode` | Serde-based bencode serialization | 14 |
| `ferrite-core` | Id20/Id32, TorrentMetaV1/V2, InfoHashes, MerkleTree, MerkleTreeState, HashRequest, HashPicker, Magnet (v1+v2), Lengths, PeerId, FastResumeData, FilePriority, StorageMode, CreateTorrent | 137 |
| `ferrite-wire` | Handshake, Message codec, BEP 6/9/10/21/52 extensions, MSE/PE encryption | 65 |
| `ferrite-tracker` | HTTP (reqwest) + UDP (BEP 15) tracker client, BEP 48 scrape, IPv6 compact peers | 35 |
| `ferrite-dht` | Kademlia DHT with actor model, KRPC, routing table, BEP 24 IPv6 | 55 |
| `ferrite-storage` | Bitfield, FileMap, ChunkTracker, TorrentStorage trait, MmapStorage, ARC disk cache | 53 |
| `ferrite-session` | Session manager, peer tasks, torrent actor, async disk I/O (DiskActor), unified Settings, runtime config, BEP 6/14/16/21, seeding, super seeding, persistence, selective download, bandwidth limiting, alerts, queue management, uTP integration, NAT port mapping, dual-stack IPv6, HTTP/web seeding (BEP 19/17), batched Have, tracker scrape + lt_trackers, smart banning, parallel hashing, IP filtering + proxy, file streaming, BEP 9 metadata serving, BEP 40 peer priority, per-class rate limits, move storage, share mode, extension plugins | 370 |
| `ferrite-utp` | uTP (BEP 29) micro transport protocol with LEDBAT congestion control | 21 |
| `ferrite-nat` | PCP (RFC 6887) / NAT-PMP (RFC 6886) / UPnP IGD automatic port mapping | 20 |
| `ferrite` | Public facade: full API + ClientBuilder + prelude + unified error | 35 |

**Total: 869 tests, zero clippy warnings.**

## Design Decisions

- **Modular crates** — each layer is independently testable and reusable
- **`thiserror` typed errors** per crate, no `anyhow`
- **Rust edition 2024** with workspace resolver 2
- **`bytes::Bytes`** for zero-copy buffer sharing across the wire protocol
- **Actor model** for DHT — single-owner event loop with cloneable handle, no `DashMap`
- **Async disk I/O** — central DiskActor with write buffering, ARC read cache, and semaphore-limited spawn_blocking
- **Wire-compatible coordinates** — `(piece, begin, length)` used throughout, matching BEP 3 directly
- **Binary search file lookup** — O(log n) piece-to-file mapping vs linear scan

## Building

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Requires Rust edition 2024 support (rustc 1.85+).

## Roadmap

See [docs/plans/2026-02-26-ferrite-roadmap-v2.md](docs/plans/2026-02-26-ferrite-roadmap-v2.md) for the full 38-milestone roadmap to libtorrent-rasterbar parity.

| Phase | Milestones | Status |
|-------|-----------|--------|
| Foundation | M1-M10 (bencode → facade) | Done |
| Phase 1: Desktop Essentials | M11-M16 (resume, selective download, end-game, bandwidth + auto upload slots, alerts, queue) | Done |
| Phase 2: Transport & Security | M17-M20 (encryption, uTP, NAT traversal) | Done |
| Phase 3: Protocol Extensions | M21-M24 (IPv6, web seed, super seed + have batching, scrape) | Done |
| Phase 4: Performance | M25-M28 (smart ban, async disk + ARC cache, parallel hash, piece picker + streaming + dynamic request queue) | Done |
| Phase 5: Network & Tools | M29-M32d (IP filter, torrent creation, settings, metadata serving, BEP 40, per-class rate limits, move storage, share mode, extension plugins) | Done |
| Phase 6: BitTorrent v2 | M33-M35 (BEP 52, hybrid torrents, BEP 53) | M33-M34a done |

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
