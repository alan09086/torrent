# M52: API Documentation & Examples — Implementation Plan

**Date:** 2026-03-01
**Milestone:** M52
**Phase:** 13 — Documentation, CLI & Benchmarking
**Depends on:** M49-M51 complete (API stable)

## Goal

Comprehensive documentation pass: enable `#![warn(missing_docs)]` on all crates,
document all high-traffic public types, create working examples, and add a
README "Getting Started" section.

## Current State

- Only the facade crate (`ferrite`) has `#![warn(missing_docs)]`
- 9 inner crates have zero doc linting
- `TorrentState`: 7 of 8 variants undocumented
- `TorrentStats`: 6 of 8 fields undocumented
- `TorrentInfo`: 7 of 7 fields undocumented
- `SessionStats`: 4 of 4 fields undocumented
- `AlertKind`: ~25-30 of ~50 variants lack `///` docs
- No `examples/` directory
- README has no "Getting Started" or usage code

## Tasks

### Task 1: Enable `#![warn(missing_docs)]` on leaf crates

Add the lint to crates with no downstream dependants first (fewer items to fix).

**Order (leaf → root):**

1. `ferrite-bencode` — small crate, mostly serde traits
2. `ferrite-utp` — self-contained transport
3. `ferrite-nat` — small (20 tests)
4. `ferrite-tracker` — announce/scrape types

For each crate:
1. Add `#![warn(missing_docs)]` to `lib.rs`
2. `cargo check -p <crate>` to see warnings
3. Add `///` doc comments to all undocumented public items
4. Verify `cargo check -p <crate>` is warning-free

**Files:** `crates/{ferrite-bencode,ferrite-utp,ferrite-nat,ferrite-tracker}/src/lib.rs` + any files with undocumented public items

### Task 2: Enable `#![warn(missing_docs)]` on mid-tier crates

These depend on ferrite-core and are depended on by ferrite-session.

1. `ferrite-core` — largest inner crate (177 tests), many public types
2. `ferrite-wire` — handshake, messages, MSE/PE, SSL
3. `ferrite-dht` — Kademlia, KRPC, BEP 42/44/51
4. `ferrite-storage` — TorrentStorage trait, Bitfield, ChunkTracker

Same process: add lint → check → fix → verify.

**Note:** ferrite-core will likely have the most gaps since it defines Id20, Id32,
Magnet, TorrentMetaV1/V2, CreateTorrent, Lengths, etc. The exploration report
rated it at ~80% coverage, so expect ~20-30 items to document.

**Files:** `crates/{ferrite-core,ferrite-wire,ferrite-dht,ferrite-storage}/src/lib.rs` + source files

### Task 3: Enable `#![warn(missing_docs)]` on ferrite-session

The largest crate (558 tests). This is where the biggest gaps are.

1. Add `#![warn(missing_docs)]` to `crates/ferrite-session/src/lib.rs`
2. Fix all warnings — focus areas:
   - `types.rs`: `TorrentState` variants, `TorrentStats` fields, `TorrentInfo` fields, `SessionStats` fields
   - `alert.rs`: `AlertKind` variants (~25-30 to document)
   - `session.rs`: `SessionHandle` methods (mostly documented, verify)
   - Any new types added in M49-M51

**Key types to document thoroughly (not just name-restating):**

`TorrentState` variants:
- `FetchingMetadata` — "Waiting for peers to send torrent metadata via BEP 9."
- `Checking` — "Verifying existing data on disk against piece hashes."
- `Downloading` — "Actively downloading pieces from peers."
- `Complete` — "All pieces downloaded, awaiting transition to seeding."
- `Seeding` — "Upload-only: all pieces verified, serving to other peers."
- `Paused` — "Manually paused by the user. No peer connections maintained."
- `Stopped` — "Removed from the session. Terminal state."

`TorrentStats` fields:
- `state` — "Current lifecycle state of the torrent."
- `downloaded` — "Total bytes downloaded from peers (payload only, excludes protocol overhead)."
- `uploaded` — "Total bytes uploaded to peers (payload only)."
- `pieces_have` — "Number of pieces we have verified and can serve."
- `pieces_total` — "Total number of pieces in the torrent."
- `peers_connected` — "Number of currently connected peers."
- `peers_available` — "Number of peers known but not connected (from DHT, PEX, trackers)."

