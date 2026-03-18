# M107: Aggressive Peer Pipeline — Design Spec

## Context

After M105 (DHT reliability, v0.105.0, 1565 tests), head-to-head benchmarking against rqbit 8.1.1 on the same Arch ISO magnet reveals a **1.46x speed gap** (39.5 MB/s vs ~57 MB/s on the same swarm, with rqbit peaks at 94 MB/s on good swarms). Detailed timeline analysis shows the gap is **not in sustained throughput** (our download engine hits 70+ MB/s once warm) but in **startup latency and peer utilisation**:

| Phase | Our Client | rqbit 8.1.1 | Gap |
|-------|-----------|-------------|-----|
| Metadata fetch | 11s | 2.4s | **+8.6s** |
| Ramp to full speed | 11–20s | 2.4–10s | **+1.4s** |
| Sustained speed | ~70 MB/s | ~94 MB/s | **1.34x** |
| Known peers | 128 | 1,187 | **9.3x** |
| Total download | ~40s | ~22s | **1.8x** |

**M104 benchmark** (v0.104.0): Mean 33.8 MB/s, 10/10 reliability
**M105 benchmark** (v0.105.0): Mean 39.5 MB/s, 10/10 reliability, +17% from DHT fixes
**rqbit 8.1.1 reference**: ~57 MB/s same-swarm, peaks 94 MB/s, 22s total, 1187 known peers

This milestone implements 8 changes to match rqbit's aggressive peer pipeline strategy. The goal is to reduce total download time by eliminating the metadata fetch bottleneck, expanding peer discovery, and accelerating connection ramp-up.

## rqbit Reference Architecture (8.1.1)

### Peer Discovery
- **All sources simultaneous**: DHT + all trackers (HTTP & UDP) + PEX merged into one `PeerStream` from T=0
- **DHT re-query every 60s**: `request_peers_forever()` continuously discovers peers throughout the download
- **UDP tracker `num_want: -1`**: Requests ALL available peers (vs our 50)
- **No connect rate limit**: Peers connect as fast as they arrive, limited only by 128-peer semaphore
- **PEX (BEP 11)**: Exchanges peer lists with connected peers every 60s, max 50 peers/message

### Metadata Fetch
- **128-way parallel**: Up to 128 simultaneous metadata connections
- **Request all pieces from each peer**: Every peer that advertises `ut_metadata` gets all missing pieces requested
- **First peer to deliver all pieces wins**: SHA1 verified against info_hash

### Request Pipeline
- **128 blocks in flight per peer**: Dumped all at once on unchoke (not trickled)
- **1-for-1 refill**: Each block received releases one semaphore permit immediately
- **Zero warm-up**: First peer unchoked gets full 128-block pipeline instantly

### Connection Strategy
- **No connect interval**: Peers connect as fast as they arrive from the discovery stream
- **Aggressive unchoke**: Sends Unchoke to every peer immediately on connect
- **Per-peer backoff**: 10s initial, 6x multiplier, 1h max (vs our 200ms initial, 2x, 30s max)

## Changes

### 1. Parallel Metadata Fetch (Critical — accounts for ~8s of gap)

**Problem**: We request metadata from only ONE peer at a time. When a peer's `ExtHandshake` arrives with `metadata_size`, we request all missing pieces from that single peer. If that peer is slow or disconnects, metadata stalls.

**Solution**: Fan out metadata requests across ALL peers that advertise `ut_metadata`:

- When any peer reports `metadata_size` in its extended handshake, send `RequestMetadata` for all missing pieces to that peer
- Track which pieces are in-flight from which peer (add `metadata_in_flight: HashMap<u32, SocketAddr>` to `MetadataDownloader`)
- When a new peer connects with `ut_metadata`, request any pieces not yet received (skip pieces already in-flight from another peer, or request them too for redundancy)
- Handle `MetadataReject`: mark piece as not in-flight, retry from another peer
- First complete set wins (SHA1 verify against info_hash)
- On piece timeout (5s), retry from another peer

**Key insight from rqbit**: rqbit requests ALL pieces from EACH peer — pure redundancy. The first peer to deliver all pieces wins. This is simpler than tracking per-piece assignments and just as effective. We can start with per-piece assignment (less network waste) and upgrade to full redundancy if needed.

### 2. Raise num_want to 200 (Trivial — accounts for ~3x more peers per tracker response)

**Problem**: `tracker_manager.rs:437` hardcodes `num_want: Some(50)`. Each tracker response gives at most 50 peers. rqbit uses `num_want: -1` (all available).

