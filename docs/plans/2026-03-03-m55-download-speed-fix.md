# M55: Download Speed Optimization — Immediate Peer Connection

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the 4x download speed gap vs rqbit by fixing peer connection delays, raising the default peer limit, and making max_peers configurable via Settings.

**Architecture:** Three targeted fixes in ferrite-session: (1) call `try_connect_peers()` after every `handle_add_peers()` so DHT/PEX/command-added peers connect immediately instead of waiting up to 30s; (2) raise default `max_peers` from 50→200 to match peer counts observed in rqbit benchmarks; (3) add `max_peers_per_torrent` to `Settings` so users can configure it. Also reduce `connect_interval` from 30s→5s for faster catch-up.

**Tech Stack:** Rust 2024, tokio, ferrite workspace

---

### Task 1: Add `try_connect_peers()` after DHT peer arrival

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:1845` (DHT v4)
- Modify: `crates/ferrite-session/src/torrent.rs:1863` (DHT v6)
- Modify: `crates/ferrite-session/src/torrent.rs:1881` (DHT v4 v2-swarm)
- Modify: `crates/ferrite-session/src/torrent.rs:1899` (DHT v6 v2-swarm)

**Step 1: Add `try_connect_peers()` calls after each DHT handle_add_peers**

In the torrent actor event loop, after each DHT peer batch arrives, immediately try to connect:

```rust
// DHT v4 (line ~1845)
Some(peers) => {
    debug!(count = peers.len(), "DHT v4 returned peers");
    self.handle_add_peers(peers, PeerSource::Dht);
    self.try_connect_peers();  // <-- ADD THIS
}

// DHT v6 (line ~1863)
Some(peers) => {
    debug!(count = peers.len(), "DHT v6 returned peers");
    self.handle_add_peers(peers, PeerSource::Dht);
    self.try_connect_peers();  // <-- ADD THIS
}

// DHT v4 v2-swarm (line ~1881)
Some(peers) => {
    debug!(count = peers.len(), "DHT v4 v2-swarm returned peers");
    self.handle_add_peers(peers, PeerSource::Dht);
    self.try_connect_peers();  // <-- ADD THIS
}

// DHT v6 v2-swarm (line ~1899)
Some(peers) => {
    debug!(count = peers.len(), "DHT v6 v2-swarm returned peers");
    self.handle_add_peers(peers, PeerSource::Dht);
    self.try_connect_peers();  // <-- ADD THIS
}
```

**Step 2: Build to verify no compile errors**

Run: `cargo build -p ferrite-session 2>&1 | tail -5`
Expected: `Finished`

**Step 3: Run existing tests**

Run: `cargo test -p ferrite-session 2>&1 | tail -5`
Expected: All pass, no regressions

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "perf: connect to DHT peers immediately instead of waiting 30s"
```

---

### Task 2: Add `try_connect_peers()` after PEX and AddPeers command

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:2964` (PEX)
- Modify: `crates/ferrite-session/src/torrent.rs:1522` (AddPeers command)

**Step 1: Add `try_connect_peers()` after PEX peers**

```rust
// PEX (line ~2964)
PeerEvent::PexPeers { new_peers } => {
    if self.config.enable_pex {
        self.handle_add_peers(new_peers, PeerSource::Pex);
        self.try_connect_peers();  // <-- ADD THIS
    }
}
```

**Step 2: Add `try_connect_peers()` after AddPeers command**

```rust
// AddPeers command (line ~1522)
Some(TorrentCommand::AddPeers { peers, source }) => {
    self.handle_add_peers(peers, source);
    self.try_connect_peers();  // <-- ADD THIS
}
```

**Step 3: Build and test**

Run: `cargo test -p ferrite-session 2>&1 | tail -5`
Expected: All pass

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "perf: connect to PEX and manually-added peers immediately"
```

---

### Task 3: Raise default max_peers from 50 to 200

**Files:**
- Modify: `crates/ferrite-session/src/types.rs:118` (Default impl)
- Modify: `crates/ferrite-session/src/types.rs:171` (From<&Settings> impl)
- Modify: `crates/ferrite-session/src/torrent.rs:5797` (test_config helper)

