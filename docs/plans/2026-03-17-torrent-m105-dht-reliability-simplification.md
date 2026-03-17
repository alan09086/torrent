# M105: DHT Reliability & Simplification

## Context

After M104 (fixed-depth pipeline, connection overhaul, DHT diagnostic logging), benchmark results showed **2/10 DHT cold-start failures** and identified several areas where our DHT implementation is fragile or over-complex compared to rqbit. M104's diagnostic logging (Gap 4) gave us visibility into bootstrap stages; M105 acts on those findings.

**M104 benchmark** (TBD — placeholder for post-M104 numbers):
- Mean: TBD MB/s | DHT reliability: TBD/10
- Cold-start failures: TBD (expected improvement from M104 logging-driven fixes)

**rqbit 8.1.1** reference:
- 10/10 DHT reliability | DNS bootstrap with exponential backoff
- JSON routing table persistence | 512 node cap | 3.75-minute ping interval

This milestone focuses on **DHT reliability and code simplification**, not download speed. Five targeted improvements inspired by rqbit's simpler patterns, adapted to our architecture.

## Changes

### 1. DNS Bootstrap Backoff (High priority)

**Problem**: `bootstrap()` (actor.rs:731-811) calls `tokio::net::lookup_host()` once per hostname, fire-and-forget. If DNS times out or fails, we proceed with whatever saved nodes responded. rqbit retries with exponential backoff.

**Solution**: Wrap DNS resolution in a retry loop with exponential backoff: 1s, 2s, 4s, 8s, 16s, 30s, 30s... (cap 30s, total timeout 120s). On success, send `find_node` to resolved addresses. On total failure after 120s, log a warning and proceed with saved nodes only.

**In actor.rs `bootstrap()`** (line 760-782): Replace the single `lookup_host` call per hostname with a retry loop. Each hostname is resolved independently — if one bootstrap server is down, others proceed.

```rust
// Pseudocode for the retry loop:
for addr_str in &hostname_strs {
    let mut delay = Duration::from_secs(1);
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match tokio::net::lookup_host(addr_str.as_str()).await {
            Ok(addrs) => { /* filter by family, send find_node */ break; }
            Err(e) if Instant::now() + delay < deadline => {
                warn!(node = %addr_str, error = %e, retry_in = ?delay, "bootstrap DNS retry");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
            Err(e) => {
                warn!(node = %addr_str, error = %e, "bootstrap DNS failed after retries");
                break;
            }
        }
    }
}
```

**Note**: The retry loop runs sequentially per hostname within `bootstrap()`, which is called once at startup before the event loop begins. This is acceptable since bootstrap is already sequential and the total timeout (120s) matches the bootstrap timeout timer (10s for gate, but we proceed with saved nodes).

### 2. Unified Lookup State Machine (Medium priority)

**Problem**: Three separate lookup state structs implement the same iterative Kademlia pattern:
- `LookupState` (actor.rs:491-499) — get_peers queries
- `ItemLookupState` (actor.rs:511-525) — BEP 44 get_item queries
- `FindNodeLookup` (actor.rs:502-508) — bootstrap find_node queries

All three maintain closest-K, query Alpha unqueried, advance on response/timeout. The get_peers handling (lines 1346-1432) and FindNode handling (lines 1270-1344) duplicate the same "accumulate returned nodes, sort by distance, truncate, query next Alpha unqueried" pattern (~300 lines duplicated across three code paths).

**Solution**: Create `lookup.rs` with a generic `IterativeLookup<C: LookupCallbacks>` that encapsulates the common state machine:

```rust
pub(crate) trait LookupCallbacks {
    /// Process a response: extract new nodes, handle query-specific data.
    /// Returns new nodes discovered in this response.
    fn on_response(
        &mut self,
        from: &CompactNodeInfo,
        response: &KrpcResponse,
    ) -> Vec<CompactNodeInfo>;

    /// Called when the lookup is exhausted (no more unqueried nodes, no pending).
    fn on_complete(&mut self);

    /// Parallelism factor (Alpha). Default: 3.
    fn alpha(&self) -> usize { 3 }
}

pub(crate) struct IterativeLookup<C: LookupCallbacks> {
    pub target: Id20,
    pub closest: Vec<CompactNodeInfo>,
    pub queried: HashSet<Id20>,
    pub callbacks: C,
}

impl<C: LookupCallbacks> IterativeLookup<C> {
    /// Return the next batch of nodes to query (up to Alpha unqueried closest).
    pub fn next_to_query(&mut self) -> Vec<CompactNodeInfo> { ... }

    /// Feed a response into the lookup. Returns nodes to query next.
    pub fn handle_response(
        &mut self,
        from: &CompactNodeInfo,
        response: &KrpcResponse,
        family: AddressFamily,
    ) -> Vec<CompactNodeInfo> { ... }

    /// Check if the lookup is exhausted.
    pub fn is_exhausted(&self, has_pending: bool) -> bool { ... }
}
```

