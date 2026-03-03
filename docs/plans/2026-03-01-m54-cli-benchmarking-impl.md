# M54: CLI Binary & Benchmarking — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a `ferrite-cli` binary for downloading/creating torrents with progress display, then benchmark it against rqbit and libtorrent.

**Architecture:** New `ferrite-cli` workspace crate with clap derive CLI, indicatif progress bar, and tokio-signal for Ctrl-C. Uses `ClientBuilder` → `SessionHandle` for session management, `subscribe()` for `TorrentFinished` alerts, and `torrent_stats()` for progress polling.

**Tech Stack:** Rust, clap 4 (derive), indicatif 0.17, tokio-signal, tracing-subscriber, ferrite facade crate

**Date:** 2026-03-01
**Milestone:** M54
**Phase:** 13 — Documentation, CLI & Benchmarking
**Depends on:** M52 complete (docs), M49 complete (pluggable disk I/O for DisabledDiskIo benchmark mode)

---

## API Reference (verified against codebase)

**Session lifecycle:**
- `ClientBuilder::new().listen_port(6881).download_dir(path).start().await` → `SessionHandle`
- `session.add_magnet(magnet: Magnet) -> Result<Id20>`
- `session.add_torrent(meta: TorrentMeta, storage: Option<Arc<dyn TorrentStorage>>) -> Result<Id20>`
- `session.torrent_stats(info_hash: Id20) -> Result<TorrentStats>`
- `session.shutdown().await` — fire-and-forget, returns immediately
- `session.subscribe() -> broadcast::Receiver<Alert>` — all alerts
- `session.subscribe_filtered(filter: AlertCategory) -> AlertStream`

**Alert for completion:** `AlertKind::TorrentFinished { info_hash: Id20 }` (NOT `TorrentCompleted`)

**TorrentStats key fields:**
- `state: TorrentState`, `progress: f32` (0.0–1.0), `total_done: u64`, `total_wanted: u64`
- `download_rate: u64`, `upload_rate: u64` (bytes/sec)
- `peers_connected: usize`, `pieces_have: u32`, `pieces_total: u32`
- `name: String`, `info_hashes: InfoHashes`, `is_finished: bool`, `is_seeding: bool`

**Torrent parsing:**
- `ferrite_core::torrent_from_bytes_any(data: &[u8]) -> Result<TorrentMeta>` — returns V1/V2/Hybrid
- `TorrentMeta::info_hashes() -> InfoHashes` — may have v1 (Id20) and/or v2 (Id32)
- `TorrentMeta::version() -> TorrentVersion` (V1Only/V2Only/Hybrid)
- `TorrentMetaV1` fields: `info_hash`, `announce`, `announce_list`, `comment`, `created_by`, `creation_date`, `info: InfoDict`
- `InfoDict` fields: `name`, `piece_length`, `pieces`, `length` (single), `files` (multi), `private`, `source`

**Torrent creation:**
- `CreateTorrent::new().add_file(path).add_tracker(url, tier).set_private(bool).generate() -> Result<CreateTorrentResult>`
- `CreateTorrentResult { meta: TorrentMeta, bytes: Vec<u8> }`

**Magnet parsing:**
- `Magnet::parse(uri: &str) -> Result<Magnet>` — `magnet.info_hashes: InfoHashes`, `magnet.display_name`

**Settings:**
- `Settings` has full `Serialize`/`Deserialize` — JSON config file support works out of the box

**rqbit CLI syntax (verified):**
- `rqbit download <TORRENT_PATH>... -o <OUTPUT_FOLDER> -e` (`-e` = exit on finish)

**Workspace:**
- Version: `0.60.0`, edition `2024`, resolver `2`
- Members: `["crates/*"]` — new crate auto-discovered

---

## Tasks

### Task 1: Create `ferrite-cli` crate scaffold

**Files:**
- Create: `crates/ferrite-cli/Cargo.toml`
- Create: `crates/ferrite-cli/src/main.rs` (minimal `fn main`)