`TorrentInfo` fields:
- `info_hash` — "SHA-1 info hash (v1) identifying this torrent."
- `name` — "Suggested name from the torrent metadata."
- `total_length` — "Total size in bytes across all files."
- `piece_length` — "Size in bytes of each piece (except possibly the last)."
- `num_pieces` — "Total number of pieces."
- `files` — "List of files in the torrent (single-file torrents have one entry)."
- `private` — "Whether this is a private torrent (BEP 27)."

`SessionStats` fields:
- `active_torrents` — "Number of torrents currently downloading or seeding."
- `total_downloaded` — "Cumulative bytes downloaded across all torrents this session."
- `total_uploaded` — "Cumulative bytes uploaded across all torrents this session."
- `dht_nodes` — "Number of nodes in the DHT routing table."

**Files:** `crates/ferrite-session/src/lib.rs`, `types.rs`, `alert.rs`, `session.rs`, and any other files with warnings

### Task 4: Create `examples/download.rs`

Minimal working example: download a torrent from a magnet link.

```rust
//! Download a torrent from a magnet link.
//!
//! Usage: cargo run --example download -- "magnet:?xt=urn:btih:..."

use ferrite::prelude::*;
use std::env;

#[tokio::main]
async fn main() -> ferrite::Result<()> {
    let magnet_uri = env::args().nth(1).expect("usage: download <magnet-uri>");
    let magnet = Magnet::parse(&magnet_uri).expect("invalid magnet URI");

    let session = ClientBuilder::new()
        .download_dir(".")
        .enable_dht(true)
        .start()
        .await?;

    let info_hash = session.add_magnet(magnet).await?;
    println!("Added torrent: {}", info_hash.to_hex());

    // Poll progress until complete
    loop {
        let stats = session.torrent_stats(info_hash).await?;
        println!(
            "[{:?}] {}/{} pieces, {} peers",
            stats.state, stats.pieces_have, stats.pieces_total, stats.peers_connected,
        );

        if matches!(stats.state, TorrentState::Seeding | TorrentState::Complete) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    println!("Download complete!");
    session.shutdown().await?;
    Ok(())
}
```

**File:** `examples/download.rs`

### Task 5: Create `examples/create.rs`

Create a .torrent file from a directory.

```rust
//! Create a .torrent file from a file or directory.
//!
//! Usage: cargo run --example create -- <path> -o output.torrent

use ferrite::prelude::*;
use std::env;
use std::fs;

fn main() -> ferrite::Result<()> {
    let path = env::args().nth(1).expect("usage: create <path> [-o output.torrent]");
    let output = env::args()
        .position(|a| a == "-o")
        .and_then(|i| env::args().nth(i + 1))
        .unwrap_or_else(|| "output.torrent".into());

    let result = CreateTorrent::new()
        .add_directory(&path)?
        .generate()?;

    fs::write(&output, &result.bytes)?;
    println!("Created {} ({} bytes)", output, result.bytes.len());
    Ok(())
}
```

**File:** `examples/create.rs`

### Task 6: Create `examples/stream.rs`

Stream a file from a torrent using `FileStream`.

```rust
//! Stream a file from a torrent via FileStream (AsyncRead + AsyncSeek).
//!
//! Usage: cargo run --example stream -- "magnet:?xt=urn:btih:..."

use ferrite::prelude::*;
use std::env;
use tokio::io::AsyncReadExt;

#[tokio::main]
async fn main() -> ferrite::Result<()> {
    let magnet_uri = env::args().nth(1).expect("usage: stream <magnet-uri>");
    let magnet = Magnet::parse(&magnet_uri).expect("invalid magnet URI");

    let session = ClientBuilder::new()
        .download_dir(".")
        .start()
        .await?;

    let info_hash = session.add_magnet(magnet).await?;

    // Wait for metadata
    loop {
        let stats = session.torrent_stats(info_hash).await?;
        if !matches!(stats.state, TorrentState::FetchingMetadata) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Open a FileStream for the first file
    let mut stream = session.file_stream(info_hash, 0).await?;
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    println!("Read {} bytes from stream", n);

    session.shutdown().await?;
    Ok(())
}
```

**File:** `examples/stream.rs`

**Note:** `session.file_stream()` may not exist yet as a public method. If not,
the example should use the alert system to get a `FileStream` handle, or this
method needs to be added to `SessionHandle` as part of this milestone.

### Task 7: Create `examples/dht_lookup.rs`

