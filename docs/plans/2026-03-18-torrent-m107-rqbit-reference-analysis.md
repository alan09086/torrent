# M107 rqbit Reference Analysis — Actual Source Code Review

Generated 2026-03-18 during eng review of M107 plan.
Source: rqbit 8.x (latest main branch)

## rqbit Source Location

Local copy: `docs/reference/rqbit-source/` (gitignored)

### Quick Reference — Key Files for M107 Review

| Topic | rqbit File (relative to `docs/reference/rqbit-source/`) | Lines |
|-------|----------------------------------------------------------|-------|
| TCP+uTP racing | `crates/librqbit/src/stream_connect.rs` | 240-324 |
| Unchoke on connect | `crates/librqbit/src/peer_connection.rs` | 330-339 |
| Parallel metadata (orchestrator) | `crates/librqbit/src/dht_utils.rs` | 30-106 |
| Parallel metadata (per-peer reader) | `crates/librqbit/src/peer_info_reader/mod.rs` | 223-251 |
| DHT re-query loop | `crates/dht/src/dht.rs` | 339-391 |
| REQUERY_INTERVAL constant | `crates/dht/src/lib.rs` | 24 |
| num_want (None for all) | `crates/tracker_comms/src/tracker_comms.rs` | 280 |
| HTTP num_want serialization | `crates/tracker_comms/src/tracker_comms_http.rs` | 194-195 |
| UDP num_want serialization | `crates/tracker_comms/src/tracker_comms_udp.rs` (in our crate) | 160 |
| Peer backoff (10s/6x/1h) | `crates/librqbit/src/torrent_state/live/peer/stats/atomic.rs` | 52-61 |
| Peer limit (128 default) | `crates/librqbit/src/torrent_state/live/mod.rs` | 282 |
| Semaphore-based connection pacing | `crates/librqbit/src/torrent_state/live/mod.rs` | 602-661 |
| Peer adder task (unbounded queue) | `crates/librqbit/src/torrent_state/live/mod.rs` | 197-198, 602-661 |

## Purpose

The M107 plan was based on an earlier rqbit investigation. This document corrects
factual errors found by reading the actual rqbit source code.

## Findings by Topic

### 1. TCP vs uTP Connection Strategy

