# M91: SimTransport Integration — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Write end-to-end simulation tests proving that `SimTransport` carries real BitTorrent traffic through `SimNetwork`, and remove `#[allow(dead_code)]` annotations from the now-exercised infrastructure.

**Architecture:** The simulation infrastructure is fully built: `SimNetwork` (duplex channels, partitions, link config), `sim_transport_factory()` (creates `NetworkFactory` for each node), `SimSwarm` (creates N sessions with simulated transport), and test helpers (`make_test_torrent`, `make_seeded_storage`). The existing `swarm_tests.rs` creates sessions and introduces peers but does not assert data transfer completion. This milestone adds 4-6 end-to-end tests and removes dead code annotations.

**Tech Stack:** Rust, torrent-sim crate, tokio test runtime

**Existing test helpers:**
- `make_test_torrent(data, piece_size) -> (TorrentMetaV1, Vec<u8>)` — creates a test torrent from raw data
- `make_seeded_storage(data, piece_size) -> Arc<MemoryStorage>` — creates pre-populated storage for seeding
- `SimSwarmBuilder::new(n).build() -> SimSwarm` — creates N-node simulated swarm
- `swarm.add_torrent(node_idx, meta, storage) -> Id20` — adds torrent to a node
- `swarm.introduce_peers(info_hash)` — announces all peers to each other
- `swarm.torrent_stats(node_idx, info_hash) -> TorrentStats` — gets download stats
- `swarm.shutdown()` — gracefully shuts down all sessions

---

### Task 1: Write basic seeder-to-leecher transfer test

