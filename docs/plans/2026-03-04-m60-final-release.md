# M60: Final Release — `torrent` v0.65.0 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Rename the ferrite workspace to `torrent`, re-apply the non-blocking transfer pipeline for 2–3x download speed, harden for production, scaffold fuzzing, and publish 11 crates to crates.io.

**Architecture:** 5 sequential phases — identity rename, performance restoration, code quality, fuzz scaffolding, publication. The rename is a big-bang atomic change. The pipeline re-implementation follows the existing M58 design doc. Quality and fuzzing are audits with targeted fixes. Publication is the final gate.

**Tech Stack:** Rust (edition 2024), tokio, bytes, sha2, cargo-fuzz, criterion, cargo publish

**Design doc:** `docs/plans/2026-03-04-m60-final-release-design.md`

**Reference:** The existing M58 non-blocking pipeline plan at `docs/plans/2026-03-04-m58-non-blocking-transfer-pipeline.md` contains 866 lines of detailed implementation steps. Task 4 of this plan references it — adapt all file paths from `ferrite-*` to `torrent-*`.

---

## Phase 1: Identity — Rename `ferrite` → `torrent`

### Task 1: Rename Crate Directories and Cargo.toml Files

**Goal:** Rename all 12 crate directories and update every Cargo.toml so the workspace compiles under `torrent-*` names.

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/ferrite-bencode/Cargo.toml` → `crates/torrent-bencode/Cargo.toml`
- Modify: `crates/ferrite-core/Cargo.toml` → `crates/torrent-core/Cargo.toml`
- Modify: `crates/ferrite-wire/Cargo.toml` → `crates/torrent-wire/Cargo.toml`
- Modify: `crates/ferrite-tracker/Cargo.toml` → `crates/torrent-tracker/Cargo.toml`
- Modify: `crates/ferrite-dht/Cargo.toml` → `crates/torrent-dht/Cargo.toml`
- Modify: `crates/ferrite-storage/Cargo.toml` → `crates/torrent-storage/Cargo.toml`
- Modify: `crates/ferrite-utp/Cargo.toml` → `crates/torrent-utp/Cargo.toml`
- Modify: `crates/ferrite-nat/Cargo.toml` → `crates/torrent-nat/Cargo.toml`
- Modify: `crates/ferrite-session/Cargo.toml` → `crates/torrent-session/Cargo.toml`
- Modify: `crates/ferrite/Cargo.toml` → `crates/torrent/Cargo.toml`
- Modify: `crates/ferrite-sim/Cargo.toml` → `crates/torrent-sim/Cargo.toml`
- Modify: `crates/ferrite-cli/Cargo.toml` → `crates/torrent-cli/Cargo.toml`

**Step 1: Rename all crate directories**

```bash
cd /mnt/TempNVME/projects/ferrite
for dir in crates/ferrite-*; do
  newdir="${dir/ferrite/torrent}"
  mv "$dir" "$newdir"
