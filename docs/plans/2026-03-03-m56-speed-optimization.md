# M56: Speed Optimization — DHT Persistence, Queue Depth, Piece Stealing

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the 4x download speed gap vs rqbit (18 MB/s → ~70+ MB/s) by implementing three optimizations observed in rqbit's codebase: DHT routing table persistence for instant peer discovery, higher initial pipeline queue depth (128 vs 2), and piece stealing from slow peers.

**Architecture:** Three independent features in ferrite-dht and ferrite-session: (1) persist and reload DHT routing table nodes across sessions so peer discovery starts instantly; (2) raise initial pipeline queue depth from 2 to 128 so peers reach full throughput immediately; (3) steal in-flight blocks from peers 10x slower than the swarm average.

**Tech Stack:** Rust 2024, tokio, ferrite workspace

---

### Task 1: DHT routing table node extraction

**Files:**
- Modify: `crates/ferrite-dht/src/routing_table.rs`
- Modify: `crates/ferrite-dht/src/actor.rs`

**Step 1: Add `all_nodes()` to RoutingTable**

In `routing_table.rs`, add a public method to extract all live nodes:

```rust
/// Return all nodes in the routing table as (id, addr) pairs.
pub fn all_nodes(&self) -> Vec<(Id20, SocketAddr)> {
    self.buckets
        .iter()
        .flat_map(|b| b.nodes.iter().map(|n| (n.id, n.addr)))
        .collect()
}
```

**Step 2: Add `GetRoutingNodes` command to DhtActor**

In `actor.rs`, add a new command variant to the DHT command enum:

```rust
GetRoutingNodes {
    tx: oneshot::Sender<Vec<(Id20, SocketAddr)>>,
},
```

Handle it in the actor event loop:

```rust
DhtCommand::GetRoutingNodes { tx } => {
    let nodes = self.routing_table.all_nodes();
    let _ = tx.send(nodes);
}
```

**Step 3: Add `get_routing_nodes()` to DhtHandle**

```rust
pub async fn get_routing_nodes(&self) -> Vec<(Id20, SocketAddr)> {
    let (tx, rx) = oneshot::channel();
    let _ = self.cmd_tx.send(DhtCommand::GetRoutingNodes { tx }).await;
    rx.await.unwrap_or_default()
}
```

**Step 4: Build to verify**

Run: `cargo build -p ferrite-dht 2>&1 | tail -5`
Expected: `Finished`

**Step 5: Commit**

```bash
git add crates/ferrite-dht/
git commit -m "feat: expose DHT routing table nodes for persistence"
```

---

### Task 2: Populate SessionState.dht_nodes on shutdown

**Files:**
- Modify: `crates/ferrite-session/src/session.rs` (or wherever Session::shutdown lives)

**Step 1: Find where SessionState is saved on shutdown**

The `SessionState.dht_nodes` field already exists but is populated with `Vec::new()`. On shutdown, query each DHT handle for its routing table nodes and populate the field.

**Step 2: Collect nodes from DHT handles**

Before saving SessionState, call `dht_handle.get_routing_nodes()` for each DHT instance (v4, v6) and convert to `DhtNodeEntry`:

```rust
let mut dht_entries = Vec::new();
for (id, addr) in dht_handle_v4.get_routing_nodes().await {
    dht_entries.push(DhtNodeEntry {
        host: addr.ip().to_string(),
        port: addr.port() as i64,
    });
}
// Same for v6 handle if present
state.dht_nodes = dht_entries;
```

**Step 3: Build and test**

Run: `cargo test -p ferrite-session 2>&1 | tail -10`
Expected: All pass

**Step 4: Commit**

```bash
git add crates/ferrite-session/
git commit -m "feat: save DHT routing table nodes on session shutdown"
```

---

### Task 3: Load saved DHT nodes on startup

**Files:**
- Modify: `crates/ferrite-session/src/session.rs` (or wherever DHT is started)

**Step 1: Prepend saved nodes to DhtConfig.bootstrap_nodes**

When starting the DHT actor, if `SessionState.dht_nodes` is non-empty, convert them to `host:port` strings and prepend to `DhtConfig.bootstrap_nodes`:

```rust
let mut bootstrap = Vec::new();
for entry in &state.dht_nodes {
    bootstrap.push(format!("{}:{}", entry.host, entry.port));
}
// Append the hardcoded fallback nodes
bootstrap.extend(config.bootstrap_nodes.iter().cloned());
config.bootstrap_nodes = bootstrap;
```

**Step 2: Build and test**

Run: `cargo test -p ferrite-session 2>&1 | tail -10`
Expected: All pass

**Step 3: Commit**

```bash
git add crates/ferrite-session/
git commit -m "feat: load saved DHT nodes on startup for instant peer discovery"
```

---

### Task 4: Raise initial queue depth to 128

**Files:**
- Modify: `crates/ferrite-session/src/pipeline.rs` (INITIAL_QUEUE_DEPTH constant)
- Modify: `crates/ferrite-session/src/settings.rs` (add initial_queue_depth setting)
- Modify: `crates/ferrite-session/src/types.rs` (wire setting to TorrentConfig)

**Step 1: Change INITIAL_QUEUE_DEPTH**

In `pipeline.rs`, change the constant:

```rust
const INITIAL_QUEUE_DEPTH: usize = 128;
```

**Step 2: Make it configurable**

Add `initial_queue_depth` to `PeerPipelineState::new()` parameter instead of using the constant:

```rust
pub fn new(max_queue_depth: usize, request_queue_time: f64, initial_queue_depth: usize) -> Self {
    Self {
        queue_depth: initial_queue_depth,
        // ...
    }
}
```

**Step 3: Add to Settings**

In `settings.rs`:

```rust
/// Initial per-peer request queue depth (default: 128).
#[serde(default = "default_initial_queue_depth")]
pub initial_queue_depth: usize,
```

Default: 128, embedded: 16, high_performance: 256.

**Step 4: Wire through TorrentConfig**

In `types.rs`, add `initial_queue_depth` field to `TorrentConfig` and populate from Settings.

**Step 5: Build and test**

Run: `cargo test -p ferrite-session 2>&1 | tail -10`
Expected: All pass

**Step 6: Commit**

```bash
git add crates/ferrite-session/src/pipeline.rs crates/ferrite-session/src/settings.rs crates/ferrite-session/src/types.rs
git commit -m "perf: raise initial queue depth from 2 to 128 per peer"
```

---

### Task 5: Implement piece stealing from slow peers

**Files:**
- Modify: `crates/ferrite-session/src/piece_selector.rs` (steal logic in pick_partial)
- Modify: `crates/ferrite-session/src/settings.rs` (steal_threshold_ratio)
- Modify: `crates/ferrite-session/src/types.rs` (wire setting)

**Step 1: Add steal_threshold_ratio to Settings**

```rust
/// Steal blocks from peers this many times slower than swarm average (default: 10.0).
#[serde(default = "default_steal_threshold_ratio")]
pub steal_threshold_ratio: f64,
```

Default: 10.0, embedded: 10.0, high_performance: 5.0.

**Step 2: Add swarm_average_rate to PickContext**

```rust
pub swarm_average_rate: f64,
pub steal_threshold_ratio: f64,
```

**Step 3: Add steal phase to pick_partial**

After the existing unassigned-block logic in `pick_partial()`, add:

```rust
// Steal phase: if no unassigned blocks, steal from slow peers
if result.is_none() {
    for (&piece, ifp) in ctx.in_flight_pieces.iter() {
        if !ctx.peer_has.get(piece as usize) { continue; }
        // Find blocks assigned to peers much slower than average
        let steal_threshold = ctx.swarm_average_rate / ctx.steal_threshold_ratio;
        let stealable: Vec<_> = ifp.assigned_blocks.iter()
            .filter(|(_, &addr)| addr != ctx.peer_addr)
            .filter(|(_, &addr)| /* peer at addr is below steal_threshold */)
            .map(|(&block, _)| block)
            .collect();
        if !stealable.is_empty() {
            // Return first stealable block for this faster peer to request
        }
    }
}
```

The exact implementation needs peer rate lookup. Options:
- Pass a `HashMap<SocketAddr, f64>` of peer rates into PickContext
- Or pass a closure/trait for rate lookup

**Step 4: Handle duplicate block delivery**

When a stolen block arrives from the faster peer, the original assignment in `InFlightPiece.assigned_blocks` needs updating. The `ChunkTracker` already handles duplicate writes safely (only accepts blocks for missing chunks).

On block receipt in `torrent.rs`, update `assigned_blocks` to remove the original slow peer's assignment.

**Step 5: Build and test**

Run: `cargo test -p ferrite-session 2>&1 | tail -10`
Expected: All pass

**Step 6: Commit**

```bash
git add crates/ferrite-session/src/piece_selector.rs crates/ferrite-session/src/settings.rs crates/ferrite-session/src/types.rs
git commit -m "perf: steal in-flight blocks from slow peers (10x threshold)"
```

---

### Task 6: Integration test and benchmark

**Files:**
- No new files

**Step 1: Run full test suite**

Run: `cargo test -p ferrite-session 2>&1 | tail -20`
Expected: All pass

**Step 2: Build release**

Run: `cargo build --release -p ferrite-cli`

**Step 3: Run benchmark (2 runs for warm DHT, 1 cold)**

First run populates DHT cache. Second run uses cached nodes.

Run: `bash benchmarks/run_benchmark.sh /tmp/archlinux.torrent 3`

**Step 4: Compare results**

Compare against M55 baseline (18.2 MB/s) and rqbit (74.0 MB/s).

**Step 5: Commit results**

```bash
git add benchmarks/results.csv
git commit -m "bench: update results after M56 speed optimizations"
```

---

### Task 7: Version bump, docs, final commit

**Files:**
- Modify: `Cargo.toml` (version 0.62.0 → 0.63.0)
- Modify: `CHANGELOG.md`
- Modify: `README.md`

**Step 1: Bump workspace version**

In root `Cargo.toml`: `version = "0.63.0"`

**Step 2: Update CHANGELOG.md**

Add M56 entry with DHT persistence, queue depth, piece stealing, and benchmark delta.

**Step 3: Update README.md**

Update version badges and test counts.

**Step 4: Commit and push**

```bash
git add Cargo.toml CHANGELOG.md README.md
git commit -m "feat: version bump to 0.63.0, changelog for M56 speed optimization"
git push origin main && git push github main
```