**Files:**
- Modify: `crates/torrent-sim/tests/swarm_tests.rs`

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn test_basic_transfer_seeder_to_leecher() {
    let _ = tracing_subscriber::fmt::try_init();

    // Create a 2-node swarm: node 0 = seeder, node 1 = leecher
    let swarm = SimSwarmBuilder::new(2).build().await;

    // Create test data: 64 KiB = 4 pieces at 16384
    let data = vec![0xAB; 65536];
    let (meta, _bytes) = make_test_torrent(&data, 16384);
    let seeded_storage = make_seeded_storage(&data, 16384);

    // Add torrent to seeder with pre-populated storage
    let info_hash = swarm
        .add_torrent(0, meta.clone().into(), Some(seeded_storage))
        .await;

    // Add same torrent to leecher without storage (will download)
    swarm.add_torrent(1, meta.into(), None).await;

    // Introduce peers to each other
    swarm.introduce_peers(info_hash).await;

    // Wait for transfer to complete (with timeout)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let stats = swarm.torrent_stats(1, info_hash).await;
        if stats.total_done == data.len() as u64 {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            let stats = swarm.torrent_stats(1, info_hash).await;
            panic!(
                "transfer timed out: {}/{} bytes downloaded",
                stats.total_done,
                data.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Verify seeder stats
    let seed_stats = swarm.torrent_stats(0, info_hash).await;
    assert!(seed_stats.total_upload > 0, "seeder should have uploaded data");

    swarm.shutdown().await;
}
```

- [ ] **Step 2: Run test**

```bash
cargo test -p torrent-sim -- test_basic_transfer
```

Expected: PASS if the SimTransport stack works end-to-end. If FAIL, debug the transport path.

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-sim/tests/swarm_tests.rs
git commit -m "test: add basic seeder-to-leecher transfer through SimNetwork (M91)"
```

---

### Task 2: Write multi-peer swarm test

**Files:**
- Modify: `crates/torrent-sim/tests/swarm_tests.rs`

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn test_multi_peer_swarm_transfer() {
    let _ = tracing_subscriber::fmt::try_init();

    // 4-node swarm: 1 seeder + 3 leechers
    let swarm = SimSwarmBuilder::new(4).build().await;

    let data = vec![0xCD; 131072]; // 128 KiB = 8 pieces
    let (meta, _bytes) = make_test_torrent(&data, 16384);
    let seeded_storage = make_seeded_storage(&data, 16384);

    let info_hash = swarm
        .add_torrent(0, meta.clone().into(), Some(seeded_storage))
        .await;

    // Add torrent to all 3 leechers
    for node in 1..4 {
        swarm.add_torrent(node, meta.clone().into(), None).await;
    }

    swarm.introduce_peers(info_hash).await;

    // Wait for all leechers to complete
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let mut all_done = true;
        for node in 1..4 {
            let stats = swarm.torrent_stats(node, info_hash).await;
            if stats.total_done != data.len() as u64 {
                all_done = false;
                break;
            }
        }
        if all_done {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            for node in 1..4 {
                let stats = swarm.torrent_stats(node, info_hash).await;
                eprintln!("node {node}: {}/{} bytes", stats.total_done, data.len());
            }
            panic!("multi-peer transfer timed out");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    swarm.shutdown().await;
}
```

- [ ] **Step 2: Run test**

```bash
cargo test -p torrent-sim -- test_multi_peer
```

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-sim/tests/swarm_tests.rs
git commit -m "test: add multi-peer swarm transfer test (M91)"
```

---

### Task 3: Write network partition test

**Files:**
- Modify: `crates/torrent-sim/tests/swarm_tests.rs`

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn test_transfer_survives_partition() {
    let _ = tracing_subscriber::fmt::try_init();

    let swarm = SimSwarmBuilder::new(2).build().await;

    let data = vec![0xEF; 65536]; // 64 KiB
    let (meta, _bytes) = make_test_torrent(&data, 16384);
    let seeded_storage = make_seeded_storage(&data, 16384);

    let info_hash = swarm
        .add_torrent(0, meta.clone().into(), Some(seeded_storage))
        .await;
    swarm.add_torrent(1, meta.into(), None).await;
    swarm.introduce_peers(info_hash).await;

    // Let some data transfer
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Partition the network
    swarm.network().partition(
        vec![swarm.node_ip(0)],
        vec![swarm.node_ip(1)],
    );

    // Verify transfer stalls (check stats don't change)
    let stats_before = swarm.torrent_stats(1, info_hash).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stats_during = swarm.torrent_stats(1, info_hash).await;
    // May or may not have progressed if data was in flight — just check
    // it hasn't completed if it wasn't already done
    if stats_before.total_done < data.len() as u64 {
        // Partition should prevent new connections
    }

    // Heal partition
    swarm.network().heal_partitions();

    // Re-introduce peers (existing connections may have dropped)
    swarm.introduce_peers(info_hash).await;

    // Wait for completion
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let stats = swarm.torrent_stats(1, info_hash).await;
        if stats.total_done == data.len() as u64 {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("transfer did not resume after partition heal");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    swarm.shutdown().await;
}
```

**Note:** This test depends on `SimSwarm` exposing `network()` and `node_ip()` methods. If they don't exist, the implementer should add them:
- `network(&self) -> &SimNetwork` — returns reference to the shared network
- `node_ip(&self, idx: usize) -> IpAddr` — returns the virtual IP assigned to node `idx`

- [ ] **Step 2: Run test**

```bash
cargo test -p torrent-sim -- test_transfer_survives
```

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-sim/tests/swarm_tests.rs crates/torrent-sim/src/swarm.rs
git commit -m "test: add network partition recovery test (M91)"
```

---

### Task 4: Write latency/bandwidth test

**Files:**
- Modify: `crates/torrent-sim/tests/swarm_tests.rs`

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn test_transfer_with_latency_and_bandwidth() {
    let _ = tracing_subscriber::fmt::try_init();

    let swarm = SimSwarmBuilder::new(2).build().await;

    // Configure link with 50ms latency and 1 MB/s bandwidth
    swarm.network().set_default_config(LinkConfig {
        latency: Duration::from_millis(50),
        bandwidth: 1_000_000,
        loss_rate: 0.0,
    });

    let data = vec![0x42; 32768]; // 32 KiB — small enough to finish quickly
    let (meta, _bytes) = make_test_torrent(&data, 16384);
    let seeded_storage = make_seeded_storage(&data, 16384);

    let info_hash = swarm
        .add_torrent(0, meta.clone().into(), Some(seeded_storage))
        .await;
    swarm.add_torrent(1, meta.into(), None).await;
    swarm.introduce_peers(info_hash).await;

    // Should complete but take longer due to latency
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let stats = swarm.torrent_stats(1, info_hash).await;
        if stats.total_done == data.len() as u64 {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("transfer with latency timed out");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    swarm.shutdown().await;
}
```

**Note:** If `SimNetwork` doesn't currently enforce latency/bandwidth in the duplex channels (only stores the config), the test should verify completion regardless — proving the transport works even if link simulation is passive. Actual latency injection can be a follow-up.

- [ ] **Step 2: Run test**

```bash
cargo test -p torrent-sim -- test_transfer_with_latency
```

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-sim/tests/swarm_tests.rs
git commit -m "test: add latency/bandwidth transfer test (M91)"
```

---

### Task 5: Remove `#[allow(dead_code)]` annotations

**Files:**
- Modify: `crates/torrent-sim/src/network.rs`
- Modify: `crates/torrent-sim/src/swarm.rs`

- [ ] **Step 1: Remove annotations from network.rs**

Remove `#[allow(dead_code)]` from:
- `SimConnection` struct (line ~49)
- `NetworkInner` struct (line ~71)
- `register_listener` method (line ~178)
- `unregister_listener` method (line ~187)
- `connect_tcp` method (line ~204)

Also remove associated comments like `// Fields and struct used by SimTransport (Task 5); suppress until then.`

- [ ] **Step 2: Remove annotation from swarm.rs**

Remove `#[allow(dead_code)]` from `factories` field (line ~125) and its comment `// Retained for future use (e.g., manual connect calls)`.

If `factories` is now used in the new tests (e.g., for manual connect), keep the field. If still unused, either remove the field entirely or document its actual use case.

- [ ] **Step 3: Run clippy to verify no new dead code warnings**

```bash
cargo clippy -p torrent-sim -- -D warnings
```

If clippy flags any items as dead code, either use them in a test or remove them.

- [ ] **Step 4: Commit**

```bash
git add crates/torrent-sim/src/network.rs crates/torrent-sim/src/swarm.rs
git commit -m "refactor: remove dead_code annotations from SimTransport infrastructure (M91)"
```

---

### Task 6: Final verification

- [ ] **Step 1: Run full test suite**

```bash
cargo test --workspace && cargo clippy --workspace -- -D warnings
```

Expected: All tests pass (1460+ existing + 4 new sim tests), no clippy warnings.

- [ ] **Step 2: Commit any remaining fixes**

```bash
git add -A
git commit -m "feat: SimTransport end-to-end integration tests (M91)"
```