Then implement callbacks for each use case:
- `GetPeersCallbacks` — stores `mpsc::Sender<Vec<SocketAddr>>`, tokens HashMap, extracts peers
- `GetItemCallbacks` — handles immutable/mutable BEP 44 item responses
- `FindNodeCallbacks` — no extra state, just populates routing table

**In actor.rs**: Replace `LookupState`, `FindNodeLookup`, and `ItemLookupState` with `IterativeLookup<GetPeersCallbacks>`, `IterativeLookup<FindNodeCallbacks>`, and `IterativeLookup<GetItemCallbacks>`. The response handlers in `handle_response()` (lines 1253-1637) call `lookup.handle_response()` instead of duplicating the iteration logic inline.

### 3. Routing Table Node Cap (Low priority)

**Problem**: Our routing table can grow unboundedly. While theoretical max is 160 buckets x 8 = 1,280 nodes, adversarial conditions or rapid churn could push beyond normal operating range. rqbit caps at 512.

**Solution**: Add `max_routing_nodes: usize` to `DhtConfig` (default 512). In `routing_table.rs`, check total node count before insertion. When at cap, only insert if replacing a Bad node (existing eviction path at line 217-236 already handles this — we just add a total-count pre-check).

**In routing_table.rs `insert_inner()`** (line 172): Add an early return before the "Room in bucket" path (line 201) if `self.len() >= max_nodes` and no evictable bad node exists. This requires passing `max_nodes` into the routing table (new field on `RoutingTable`).

```rust
// In insert_inner(), after the "Already known — update" block:
if self.len() >= self.max_nodes {
    // At cap: only proceed if we can evict a Bad node in this bucket
    let bucket = &self.buckets[bucket_idx];
    if !bucket.nodes.iter().any(|n| n.fail_count > 0) {
        return false;
    }
}
```

**In DhtConfig**: Add `max_routing_nodes: usize` with default 512.

**In RoutingTable**: Add `max_nodes: usize` field, exposed via `new_with_config()` and a new `new_with_full_config()` constructor.

### 4. Reduce Ping Frequency (Medium priority)

**Problem**: `PING_INTERVAL` is 5s (M97 change for cold-start, actor.rs:559). This is 45x more aggressive than rqbit's 3.75 minutes. After bootstrap completes, this generates unnecessary network traffic — every 5s we ping all Questionable nodes (actor.rs:2020-2024), which can be dozens of pings.

**Solution**: Two-phase ping interval:
- During bootstrap (`bootstrap_complete == false`): 5s (keep M97 cold-start behavior)
- After bootstrap: 60s (12x reduction in steady-state traffic, still 3.75x more frequent than rqbit)

**Implementation**: In the `ping_tick` arm of the `select!` loop (actor.rs:693-711), check elapsed time since last ping instead of relying on the fixed interval. Add a `last_ping: Instant` field to `DhtActor`. The `ping_tick` interval stays at 5s (for the gate check), but the actual ping is gated:

```rust
// In the ping_tick arm:
let ping_interval = if self.bootstrap_complete {
    Duration::from_secs(60)
} else {
    Duration::from_secs(5)
};
if self.last_ping.elapsed() >= ping_interval {
    self.ping_questionable_nodes().await;
    self.last_ping = Instant::now();
}
// Node-count gate check runs every 5s regardless
if !self.bootstrap_complete && self.routing_table.len() >= 8 {
    self.on_bootstrap_complete().await;
}
```

### 5. JSON Routing Table Persistence (Medium priority)

**Problem**: Our routing table is loaded from `DhtConfig::bootstrap_nodes` strings populated by `SessionState::dht_nodes` (session.rs:2945-2954). This works but has a different save/load path from the DHT actor — the session must call `save_session_state()` and feed saved nodes back through config on next startup. rqbit persists directly from the DHT actor to a JSON file with atomic writes.

**Solution**: Add self-contained persistence to the DHT actor:
- **Save**: On every maintenance tick (60s, actor.rs:680-681), serialize routing table to JSON and write atomically (temp file + rename) to `{state_dir}/dht_state.json` (V4) or `dht_state_v6.json` (V6).
- **Load**: On startup in `DhtActor::new()`, load saved state file if it exists and insert all nodes as Questionable (using existing `mark_all_questionable()` from M97).
- **Format**: `{ "node_id": "hex", "nodes": [{"id": "hex", "addr": "ip:port"}] }`

**In DhtConfig**: Add `state_dir: Option<PathBuf>`. When `None`, persistence is disabled (default for tests). The session layer sets this from `Settings::resume_data_dir`.