**Solution**: Change `num_want` to `Some(200)` (or `None` to use tracker default). Using 200 matches libtorrent's default and is less aggressive than rqbit's -1 while still providing 4x more peers per response.

For UDP trackers, the `num_want` field should be set to `-1` (i32, meaning "as many as possible") per BEP 15.

### 3. Raise max_peers_per_torrent to 200 (Low risk — enables more connections)

**Problem**: `max_peers_per_torrent` defaults to 128. This caps CONNECTED peers and also stops DHT discovery when reached. rqbit also caps at 128 connected but discovers 1187 known peers.

**Solution**:
- Raise `max_peers_per_torrent` default to 200 (matches `high_performance` preset territory)
- **Critically**: Decouple peer discovery from connection count — continue DHT re-queries and accepting tracker responses even when at the connection cap. The `available_peers` pool should grow unbounded (it's just a HashSet of SocketAddrs).

### 4. Continuous DHT Re-query During Download (Medium — keeps discovering peers)

**Problem**: We call `get_peers()` once at torrent start. The lookup runs, exhausts, and stops. No new DHT peer discovery happens during the download. rqbit re-queries every 60 seconds via `request_peers_forever()`.

**Solution**: Add a periodic DHT re-query timer to `TorrentActor`:
- Every 60 seconds, if the torrent is still downloading, call `dht.get_peers(info_hash)` again
- Merge newly discovered peers into `available_peers`
- Guard: don't re-query if the torrent is complete or paused
- Guard: don't re-query if we already have >500 known peers (diminishing returns)

### 5. Remove Connection Ramp-Down (Low risk — keeps connecting aggressively)

**Problem**: At T=15s, the connect interval jumps from 100ms to 500ms (5x slower). This reduces our ability to replace dead peers and connect new ones during the critical download phase.

**Solution**:
- Keep the connect interval at 100ms throughout the download
- Remove the `connect_ramped_down` flag and the 15-second ramp-down logic
- The per-peer exponential backoff already prevents hammering individual peers

### 6. Parallel uTP+TCP Connection (Medium — eliminates 5s timeout on TCP-only peers)

**Problem**: Each connection attempt tries uTP first with a 5-second timeout, then falls back to TCP. For TCP-only peers (the majority), this wastes 5 seconds per peer.

**Solution**: Try uTP and TCP simultaneously using `tokio::select!`:
```rust
tokio::select! {
    result = try_utp_connect(addr) => { /* use uTP if it wins */ }
    result = async {
        tokio::time::sleep(Duration::from_millis(500)).await;  // give uTP a 500ms head start
        try_tcp_connect(addr).await
    } => { /* use TCP if uTP is slow */ }
}
```
Give uTP a 500ms head start (not 5s). If uTP hasn't connected in 500ms, start TCP in parallel. First to succeed wins.

### 7. Send Unchoke on Connect (Trivial — improves reciprocity)

**Problem**: We don't proactively unchoke peers on connect. rqbit sends Unchoke immediately upon connecting, which makes peers more willing to unchoke us (tit-for-tat).

**Solution**: In `run_peer()`, immediately after sending the handshake and bitfield, send an Unchoke message to the peer. This costs nothing (we're downloading, not seeding rate-limited content) and improves unchoke reciprocity.

### 8. Handle MetadataReject + Timeout (Low risk — prevents metadata stalls)

**Problem**: `MetadataReject` events are completely ignored (`// Could retry from a different peer; for now, ignore.`). If a peer rejects a metadata piece, that piece is never retried. Also, no timeout on individual metadata piece requests.

**Solution**:
- On `MetadataReject`: mark piece as not in-flight, request from another peer that has `ut_metadata`
- Add a 5-second timeout per metadata piece request. If a peer doesn't respond in 5s, retry from another peer.
- If all peers have been tried and some pieces are still missing, cycle through peers again.

## Files Changed

| File | Change | Est. Lines |
|------|--------|------------|
| `crates/torrent-session/src/torrent.rs` | Parallel metadata, DHT re-query timer, remove ramp-down, unchoke-on-connect, metadata retry | ~+200, -50 |
| `crates/torrent-session/src/metadata.rs` | Multi-peer tracking, in-flight map, piece timeout, reject handling | ~+80 |
| `crates/torrent-session/src/tracker_manager.rs` | num_want 50→200, UDP num_want=-1 | ~+5 |
| `crates/torrent-session/src/settings.rs` | max_peers 128→200 | ~+2 |
| `crates/torrent-session/src/peer.rs` | Unchoke on connect, parallel uTP+TCP | ~+30, -20 |
| **Net** | | **~+250 lines** |

## Key Decisions

| # | Decision | Rationale |
|---|----------|-----------|
| 1 | Per-piece metadata fan-out (not full redundancy) | Less network waste than rqbit's "request everything from everyone" while still parallelising. Can upgrade to full redundancy later if needed. |
| 2 | num_want=200 (not -1) | -1 is more aggressive than needed. 200 matches libtorrent's default and provides 4x improvement over current 50. |
| 3 | max_peers=200 (not 128 or 250) | Sweet spot between resource usage and peer pool quality. 250 is the high_performance preset; 200 is a good default. |
| 4 | 60s DHT re-query (matching rqbit) | Proven interval from rqbit. Aggressive enough to discover new peers, conservative enough to avoid hammering DHT. |
| 5 | 500ms uTP head start (not 0ms or 5s) | Gives uTP a fair chance without penalising TCP-only peers. 0ms would spam both protocols; 5s (current) wastes time. |
| 6 | Keep 100ms connect interval (no ramp-down) | Per-peer backoff already handles repeated failures. The ramp-down at 15s actively harms downloads that are still building connections. |
| 7 | Unchoke on connect (unconditional) | Costs nothing for a downloader. Matches rqbit. Improves tit-for-tat reciprocity. |
| 8 | 5s metadata piece timeout | Matches our KRPC query timeout. Long enough for slow peers, short enough to recover quickly. |

## Testing

### Unit Tests
| # | Test | File | Validates |
|---|------|------|-----------|
| 1 | `metadata_parallel_fetch_from_multiple_peers` | torrent.rs | Multiple peers contribute metadata pieces simultaneously |
| 2 | `metadata_reject_triggers_retry` | torrent.rs | Rejected piece is re-requested from another peer |
| 3 | `metadata_timeout_triggers_retry` | torrent.rs | Piece not received in 5s retried from another peer |
| 4 | `tracker_num_want_200` | tracker_manager.rs | HTTP announces use num_want=200 |
| 5 | `tracker_udp_num_want_all` | tracker_manager.rs | UDP announces use num_want=-1 |
| 6 | `dht_requery_interval_60s` | torrent.rs | DHT re-queries every 60s during download |
| 7 | `no_connect_ramp_down` | torrent.rs | Connect interval stays at 100ms past 15s |
| 8 | `unchoke_sent_on_connect` | peer.rs | Unchoke message sent immediately after handshake |

### Integration Tests
| # | Test | Validates |
|---|------|-----------|
| 9 | `parallel_metadata_completes_faster` | Metadata assembly uses multiple peers |
| 10 | `peer_discovery_continues_at_cap` | DHT/tracker peers still accepted when at max_peers |

## Verification

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings

# Benchmark: 10-trial reliability test
./benchmarks/analyze.sh "magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso" -n 10
# Target: 10/10 reliability, mean >50 MB/s, metadata <5s

# Head-to-head comparison
# Run rqbit: timeout 300 rqbit download -e <magnet> -o /tmp/rqbit
# Run ours:  timeout 300 torrent download <magnet> -o /tmp/torrent -q
# Compare wall times
```

## Success Criteria

- **Metadata fetch under 5 seconds** (was 11s, rqbit does 2.4s)
- **Known peers >500** within 30 seconds (was capped at 128)
- **Mean download speed >50 MB/s** on 10-trial Arch ISO benchmark (was 39.5 MB/s)
- **10/10 reliability** maintained
- **Time-to-first-data under 5 seconds** (was 11s)
- **No speed regression** — sustained throughput should be same or better

## Task Dependencies

```
Change 2 (num_want) ─────────────────────┐
Change 3 (max_peers) ────────────────────┤
Change 5 (no ramp-down) ────────────────┤── all independent
Change 7 (unchoke on connect) ──────────┤
Change 8 (metadata reject/timeout) ─────┤
Change 1 (parallel metadata) ───────────┤── needs Change 8 first
Change 4 (DHT re-query) ───────────────┤── needs Change 3 first
Change 6 (parallel uTP+TCP) ───────────┘── independent
```

## NOT in Scope

- **PEX (BEP 11)** — Valuable but a separate feature with its own protocol complexity. Should be its own milestone.
- **Adaptive pipeline depth** — Our fixed 128 matches rqbit. Not the bottleneck.
- **Piece selection changes** — Our CAS-based rarest-first (M93) is competitive. rqbit uses file-ordered which is simpler but not necessarily better.
- **Rate limiting changes** — Our 250 qps DHT rate limit matches rqbit. Not the bottleneck.
- **Per-piece write locking** — rqbit uses per-piece RwLocks for steal coordination. Our BlockMaps (M103) handle this differently. Not the bottleneck.
