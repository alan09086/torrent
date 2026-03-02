# M53: CLI Binary & Benchmarking — Implementation Plan

**Date:** 2026-03-01
**Milestone:** M53
**Phase:** 13 — Documentation, CLI & Benchmarking
**Depends on:** M52 complete (docs), M49 complete (pluggable disk I/O for DisabledDiskIo benchmark mode)

## Goal

Build a `ferrite-cli` binary for downloading/creating torrents with progress
display, then benchmark it against rqbit and libtorrent.

## System Inventory

| Tool | Available | Version | Path |
|------|-----------|---------|------|
| rqbit | Yes | 8.1.1 | `/usr/bin/rqbit` |
| libtorrent (Python) | Yes | 2.0.11.0 | `python -c "import libtorrent"` |
| qbittorrent-nox | No | — | — |

Benchmarks will use rqbit CLI and a Python script using libtorrent bindings.

## API Notes

**Adding torrents — use SessionHandle directly:**
- `session.add_magnet(magnet).await` — magnet links
- `session.add_torrent(meta, storage).await` — parsed .torrent files

The `AddTorrentParams` facade builder exists but its `TorrentSource` fields are
`#[allow(dead_code)]` — the session integration is not wired through the facade
yet. The CLI should use `SessionHandle` methods directly. If time permits, wire
up `AddTorrentParams` → `SessionHandle` as part of this milestone.

**Shutdown is fire-and-forget:**
`session.shutdown().await` sends a command and returns immediately. The CLI must
not drop the tokio runtime immediately — wait for a `TorrentCompleted` alert or
use a brief drain period.

**Monitoring:** `session.torrent_stats(info_hash).await` returns `TorrentStats`
with `state`, `downloaded`, `uploaded`, `pieces_have`, `pieces_total`,
`peers_connected`, `peers_available`.

## Tasks

### Task 1: Create `ferrite-cli` crate

Set up the new workspace member.

**`crates/ferrite-cli/Cargo.toml`:**
```toml
[package]
name = "ferrite-cli"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Command-line BitTorrent client powered by ferrite"

[[bin]]
name = "ferrite"
path = "src/main.rs"

[dependencies]
ferrite = { path = "../ferrite" }
clap = { version = "4", features = ["derive"] }
indicatif = "0.17"
ctrlc = { version = "3", features = ["termination"] }
tokio = { version = "1", features = ["full"] }
```

The binary will be called `ferrite` (not `ferrite-cli`).

**File:** `crates/ferrite-cli/Cargo.toml`

### Task 2: Implement CLI argument parsing

Use clap derive for subcommands.

```rust
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ferrite", about = "BitTorrent client powered by ferrite")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download a torrent from a magnet link or .torrent file
    Download {
        /// Magnet URI or path to .torrent file
        source: String,
        /// Output directory
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
        /// Download pieces sequentially
        #[arg(long)]
        sequential: bool,
        /// Disable DHT
        #[arg(long)]
        no_dht: bool,
        /// SOCKS5 proxy (host:port)
        #[arg(long)]
        proxy: Option<String>,
        /// Path to JSON settings file
        #[arg(long)]
        config: Option<PathBuf>,
        /// Seed after completion (default: exit on completion)
        #[arg(long)]
        seed: bool,
        /// Listen port (default: 6881)
        #[arg(short, long, default_value = "6881")]
        port: u16,
    },
    /// Create a .torrent file
    Create {
        /// Path to file or directory
        path: PathBuf,
        /// Output .torrent file path
        #[arg(short, long, default_value = "output.torrent")]
        output: PathBuf,
        /// Tracker URL(s)
        #[arg(short, long)]
        tracker: Vec<String>,
        /// Create as private torrent
        #[arg(long)]
        private: bool,
        /// Piece size in KiB (auto-selected if omitted)
        #[arg(long)]
        piece_size: Option<u64>,
    },
    /// Display torrent file information
    Info {
        /// Path to .torrent file
        path: PathBuf,
    },
}
```

**File:** `crates/ferrite-cli/src/main.rs`

### Task 3: Implement `download` subcommand

Core download logic with progress bar.

**Flow:**
1. Parse source — detect magnet URI (`magnet:?`) vs file path
2. Load settings from `--config` JSON if provided, otherwise defaults
3. Apply CLI overrides (`--no-dht`, `--proxy`, `--port`, `--sequential`)
4. Start session via `ClientBuilder`
5. Add torrent via `session.add_magnet()` or `session.add_torrent()`
6. Set up Ctrl-C handler to trigger graceful shutdown
7. Poll `session.torrent_stats()` every 500ms, update progress bar
8. On completion: if `--seed`, continue; otherwise shutdown and exit

**Progress bar format (indicatif):**
```
[downloading] ████████░░░░░░░░░░ 45.2% | 1.2 GiB/2.7 GiB | ↓ 12.4 MB/s ↑ 1.2 MB/s | 24 peers | ETA 1m 42s
```

**Ctrl-C handling:**
```rust
let shutdown = Arc::new(AtomicBool::new(false));
let s = shutdown.clone();
ctrlc::set_handler(move || { s.store(true, Ordering::SeqCst); })?;

// In progress loop:
if shutdown.load(Ordering::SeqCst) {
    eprintln!("\nShutting down...");
    session.shutdown().await?;
    // Brief drain for in-flight writes
    tokio::time::sleep(Duration::from_secs(1)).await;
    break;
}
```

**File:** `crates/ferrite-cli/src/main.rs` (or split into `crates/ferrite-cli/src/download.rs`)

### Task 4: Implement `create` subcommand

Torrent creation.

**Flow:**
1. Build `CreateTorrent` from path
2. Apply options (tracker, private, piece size)
3. Generate and write to output path
4. Print summary (name, size, piece count, info hash)

