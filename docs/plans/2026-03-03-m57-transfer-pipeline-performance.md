# M57: Transfer Pipeline Performance

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the 5-6x speed gap between ferrite (~15 MB/s) and rqbit (~85 MB/s) by fixing connection management, hot-path allocations, and data copy waste in the transfer pipeline.

**Architecture:** The torrent actor's main loop handles peer events, piece selection, and disk I/O in a single `tokio::select!` with 16 branches. The bottlenecks are: (1) TCP connections to unreachable peers block for up to 2 minutes (OS default) instead of 2 seconds, starving connection slots; (2) `missing_chunks()` allocates a new `Vec` on every call — called ~9 times per peer per request cycle; (3) `Bytes::copy_from_slice(data)` copies every received block before writing to disk; (4) `available_peers` uses O(N) linear scan for dedup. We also lack benchmark instrumentation to measure peer counts, so we're partially blind.

**Tech Stack:** Rust 2024, tokio, `bytes` crate, ferrite workspace (13 crates)

---

### Task 1: Instrument benchmark with peer count logging

**Files:**
- Modify: `crates/ferrite-cli/src/download.rs:141-158` (progress display)
- Modify: `benchmarks/run_benchmark.sh:48-57` (capture stderr)

**Step 1: Add peer count to quiet-mode stderr output**

In `download.rs`, the quiet mode (`-q`) suppresses the progress bar. Add a one-line stderr summary on completion that includes peak peer count and total peers seen. This requires tracking max peers across progress ticks.

After the existing progress loop (around line 160), before the download completes, add a final stderr line:

```rust
// Inside the progress loop, track peak peers
let mut peak_peers: u32 = 0;
// ... in the loop body:
peak_peers = peak_peers.max(stats.peers_connected);
// ... after download completes:
eprintln!("ferrite: peak_peers={peak_peers} final_speed={:.1}MB/s",
    stats.download_rate as f64 / 1_048_576.0);
```

**Step 2: Run test to verify it compiles**

Run: `cargo build -p ferrite-cli 2>&1 | tail -5`
Expected: successful build

**Step 3: Update benchmark script to capture ferrite stats**

In `run_benchmark.sh`, redirect ferrite's stderr to a separate log so we can parse the peer count:

```bash
# In the ferrite run_trial call, capture stderr separately
run_trial "ferrite" "$trial" \
    "$FERRITE download '$MAGNET' -o '$OUTPUT_DIR/ferrite' -q 2>'$OUTPUT_DIR/ferrite-stats-${trial}.txt'"
```

After the trial loop, print ferrite stats:

```bash
echo "=== Ferrite peer stats ==="
cat "$OUTPUT_DIR"/ferrite-stats-*.txt 2>/dev/null || true
```

**Step 4: Commit**

```bash
git add crates/ferrite-cli/src/download.rs benchmarks/run_benchmark.sh
git commit -m "feat: instrument benchmark with peak peer count logging"
```

---

### Task 2: Add TCP connect timeout (2 seconds)

This is likely the single highest-impact fix. Currently, `TcpStream::connect(addr).await` uses the OS default timeout (~2 minutes on Linux). Unreachable peers block a connection slot for 2 minutes. rqbit uses a 2-second timeout.

**Files:**
- Modify: `crates/ferrite-session/src/transport.rs:167-172` (TCP connect)
- Modify: `crates/ferrite-session/src/settings.rs` (add `peer_connect_timeout` setting)
- Modify: `crates/ferrite-session/src/types.rs` (add to TorrentConfig)
- Modify: `crates/ferrite-session/src/torrent.rs:5236-5242` (use timeout)
- Test: `cargo test --workspace`

**Step 1: Add `peer_connect_timeout` to Settings**

In `settings.rs`, add a new field to `Settings`:

```rust
#[serde(default = "default_peer_connect_timeout")]
pub peer_connect_timeout: u64,  // seconds, default 2
```

Add the default helper:

```rust
fn default_peer_connect_timeout() -> u64 { 2 }
```

Update `min_memory_usage()` preset: `peer_connect_timeout: 2`
Update `high_performance_seed()` preset: `peer_connect_timeout: 2`
Update the manual `PartialEq` impl to include the new field.

**Step 2: Add to TorrentConfig**

In `types.rs`, add:

```rust
/// TCP connect timeout in seconds (0 = OS default).
pub peer_connect_timeout: u64,
```

Default: `peer_connect_timeout: 2`

Update `From<&Settings>` impl to wire the field.

**Step 3: Wrap TCP connect with timeout**

In `torrent.rs`, around line 5241, wrap `factory.connect_tcp(addr).await` with a timeout:

```rust
let tcp_timeout = Duration::from_secs(peer_connect_timeout);
let tcp_result = if use_proxy {
    // proxy connections use their own timeout
    crate::proxy::connect_through_proxy(&proxy_config, addr).await
        .map(crate::transport::BoxedStream::new)
} else if peer_connect_timeout > 0 {
    match tokio::time::timeout(tcp_timeout, factory.connect_tcp(addr)).await {
        Ok(result) => result,
        Err(_) => {
            debug!(%addr, "TCP connect timed out after {peer_connect_timeout}s");
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))
        }
    }
} else {
    factory.connect_tcp(addr).await
};
```

The `peer_connect_timeout` variable needs to be captured into the spawned task. Add it alongside the other captured variables in the `try_connect_peers` → `tokio::spawn` block (around line 5145-5188). It should be read from `self.config.peer_connect_timeout`.

**Step 4: Run tests**

Run: `cargo test --workspace 2>&1 | grep "test result:"`
Expected: all pass (1152+)

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/settings.rs crates/ferrite-session/src/types.rs crates/ferrite-session/src/torrent.rs
git commit -m "perf: add 2-second TCP connect timeout (was OS default ~2min)"
```

---

### Task 3: Cache `missing_chunks()` per request cycle

`missing_chunks()` is called ~9 times in `piece_selector.rs` for every peer's request cycle. Each call allocates a new `Vec<(u32, u32)>`. With 50 peers and 50 in-flight pieces, that's thousands of allocations per second in the hottest loop.

**Files:**
- Modify: `crates/ferrite-session/src/piece_selector.rs` (all methods that call `missing_chunks`)
- Modify: `crates/ferrite-session/src/torrent.rs:4575-4581` (pass cache)
- Test: `cargo test -p ferrite-session`

**Step 1: Add a cache to `pick_blocks()`**

In `piece_selector.rs`, modify `pick_blocks()` to build a `HashMap<u32, Vec<(u32, u32)>>` cache, and change the `missing_chunks` closure to use it:

```rust
pub fn pick_blocks<F>(
    &self,
    ctx: &PickContext<'_>,
    missing_chunks_fn: F,
) -> Option<PickResult>
where
    F: Fn(u32) -> Vec<(u32, u32)>,
{
    // Cache: compute missing_chunks once per piece, reuse across all pick methods
    let mut cache: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
    let cached_missing = |piece: u32, cache: &mut HashMap<u32, Vec<(u32, u32)>>| -> &Vec<(u32, u32)> {
        cache.entry(piece).or_insert_with(|| missing_chunks_fn(piece))
    };
    // ... pass cache through to sub-methods
}
```

However, Rust borrow rules make a mutable cache inside closures tricky. The practical approach: use `RefCell<HashMap<u32, Vec<(u32, u32)>>>` wrapping the closure:

```rust
use std::cell::RefCell;

let cache: RefCell<HashMap<u32, Vec<(u32, u32)>>> = RefCell::new(HashMap::new());
let cached_missing = |piece: u32| -> Vec<(u32, u32)> {
    let mut c = cache.borrow_mut();
    c.entry(piece).or_insert_with(|| missing_chunks_fn(piece)).clone()
};
```

This still clones on each call but the underlying `missing_chunks_fn` is only called once per piece. Better approach — change the signature to accept a `&dyn Fn` and pre-populate for all in-flight pieces:

In `torrent.rs:4575`, pre-compute the cache before calling `pick_blocks`:

```rust
let mut chunks_cache: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
if let Some(ref ct) = self.chunk_tracker {
    for &piece in self.in_flight_pieces.keys() {
        chunks_cache.insert(piece, ct.missing_chunks(piece));
    }
}
let missing_chunks_fn = |piece: u32| -> Vec<(u32, u32)> {
    chunks_cache.get(&piece).cloned()
        .unwrap_or_else(|| {
            self.chunk_tracker.as_ref()
                .map(|ct| ct.missing_chunks(piece))
                .unwrap_or_default()
        })
};
```

This pre-computes for all in-flight pieces (the hot path), and falls back to on-demand for new pieces (cold path, called once).

**Step 2: Run tests**

Run: `cargo test -p ferrite-session 2>&1 | grep "test result:"`
Expected: all pass

**Step 3: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "perf: cache missing_chunks() per request cycle (eliminate repeated allocs)"
```

---

### Task 4: Eliminate `Bytes::copy_from_slice` in write path

In `torrent.rs:3124`, every received piece block is copied before writing to disk:

```rust
disk.write_chunk(index, begin, Bytes::copy_from_slice(data), ...)
```

The `data` comes from the peer's wire protocol decoder, which already has the data in a `BytesMut` buffer. We should pass ownership of the `Bytes` directly instead of copying.

**Files:**
- Modify: `crates/ferrite-session/src/peer.rs` (send owned Bytes in PeerEvent)
- Modify: `crates/ferrite-session/src/torrent.rs:3115-3128` (accept owned Bytes)
- Test: `cargo test --workspace`