**In actor.rs `maintenance()`** (line 1910): After existing maintenance logic, call `self.save_routing_table()` — a new method that:
1. Skips if `self.config.state_dir` is `None`
2. Serializes `self.routing_table.all_nodes()` + `self.routing_table.own_id()` to JSON
3. Writes to a temp file, then `std::fs::rename()` to the final path

**In actor.rs `new()`** (line 562): After creating the routing table, call `self.load_routing_table()` — a new method that:
1. Skips if `self.config.state_dir` is `None`
2. Reads the JSON file
3. Inserts all nodes into the routing table
4. Calls `mark_all_questionable()` (so loaded nodes are Questionable until they respond)

**Coexistence with session persistence**: Both systems can coexist. The JSON file is the primary path (faster, actor-owned). The session-level `save_session_state()` / `dht_saved_nodes` config path remains as a fallback for applications that don't set `state_dir`. The actor's JSON persistence runs independently without session coordination.

## Files Changed

| File | Change | Est. Lines |
|------|--------|------------|
| `crates/torrent-dht/src/actor.rs` | DNS retry loop in `bootstrap()`, two-phase ping interval, `save_routing_table()` / `load_routing_table()` methods, replace 3 lookup structs with `IterativeLookup<C>` usage, `last_ping` field, `state_dir` handling | ~-200, +150 |
| `crates/torrent-dht/src/lookup.rs` | **NEW** — `IterativeLookup<C>`, `LookupCallbacks` trait, `GetPeersCallbacks`, `GetItemCallbacks`, `FindNodeCallbacks` | ~+200 |
| `crates/torrent-dht/src/routing_table.rs` | `max_nodes` field, `new_with_full_config()` constructor, cap check in `insert_inner()` | ~+25 |
| `crates/torrent-dht/src/lib.rs` | Re-export `lookup` module (pub(crate)) | ~+1 |
| `crates/torrent-session/src/settings.rs` | Wire `resume_data_dir` into `to_dht_config()` as `state_dir` | ~+5 |
| **Net total** | | **~+180 lines (net addition; lookup.rs offsets actor.rs reduction)** |

## Key Decisions

| # | Decision | Rationale |
|---|----------|-----------|
| 1 | DNS backoff cap 30s, total 120s | Simpler than rqbit's 24hr window. We just need to survive transient DNS hiccups (typical resolver timeout is 5-30s). 120s total covers 6-7 retries — enough for any reasonable DNS outage. |
| 2 | Unified `IterativeLookup<C>` with callbacks trait | DRY: three code paths share identical "closest-K, Alpha unqueried, sort, truncate" logic. The trait is internal (`pub(crate)`) — no API surface change. Callbacks handle the divergent parts (peer extraction, item validation, routing table population). |
| 3 | Node cap 512 (configurable) | Matches rqbit. 512 nodes is sufficient for a well-distributed routing table (theoretical ideal is ~160 nodes). The cap prevents memory waste from adversarial node injection without affecting normal operation. |
| 4 | Two-phase ping: 5s bootstrap, 60s steady-state | Preserves M97's fast cold-start verification while reducing steady-state traffic 12x. 60s is still conservative vs rqbit's 225s — we can increase later if telemetry shows it's safe. |
| 5 | Actor-owned JSON persistence alongside session persistence | Self-contained: the DHT actor doesn't depend on the session save cycle. Atomic writes prevent corruption. Session-level persistence remains as fallback for apps without `state_dir`. |
| 6 | `state_dir: Option<PathBuf>` defaulting to `None` | Tests don't need filesystem persistence. Only production sessions set the path. This keeps the DHT crate test-friendly. |

## Testing (15 tests)

### Unit (9)

| # | Test | File | Validates |
|---|------|------|-----------|
| 1 | `dns_backoff_retries_on_failure` | actor.rs | DNS resolution is retried with increasing delay |
| 2 | `dns_backoff_succeeds_on_retry` | actor.rs | Successful retry after initial DNS failure proceeds normally |
| 3 | `dns_backoff_total_timeout_120s` | actor.rs | After 120s of failures, bootstrap proceeds with saved nodes |
| 4 | `routing_table_node_cap_rejects_at_limit` | routing_table.rs | Insert returns false when at max_nodes and no bad nodes to evict |
| 5 | `routing_table_node_cap_allows_eviction` | routing_table.rs | Insert succeeds at max_nodes if a bad node can be evicted |
| 6 | `routing_table_node_cap_allows_update` | routing_table.rs | Updating an existing node works even at cap |
| 7 | `routing_table_default_cap_512` | routing_table.rs | Default max_nodes is 512 |
| 8 | `iterative_lookup_next_to_query_alpha` | lookup.rs | Returns up to Alpha unqueried closest nodes |
| 9 | `iterative_lookup_exhausted` | lookup.rs | Reports exhausted when no unqueried + no pending |