**Plan claims (Change 5, line 94):** "rqbit uses TCP-only for outgoing"
**Plan claims (Decision #5, line 128):** "500ms uTP head start"
**These contradict each other.**

**Actual rqbit behaviour** (`crates/librqbit/src/stream_connect.rs:240-324`):
- TCP starts immediately
- uTP starts after a **1-second delay** (or immediately if TCP fails)
- Both race in parallel — first to connect wins
- Code comment: "Try to connect over TCP first. If in 1 second we haven't connected, try uTP also (if configured). Whoever connects first wins."
- If TCP connects before the 1s delay, uTP never starts
- If both fail, errors from both are reported

**Key code:**
```rust
// stream_connect.rs lines 240-324
let tcp_connect = async { self.tcp_connect(addr).await };
let utp_connect = async {
    if self.enable_tcp {
        tokio::select! {
            _ = tcp_failed_notify.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
    sock.connect(addr).await
};
// Race both, first to connect wins
```

### 2. num_want

**Plan claims (line 26, 61-65):** "UDP tracker num_want: -1: Requests ALL available peers"
**Plan claims (Change 2):** Use 200 for HTTP, -1 for UDP

**Actual rqbit behaviour** (`crates/tracker_comms/src/tracker_comms.rs:280`):
- Uses `numwant: None` for ALL trackers (both HTTP and UDP)
- HTTP: `None` means the `numwant` parameter is omitted from the query string entirely
- UDP: The serializer defaults `None` to `-1` automatically (`req.num_want.unwrap_or(-1i32)`)
- So rqbit doesn't explicitly differentiate — it just doesn't specify num_want at all

**Our tracker code** (`crates/torrent-tracker/src/udp.rs:160`):
```rust
buf.extend_from_slice(&req.num_want.unwrap_or(-1i32).to_be_bytes());
```
So setting `num_want: None` would match rqbit exactly (HTTP omits, UDP sends -1).

### 3. Metadata Fetch

**Plan claims:** "128-way parallel" — CORRECT

**Actual rqbit behaviour** (`crates/librqbit/src/dht_utils.rs:30-106`):
- Uses `FuturesUnordered` for parallel metadata connections
- `Semaphore::new(128)` — up to 128 concurrent metadata reader connections
- Each peer gets a DEDICATED connection just for metadata (via `peer_info_reader::read_metainfo_from_peer`)
- NOT in-band on the main peer connection
- Each peer_info_reader requests ALL pieces from its peer (lines 245-250)
- First peer to complete wins, SHA1 verified
- New peers from DHT stream are continuously added to the futures set

**Key difference from our architecture:** rqbit uses separate metadata-only connections.
Our plan proposes in-band metadata requests on the main peer connections. This is
architecturally different but achieves the same goal with less overhead (no extra
connections needed).

### 4. Unchoke on Connect

**Plan claims:** rqbit sends Unchoke immediately — CORRECT

**Actual rqbit behaviour** (`crates/librqbit/src/peer_connection.rs:332-339`):
```rust
let len = Message::Unchoke.serialize(&mut *write_buf, &Default::default)?;
write.write_all(&write_buf[..len]).await?;
trace!("sent unchoke");
```
Sent unconditionally immediately after bitfield. No conditions.

### 5. Connection Pacing

**Plan claims:** "No connect rate limit" — CORRECT

**Actual rqbit behaviour** (`crates/librqbit/src/torrent_state/live/mod.rs`):
- `Semaphore::new(peer_limit.unwrap_or(128))` — connections limited by semaphore
- `task_peer_adder` (line 602-661): pulls from unbounded channel, acquires semaphore permit, spawns
- No timer-based pacing — connects as fast as permits are available
- When a peer disconnects, permit is released, next queued peer connects immediately

### 6. Peer Limit

**Plan claims:** rqbit caps at 128 connected — CORRECT

**Actual rqbit behaviour:**
- `peer_limit.unwrap_or(128)` (line 282)
- Semaphore-based, so exactly 128 concurrent connections max
- Known peers pool is unbounded (HashSet<SocketAddr>)

### 7. DHT Re-query

**Plan claims:** "re-queries every 60 seconds via request_peers_forever()" — CORRECT

**Actual rqbit behaviour** (`crates/dht/src/dht.rs:339-391`, `crates/dht/src/lib.rs:24`):
- `REQUERY_INTERVAL = Duration::from_secs(60)` — confirmed 60-second interval
- `request_peers_forever()` runs a loop: calls `get_peers_root()` (which sends get_peers
  queries to routing table nodes sorted by distance), then sleeps for REQUERY_INTERVAL
- Adaptive sleep: if 0 nodes responded, retries after 1s. If <8 nodes, proportional
  sleep (`REQUERY_INTERVAL / 8 * n`). If ≥8 nodes, full 60s sleep.
- Runs for the entire lifetime of the torrent (until cancelled)
- The DHT crate has a TODO comment: "Not sure if we should re-query tbh." — it works
  but even the author isn't sure it's necessary. For our use case it definitely helps.

## Plan Internal Contradictions

1. **Metadata strategy:** Decision #1 says "Per-piece metadata fan-out (not full redundancy)"
   but Change 1 says "full redundancy, rqbit style". These conflict.

2. **uTP strategy:** Change 5 says "Use TCP only for outgoing connections (skip uTP attempt
   entirely)" but Decision #5 says "500ms uTP head start (not 0ms or 5s)". These directly
   conflict.

### 8. Peer Backoff Strategy

**Plan claims (line 43):** "Per-peer backoff: 10s initial, 6x multiplier, 1h max" — CORRECT

**Actual rqbit behaviour** (`crates/librqbit/src/torrent_state/live/peer/stats/atomic.rs:52-61`):
```rust
fn backoff() -> ExponentialBackoff {
    ExponentialBuilder::new()
        .with_min_delay(Duration::from_secs(10))
        .with_factor(6.)
        .with_jitter()
        .with_max_delay(Duration::from_secs(3600))    // 1 hour max
        .with_total_delay(Some(Duration::from_secs(86400)))  // 24h total
        .without_max_times()
        .build()
}
```
Uses the `backon` crate. Backoff resets on successful connection (`reset_backoff()`).
Compare our backoff: 200ms initial, 2x multiplier, 30s max — much more aggressive
reconnection (shorter delays, lower cap).

## Summary of Plan Accuracy

| Claim | Verdict |
|-------|---------|
| 128-way parallel metadata | CORRECT |
| num_want: -1 for UDP | MISLEADING — rqbit uses None for all, UDP serializer defaults to -1 |
| TCP-only outgoing | WRONG — rqbit races TCP+uTP (TCP first, uTP after 1s) |
| Unchoke on connect | CORRECT |
| No connect rate limit | CORRECT (semaphore-paced) |
| 128 peer limit | CORRECT |
| DHT re-query every 60s | CORRECT |
| 10s/6x/1h backoff | CORRECT |

## Recommended Plan Corrections

1. **Change 5 (connection strategy):** Match rqbit — race TCP and uTP with TCP getting a 1s
   head start. Don't go TCP-only (loses uTP benefits unnecessarily).

2. **Change 2 (num_want):** Simplest approach is `num_want: Some(200)` for both HTTP and UDP.
   Or `None` to exactly match rqbit. UDP trackers handle positive values fine.

3. **Resolve metadata contradiction:** Pick one — full redundancy or per-piece fan-out.
   rqbit uses full redundancy (all pieces to all peers).

4. **Update Key Decisions table** to match the Changes section after resolving contradictions.