**File:** `crates/ferrite-cli/src/main.rs` (or `create.rs`)

### Task 5: Implement `info` subcommand

Display .torrent metadata.

**Flow:**
1. Read .torrent file bytes
2. Parse with `torrent_from_bytes_any()` to handle v1/v2/hybrid
3. Print: name, info hash(es), total size, piece size, piece count, files list,
   trackers, creation date, comment, private flag, version (v1/v2/hybrid)

**Output format:**
```
Name:         Ubuntu 24.04 Desktop
Info hash:    a1b2c3d4e5f6...
Version:      v1
Total size:   4.7 GiB
Piece size:   2 MiB
Pieces:       2,400
Files:        1
  ubuntu-24.04-desktop-amd64.iso (4.7 GiB)
Trackers:
  https://torrent.ubuntu.com/announce
  https://ipv6.torrent.ubuntu.com/announce
Private:      no
Created:      2024-04-25 12:00:00 UTC
Comment:      Ubuntu CD releases.ubuntu.com
```

**File:** `crates/ferrite-cli/src/main.rs` (or `info.rs`)

### Task 6: Write benchmark script

A shell/Python script that downloads the same torrent with all three clients
and compares results.

**Benchmark torrent:** Ubuntu latest ISO (well-seeded, ~4-5 GiB, consistent swarm).

**`benchmarks/run_benchmark.sh`:**
```bash
#!/bin/bash
# Benchmark: ferrite vs rqbit vs libtorrent
# Downloads the same torrent N times with each client

MAGNET="magnet:?xt=urn:btih:..."  # Ubuntu ISO magnet
TRIALS=3
OUTPUT_DIR="/tmp/ferrite-bench"
RESULTS="benchmarks/results.csv"

echo "client,trial,time_secs,avg_speed_mbps,peak_rss_mb" > "$RESULTS"

for trial in $(seq 1 $TRIALS); do
    # --- ferrite ---
    rm -rf "$OUTPUT_DIR/ferrite"
    mkdir -p "$OUTPUT_DIR/ferrite"
    /usr/bin/time -v ferrite download "$MAGNET" -o "$OUTPUT_DIR/ferrite" 2>"$OUTPUT_DIR/ferrite-time.txt"
    # parse time and RSS from /usr/bin/time output

    # --- rqbit ---
    rm -rf "$OUTPUT_DIR/rqbit"
    mkdir -p "$OUTPUT_DIR/rqbit"
    /usr/bin/time -v rqbit download "$MAGNET" -o "$OUTPUT_DIR/rqbit" 2>"$OUTPUT_DIR/rqbit-time.txt"

    # --- libtorrent (Python) ---
    rm -rf "$OUTPUT_DIR/libtorrent"
    mkdir -p "$OUTPUT_DIR/libtorrent"
    /usr/bin/time -v python benchmarks/lt_download.py "$MAGNET" "$OUTPUT_DIR/libtorrent" 2>"$OUTPUT_DIR/lt-time.txt"
done
```

**`benchmarks/lt_download.py`:** Minimal libtorrent Python script that downloads
a magnet link, prints progress, and exits on completion. Uses `libtorrent.session`
and polls `torrent_handle.status()`.

**Metrics captured per trial:**
- Wall-clock time to completion (seconds)
- Average download speed (MB/s)
- Peak RSS (from `/usr/bin/time -v`)
- CPU time (user + sys, from `/usr/bin/time -v`)

**`benchmarks/summarize.py`:** Read `results.csv`, compute mean/stddev per client,
print comparison table.

**Files:**
- `benchmarks/run_benchmark.sh`
- `benchmarks/lt_download.py`
- `benchmarks/summarize.py`

### Task 7: Tests

Minimal CLI tests — don't test ferrite internals, just CLI arg parsing and output.

1. `download` subcommand parses magnet URI correctly
2. `download` subcommand parses .torrent file path correctly
3. `info` subcommand displays correct metadata for a test .torrent
4. `create` subcommand produces a valid .torrent file
5. Ctrl-C triggers graceful shutdown (integration test with signal)

**File:** `crates/ferrite-cli/tests/cli.rs`

### Task 8: Wire `AddTorrentParams` facade (optional)

Currently `AddTorrentParams` fields are `#[allow(dead_code)]`. If time permits:

1. Add `SessionHandle::add(params: AddTorrentParams)` method that dispatches
   based on `TorrentSource` variant
2. Remove `#[allow(dead_code)]` from `AddTorrentParams` fields
3. Update CLI to use `AddTorrentParams` instead of direct `SessionHandle` methods
4. Update examples from M52 to use `AddTorrentParams`

This is a nice-to-have — the CLI works fine without it.

**Files:** `crates/ferrite-session/src/session.rs`, `crates/ferrite/src/client.rs`

### Task 9: Final verification

1. `cargo test --workspace` — all tests pass
2. `cargo clippy --workspace -- -D warnings` — zero warnings
3. `cargo build --release -p ferrite-cli` — binary builds
4. `ferrite info <test.torrent>` — sanity check
5. `ferrite download <magnet> -o /tmp/test` — downloads successfully
6. Run benchmark script once end-to-end
7. Commit and push

## Binary Name Consideration

The binary is named `ferrite` (via `[[bin]] name = "ferrite"`). This avoids
collision with any system package. If it conflicts with something on the system,
rename to `ferrite-cli` in the Cargo.toml.

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| clap | 4.x | CLI argument parsing (derive) |
| indicatif | 0.17 | Progress bar |
| ctrlc | 3.x | Ctrl-C signal handling |
| tokio | 1.x | Async runtime (already a workspace dep) |

## Version

Bump workspace version for this milestone (new binary crate).
Commit as `feat: add ferrite CLI binary and benchmark harness (M53)`.