### Integration (6)

| # | Test | File | Validates |
|---|------|------|-----------|
| 10 | `ping_interval_5s_during_bootstrap` | actor.rs | Questionable nodes pinged every 5s before bootstrap_complete |
| 11 | `ping_interval_60s_after_bootstrap` | actor.rs | Questionable nodes pinged at most every 60s after bootstrap_complete |
| 12 | `json_persistence_round_trip` | actor.rs | Save → shutdown → load restores routing table nodes as Questionable |
| 13 | `json_persistence_atomic_write` | actor.rs | Temp file is written first, then renamed (no partial reads) |
| 14 | `json_persistence_no_state_dir` | actor.rs | Persistence is silently skipped when state_dir is None |
| 15 | `lookup_callbacks_get_peers_extracts_peers` | lookup.rs | GetPeersCallbacks correctly extracts peers and tokens from response |

## Failure Modes

| Codepath | Failure | Test | Handling | Silent? |
|----------|---------|------|----------|---------|
| DNS backoff: all bootstrap DNS down for >120s | No DNS nodes discovered | T3 | Proceed with saved nodes only; log warning | No |
| DNS backoff: DNS resolution blocks bootstrap() for 120s | Delayed startup | — | Saved nodes still process via ping in parallel (they're sent before DNS loop). Node-count gate or 10s timeout opens bootstrap. | No |
| Node cap: all 512 nodes are Good, new nodes rejected | Lost discovery of closer nodes | T4 | Maintenance refreshes stale buckets; bad nodes get evicted naturally. 512 is generous enough. | Yes (by design) |
| JSON persistence: state_dir not writable | Save fails | T14 | Log warning, continue operation (in-memory routing table unaffected) | No |
| JSON persistence: corrupt file on load | Load fails | T12 | Log warning, treat as empty (bootstrap from scratch) | No |
| JSON persistence: rename fails (cross-device) | Atomic write fails | T13 | Fall back to direct write; log warning | No |
| Unified lookup: callback panics | Lookup aborts | — | Callbacks are our code, not user-provided. Covered by type safety + tests. | N/A |
| Two-phase ping: bootstrap never completes (all nodes timeout) | Stuck at 5s ping forever | — | 10s bootstrap timeout forces `on_bootstrap_complete()`, switching to 60s interval | No |

## NOT in Scope

- **DHT crawling / BEP 51 improvements** — separate milestone, orthogonal to reliability
- **BEP 42 node ID enforcement by default** — currently opt-in; changing default is a policy decision
- **UDP socket buffer tuning** — kernel-level, not DHT logic
- **IPv6 DHT give-up elimination** — M85 added V6 give-up after 30 retries; revisit separately
- **Announce retry logic** — announce is fire-and-forget; adding retries is a feature, not reliability
- **Rate limiter tuning** — 250 queries/s is proven sufficient; no evidence it needs adjustment
- **Session-level persistence removal** — the session `save_session_state()` path is kept for backward compatibility; we add JSON persistence alongside it, not replacing it

## Verification

```bash
# Build + test
cargo test --workspace
cargo clippy --workspace -- -D warnings

# Specifically run new DHT tests
cargo test -p torrent-dht -- --nocapture

# Manual verification: start a download, observe DHT logs
# Expect: DNS retries on failure, 60s ping interval after bootstrap, JSON file created
RUST_LOG=torrent_dht=debug cargo run -- download "<magnet>" --download-dir /tmp/test

# Check JSON persistence file exists after first maintenance tick (60s)
ls -la ~/.config/torrent/dht_state.json

# Benchmark: 10-trial reliability test
./benchmarks/analyze.sh "<arch iso magnet>" -n 10
# Primary metric: DHT reliability (10/10 target)
# Secondary: no speed regression from M104 baseline
```

## Success Criteria

- **10/10 DHT bootstrap reliability** — no cold-start failures on 10-trial benchmark runs (was 8/10 in M103)
- **DNS transient failure resilience** — bootstrap succeeds even when 1-2 DNS servers are temporarily unreachable
- **Steady-state ping traffic reduced 12x** — from every 5s to every 60s after bootstrap
- **Routing table bounded** — never exceeds 512 nodes (configurable)
- **No speed regression** — M105 is reliability-focused; mean speed should match M104 baseline within noise margin
- **~300 lines of duplicated lookup code eliminated** — replaced by unified `IterativeLookup<C>` in lookup.rs
- **JSON routing table persisted** — `dht_state.json` created within 60s of first bootstrap, loads correctly on restart