done
mv crates/ferrite crates/torrent
```

Expected: 12 directories renamed. `ls crates/` shows `torrent-bencode`, `torrent-core`, `torrent-wire`, `torrent-tracker`, `torrent-dht`, `torrent-storage`, `torrent-utp`, `torrent-nat`, `torrent-session`, `torrent`, `torrent-sim`, `torrent-cli`.

**Step 2: Update root Cargo.toml**

In `/mnt/TempNVME/projects/ferrite/Cargo.toml`:
- Change `version = "0.64.0"` → `version = "0.65.0"`
- Change `repository = "https://codeberg.org/alan090/ferrite"` → `repository = "https://codeberg.org/alan090/torrent"`
- All workspace dependency paths are `crates/*` glob — no changes needed for member resolution.

**Step 3: Update every crate Cargo.toml — package name**

Use sed across all crate Cargo.toml files:

```bash
cd /mnt/TempNVME/projects/ferrite
# Update package names
find crates/ -name "Cargo.toml" -exec sed -i 's/name = "ferrite-/name = "torrent-/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/name = "ferrite"/name = "torrent"/g' {} +

# Update inter-crate dependency references
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-bencode/torrent-bencode/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-core/torrent-core/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-wire/torrent-wire/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-tracker/torrent-tracker/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-dht/torrent-dht/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-storage/torrent-storage/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-utp/torrent-utp/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-nat/torrent-nat/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-session/torrent-session/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-sim/torrent-sim/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite-cli/torrent-cli/g' {} +

# Update description strings
find crates/ -name "Cargo.toml" -exec sed -i 's/ferrite crate family/torrent crate family/g' {} +
find crates/ -name "Cargo.toml" -exec sed -i 's/for ferrite/for torrent/g' {} +
```

**Step 4: Update torrent-cli binary name and publish flag**

In `crates/torrent-cli/Cargo.toml`:
- Change `[[bin]] name = "ferrite"` → `name = "torrent"`
- Add `publish = false` to `[package]` section.

**Step 5: Add crates.io metadata to all publishable crates**

For each of the 11 publishable crates (everything except `torrent-cli`), ensure `[package]` contains:

```toml
keywords = ["bittorrent", "torrent", "p2p", "download"]
categories = ["network-programming"]
homepage = "https://codeberg.org/alan090/torrent"
documentation = "https://docs.rs/torrent"
```

Adjust `documentation` per crate (e.g., `https://docs.rs/torrent-wire`).

**Step 6: Verify workspace resolves**

```bash
cargo metadata --no-deps 2>&1 | head -5
```

Expected: No errors. Workspace resolves with all `torrent-*` crate names.

Do NOT run `cargo build` yet — source code still has `ferrite_*` imports that will fail.

---

### Task 2: Update All Source Code Paths

**Goal:** Replace every `ferrite_*` and `ferrite::` reference in Rust source code so the workspace compiles and all 1378 tests pass.

**Files:**
- Modify: All `*.rs` files under `crates/` (~705 `ferrite_` identifiers, ~158 `use ferrite` imports)

**Step 1: Replace crate-level import paths**

```bash
cd /mnt/TempNVME/projects/ferrite

# Replace extern crate and use statements
find crates/ -name "*.rs" -exec sed -i 's/ferrite_bencode/torrent_bencode/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite_core/torrent_core/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite_wire/torrent_wire/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite_tracker/torrent_tracker/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite_dht/torrent_dht/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite_storage/torrent_storage/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite_utp/torrent_utp/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite_nat/torrent_nat/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite_session/torrent_session/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite_sim/torrent_sim/g' {} +

# Replace facade-level imports (use ferrite::)
find crates/ -name "*.rs" -exec sed -i 's/use ferrite::/use torrent::/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::prelude/torrent::prelude/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::client/torrent::client/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::error/torrent::error/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::core/torrent::core/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::session/torrent::session/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::dht/torrent::dht/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::wire/torrent::wire/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::tracker/torrent::tracker/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::storage/torrent::storage/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::bencode/torrent::bencode/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::utp/torrent::utp/g' {} +
find crates/ -name "*.rs" -exec sed -i 's/ferrite::nat/torrent::nat/g' {} +
```

**Step 2: Update string literals, doc comments, and display impls**

```bash
cd /mnt/TempNVME/projects/ferrite

# CLI about text
sed -i 's/powered by ferrite/powered by torrent/g' crates/torrent-cli/src/main.rs
sed -i 's/name = "ferrite"/name = "torrent"/g' crates/torrent-cli/src/main.rs

# Doc comments referencing ferrite
find crates/ -name "*.rs" -exec sed -i 's|//! ferrite|//! torrent|g' {} +
find crates/ -name "*.rs" -exec sed -i 's|/// ferrite|/// torrent|g' {} +
find crates/ -name "*.rs" -exec sed -i 's|ferrite library|torrent library|g' {} +
find crates/ -name "*.rs" -exec sed -i 's|ferrite crate|torrent crate|g' {} +

# Tracing target strings (tracing spans use crate name)
find crates/ -name "*.rs" -exec sed -i 's/"ferrite"/"torrent"/g' {} +
```

**Step 3: Catch remaining references**

```bash
# Find any remaining ferrite references in .rs files (excluding plan docs)
grep -rn "ferrite" crates/ --include="*.rs" | grep -v "target/" | head -30
```

Fix any remaining references manually. Common stragglers:
- Error variant names containing "ferrite"
- Test helper function names
- Inline comments

**Step 4: Build the workspace**

```bash
cargo build --workspace 2>&1 | head -40
```

Expected: Compiles successfully with zero errors.

**Step 5: Run all tests**

```bash
cargo test --workspace 2>&1 | tail -30
```

Expected: All 1378 tests pass.

**Step 6: Run clippy**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -10
```

Expected: Zero warnings.

**Step 7: Commit the rename**

```bash
git add -A
git commit -m "feat(m60): rename ferrite → torrent across entire workspace

Rename all 12 crates from ferrite-* to torrent-*. Update all import
paths, doc comments, CLI binary name, and inter-crate dependencies.
Version bumped to 0.65.0.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 3: Documentation and Repository Updates

**Goal:** Update all documentation to use the new `torrent` naming and create new repos.

**Files:**
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `CLAUDE.md`
- Modify: `crates/torrent-cli/src/main.rs` (--version output if applicable)

**Step 1: Update README.md**

Replace all `ferrite` references with `torrent`:

```bash
cd /mnt/TempNVME/projects/ferrite
sed -i 's/ferrite-bencode/torrent-bencode/g' README.md
sed -i 's/ferrite-core/torrent-core/g' README.md
sed -i 's/ferrite-wire/torrent-wire/g' README.md
sed -i 's/ferrite-tracker/torrent-tracker/g' README.md
sed -i 's/ferrite-dht/torrent-dht/g' README.md
sed -i 's/ferrite-storage/torrent-storage/g' README.md
sed -i 's/ferrite-utp/torrent-utp/g' README.md
sed -i 's/ferrite-nat/torrent-nat/g' README.md
sed -i 's/ferrite-session/torrent-session/g' README.md
sed -i 's/ferrite-sim/torrent-sim/g' README.md
sed -i 's/ferrite-cli/torrent-cli/g' README.md
sed -i 's/ferrite/torrent/g' README.md  # catch remaining (careful — review diff)
```

Review the diff manually to ensure no false replacements (e.g., the word "ferrite" in prose that should stay).

**Step 2: Add CHANGELOG entry for 0.65.0**

Prepend to `CHANGELOG.md` after the `[Unreleased]` section:

```markdown
## 0.65.0 — M60: Final Release as `torrent`

### Changed
- **Crate rename**: All 12 `ferrite-*` crates renamed to `torrent-*`
- **CLI binary**: `ferrite` command renamed to `torrent`
- **Version**: 0.64.0 → 0.65.0
- **Repository**: Migrated to `torrent` repos on Codeberg and GitHub

### Added
- crates.io metadata (keywords, categories, homepage, documentation) on all 11 library crates
- `torrent-cli` marked `publish = false` (development tool, not for crates.io)
```

**Step 3: Update CLAUDE.md**

Replace `ferrite` references throughout. Key sections:
- Build commands: `cargo test --workspace` (unchanged)
- Architecture: update crate names in the dependency diagram
- Crate descriptions: `ferrite-session` → `torrent-session`, etc.

```bash
sed -i 's/ferrite-/torrent-/g' CLAUDE.md
sed -i 's/`ferrite`/`torrent`/g' CLAUDE.md
sed -i 's/Ferrite/Torrent/g' CLAUDE.md
```

Review diff — the word "Ferrite" in the project title should become "Torrent".

**Step 4: Update docs/ plan files (informational)**

These are historical — light-touch update of the most-referenced files:

```bash
# Only update the roadmap header and design doc references
sed -i 's/ferrite-/torrent-/g' docs/plans/2026-03-04-m60-final-release-design.md
```

Other plan files can retain `ferrite` names as historical records.

**Step 5: Commit docs**

```bash
git add README.md CHANGELOG.md CLAUDE.md docs/
git commit -m "docs(m60): update documentation for ferrite → torrent rename

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

**Step 6: Create new repos and archive old ones**

This step requires user interaction (Alan must create repos on Codeberg/GitHub and archive the old ones). Document the steps:

```bash
# 1. Create new repos on Codeberg and GitHub named "torrent"
# 2. Update remotes:
git remote set-url origin https://codeberg.org/alan090/torrent.git
git remote set-url github https://github.com/<username>/torrent.git

# 3. Push to both:
git push origin main && git push github main

# 4. Archive old ferrite repos via web UI (Settings → Archive)
```

---

## Phase 2: Performance — Non-Blocking Transfer Pipeline

### Task 4: Re-implement Non-Blocking Transfer Pipeline

**Goal:** Replace blocking `disk.write_chunk().await` with fire-and-forget `enqueue_write()` and make piece verification async, targeting 50–80 MB/s download speed.

**Reference:** The full M58 implementation plan at `docs/plans/2026-03-04-m58-non-blocking-transfer-pipeline.md` contains 866 lines of detailed step-by-step instructions with exact code. Use that plan as the primary reference — all file paths need `ferrite` → `torrent` adjustment.

**Files:**
- Modify: `crates/torrent-session/src/disk.rs` (DiskJob enum, DiskHandle, DiskActor)
- Modify: `crates/torrent-session/src/torrent.rs` (TorrentActor — handle_piece_data, select! loop, verify_and_mark_piece)
- Test: existing tests + 4 new tests for async write/verify paths

**Summary of changes (detailed code in M58 plan):**

**Step 1: Add async disk types to `disk.rs`**

After `DiskJobFlags` (line 24 in original, now in `crates/torrent-session/src/disk.rs`), add:

```rust
/// Error report from an async (fire-and-forget) disk write.
#[derive(Debug)]
pub struct DiskWriteError {
    pub info_hash: Id20,
    pub piece: u32,
    pub begin: u32,
    pub error: torrent_storage::Error,
}

/// Result of an async piece verification.
#[derive(Debug)]
pub struct VerifyResult {
    pub info_hash: Id20,
    pub piece: u32,
    pub passed: bool,
}
```

Add three new variants to `DiskJob`:

```rust
WriteAsync {
    info_hash: Id20,
    piece: u32,
    begin: u32,
    data: Bytes,
    flags: DiskJobFlags,
    error_tx: mpsc::Sender<DiskWriteError>,
},
VerifyAsync {
    info_hash: Id20,
    piece: u32,
    expected: Id20,
    flags: DiskJobFlags,
    result_tx: mpsc::Sender<VerifyResult>,
},
VerifyV2Async {
    info_hash: Id20,
    piece: u32,
    expected: Id32,
    flags: DiskJobFlags,
    result_tx: mpsc::Sender<VerifyResult>,
},
```

**Step 2: Add enqueue methods to `DiskHandle`**

```rust
pub fn enqueue_write(
    &self,
    piece: u32,
    begin: u32,
    data: Bytes,
    flags: DiskJobFlags,
) -> Result<(), mpsc::error::TrySendError<DiskJob>> {
    self.tx.try_send(DiskJob::WriteAsync {
        info_hash: self.info_hash,
        piece,
        begin,
        data,
        flags,
        error_tx: self.write_error_tx.clone().expect("async channels not set"),
    })
}

pub fn enqueue_verify(
    &self,
    piece: u32,
    expected: Id20,
    flags: DiskJobFlags,
) -> Result<(), mpsc::error::TrySendError<DiskJob>> {
    self.tx.try_send(DiskJob::VerifyAsync {
        info_hash: self.info_hash,
        piece,
        expected,
        flags,
        result_tx: self.verify_result_tx.clone().expect("async channels not set"),
    })
}

pub fn enqueue_verify_v2(
    &self,
    piece: u32,
    expected: Id32,
    flags: DiskJobFlags,
) -> Result<(), mpsc::error::TrySendError<DiskJob>> {
    self.tx.try_send(DiskJob::VerifyV2Async {
        info_hash: self.info_hash,
        piece,
        expected,
        flags,
        result_tx: self.verify_result_tx.clone().expect("async channels not set"),
    })
}

pub fn set_async_channels(
    &mut self,
    write_error_tx: mpsc::Sender<DiskWriteError>,
    verify_result_tx: mpsc::Sender<VerifyResult>,
) {
    self.write_error_tx = Some(write_error_tx);
    self.verify_result_tx = Some(verify_result_tx);
}
```

Add fields to `DiskHandle`:

```rust
pub struct DiskHandle {
    tx: mpsc::Sender<DiskJob>,
    info_hash: Id20,
    write_error_tx: Option<mpsc::Sender<DiskWriteError>>,
    verify_result_tx: Option<mpsc::Sender<VerifyResult>>,
}
```

**Step 3: Handle async jobs in DiskActor**

In the DiskActor job dispatch (match on DiskJob), add handlers for the three async variants. Each uses `spawn_blocking` and sends results/errors via the provided channels. For `VerifyAsync`, flush the piece's write buffer before hashing (same as sync path).

See M58 plan Task 3 for exact code.

**Step 4: Wire channels into TorrentActor**

In `crates/torrent-session/src/torrent.rs`, add fields to `TorrentActor`:

```rust
write_error_rx: mpsc::Receiver<DiskWriteError>,
verify_result_rx: mpsc::Receiver<VerifyResult>,
```

Create channels during `TorrentActor::new()` (capacity 64 for verify, 128 for write errors). Call `disk.set_async_channels()` after `register_torrent()`.

Add two new branches to the main `select!` loop:

```rust
// Async write error
Some(err) = self.write_error_rx.recv() => {
    warn!(piece = err.piece, begin = err.begin, "async write error: {}", err.error);
    // Optionally pause torrent on ENOSPC
}

// Async verify result
Some(result) = self.verify_result_rx.recv() => {
    if result.passed {
        self.on_piece_verified(result.piece).await;
    } else {
        self.on_piece_hash_failed(result.piece).await;
    }
}
```

**Step 5: Rewrite `handle_piece_data()` for non-blocking writes**

Replace the blocking write at `torrent.rs:3188-3202`:

```rust
// BEFORE (blocking):
if let Some(ref disk) = self.disk
    && let Err(e) = disk.write_chunk(index, begin, Bytes::copy_from_slice(data), DiskJobFlags::empty()).await
{
    warn!(index, begin, "failed to write chunk: {e}");
    return;
}

// AFTER (fire-and-forget):
if let Some(ref disk) = self.disk {
    let data_bytes = Bytes::copy_from_slice(data);
    match disk.enqueue_write(index, begin, data_bytes, DiskJobFlags::empty()) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(job)) => {
            // Backpressure: channel full, fall back to blocking send
            let _ = disk.tx.send(job).await;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            warn!(index, begin, "disk channel closed");
            return;
        }
    }
}
```

**Step 6: Make piece verification async**

Replace the blocking `verify_and_mark_piece()` call with `enqueue_verify()`. For V1Only torrents, use the async path. For V2Only/Hybrid, keep blocking (Merkle verification needs sequential state).

See M58 plan Task 6 for exact dispatch logic.

**Step 7: Add in-memory v2 block hashing**

For v2 torrents, compute `sha256(&data)` at receive time in `handle_piece_data()` and insert into `MerkleTreeState` immediately, instead of round-tripping through `DiskJob::BlockHash`.

**Step 8: Run tests**

```bash
cargo test --workspace 2>&1 | tail -20
```

Expected: All existing tests pass + 4 new tests for async paths.

**Step 9: Run clippy**

```bash
cargo clippy --workspace -- -D warnings
```

Expected: Zero warnings.

**Step 10: Commit**

```bash
git add crates/torrent-session/
git commit -m "perf(m60): non-blocking disk writes and async piece verification

Replace blocking write_chunk().await with fire-and-forget enqueue_write().
Add async piece verification via verify_result_rx select! branch.
Compute v2 block hashes in-memory at receive time.
Expected 2-3x download speed improvement.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 5: Supplementary Performance Fixes

**Goal:** Apply two small independent performance improvements.

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs` (handle_add_peers, try_connect_peers)

**Step 1: Add TCP 5-second connect timeout**

Find `TcpStream::connect()` calls in `torrent.rs` and `peer.rs`. Wrap with:

```rust
match tokio::time::timeout(
    std::time::Duration::from_secs(5),
    TcpStream::connect(addr),
).await {
    Ok(Ok(stream)) => stream,
    Ok(Err(e)) => { /* connection error */ return; }
    Err(_) => { /* timeout */ return; }
}
```

Search for the exact location:

```bash
grep -n "TcpStream::connect" crates/torrent-session/src/*.rs
```

**Step 2: Add O(1) peer dedup**

In `handle_add_peers()` (search for `fn handle_add_peers` in `torrent.rs`), the current implementation likely uses linear scan to check for duplicate peers. Replace with `HashSet<SocketAddr>`:

```rust
use std::collections::HashSet;

fn handle_add_peers(&mut self, peers: Vec<(SocketAddr, PeerSource)>) {
    let existing: HashSet<SocketAddr> = self.peers.keys().copied().collect();
    let available: HashSet<SocketAddr> = self.available_peers.iter().map(|(a, _)| *a).collect();

    for (addr, source) in peers {
        if existing.contains(&addr) || available.contains(&addr) {
            continue;
        }
        // ... add to available_peers
    }
}
```

**Step 3: Run tests and clippy**

```bash
cargo test --workspace 2>&1 | tail -10
cargo clippy --workspace -- -D warnings
```

**Step 4: Commit**

```bash
git add crates/torrent-session/
git commit -m "perf(m60): TCP 5s connect timeout and O(1) peer dedup

Prevent 2-minute OS default TCP timeout from wasting connection slots.
Replace linear peer dedup scan with HashSet for O(1) lookup.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 6: Live Download Verification + Criterion Benchmarks

**Goal:** Verify the non-blocking pipeline works with real BitTorrent downloads and add a Criterion benchmark suite.

**Files:**
- Create: `crates/torrent-bencode/benches/bencode_bench.rs`
- Create: `crates/torrent-session/benches/disk_bench.rs`
- Modify: `crates/torrent-bencode/Cargo.toml` (add criterion dev-dep)
- Modify: `crates/torrent-session/Cargo.toml` (add criterion dev-dep)

**Step 1: Live BT download test**

```bash
cd /mnt/TempNVME/projects/ferrite
cargo build --release -p torrent-cli
./target/release/torrent download "magnet:?xt=urn:btih:ab6ad7ff24b5ed3a61352a1f1a7f6c3c51f0f7d2&dn=archlinux-2024.12.01-x86_64.iso" -o /tmp/torrent-test
```

Monitor download speed. Target: >30 MB/s average (expecting 50–80 MB/s with the pipeline changes).

If download fails or speed is significantly lower than expected, investigate before proceeding.

**Step 2: Add Criterion to torrent-bencode**

Add to `crates/torrent-bencode/Cargo.toml`:

```toml
[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "bencode_bench"
harness = false
```

Create `crates/torrent-bencode/benches/bencode_bench.rs`:

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use torrent_bencode::{to_bytes, from_bytes, BencodeValue};
use std::collections::BTreeMap;

fn bench_encode_dict(c: &mut Criterion) {
    let mut dict = BTreeMap::new();
    for i in 0..100 {
        dict.insert(format!("key_{i:04}"), BencodeValue::Integer(i));
    }
    let value = BencodeValue::Dict(
        dict.into_iter()
            .map(|(k, v)| (k.into_bytes(), v))
            .collect(),
    );

    c.bench_function("encode_100_key_dict", |b| {
        b.iter(|| to_bytes(black_box(&value)).unwrap())
    });
}

fn bench_decode_torrent(c: &mut Criterion) {
    // Encode a realistic torrent-like structure
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test.iso".to_vec()));
    info.insert(b"piece length".to_vec(), BencodeValue::Integer(262144));
    info.insert(b"length".to_vec(), BencodeValue::Integer(1_500_000_000));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 20 * 5730]));

    let mut outer = BTreeMap::new();
    outer.insert(b"info".to_vec(), BencodeValue::Dict(info));
    outer.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );

    let encoded = to_bytes(&BencodeValue::Dict(outer)).unwrap();

    c.bench_function("decode_torrent_like", |b| {
        b.iter(|| from_bytes::<BencodeValue>(black_box(&encoded)).unwrap())
    });
}

criterion_group!(benches, bench_encode_dict, bench_decode_torrent);
criterion_main!(benches);
```

**Step 3: Add Criterion to torrent-session (disk benchmarks)**

Add to `crates/torrent-session/Cargo.toml`:

```toml
[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "disk_bench"
harness = false
```

Create `crates/torrent-session/benches/disk_bench.rs`:

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use bytes::Bytes;

fn bench_sha1_16k(c: &mut Criterion) {
    let data = vec![0xABu8; 16384];
    let mut group = c.benchmark_group("hashing");
    group.throughput(Throughput::Bytes(16384));
    group.bench_function("sha1_16k", |b| {
        b.iter(|| torrent_core::sha1(black_box(&data)))
    });
    group.bench_function("sha256_16k", |b| {
        b.iter(|| torrent_core::sha256(black_box(&data)))
    });
    group.finish();
}

fn bench_sha1_piece(c: &mut Criterion) {
    let data = vec![0xABu8; 262144]; // 256 KiB piece
    let mut group = c.benchmark_group("hashing");
    group.throughput(Throughput::Bytes(262144));
    group.bench_function("sha1_256k", |b| {
        b.iter(|| torrent_core::sha1(black_box(&data)))
    });
    group.bench_function("sha256_256k", |b| {
        b.iter(|| torrent_core::sha256(black_box(&data)))
    });
    group.finish();
}

criterion_group!(benches, bench_sha1_16k, bench_sha1_piece);
criterion_main!(benches);
```

**Step 4: Run benchmarks**

```bash
cargo bench -p torrent-bencode
cargo bench -p torrent-session
```

Record baseline numbers for future regression comparison.

**Step 5: Commit**

```bash
git add crates/torrent-bencode/benches/ crates/torrent-bencode/Cargo.toml \
        crates/torrent-session/benches/ crates/torrent-session/Cargo.toml
git commit -m "perf(m60): add Criterion benchmarks for bencode and hashing

Benchmark suite for encode/decode throughput, SHA-1/SHA-256 hashing at
block (16 KiB) and piece (256 KiB) sizes. Baseline for regression tracking.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Phase 3: Code Quality

### Task 7: rustfmt.toml + Format Enforcement

**Goal:** Add workspace-level formatting config and normalise all code.

**Files:**
- Create: `rustfmt.toml`
- Modify: All `*.rs` files (formatting only)

**Step 1: Create rustfmt.toml**

```toml
max_width = 100
imports_granularity = "Crate"
group_imports = "StdExternalCrate"
```

**Step 2: Run cargo fmt**

```bash
cargo fmt --all
```

**Step 3: Verify no breakage**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: All tests pass (formatting is cosmetic).

**Step 4: Commit**

```bash
git add rustfmt.toml
git add -A  # all formatting changes
git commit -m "style(m60): add rustfmt.toml and normalise formatting

max_width=100, imports_granularity=Crate, group_imports=StdExternalCrate.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 8: Clippy Lint Expansion

**Goal:** Add deny-level lints for critical patterns and fix violations.

**Files:**
- Modify: `Cargo.toml` (workspace root — add lints section)
- Modify: Any files that violate the new lints

**Step 1: Add workspace lint config**

In root `Cargo.toml`, add:

```toml
[workspace.lints.clippy]
await_holding_lock = "deny"
large_enum_variant = "deny"
needless_pass_by_value = "warn"
inefficient_to_string = "warn"
```

In each crate's `Cargo.toml`, add (if not already present):

```toml
[lints]
workspace = true
```

**Step 2: Run clippy to find violations**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | grep "error\[" | head -20
```

**Step 3: Fix deny-level violations**

For `await_holding_lock`: Find any `MutexGuard` or `RwLockGuard` held across `.await` points. Drop the guard before awaiting.

For `large_enum_variant`: Box the large variants. Likely candidates:
- `DiskJob` (some variants have inline `Bytes` + multiple fields)
- `PeerEvent` (PieceData carries data inline)
- `AlertKind` (~30 variants, some large)

Example fix:
```rust
// Before:
LargeVariant { field1: Bytes, field2: HashMap<...>, field3: Vec<...> },

// After:
LargeVariant(Box<LargeVariantData>),
```

**Step 4: Verify**

```bash
cargo clippy --workspace -- -D warnings
cargo test --workspace 2>&1 | tail -5
```

Expected: Zero clippy warnings, all tests pass.

**Step 5: Commit**

```bash
git add -A
git commit -m "fix(m60): expand clippy lints — deny await_holding_lock, large_enum_variant

Add workspace-level lint configuration. Fix all deny-level violations.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 9: Panic Audit (Network-Facing Code)

**Goal:** Eliminate all `unwrap()`, `expect()`, and `panic!()` from network-facing production code paths.

**Files:**
- Modify: `crates/torrent-wire/src/*.rs`
- Modify: `crates/torrent-tracker/src/*.rs`
- Modify: `crates/torrent-dht/src/*.rs`
- Modify: `crates/torrent-bencode/src/*.rs`
- Modify: `crates/torrent-session/src/peer.rs` (peer task)
- Modify: `crates/torrent-session/src/torrent.rs` (incoming connections)

**Step 1: Identify panicking calls in production code**

```bash
cd /mnt/TempNVME/projects/ferrite

# Find unwrap/expect/panic in non-test code
for crate in torrent-wire torrent-tracker torrent-dht torrent-bencode; do
  echo "=== $crate ==="
  grep -n "unwrap()\|expect(\|panic!" "crates/$crate/src/"*.rs | grep -v "#\[cfg(test)\]" | grep -v "mod tests" | grep -v "fn test_"
done
```

Note: The exploration found ~422 total calls, but the vast majority are in test code. Expect ~20–40 in production paths.

**Step 2: Categorise each finding**

For each `unwrap()`/`expect()`:
- **Infallible**: The value is guaranteed to be `Some`/`Ok` by construction (e.g., regex compilation, static data). Add a comment explaining why. These can stay.
- **Network input**: Parsing data from peers/trackers/DHT. Replace with `?` or `.ok_or(Error::...)`.
- **Internal logic**: State machine invariants. Replace with `debug_assert!` + graceful fallback.

**Step 3: Fix network-facing panics**

Common patterns:

```rust
// Before (panics on malformed input):
let value = dict.get("key").unwrap();

// After (returns error):
let value = dict.get("key").ok_or(Error::MissingField("key"))?;
```

```rust
// Before:
let num = bytes[0..4].try_into().unwrap();

// After:
let num = bytes.get(0..4)
    .and_then(|b| b.try_into().ok())
    .ok_or(Error::TruncatedMessage)?;
```

**Step 4: Verify no test breakage**

```bash
cargo test --workspace 2>&1 | tail -10
cargo clippy --workspace -- -D warnings
```

**Step 5: Commit**

```bash
git add crates/torrent-wire/ crates/torrent-tracker/ crates/torrent-dht/ crates/torrent-bencode/ crates/torrent-session/
git commit -m "fix(m60): eliminate panics from network-facing code paths

Replace unwrap()/expect()/panic!() with proper error propagation
in wire, tracker, DHT, and bencode parsing. Test-only code unchanged.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 10: Resource Exhaustion Limits Audit

**Goal:** Add configurable bounds for resource consumption to prevent malicious peer attacks.

**Files:**
- Modify: `crates/torrent-session/src/settings.rs` (add new limit fields)
- Modify: `crates/torrent-session/src/torrent.rs` (enforce limits)
- Modify: `crates/torrent-session/src/peer.rs` (message size limits)
- Modify: `crates/torrent/src/client.rs` (ClientBuilder methods)

**Step 1: Audit existing limits**

```bash
grep -rn "max_\|limit\|MAX_\|LIMIT" crates/torrent-session/src/settings.rs | head -20
grep -rn "16384\|65536\|1048576\|100_000" crates/torrent-session/src/*.rs | head -20
```

Identify hardcoded values that should be configurable.

**Step 2: Add new Settings fields**

```rust
// In Settings struct:

/// Maximum metadata size in bytes (BEP 9). Default: 100 MiB.
#[serde(default = "default_max_metadata_size")]
pub max_metadata_size: usize,

/// Maximum peer message size in bytes. Default: 16 MiB.
#[serde(default = "default_max_message_size")]
pub max_message_size: usize,

/// Maximum piece length in bytes. Reject torrents exceeding this. Default: 64 MiB.
#[serde(default = "default_max_piece_length")]
pub max_piece_length: u64,

/// Maximum outstanding requests per peer. Default: 500.
#[serde(default = "default_max_outstanding_requests")]
pub max_outstanding_requests: usize,
```

Add default functions:

```rust
fn default_max_metadata_size() -> usize { 100 * 1024 * 1024 }
fn default_max_message_size() -> usize { 16 * 1024 * 1024 }
fn default_max_piece_length() -> u64 { 64 * 1024 * 1024 }
fn default_max_outstanding_requests() -> usize { 500 }
```

**Step 3: Enforce limits**

- **Metadata size**: In BEP 9 metadata download (`MetadataDownloader` or equivalent), check `metadata_size <= settings.max_metadata_size` before allocating.
- **Message size**: In the wire codec (`MessageCodec`), reject messages larger than `max_message_size`.
- **Piece length**: In `add_torrent()` / `try_assemble_metadata()`, reject torrents with `piece_length > max_piece_length`.
- **Outstanding requests**: In `request_pieces_from_peer()`, cap `pending_requests.len()` at `max_outstanding_requests`.

**Step 4: Add ClientBuilder methods**

```rust
pub fn max_metadata_size(mut self, size: usize) -> Self {
    self.settings.max_metadata_size = size;
    self
}
// ... etc for each new field
```

**Step 5: Add to presets**

In `Settings::min_memory()`:
```rust
max_metadata_size: 10 * 1024 * 1024,  // 10 MiB
max_message_size: 4 * 1024 * 1024,    // 4 MiB
```

**Step 6: Run tests and clippy**

```bash
cargo test --workspace 2>&1 | tail -10
cargo clippy --workspace -- -D warnings
```

**Step 7: Commit**

```bash
git add crates/torrent-session/ crates/torrent/
git commit -m "fix(m60): add configurable resource exhaustion limits

Add max_metadata_size, max_message_size, max_piece_length, and
max_outstanding_requests to Settings. Enforce at parsing and request
boundaries. Prevents OOM from malicious peers.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Phase 4: Fuzz Scaffolding

### Task 11: Fuzz Architecture Setup

**Goal:** Set up `cargo-fuzz` infrastructure with 4 target stubs and seed corpus. Actual fuzzing campaigns deferred to M61.

**Files:**
- Create: `fuzz/Cargo.toml`
- Create: `fuzz/fuzz_targets/fuzz_bencode.rs`
- Create: `fuzz/fuzz_targets/fuzz_peer_wire.rs`
- Create: `fuzz/fuzz_targets/fuzz_tracker.rs`
- Create: `fuzz/fuzz_targets/fuzz_metadata.rs`
- Create: `docs/fuzzing.md`

**Step 1: Install cargo-fuzz (if not present)**

```bash
cargo install cargo-fuzz 2>/dev/null || true
```

**Step 2: Create fuzz directory structure**

```bash
mkdir -p fuzz/fuzz_targets fuzz/corpus/bencode fuzz/corpus/peer_wire fuzz/corpus/tracker fuzz/corpus/metadata
```

**Step 3: Create fuzz/Cargo.toml**

```toml
[package]
name = "torrent-fuzz"
version = "0.0.0"
publish = false
edition = "2024"

[package.metadata]
cargo-fuzz = true

[dependencies]
libfuzzer-sys = "0.4"
torrent-bencode = { path = "../crates/torrent-bencode" }
torrent-core = { path = "../crates/torrent-core" }
torrent-wire = { path = "../crates/torrent-wire" }
torrent-tracker = { path = "../crates/torrent-tracker" }
bytes = "1"

[[bin]]
name = "fuzz_bencode"
path = "fuzz_targets/fuzz_bencode.rs"
doc = false

[[bin]]
name = "fuzz_peer_wire"
path = "fuzz_targets/fuzz_peer_wire.rs"
doc = false

[[bin]]
name = "fuzz_tracker"
path = "fuzz_targets/fuzz_tracker.rs"
doc = false

[[bin]]
name = "fuzz_metadata"
path = "fuzz_targets/fuzz_metadata.rs"
doc = false
```

**Step 4: Create fuzz targets**

`fuzz/fuzz_targets/fuzz_bencode.rs`:
```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use torrent_bencode::{from_bytes, BencodeValue};

fuzz_target!(|data: &[u8]| {
    // Attempt to decode arbitrary bencode input
    let _ = from_bytes::<BencodeValue>(data);

    // If decode succeeds, re-encode and verify round-trip
    if let Ok(value) = from_bytes::<BencodeValue>(data) {
        let _ = torrent_bencode::to_bytes(&value);
    }
});
```

`fuzz/fuzz_targets/fuzz_peer_wire.rs`:
```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use bytes::BytesMut;
use torrent_wire::MessageCodec;
use tokio_util::codec::Decoder;

fuzz_target!(|data: &[u8]| {
    let mut codec = MessageCodec::new();
    let mut buf = BytesMut::from(data);
    // Attempt to decode arbitrary wire protocol data
    let _ = codec.decode(&mut buf);
});
```

`fuzz/fuzz_targets/fuzz_tracker.rs`:
```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use torrent_bencode::{from_bytes, BencodeValue};

fuzz_target!(|data: &[u8]| {
    // Fuzz HTTP tracker response parsing (bencode-based)
    let _ = from_bytes::<BencodeValue>(data);

    // Fuzz UDP tracker response parsing (binary)
    if data.len() >= 8 {
        // UDP responses start with action (4 bytes) + transaction_id (4 bytes)
        let _action = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let _txn_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    }
});
```

`fuzz/fuzz_targets/fuzz_metadata.rs`:
```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use torrent_core::metainfo::torrent_from_bytes;
use torrent_core::detect::torrent_from_bytes_any;
use torrent_core::magnet::Magnet;

fuzz_target!(|data: &[u8]| {
    // Fuzz v1 .torrent parsing
    let _ = torrent_from_bytes(data);

    // Fuzz auto-detect v1/v2/hybrid parsing
    let _ = torrent_from_bytes_any(data);

    // Fuzz magnet URI parsing (treat data as UTF-8)
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = s.parse::<Magnet>();
    }
});
```

**Step 5: Seed the corpus**

```bash
# Add a minimal valid bencode value as seed
echo -n "d3:foo3:bare" > fuzz/corpus/bencode/simple_dict
echo -n "i42e" > fuzz/corpus/bencode/integer
echo -n "l3:foo3:bare" > fuzz/corpus/bencode/list
```

**Step 6: Verify fuzz targets compile**

```bash
cd fuzz && cargo build 2>&1 | tail -5
```

Expected: All 4 targets compile without errors.

**Step 7: Create docs/fuzzing.md**

```markdown
# Fuzzing Guide

## Setup

```bash
cargo install cargo-fuzz
```

## Running Fuzz Targets

```bash
# From the repository root:
cargo fuzz run fuzz_bencode
cargo fuzz run fuzz_peer_wire
cargo fuzz run fuzz_tracker
cargo fuzz run fuzz_metadata
```

## Targets

| Target | Crate | What It Tests |
|--------|-------|--------------|
| fuzz_bencode | torrent-bencode | Bencode decode + encode round-trip |
| fuzz_peer_wire | torrent-wire | Peer wire message frame decoding |
| fuzz_tracker | torrent-tracker | HTTP (bencode) + UDP tracker responses |
| fuzz_metadata | torrent-core | .torrent v1/v2/hybrid + magnet URI parsing |

## Corpus

Seed corpus files are in `fuzz/corpus/<target>/`. Add interesting inputs as they are discovered.

## Future Work (M61)

- Run each target for 24+ hours and fix any crashes
- Add sanitiser testing (ASan, TSan, LeakSan)
- Submit to Google OSS-Fuzz for continuous fuzzing
- Add coverage-guided corpus expansion
```

**Step 8: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add fuzz/ docs/fuzzing.md
git commit -m "feat(m60): scaffold cargo-fuzz with 4 fuzz targets

Add fuzz_bencode, fuzz_peer_wire, fuzz_tracker, fuzz_metadata targets.
Seed corpus included. Actual fuzzing campaigns deferred to M61.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Phase 5: Publication

### Task 12: README Overhaul

**Goal:** Rewrite README.md for the `torrent` library's public launch.

**Files:**
- Modify: `README.md`

**Step 1: Read current README**

```bash
wc -l README.md  # Check current length
```

**Step 2: Rewrite README**

The README should follow this structure (adapt existing content, don't start from scratch):

1. **Title + badges** — `torrent` with crates.io, docs.rs, license badges
2. **One-line description** — "A production-grade BitTorrent library for Rust"
3. **Feature highlights** — bullet list: 27 BEPs, v1/v2/hybrid, streaming, uTP, DHT, encryption, proxy, I2P, SSL, simulation
4. **Quick start** — `cargo add torrent` + minimal download example using `torrent::prelude::*`
5. **Architecture** — crate dependency diagram with `torrent-*` names
6. **Configuration** — Settings struct overview, presets
7. **Performance** — benchmark table vs rqbit and libtorrent
8. **Security** — SSRF mitigation, IP filtering, smart banning, MSE encryption, panic-free network paths
9. **Examples** — link to `examples/` directory (download.rs, create.rs, stream.rs, dht_lookup.rs)
10. **Stability** — "0.x API, aiming for 1.0 after real-world validation"
11. **License** — GPL-3.0-or-later

**Step 3: Commit**

```bash
git add README.md
git commit -m "docs(m60): overhaul README for torrent crates.io launch

Vision statement, feature matrix, quick start, architecture diagram,
security posture, performance benchmarks, stability guarantees.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 13: Architecture Documentation

**Goal:** Create reference documentation for the library's internals.

**Files:**
- Create: `docs/architecture.md`
- Create: `docs/configuration.md`
- Create: `docs/security.md`

**Step 1: Write docs/architecture.md**

Cover:
- Crate dependency graph (text diagram)
- Actor model: SessionActor → TorrentActor → PeerTask
- Data flow: torrent add → tracker announce → peer connect → piece download → disk write → verify → have broadcast
- Concurrency model: tokio `select!` loops, mpsc channels, no shared mutable state between actors
- Disk I/O pipeline: DiskActor with WriteBuffer, ARC cache, semaphore-gated spawn_blocking
- Key types: SessionHandle, TorrentHandle, DiskHandle, Settings

**Step 2: Write docs/configuration.md**

Cover:
- Settings struct overview (102 fields)
- Preset profiles: `default()`, `min_memory()`, `high_performance()`
- Runtime mutation via `SessionHandle::apply_settings()`
- JSON serialization for config files
- Key fields by category (network, disk, DHT, tracker, security)

**Step 3: Write docs/security.md**

Cover:
- Threat model: adversarial peers, malicious torrents, tracker SSRF
- Mitigations: IP filtering, smart banning, URL security (SSRF, IDNA), MSE encryption, anonymous mode, proxy support, resource limits
- Panic-free guarantee on network paths
- Fuzzing status

**Step 4: Commit**

```bash
git add docs/architecture.md docs/configuration.md docs/security.md
git commit -m "docs(m60): add architecture, configuration, and security reference docs

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 14: crates.io Publication

**Goal:** Publish all 11 library crates to crates.io in dependency order.

**Files:**
- No file changes (all prep done in prior tasks)

**Pre-flight checks:**

```bash
# 1. All tests pass
cargo test --workspace

# 2. No clippy warnings
cargo clippy --workspace -- -D warnings

# 3. No doc warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace

# 4. Formatting clean
cargo fmt --all -- --check
```

**Step 1: Dry-run publish for each crate**

```bash
# Leaves first (no inter-crate deps)
cargo publish --dry-run -p torrent-bencode

# Second tier
cargo publish --dry-run -p torrent-core

# Third tier (depend on bencode + core)
cargo publish --dry-run -p torrent-wire
cargo publish --dry-run -p torrent-tracker
cargo publish --dry-run -p torrent-dht
cargo publish --dry-run -p torrent-storage
cargo publish --dry-run -p torrent-utp
cargo publish --dry-run -p torrent-nat

# Fourth tier
cargo publish --dry-run -p torrent-session

# Fifth tier
cargo publish --dry-run -p torrent-sim

# Facade
cargo publish --dry-run -p torrent
```

Fix any issues (missing fields, license, readme path, etc.).

**Step 2: Login to crates.io**

```bash
cargo login  # Paste API token when prompted
```

**Step 3: Publish in order (with sleep between for index propagation)**

```bash
cargo publish -p torrent-bencode && sleep 30
cargo publish -p torrent-core && sleep 30
cargo publish -p torrent-wire && sleep 15
cargo publish -p torrent-tracker && sleep 15
cargo publish -p torrent-dht && sleep 15
cargo publish -p torrent-storage && sleep 15
cargo publish -p torrent-utp && sleep 15
cargo publish -p torrent-nat && sleep 15
cargo publish -p torrent-session && sleep 30
cargo publish -p torrent-sim && sleep 15
cargo publish -p torrent
```

**Step 4: Tag the release**

```bash
git tag -a v0.65.0 -m "v0.65.0 — Final release as torrent on crates.io"
git push origin main --tags
git push github main --tags
```

**Step 5: Verify docs.rs**

Visit `https://docs.rs/torrent/0.65.0` and verify documentation renders correctly for all 11 crates.

**Step 6: Archive old repos**

Via Codeberg and GitHub web UI:
1. Archive the `ferrite` repositories
2. Add a note in the archived repo README pointing to the new `torrent` repos

---

## Post-Completion Checklist

- [ ] All 12 crates compile under `torrent-*` names
- [ ] All 1378+ tests pass
- [ ] Zero clippy warnings with expanded lints
- [ ] Live BT download achieves >30 MB/s
- [ ] No unwrap/expect/panic on network paths
- [ ] rustfmt.toml enforced, all code formatted
- [ ] Criterion benchmarks established (bencode, hashing)
- [ ] 4 fuzz targets compile and run on seed corpus
- [ ] README overhauled with vision, features, security
- [ ] Architecture docs written (architecture.md, configuration.md, security.md)
- [ ] 11 crates published on crates.io
- [ ] docs.rs builds verified
- [ ] v0.65.0 tagged and pushed to both remotes
- [ ] Old ferrite repos archived