**Step 1: Check PeerEvent::PieceData current definition**

Read `peer.rs` to find `PeerEvent::PieceData` and the wire message handling that constructs it. The key question: does the wire codec already give us `Bytes` (zero-copy frozen from BytesMut), or does it give us `&[u8]`?

If the wire codec provides `BytesMut` or `Bytes`, we can pass it through. If it provides `&[u8]`, the copy must happen somewhere — but we want it to happen once (at the codec level as `BytesMut::freeze()`) rather than twice.

```rust
// Change PeerEvent::PieceData from:
PieceData { peer_addr: SocketAddr, index: u32, begin: u32, data: Vec<u8> }
// To:
PieceData { peer_addr: SocketAddr, index: u32, begin: u32, data: Bytes }
```

Then in `handle_piece_data`, change signature from `data: &[u8]` to `data: Bytes` and pass directly:

```rust
disk.write_chunk(index, begin, data, DiskJobFlags::empty()).await
```

**Step 2: Update wire codec to produce Bytes**

Check `crates/ferrite-wire/src/codec.rs` — the decoder uses `src.split_to()` which returns `BytesMut`. After parsing the message, the piece data portion can be `.freeze()`d into `Bytes` for zero-copy transfer.

**Step 3: Run tests**

Run: `cargo test --workspace 2>&1 | grep "test result:"`
Expected: all pass

**Step 4: Commit**

```bash
git add crates/ferrite-wire/src/codec.rs crates/ferrite-session/src/peer.rs crates/ferrite-session/src/torrent.rs
git commit -m "perf: zero-copy piece data from wire codec to disk (eliminate Bytes::copy_from_slice)"
```

---

### Task 5: Replace `available_peers` Vec with HashSet-backed dedup

`handle_add_peers()` in `torrent.rs:1974` uses `self.available_peers.iter().any(|(a, _)| *a == addr)` for O(N) dedup on every peer add. With hundreds of peers from DHT/PEX, this is wasteful.

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` (TorrentActor fields, handle_add_peers, try_connect_peers, sort_available_peers)
- Test: `cargo test -p ferrite-session`

**Step 1: Add a HashSet alongside the Vec**

Add a `available_peers_set: HashSet<SocketAddr>` field to `TorrentActor`. Keep the `Vec` for priority ordering (needed by `sort_available_peers` and `pop()`), but use the `HashSet` for O(1) dedup checks.

```rust
// In TorrentActor struct:
available_peers_set: HashSet<SocketAddr>,

// In handle_add_peers:
if !self.peers.contains_key(&addr)
    && self.available_peers_set.insert(addr)  // O(1) insert-if-absent
{
    self.available_peers.push((addr, source));
    added = true;
}

// In try_connect_peers, when popping:
if let Some((addr, source)) = self.available_peers.pop() {
    self.available_peers_set.remove(&addr);
    // ... rest of connection logic
}
```

**Step 2: Run tests**

Run: `cargo test -p ferrite-session 2>&1 | grep "test result:"`
Expected: all pass

**Step 3: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "perf: O(1) peer dedup with HashSet (was O(N) linear scan)"
```

---

### Task 6: Benchmark and measure improvement

**Files:**
- Run: `benchmarks/run_benchmark.sh`
- Update: `benchmarks/results.csv`

**Step 1: Build release binary**

Run: `cargo build --release -p ferrite-cli`

**Step 2: Run benchmark**

Run: `./benchmarks/run_benchmark.sh "magnet:?xt=urn:btih:..." 3`

Use the same Arch Linux ISO magnet as previous benchmarks.

**Step 3: Record results and peer count**

Compare:
- M56 baseline: ferrite 15.0 MB/s, rqbit 84.9 MB/s
- M57 result: ferrite ??? MB/s

Check the ferrite stats output for peak peer count. Compare with rqbit peer count if available.

**Step 4: Commit results**

```bash
git add benchmarks/results.csv
git commit -m "bench: update results after M57 transfer pipeline performance"
```

---

### Task 7: Version bump, docs, final commit

**Step 1: Bump workspace version**

In root `Cargo.toml`: `version = "0.64.0"`

**Step 2: Update CHANGELOG.md**

Add entry for 0.64.0 — M57: Transfer Pipeline Performance, covering:
- TCP connect timeout (2s)
- missing_chunks cache
- Zero-copy piece data
- O(1) peer dedup
- Benchmark instrumentation
- Results

**Step 3: Update README.md**

Update version badge to 0.64.0.

**Step 4: Commit and push**

```bash
git add Cargo.toml CHANGELOG.md README.md
git commit -m "feat: version bump to 0.64.0, changelog for M57 transfer pipeline performance"
git push origin main && git push github main
```