Standalone DHT peer lookup.

```rust
//! Look up peers for an info hash via DHT.
//!
//! Usage: cargo run --example dht_lookup -- <info-hash-hex>

use ferrite::prelude::*;
use std::env;

#[tokio::main]
async fn main() -> ferrite::Result<()> {
    let hex = env::args().nth(1).expect("usage: dht_lookup <info-hash-hex>");
    let info_hash = Id20::from_hex(&hex).expect("invalid hex info hash");

    let session = ClientBuilder::new()
        .enable_dht(true)
        .download_dir("/tmp")
        .start()
        .await?;

    println!("Bootstrapping DHT...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let stats = session.session_stats().await?;
    println!("DHT nodes: {}", stats.dht_nodes);
    println!("Looking up peers for {}...", info_hash.to_hex());

    // DHT lookup happens internally when a torrent is added;
    // demonstrate by adding a magnet and watching peer discovery
    let magnet = Magnet {
        info_hashes: InfoHashes::v1_only(info_hash),
        display_name: None,
        trackers: vec![],
        peers: vec![],
        selected_files: None,
    };

    let _ih = session.add_magnet(magnet).await?;
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let ts = session.torrent_stats(info_hash).await?;
        println!("Peers: {} connected, {} available", ts.peers_connected, ts.peers_available);
    }

    session.shutdown().await?;
    Ok(())
}
```

**File:** `examples/dht_lookup.rs`

### Task 8: Add README "Getting Started" section

Insert after the "Highlights" section in README.md:

```markdown
## Getting Started

Add ferrite to your `Cargo.toml`:

\`\`\`toml
[dependencies]
ferrite = "0.X.0"  # replace with current version
tokio = { version = "1", features = ["full"] }
\`\`\`

Download a torrent from a magnet link:

\`\`\`rust,no_run
use ferrite::prelude::*;

#[tokio::main]
async fn main() -> ferrite::Result<()> {
    let session = ClientBuilder::new()
        .download_dir("/tmp/downloads")
        .start()
        .await?;

    let magnet = Magnet::parse("magnet:?xt=urn:btih:...").unwrap();
    let info_hash = session.add_magnet(magnet).await?;

    loop {
        let stats = session.torrent_stats(info_hash).await?;
        if matches!(stats.state, TorrentState::Seeding) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    session.shutdown().await
}
\`\`\`

See [`examples/`](examples/) for more usage patterns.
```

**File:** `README.md`

### Task 9: `cargo doc` audit

1. Run `cargo doc --workspace --no-deps --document-private-items 2>&1` and check for warnings
2. Fix any broken intra-doc links (`[`TypeName`]` references)
3. Verify all public items render correctly in the generated docs
4. Check that re-exports in `prelude.rs` show inherited docs properly

**Files:** various (fix as discovered)

### Task 10: Final verification

1. `cargo test --workspace` — all tests pass
2. `cargo clippy --workspace -- -D warnings` — zero warnings
3. `cargo doc --workspace --no-deps 2>&1 | grep -i warning` — zero doc warnings
4. Commit and push

## Crate Order (for lint enablement)

```
ferrite-bencode  (leaf, 64 tests, small)
ferrite-utp      (leaf, 21 tests, self-contained)
ferrite-nat      (leaf, 20 tests, small)
ferrite-tracker  (leaf, 35 tests, announce types)
ferrite-core     (mid, 177 tests, largest inner — Id20, Magnet, metainfo)
ferrite-wire     (mid, 75 tests, handshake, messages)
ferrite-dht      (mid, 148 tests, Kademlia, KRPC)
ferrite-storage  (mid, 65 tests, TorrentStorage, Bitfield)
ferrite-session  (root, 558 tests, biggest gaps — types.rs, alert.rs)
```

The facade (`ferrite`) already has the lint.

## Estimated Effort

| Task | Items to document | Difficulty |
|------|------------------|------------|
| Leaf crates lint (4 crates) | ~30-40 items | Low |
| Mid-tier crates lint (4 crates) | ~60-80 items | Medium |
| ferrite-session lint | ~50-60 items | Medium |
| Examples (4 programs) | Net-new code | Medium |
| README update | 1 section | Low |
| cargo doc audit | Fix-as-found | Low |

## Version

No version bump for this milestone — documentation-only changes.
Commit as `docs: add API documentation and examples (M52)`.