**Step 1: Create Cargo.toml**

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
tokio = { workspace = true }
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

Note: Using `tokio::signal` instead of the `ctrlc` crate — tokio already provides signal handling.

**Step 2: Create minimal main.rs**

```rust
fn main() {
    println!("ferrite CLI placeholder");
}
```

**Step 3: Verify it compiles**

Run: `cargo build -p ferrite-cli`
Expected: Compiles successfully

**Step 4: Commit**

```bash
git add crates/ferrite-cli/
git commit -m "feat(cli): scaffold ferrite-cli crate (M54)"
```

---

### Task 2: Implement CLI argument parsing

**Files:**
- Modify: `crates/ferrite-cli/src/main.rs`

**Step 1: Write arg parsing tests**

Create `crates/ferrite-cli/tests/cli_args.rs`:

```rust
use std::process::Command;

#[test]
fn test_no_args_shows_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .output()
        .expect("failed to run ferrite");
    // clap exits 2 on missing subcommand
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Usage") || stderr.contains("usage"),
        "expected usage text, got: {stderr}"
    );
}

#[test]
fn test_help_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .arg("--help")
        .output()
        .expect("failed to run ferrite");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("download"));
    assert!(stdout.contains("create"));
    assert!(stdout.contains("info"));
}

#[test]
fn test_download_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args(["download", "--help"])
        .output()
        .expect("failed to run ferrite");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--output"));
    assert!(stdout.contains("--seed"));
    assert!(stdout.contains("--port"));
}

#[test]
fn test_create_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args(["create", "--help"])
        .output()
        .expect("failed to run ferrite");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--tracker"));
    assert!(stdout.contains("--private"));
}

#[test]
fn test_info_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args(["info", "--help"])
        .output()
        .expect("failed to run ferrite");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("path"));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p ferrite-cli --test cli_args`
Expected: FAIL (main doesn't use clap yet)

**Step 3: Implement clap CLI parsing**

Replace `crates/ferrite-cli/src/main.rs`:

```rust
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ferrite", about = "BitTorrent client powered by ferrite")]
struct Cli {
    /// Log level (error, warn, info, debug, trace)
    #[arg(short, long, default_value = "warn")]
    log_level: String,

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
        /// Path to JSON settings file
        #[arg(long)]
        config: Option<PathBuf>,
        /// Seed after completion (default: exit on completion)
        #[arg(long)]
        seed: bool,
        /// Listen port
        #[arg(short, long, default_value = "6881")]
        port: u16,
        /// Quiet mode — suppress progress bar
        #[arg(short, long)]
        quiet: bool,
    },
    /// Create a .torrent file
    Create {
        /// Path to file or directory
        path: PathBuf,
        /// Output .torrent file path
        #[arg(short, long, default_value = "output.torrent")]
        output: PathBuf,
        /// Tracker URL(s) — can specify multiple: -t url1 -t url2
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

fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log_level.parse().unwrap_or_else(|_| "warn".into())),
        )
        .with_target(false)
        .init();

    match cli.command {
        Command::Download { .. } => {
            eprintln!("download: not yet implemented");
            std::process::exit(1);
        }
        Command::Create { .. } => {
            eprintln!("create: not yet implemented");
            std::process::exit(1);
        }
        Command::Info { .. } => {
            eprintln!("info: not yet implemented");
            std::process::exit(1);
        }
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferrite-cli --test cli_args`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/ferrite-cli/
git commit -m "feat(cli): add clap argument parsing with download/create/info subcommands (M54)"
```

---

### Task 3: Implement `info` subcommand

Simplest subcommand — no async, no session. Good starting point.

**Files:**
- Create: `crates/ferrite-cli/src/info.rs`
- Modify: `crates/ferrite-cli/src/main.rs`

**Step 1: Write info test**

Add to `crates/ferrite-cli/tests/cli_args.rs`:

```rust
#[test]
fn test_info_nonexistent_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args(["info", "/tmp/nonexistent_ferrite_test.torrent"])
        .output()
        .expect("failed to run ferrite");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No such file") || stderr.contains("not found") || stderr.contains("Error"),
        "expected error message, got: {stderr}"
    );
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferrite-cli --test cli_args::test_info_nonexistent_file`
Expected: FAIL (exits 1 with "not yet implemented")

**Step 3: Implement info subcommand**

Create `crates/ferrite-cli/src/info.rs`:

```rust
use std::path::Path;