**Step 1: Update defaults**

In `types.rs`, change `max_peers: 50` to `max_peers: 200` in both the `Default` impl (line 118) and the `From<&Settings>` impl (line 171).

```rust
// Default impl (line ~118)
max_peers: 200,

// From<&Settings> impl (line ~171)
max_peers: 200,
```

**Step 2: Update test helper**

In `torrent.rs` test helper `test_config()` (line ~5797), update:

```rust
max_peers: 200,
```

**Step 3: Fix any test assertions that check max_peers == 50**

Search for `max_peers` or `connections_limit` assertions in tests and update.

Run: `cargo test -p ferrite-session 2>&1 | tail -20`
Expected: All pass (fix any assertion failures)

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/types.rs crates/ferrite-session/src/torrent.rs
git commit -m "perf: raise default max_peers from 50 to 200"
```

---

### Task 4: Add `max_peers_per_torrent` to Settings

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs` (add field + defaults)
- Modify: `crates/ferrite-session/src/types.rs:171` (read from Settings)

**Step 1: Add the field to Settings**

In `settings.rs`, add after the existing peer-related fields:

```rust
/// Maximum peer connections per torrent (default: 200).
#[serde(default = "default_max_peers_per_torrent")]
pub max_peers_per_torrent: usize,
```

Add default function:

```rust
fn default_max_peers_per_torrent() -> usize { 200 }
```

Add to `Default` impl, `embedded()`, and `high_performance()`:
- Default: 200
- Embedded: 30
- High Performance: 500

**Step 2: Wire into TorrentConfig**

In `types.rs` `From<&Settings>` impl, change:

```rust
max_peers: s.max_peers_per_torrent,
```

**Step 3: Update settings tests**

Update the settings test assertions for default, embedded, and high_performance presets.

**Step 4: Build and test**

Run: `cargo test -p ferrite-session 2>&1 | tail -10`
Expected: All pass

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/settings.rs crates/ferrite-session/src/types.rs
git commit -m "feat: add max_peers_per_torrent to Settings"
```

---

### Task 5: Reduce connect_interval from 30s to 5s

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:1427`

**Step 1: Change the interval**

```rust
// Line ~1427
let mut connect_interval = tokio::time::interval(Duration::from_secs(5));
```

This is the catch-up timer. Even with immediate connection on peer discovery, this periodic sweep handles edge cases (peers added while at capacity, transient failures).

**Step 2: Update any test comments referencing 30s connect interval**

Search for "30s" or "30 second" in torrent.rs test comments and update to reflect 5s.

**Step 3: Build and test**

Run: `cargo test -p ferrite-session 2>&1 | tail -10`
Expected: All pass

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "perf: reduce peer connect interval from 30s to 5s"
```

---

### Task 6: Rebuild CLI, re-run benchmark, update results

**Files:**
- Modify: `benchmarks/results.csv` (updated results)

**Step 1: Build release**

Run: `cargo build --release -p ferrite-cli`

**Step 2: Run benchmark**

Run: `bash benchmarks/run_benchmark.sh /tmp/archlinux.torrent 3`

**Step 3: Compare results**

Compare new ferrite speed against previous 18.2 MB/s baseline.

**Step 4: Commit results**

```bash
git add benchmarks/results.csv
git commit -m "bench: update results after M55 peer connection optimizations"
```

---

### Task 7: Version bump, docs, final commit

**Files:**
- Modify: `Cargo.toml` (version 0.61.0 → 0.62.0)
- Modify: `CHANGELOG.md`
- Modify: `README.md`

**Step 1: Bump workspace version**

In root `Cargo.toml`: `version = "0.62.0"`

**Step 2: Update CHANGELOG.md**

Add M55 entry with the performance improvements and benchmark delta.

**Step 3: Update README.md**

Update version badges and any relevant test counts.

**Step 4: Commit and push**

```bash
git add Cargo.toml CHANGELOG.md README.md
git commit -m "feat: version bump to 0.62.0, changelog for M55 download speed optimization"
git push origin main && git push github main
```
