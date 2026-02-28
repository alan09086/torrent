# Ferrite

Rust BitTorrent library targeting libtorrent-rasterbar + rqbit streaming parity.

## Build & Test

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Architecture

11-crate workspace: `ferrite-bencode` → `ferrite-core` → `ferrite-wire`/`ferrite-tracker`/`ferrite-dht`/`ferrite-storage`/`ferrite-utp`/`ferrite-nat` → `ferrite-session` → `ferrite` (facade)

- **ferrite-session**: Actor model — `SessionActor`/`TorrentActor` with `tokio::select!` loops and command channels
- **ferrite** (facade): `ClientBuilder` fluent API, `AddTorrentParams`, unified `Error`, `prelude` module

## Conventions

- Edition 2024, workspace resolver = "2"
- `thiserror` typed errors per crate, no `anyhow`
- `bytes::Bytes` throughout (not custom wrappers)
- Manual big-endian serialization for wire protocol
- Random bytes: thread-local xorshift64 seeded from SystemTime (avoids `rand` dep)
- Sorted map serializer for BEP 3 key ordering
- KRPC: `BTreeMap<Vec<u8>, BencodeValue>` for encoding, pattern-match on "y" field for decoding

## Milestones

- 35-milestone roadmap: `docs/plans/2026-02-26-ferrite-roadmap-v2.md`
- Commit format: `feat: description (M24)` — milestone tag in parentheses
- Version bumps: workspace version in root `Cargo.toml`, bump with each milestone
- Plan files: `docs/plans/YYYY-MM-DD-ferrite-<milestone>-<topic>.md`

## Remotes

- `origin` → Codeberg, `github` → GitHub
- Push to BOTH on every push: `git push origin main && git push github main`

## License

GPL-3.0-or-later