pub fn run(path: &Path) -> anyhow::Result<()> {
    let data = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;

    let meta = ferrite::core::torrent_from_bytes_any(&data)
        .map_err(|e| anyhow::anyhow!("failed to parse torrent: {e}"))?;

    let hashes = meta.info_hashes();
    let version = meta.version();

    println!("Name:         {}", meta_name(&meta));
    if let Some(v1) = hashes.v1 {
        println!("Info hash v1: {v1}");
    }
    if let Some(v2) = hashes.v2 {
        println!("Info hash v2: {v2}");
    }
    println!("Version:      {version:?}");

    match &meta {
        ferrite::core::TorrentMeta::V1(t) => print_v1_details(t),
        ferrite::core::TorrentMeta::V2(t) => print_v2_details(t),
        ferrite::core::TorrentMeta::Hybrid(v1, _v2) => {
            // Show v1 details (they share the same content)
            print_v1_details(v1);
        }
    }

    Ok(())
}

fn meta_name(meta: &ferrite::core::TorrentMeta) -> &str {
    match meta {
        ferrite::core::TorrentMeta::V1(t) => &t.info.name,
        ferrite::core::TorrentMeta::V2(t) => &t.info.name,
        ferrite::core::TorrentMeta::Hybrid(v1, _) => &v1.info.name,
    }
}

fn print_v1_details(t: &ferrite::core::TorrentMetaV1) {
    let total_size = if let Some(len) = t.info.length {
        len
    } else if let Some(ref files) = t.info.files {
        files.iter().map(|f| f.length).sum()
    } else {
        0
    };

    let pieces = t.info.pieces.len() / 20;

    println!("Total size:   {}", format_size(total_size));
    println!("Piece size:   {}", format_size(t.info.piece_length));
    println!("Pieces:       {pieces}");

    // Files
    if let Some(ref files) = t.info.files {
        println!("Files:        {}", files.len());
        for f in files {
            let path = f.path.join("/");
            println!("  {path} ({})", format_size(f.length));
        }
    } else {
        println!("Files:        1");
        println!("  {} ({})", t.info.name, format_size(total_size));
    }

    // Trackers
    if let Some(ref tiers) = t.announce_list {
        println!("Trackers:");
        for tier in tiers {
            for url in tier {
                println!("  {url}");
            }
        }
    } else if let Some(ref url) = t.announce {
        println!("Trackers:");
        println!("  {url}");
    }

    println!(
        "Private:      {}",
        if t.info.private == Some(1) {
            "yes"
        } else {
            "no"
        }
    );

    if let Some(ts) = t.creation_date {
        println!("Created:      {ts} (unix)");
    }
    if let Some(ref c) = t.comment {
        println!("Comment:      {c}");
    }
    if let Some(ref by) = t.created_by {
        println!("Created by:   {by}");
    }
}

