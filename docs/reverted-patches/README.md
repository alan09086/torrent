# Reverted M57 Patches (2026-03-04)

## Why Reverted

These patches implemented the M57 "Non-Blocking Transfer Pipeline" milestone.
All unit tests (1155) and clippy passed, but **live BitTorrent downloads failed** —
ferrite established uTP/TCP connections but never completed BT handshakes or
transferred any data (0 bytes downloaded).

Bisection revealed the regression predates M57 — the base commit (`c677d23`)
also fails to download. The root cause appears to be in the wire/handshake layer
and needs investigation before re-applying these optimizations.

## Commits (oldest first)

```
dca4e43 feat: instrument benchmark with peak peer count logging (M57)
c677d23 perf: add 5-second TCP connect timeout (was OS default ~2min) (M57)
7e58972 feat: add async disk write/verify types and enqueue methods (M57)
7054925 feat: wire async disk channels into TorrentActor select loop (M57)
6491e89 perf: non-blocking disk writes in handle_piece_data with zero-copy (M57)
87c8991 perf: async v1 piece verification via enqueue_verify (M57)
ab45d4c perf: O(1) peer dedup with HashSet in handle_add_peers (M57)
c629c9f test: add unit tests for async disk write and verify (M57)
c24bddc style: run cargo fmt --all across workspace (M57)
459d311 bench: add criterion disk write benchmarks (M57)
37a2b73 feat: version bump to 0.64.0, changelog for M57 non-blocking pipeline
48248a8 Merge feature/m57-non-blocking-pipeline
5cc1c55 docs: update README badges for v0.64.0 (M57)
```

## What the patches contained

1. **Async disk types** — `DiskWriteError`, `VerifyResult`, `WriteAsync`/`VerifyAsync`/`VerifyV2Async` DiskJob variants
2. **Enqueue methods** — `enqueue_write()`, `enqueue_verify()`, `enqueue_verify_v2()` on DiskHandle
3. **DiskActor handlers** — `spawn_blocking` for all three async variants with flush-before-hash
4. **TorrentActor channels** — `write_error_rx/tx`, `verify_result_rx/tx` with select! branches
5. **handle_piece_data() rewrite** — zero-copy `Bytes` move, fire-and-forget `enqueue_write()` with backpressure fallback
6. **Async v1 verification** — `enqueue_verify()` for V1Only, blocking kept for V2Only/Hybrid
7. **O(1) peer dedup** — `HashSet<SocketAddr>` replacing linear scan in `handle_add_peers()`
8. **TCP 5s timeout** — `TcpStream::connect()` with `tokio::time::timeout(5s)`
9. **Criterion benchmarks** — disk write throughput benchmarks (blocking vs enqueue)

## Re-applying

```bash
git am docs/reverted-patches/0001-*.patch
# ... or apply selectively
```

These patches are architecturally sound — the blocker is a pre-existing
wire/handshake issue that must be fixed first.