fn print_v2_details(t: &ferrite::core::TorrentMetaV2) {
    println!("Total size:   {}", format_size(t.info.length));
    println!("Piece size:   {}", format_size(t.info.piece_length));

    let files = t.info.files();
    println!("Files:        {}", files.len());
    for f in &files {
        println!("  {} ({})", f.path, format_size(f.length));
    }

    if let Some(ref tiers) = t.announce_list {
        println!("Trackers:");
        for tier in tiers {
            for url in tier {
                println!("  {url}");
            }
        }
    } else if let Some(ref url) = t.announce {
        println!("Trackers:");
        println!("  {url}");
    }
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}
```

Add `anyhow = "1"` to `[dependencies]` in `crates/ferrite-cli/Cargo.toml`.

**Step 4: Wire into main.rs**

Add `mod info;` and update the `Info` match arm:

```rust
Command::Info { path } => {
    if let Err(e) = info::run(&path) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
```

**Step 5: Run tests**

Run: `cargo test -p ferrite-cli --test cli_args`
Expected: PASS (nonexistent file now gives proper error)

**Step 6: Commit**

```bash
git add crates/ferrite-cli/
git commit -m "feat(cli): implement info subcommand with v1/v2/hybrid support (M54)"
```

---

### Task 4: Implement `create` subcommand

**Files:**
- Create: `crates/ferrite-cli/src/create.rs`
- Modify: `crates/ferrite-cli/src/main.rs`

**Step 1: Implement create subcommand**

Create `crates/ferrite-cli/src/create.rs`:

```rust
use std::path::Path;

pub fn run(
    path: &Path,
    output: &Path,
    trackers: &[String],
    private: bool,
    piece_size: Option<u64>,
) -> anyhow::Result<()> {
    let mut builder = ferrite::core::CreateTorrent::new();

    if path.is_dir() {
        builder = builder.add_directory(path);
    } else {
        builder = builder.add_file(path);
    }

    for (i, url) in trackers.iter().enumerate() {
        builder = builder.add_tracker(url, i as u32);
    }

    if private {
        builder = builder.set_private(true);
    }

    if let Some(size_kib) = piece_size {
        builder = builder.set_piece_size(size_kib * 1024);
    }

    let result = builder.generate_with_progress(|current, total| {
        if total > 0 {
            eprint!("\rHashing: {current}/{total} pieces");
        }
    })?;
    eprintln!(); // newline after progress

    std::fs::write(output, &result.bytes)?;

    let hashes = result.meta.info_hashes();
    eprintln!("Created: {}", output.display());
    if let Some(v1) = hashes.v1 {
        eprintln!("Info hash v1: {v1}");
    }
    if let Some(v2) = hashes.v2 {
        eprintln!("Info hash v2: {v2}");
    }
    eprintln!("Pieces: {}", result.meta.as_v1().map(|t| t.info.pieces.len() / 20).unwrap_or(0));

    Ok(())
}
```

**Step 2: Wire into main.rs**

Add `mod create;` and update the `Create` match arm:

```rust
Command::Create {
    path,
    output,
    tracker,
    private,
    piece_size,
} => {
    if let Err(e) = create::run(&path, &output, &tracker, private, piece_size) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
```

**Step 3: Add integration test**

Add to `crates/ferrite-cli/tests/cli_args.rs`:

```rust
#[test]
fn test_create_and_info_roundtrip() {
    let dir = std::env::temp_dir().join("ferrite_cli_test_create");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let test_file = dir.join("test.txt");
    std::fs::write(&test_file, "hello ferrite").unwrap();

    let torrent_path = dir.join("test.torrent");

    // Create torrent
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args([
            "create",
            test_file.to_str().unwrap(),
            "-o",
            torrent_path.to_str().unwrap(),
            "-t",
            "http://tracker.example.com/announce",
        ])
        .output()
        .expect("failed to run ferrite create");
    assert!(output.status.success(), "create failed: {}", String::from_utf8_lossy(&output.stderr));
    assert!(torrent_path.exists());

    // Info on created torrent
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite"))
        .args(["info", torrent_path.to_str().unwrap()])
        .output()
        .expect("failed to run ferrite info");
    assert!(output.status.success(), "info failed: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("test.txt"), "expected filename in info output");
    assert!(stdout.contains("tracker.example.com"), "expected tracker in info output");

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}
```

**Step 4: Run tests**

Run: `cargo test -p ferrite-cli`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/ferrite-cli/
git commit -m "feat(cli): implement create subcommand with progress display (M54)"
```

---

### Task 5: Implement `download` subcommand

The main feature. Uses async tokio runtime, `ClientBuilder`, alert subscription, and indicatif progress bar.

**Files:**
- Create: `crates/ferrite-cli/src/download.rs`
- Modify: `crates/ferrite-cli/src/main.rs`

**Step 1: Implement download subcommand**

Create `crates/ferrite-cli/src/download.rs`:

```rust
use anyhow::Context;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ferrite::core::{Magnet, TorrentMeta, torrent_from_bytes_any};
use ferrite::session::{AlertCategory, SessionHandle, TorrentState};
use ferrite::ClientBuilder;

pub async fn run(
    source: &str,
    output: &Path,
    sequential: bool,
    no_dht: bool,
    config: Option<&Path>,
    seed: bool,
    port: u16,
    quiet: bool,
) -> anyhow::Result<()> {
    // Load settings
    let mut builder = if let Some(config_path) = config {
        let data = std::fs::read_to_string(config_path)
            .with_context(|| format!("failed to read config: {}", config_path.display()))?;
        let settings: ferrite::session::Settings = serde_json::from_str(&data)
            .with_context(|| "failed to parse settings JSON")?;
        ClientBuilder::from_settings(settings)
    } else {
        ClientBuilder::new()
    };

    builder = builder.listen_port(port).download_dir(output);

    if no_dht {
        builder = builder.enable_dht(false);
    }

    let session = builder.start().await?;

    // Subscribe for alerts before adding torrent
    let mut alerts = session.subscribe_filtered(AlertCategory::STATUS);

    // Parse source and add torrent
    let info_hash = if source.starts_with("magnet:") {
        let magnet = Magnet::parse(source)
            .map_err(|e| anyhow::anyhow!("invalid magnet URI: {e}"))?;
        let name = magnet.display_name.clone().unwrap_or_default();
        if !name.is_empty() {
            eprintln!("Adding: {name}");
        }
        session.add_magnet(magnet).await?
    } else {
        let data = std::fs::read(source)
            .with_context(|| format!("failed to read torrent file: {source}"))?;
        let meta = torrent_from_bytes_any(&data)
            .map_err(|e| anyhow::anyhow!("failed to parse torrent: {e}"))?;
        let ih = meta.info_hashes().best_v1()
            .ok_or_else(|| anyhow::anyhow!("torrent has no info hash"))?;
        session.add_torrent(meta, None).await?;
        ih
    };

    if sequential {
        // If sequential download is needed, set it via torrent handle
        // session.set_sequential_download(info_hash, true) — if available
        tracing::info!("sequential download requested");
    }

    // Ctrl-C handler
    let shutdown = Arc::new(AtomicBool::new(false));
    let s = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        s.store(true, Ordering::SeqCst);
    });

    // Progress bar
    let pb = if quiet {
        None
    } else {
        let pb = ProgressBar::new(100);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{bar:40.cyan/blue}] {msg}"
            )
            .unwrap()
            .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        Some(pb)
    };

    // Poll loop
    let mut finished = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            if let Some(ref pb) = pb {
                pb.finish_with_message("shutting down...");
            }
            eprintln!("\nShutting down...");
            session.shutdown().await?;
            tokio::time::sleep(Duration::from_secs(1)).await;
            break;
        }

        // Check for TorrentFinished alert (non-blocking drain)
        while let Ok(alert) = alerts.try_recv() {
            if let ferrite::session::AlertKind::TorrentFinished { info_hash: ih } = alert.kind {
                if ih == info_hash {
                    finished = true;
                }
            }
        }

        if finished {
            if let Some(ref pb) = pb {
                pb.finish_with_message("download complete!");
            }
            if seed {
                eprintln!("Seeding... press Ctrl-C to stop.");
                // Wait for Ctrl-C while seeding
                tokio::signal::ctrl_c().await?;
                eprintln!("\nShutting down...");
                session.shutdown().await?;
                tokio::time::sleep(Duration::from_secs(1)).await;
            } else {
                session.shutdown().await?;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            break;
        }

        // Update progress
        if let Ok(stats) = session.torrent_stats(info_hash).await {
            if let Some(ref pb) = pb {
                let pct = (stats.progress * 100.0) as u64;
                pb.set_position(pct);

                let done = format_size(stats.total_done);
                let total = format_size(stats.total_wanted);
                let down_rate = format_rate(stats.download_rate);
                let up_rate = format_rate(stats.upload_rate);
                let peers = stats.peers_connected;

                pb.set_message(format!(
                    "{:.1}% | {done}/{total} | ↓ {down_rate} ↑ {up_rate} | {peers} peers",
                    stats.progress * 100.0,
                ));
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Ok(())
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_rate(bytes_per_sec: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;

    if bytes_per_sec >= MIB {
        format!("{:.1} MB/s", bytes_per_sec as f64 / MIB as f64)
    } else if bytes_per_sec >= KIB {
        format!("{:.1} KB/s", bytes_per_sec as f64 / KIB as f64)
    } else {
        format!("{bytes_per_sec} B/s")
    }
}
```

**Step 2: Wire into main.rs**

Add `mod download;` and update the `Download` match arm. Since download is async, wrap main in tokio:

Replace the `main()` function:

```rust
mod create;
mod download;
mod info;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log_level.parse().unwrap_or_else(|_| "warn".into())),
        )
        .with_target(false)
        .init();

    let result = match cli.command {
        Command::Download {
            source,
            output,
            sequential,
            no_dht,
            config,
            seed,
            port,
            quiet,
        } => {
            download::run(
                &source,
                &output,
                sequential,
                no_dht,
                config.as_deref(),
                seed,
                port,
                quiet,
            )
            .await
        }
        Command::Create {
            path,
            output,
            tracker,
            private,
            piece_size,
        } => create::run(&path, &output, &tracker, private, piece_size),
        Command::Info { path } => info::run(&path),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
```

Add `serde_json = "1"` to `[dependencies]` in `crates/ferrite-cli/Cargo.toml`.

**Step 3: Verify it compiles**

Run: `cargo build -p ferrite-cli`
Expected: Compiles (may need minor API adjustments — see notes below)

**API Adaptation Notes:**
The plan code uses the API as documented. If any method signatures differ slightly (e.g., `ClientBuilder::from_settings` might not exist — use `ClientBuilder::new()` + manual field assignment, or construct `Settings` and convert), adapt accordingly. The key flow is:
1. Build `ClientBuilder` → `.start().await` → `SessionHandle`
2. `session.add_magnet()` or `session.add_torrent(meta, None)`
3. `session.subscribe_filtered(AlertCategory::STATUS)` for `TorrentFinished`
4. Poll `session.torrent_stats(info_hash)` every 500ms
5. `session.shutdown().await` on exit

Check if `AlertStream` has `try_recv()` — if not, use `tokio::select!` with `alerts.recv()` and a 500ms sleep timer instead.

If `ClientBuilder::download_dir()` doesn't exist, set it on `Settings` directly:
```rust
let mut settings = Settings::default();
settings.download_dir = Some(output.to_path_buf());
```

**Step 4: Run all tests**

Run: `cargo test -p ferrite-cli`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/ferrite-cli/
git commit -m "feat(cli): implement download subcommand with progress bar and Ctrl-C handling (M54)"
```

---

### Task 6: Write benchmark harness

**Files:**
- Create: `benchmarks/run_benchmark.sh`
- Create: `benchmarks/lt_download.py`
- Create: `benchmarks/summarize.py`

**Step 1: Create libtorrent benchmark script**

Create `benchmarks/lt_download.py`:

```python
#!/usr/bin/env python3
"""Download a torrent using libtorrent and exit on completion."""
import sys
import time
import libtorrent as lt

def main():
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <magnet_or_torrent> <output_dir>", file=sys.stderr)
        sys.exit(1)

    source = sys.argv[1]
    output_dir = sys.argv[2]

    ses = lt.session()
    ses.listen_on(6891, 6899)

    settings = ses.get_settings()
    settings["alert_mask"] = lt.alert.category_t.all_categories
    ses.apply_settings(settings)

    if source.startswith("magnet:"):
        handle = lt.add_magnet_uri(ses, source, {"save_path": output_dir})
    else:
        info = lt.torrent_info(source)
        handle = ses.add_torrent({"ti": info, "save_path": output_dir})

    print(f"Downloading to {output_dir}...", file=sys.stderr)
    start = time.monotonic()

    while not handle.is_seed():
        s = handle.status()
        print(
            f"\r{s.progress * 100:.1f}% | "
            f"↓ {s.download_rate / 1024:.0f} KB/s | "
            f"{s.num_peers} peers",
            end="",
            file=sys.stderr,
        )
        time.sleep(1)

    elapsed = time.monotonic() - start
    s = handle.status()
    total_mb = s.total_done / (1024 * 1024)
    avg_speed = total_mb / elapsed if elapsed > 0 else 0
    print(f"\nDone in {elapsed:.1f}s ({avg_speed:.1f} MB/s)", file=sys.stderr)

if __name__ == "__main__":
    main()
```

**Step 2: Create benchmark runner**

Create `benchmarks/run_benchmark.sh`:

```bash
#!/bin/bash
set -euo pipefail

# Benchmark: ferrite vs rqbit vs libtorrent
# Usage: ./benchmarks/run_benchmark.sh <magnet_uri> [trials]

MAGNET="${1:?Usage: $0 <magnet_uri> [trials]}"
TRIALS="${2:-3}"
OUTPUT_DIR="/tmp/ferrite-bench"
RESULTS="benchmarks/results.csv"

FERRITE="$(cargo build --release -p ferrite-cli --message-format=json 2>/dev/null \
    | jq -r 'select(.executable != null and .target.name == "ferrite") | .executable' \
    | tail -1)"

if [ -z "$FERRITE" ] || [ ! -f "$FERRITE" ]; then
    echo "Building ferrite-cli..."
    cargo build --release -p ferrite-cli
    FERRITE="target/release/ferrite"
fi

echo "Ferrite binary: $FERRITE"
echo "Magnet: $MAGNET"
echo "Trials: $TRIALS"
echo ""

echo "client,trial,time_secs,avg_speed_mbps,peak_rss_kb" > "$RESULTS"

parse_time_output() {
    local file="$1"
    local wall_time
    wall_time=$(grep "Elapsed (wall clock)" "$file" | sed 's/.*: //' | awk -F: '{
        if (NF == 3) print $1*3600 + $2*60 + $3;
        else if (NF == 2) print $1*60 + $2;
        else print $1
    }')
    local rss
    rss=$(grep "Maximum resident" "$file" | sed 's/[^0-9]//g')
    echo "${wall_time:-0} ${rss:-0}"
}

run_trial() {
    local client="$1"
    local trial="$2"
    local cmd="$3"
    local client_dir="$OUTPUT_DIR/$client"

    rm -rf "$client_dir"
    mkdir -p "$client_dir"

    echo "  Trial $trial: $client..."
    local time_file="$OUTPUT_DIR/${client}-time-${trial}.txt"

    /usr/bin/time -v bash -c "$cmd" 2>"$time_file" || true

    read -r wall_time rss <<< "$(parse_time_output "$time_file")"
    local size
    size=$(du -sb "$client_dir" 2>/dev/null | awk '{print $1}')
    local avg_speed
    avg_speed=$(echo "$size $wall_time" | awk '{if ($2 > 0) printf "%.2f", $1/1048576/$2; else print 0}')

    echo "$client,$trial,$wall_time,$avg_speed,$rss" >> "$RESULTS"
    echo "    → ${wall_time}s, ${avg_speed} MB/s, RSS ${rss} KB"
}

for trial in $(seq 1 "$TRIALS"); do
    echo "=== Trial $trial/$TRIALS ==="

    run_trial "ferrite" "$trial" \
        "$FERRITE download '$MAGNET' -o '$OUTPUT_DIR/ferrite' -q"

    run_trial "rqbit" "$trial" \
        "rqbit download '$MAGNET' -o '$OUTPUT_DIR/rqbit' -e"

    run_trial "libtorrent" "$trial" \
        "python benchmarks/lt_download.py '$MAGNET' '$OUTPUT_DIR/libtorrent'"

    echo ""
done

echo "Results saved to $RESULTS"
echo ""
python benchmarks/summarize.py "$RESULTS"
```

**Step 3: Create summary script**

Create `benchmarks/summarize.py`:

```python
#!/usr/bin/env python3
"""Summarize benchmark results from CSV."""
import csv
import sys
from collections import defaultdict
from math import sqrt

def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "benchmarks/results.csv"

    data = defaultdict(lambda: {"time": [], "speed": [], "rss": []})
    with open(path) as f:
        for row in csv.DictReader(f):
            c = row["client"]
            data[c]["time"].append(float(row["time_secs"]))
            data[c]["speed"].append(float(row["avg_speed_mbps"]))
            data[c]["rss"].append(float(row["peak_rss_kb"]) / 1024)  # to MiB

    def stats(vals):
        n = len(vals)
        if n == 0:
            return 0, 0
        mean = sum(vals) / n
        if n < 2:
            return mean, 0
        sd = sqrt(sum((x - mean) ** 2 for x in vals) / (n - 1))
        return mean, sd

    print(f"{'Client':<12} {'Time (s)':>12} {'Speed (MB/s)':>14} {'RSS (MiB)':>12}")
    print("-" * 52)
    for client in sorted(data):
        t_mean, t_sd = stats(data[client]["time"])
        s_mean, s_sd = stats(data[client]["speed"])
        r_mean, r_sd = stats(data[client]["rss"])
        print(
            f"{client:<12} "
            f"{t_mean:>7.1f} ±{t_sd:<4.1f} "
            f"{s_mean:>9.1f} ±{s_sd:<4.1f} "
            f"{r_mean:>7.1f} ±{r_sd:<4.1f}"
        )

if __name__ == "__main__":
    main()
```

**Step 4: Make scripts executable**

```bash
chmod +x benchmarks/run_benchmark.sh benchmarks/lt_download.py benchmarks/summarize.py
```

**Step 5: Commit**

```bash
git add benchmarks/
git commit -m "feat(bench): add benchmark harness for ferrite vs rqbit vs libtorrent (M54)"
```

---

### Task 7: Final verification

**Step 1: Run workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass (existing + new CLI tests)

**Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Zero warnings

**Step 3: Build release binary**

Run: `cargo build --release -p ferrite-cli`
Expected: Binary at `target/release/ferrite`

**Step 4: Smoke test info subcommand**

Run: `./target/release/ferrite info <path_to_any_test_torrent>`

Look for a .torrent file in the test fixtures:
```bash
find crates/ -name "*.torrent" -type f | head -1
```
If none exist, create one:
```bash
echo "test content" > /tmp/ferrite_test_file.txt
./target/release/ferrite create /tmp/ferrite_test_file.txt -o /tmp/test.torrent
./target/release/ferrite info /tmp/test.torrent
```
Expected: Displays torrent metadata

**Step 5: Bump version**

Update workspace version in root `Cargo.toml` from `0.60.0` to `0.61.0`.

**Step 6: Update CHANGELOG.md and README.md**

Add M54 entry to CHANGELOG.md. Add CLI usage section to README.md:

```markdown
## CLI Usage

```bash
# Download a torrent
ferrite download "magnet:?xt=urn:btih:..." -o /tmp/downloads

# Create a .torrent file
ferrite create ./my-file.tar.gz -t http://tracker.example.com/announce -o my-file.torrent

# Display torrent metadata
ferrite info my-file.torrent
```
```

**Step 7: Final commit**

```bash
git add -A
git commit -m "feat: add ferrite CLI binary and benchmark harness (M54)"
```

Push to both remotes:
```bash
git push origin main && git push github main
```

---

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| clap | 4.x | CLI argument parsing (derive) |
| indicatif | 0.17 | Progress bar |
| tokio | workspace | Async runtime + signal handling |
| tracing-subscriber | 0.3 | Log output for CLI |
| serde_json | 1 | Settings JSON config loading |
| anyhow | 1 | CLI error handling |
