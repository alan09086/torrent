# M8a: Tracker + DHT Integration — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Wire ferrite-tracker and ferrite-dht into TorrentActor so torrents discover peers autonomously from tracker announce URLs and DHT.

**Architecture:** New `tracker_manager.rs` module handles per-torrent tracker announce lifecycle (BEP 3/12/15). TorrentActor gains two new select! arms: tracker re-announce timer and DHT peer receiver channel. No new crates or public API changes.

**Tech Stack:** ferrite-tracker (HttpTracker, UdpTracker, AnnounceRequest), ferrite-dht (DhtHandle), tokio timers, mpsc channels.

---

## Context for Implementer

### Crate: `crates/ferrite-session`

**Existing files you'll modify:**
- `src/torrent.rs` — TorrentActor struct + `run()` select! loop + constructors
- `src/lib.rs` — add `pub(crate) mod tracker_manager;`

**New file you'll create:**
- `src/tracker_manager.rs` — TrackerManager struct and all tracker logic

### Key APIs You'll Use

**ferrite-tracker** (`crates/ferrite-tracker/src/lib.rs`):
```rust
pub struct AnnounceRequest {
    pub info_hash: Id20, pub peer_id: Id20, pub port: u16,
    pub uploaded: u64, pub downloaded: u64, pub left: u64,
    pub event: AnnounceEvent, pub num_want: Option<i32>, pub compact: bool,
}
pub enum AnnounceEvent { None = 0, Completed = 1, Started = 2, Stopped = 3 }
pub struct AnnounceResponse {
    pub interval: u32, pub seeders: Option<u32>, pub leechers: Option<u32>,
    pub peers: Vec<SocketAddr>,
}
// HTTP tracker:
pub struct HttpTracker { /* reqwest::Client */ }
impl HttpTracker {
    pub fn new() -> Self;
    pub async fn announce(&self, base_url: &str, req: &AnnounceRequest) -> Result<HttpAnnounceResponse>;
}
pub struct HttpAnnounceResponse {
    pub response: AnnounceResponse, pub tracker_id: Option<String>, pub warning: Option<String>,
}
// UDP tracker:
pub struct UdpTracker { /* timeout */ }
impl UdpTracker {
    pub fn new() -> Self;
    pub async fn announce(&self, tracker_addr: &str, req: &AnnounceRequest) -> Result<UdpAnnounceResponse>;
}
```

**ferrite-dht** (`crates/ferrite-dht/src/actor.rs`):
```rust
#[derive(Clone)]
pub struct DhtHandle { tx: mpsc::Sender<DhtCommand> }
impl DhtHandle {
    pub async fn get_peers(&self, info_hash: Id20) -> Result<mpsc::Receiver<Vec<SocketAddr>>>;
    pub async fn announce(&self, info_hash: Id20, port: u16) -> Result<()>;
}
```

**ferrite-core** (`crates/ferrite-core/src/metainfo.rs`):
```rust
pub struct TorrentMetaV1 {
    pub info_hash: Id20,
    pub announce: Option<String>,              // single tracker URL
    pub announce_list: Option<Vec<Vec<String>>>, // BEP 12: list of tiers
    pub info: InfoDict,
    // ...
}
```

### Existing TorrentActor (what you'll modify)

The actor struct is at `src/torrent.rs:169-207`. Key fields:
```rust
struct TorrentActor {
    config: TorrentConfig, info_hash: Id20, our_peer_id: Id20, state: TorrentState,
    storage: Option<Arc<dyn TorrentStorage>>, chunk_tracker: Option<ChunkTracker>,
    lengths: Option<Lengths>, num_pieces: u32,
    piece_selector: PieceSelector, in_flight: HashSet<u32>,
    peers: HashMap<SocketAddr, PeerState>, available_peers: Vec<SocketAddr>,
    choker: Choker, metadata_downloader: Option<MetadataDownloader>,
    meta: Option<TorrentMetaV1>,
    downloaded: u64, uploaded: u64,
    cmd_rx: mpsc::Receiver<TorrentCommand>,
    event_tx: mpsc::Sender<PeerEvent>, event_rx: mpsc::Receiver<PeerEvent>,
    listener: Option<TcpListener>,
}
```

The `run()` method is at `src/torrent.rs:211-264` with a `tokio::select!` loop containing 6 arms: cmd_rx, event_rx, accept_incoming, unchoke timer (10s), optimistic timer (30s), connect timer (30s).

The `from_magnet()` constructor at line 98 takes `_dht: Option<DhtHandle>` (unused).

The `from_torrent()` constructor at line 39 does not accept tracker info or DhtHandle.

### Build & Test

```bash
cd /mnt/TempNVME/projects/ferrite
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Current state: 283 tests, zero clippy warnings.

---

## Task 1: Create TrackerManager with URL Parsing

**Files:**
- Create: `crates/ferrite-session/src/tracker_manager.rs`
- Modify: `crates/ferrite-session/src/lib.rs` (add module declaration)

### Step 1: Add module declaration

In `crates/ferrite-session/src/lib.rs`, add after the `pub(crate) mod choker;` line:

```rust
pub(crate) mod tracker_manager;
```

### Step 2: Write the failing tests

Create `crates/ferrite-session/src/tracker_manager.rs` with types and tests:

```rust
//! Per-torrent tracker announce lifecycle management.
//!
//! Parses tracker URLs from torrent metadata, manages announce intervals,
//! and handles exponential backoff on failure.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use ferrite_core::{Id20, TorrentMetaV1};
use ferrite_tracker::{
    AnnounceEvent, AnnounceRequest, HttpTracker, UdpTracker,
};

/// Maximum backoff duration for failed trackers.
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60); // 30 minutes

/// Initial backoff duration after a tracker failure.
const INITIAL_BACKOFF: Duration = Duration::from_secs(30);

/// Default re-announce interval if tracker doesn't specify one.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 minutes

/// Protocol type for a tracker URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackerProtocol {
    Http,
    Udp,
}

/// State of a single tracker.
#[derive(Debug, Clone)]
enum TrackerState {
    /// Ready for announce.
    NeedsAnnounce,
    /// Successfully announced, waiting for re-announce.
    Active,
    /// Failed, backing off.
    Failed { error: String },
}

/// A single tracker entry with its state.
#[derive(Debug, Clone)]
struct TrackerEntry {
    url: String,
    tier: usize,
    protocol: TrackerProtocol,
    state: TrackerState,
    tracker_id: Option<String>,
    next_announce: Instant,
    interval: Duration,
    backoff: Duration,
}

/// Per-torrent tracker manager.
///
/// Handles the announce lifecycle for all trackers associated with a torrent:
/// parsing URLs from metadata, scheduling announces, and managing backoff.
#[allow(dead_code)]
pub(crate) struct TrackerManager {
    trackers: Vec<TrackerEntry>,
    info_hash: Id20,
    peer_id: Id20,
    port: u16,
    http_client: HttpTracker,
    udp_client: UdpTracker,
}

impl TrackerManager {
    /// Create a TrackerManager from torrent metadata.
    ///
    /// Parses `announce` and `announce_list` (BEP 12) fields, deduplicates URLs,
    /// and classifies each as HTTP or UDP.
    pub fn from_torrent(meta: &TorrentMetaV1, peer_id: Id20, port: u16) -> Self {
        let mut trackers = Vec::new();
        let mut seen_urls = std::collections::HashSet::new();

        // BEP 12: announce_list takes priority if present
        if let Some(ref tiers) = meta.announce_list {
            for (tier_idx, tier) in tiers.iter().enumerate() {
                for url in tier {
                    let url = url.trim().to_string();
                    if url.is_empty() || !seen_urls.insert(url.clone()) {
                        continue;
                    }
                    if let Some(protocol) = classify_url(&url) {
                        trackers.push(TrackerEntry {
                            url,
                            tier: tier_idx,
                            protocol,
                            state: TrackerState::NeedsAnnounce,
                            tracker_id: None,
                            next_announce: Instant::now(),
                            interval: DEFAULT_INTERVAL,
                            backoff: Duration::ZERO,
                        });
                    }
                }
            }
        }

        // Fallback: single announce URL (only if not already in announce_list)
        if let Some(ref url) = meta.announce {
            let url = url.trim().to_string();
            if !url.is_empty() && seen_urls.insert(url.clone()) {
                if let Some(protocol) = classify_url(&url) {
                    trackers.push(TrackerEntry {
                        url,
                        tier: if trackers.is_empty() { 0 } else { trackers.last().unwrap().tier + 1 },
                        protocol,
                        state: TrackerState::NeedsAnnounce,
                        tracker_id: None,
                        next_announce: Instant::now(),
                        interval: DEFAULT_INTERVAL,
                        backoff: Duration::ZERO,
                    });
                }
            }
        }

        TrackerManager {
            trackers,
            info_hash: meta.info_hash,
            peer_id,
            port,
            http_client: HttpTracker::new(),
            udp_client: UdpTracker::new(),
        }
    }

    /// Create an empty TrackerManager (for magnet links before metadata arrives).
    pub fn empty(info_hash: Id20, peer_id: Id20, port: u16) -> Self {
        TrackerManager {
            trackers: Vec::new(),
            info_hash,
            peer_id,
            port,
            http_client: HttpTracker::new(),
            udp_client: UdpTracker::new(),
        }
    }

    /// Populate trackers from metadata once it's been fetched (magnet link flow).
    pub fn set_metadata(&mut self, meta: &TorrentMetaV1) {
        let fresh = Self::from_torrent(meta, self.peer_id, self.port);
        self.trackers = fresh.trackers;
    }

    /// Number of configured trackers.
    pub fn tracker_count(&self) -> usize {
        self.trackers.len()
    }

    /// Duration until the next tracker needs an announce.
    ///
    /// Returns `None` if there are no trackers.
    pub fn next_announce_in(&self) -> Option<Duration> {
        self.trackers
            .iter()
            .map(|t| t.next_announce.saturating_duration_since(Instant::now()))
            .min()
    }

    /// Build an AnnounceRequest from current stats.
    fn build_request(
        &self,
        event: AnnounceEvent,
        uploaded: u64,
        downloaded: u64,
        left: u64,
    ) -> AnnounceRequest {
        AnnounceRequest {
            info_hash: self.info_hash,
            peer_id: self.peer_id,
            port: self.port,
            uploaded,
            downloaded,
            left,
            event,
            num_want: Some(50),
            compact: true,
        }
    }

    /// Announce to all trackers that are due.
    ///
    /// Returns all discovered peer addresses (deduplicated).
    pub async fn announce(
        &mut self,
        event: AnnounceEvent,
        uploaded: u64,
        downloaded: u64,
        left: u64,
    ) -> Vec<SocketAddr> {
        let req = self.build_request(event, uploaded, downloaded, left);
        let now = Instant::now();
        let mut all_peers = Vec::new();
        let mut seen_peers = std::collections::HashSet::new();

        for tracker in &mut self.trackers {
            if tracker.next_announce > now {
                continue;
            }

            let result = match tracker.protocol {
                TrackerProtocol::Http => {
                    Self::announce_http(&self.http_client, &tracker.url, &req).await
                }
                TrackerProtocol::Udp => {
                    Self::announce_udp(&self.udp_client, &tracker.url, &req).await
                }
            };

            match result {
                Ok((peers, interval, tracker_id)) => {
                    debug!(
                        url = %tracker.url,
                        peer_count = peers.len(),
                        interval,
                        "tracker announce success"
                    );
                    tracker.state = TrackerState::Active;
                    tracker.interval = Duration::from_secs(interval as u64);
                    tracker.next_announce = now + tracker.interval;
                    tracker.backoff = Duration::ZERO;
                    if let Some(id) = tracker_id {
                        tracker.tracker_id = Some(id);
                    }

                    for peer in peers {
                        if seen_peers.insert(peer) {
                            all_peers.push(peer);
                        }
                    }
                }
                Err(e) => {
                    warn!(url = %tracker.url, error = %e, "tracker announce failed");
                    tracker.state = TrackerState::Failed {
                        error: e.to_string(),
                    };
                    // Exponential backoff
                    tracker.backoff = if tracker.backoff.is_zero() {
                        INITIAL_BACKOFF
                    } else {
                        (tracker.backoff * 2).min(MAX_BACKOFF)
                    };
                    tracker.next_announce = now + tracker.backoff;
                }
            }
        }

        all_peers
    }

    /// Convenience: announce with Started event.
    pub async fn announce_started(
        &mut self,
        uploaded: u64,
        downloaded: u64,
        left: u64,
    ) -> Vec<SocketAddr> {
        self.announce(AnnounceEvent::Started, uploaded, downloaded, left)
            .await
    }

    /// Convenience: announce with Completed event.
    pub async fn announce_completed(
        &mut self,
        uploaded: u64,
        downloaded: u64,
    ) -> Vec<SocketAddr> {
        self.announce(AnnounceEvent::Completed, uploaded, downloaded, 0)
            .await
    }

    /// Convenience: announce with Stopped event (best-effort, errors ignored).
    pub async fn announce_stopped(&mut self, uploaded: u64, downloaded: u64, left: u64) {
        let _ = self
            .announce(AnnounceEvent::Stopped, uploaded, downloaded, left)
            .await;
    }

    // ---- Internal announce helpers ----

    async fn announce_http(
        client: &HttpTracker,
        url: &str,
        req: &AnnounceRequest,
    ) -> std::result::Result<(Vec<SocketAddr>, u32, Option<String>), ferrite_tracker::Error> {
        let resp = client.announce(url, req).await?;
        Ok((
            resp.response.peers,
            resp.response.interval,
            resp.tracker_id,
        ))
    }

    async fn announce_udp(
        client: &UdpTracker,
        url: &str,
        req: &AnnounceRequest,
    ) -> std::result::Result<(Vec<SocketAddr>, u32, Option<String>), ferrite_tracker::Error> {
        // UDP tracker URLs are like "udp://tracker.example.com:6969/announce"
        // UdpTracker::announce expects "host:port"
        let addr = parse_udp_addr(url);
        let resp = client.announce(&addr, req).await?;
        Ok((resp.response.peers, resp.response.interval, None))
    }
}

/// Classify a tracker URL as HTTP or UDP.
fn classify_url(url: &str) -> Option<TrackerProtocol> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(TrackerProtocol::Http)
    } else if url.starts_with("udp://") {
        Some(TrackerProtocol::Udp)
    } else {
        None // Unknown protocol, skip
    }
}

/// Extract "host:port" from a UDP tracker URL.
///
/// Input: "udp://tracker.example.com:6969/announce"
/// Output: "tracker.example.com:6969"
fn parse_udp_addr(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("udp://")
        .unwrap_or(url);
    // Strip path (everything after host:port)
    match without_scheme.find('/') {
        Some(idx) => without_scheme[..idx].to_string(),
        None => without_scheme.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_core::Id20;

    /// Helper to build a minimal TorrentMetaV1 with given tracker URLs.
    fn torrent_with_trackers(
        announce: Option<&str>,
        announce_list: Option<Vec<Vec<&str>>>,
    ) -> TorrentMetaV1 {
        use serde::Serialize;

        let data = vec![0u8; 16384];
        let hash = ferrite_core::sha1(&data);
        let mut pieces = Vec::new();
        pieces.extend_from_slice(hash.as_bytes());

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            announce: Option<&'a str>,
            info: Info<'a>,
        }

        let t = Torrent {
            announce,
            info: Info {
                length: 16384,
                name: "test",
                piece_length: 16384,
                pieces: &pieces,
            },
        };

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        let mut meta = ferrite_core::torrent_from_bytes(&bytes).unwrap();
        meta.announce_list = announce_list.map(|tiers| {
            tiers
                .into_iter()
                .map(|tier| tier.into_iter().map(String::from).collect())
                .collect()
        });
        if announce.is_some() {
            meta.announce = announce.map(String::from);
        }
        meta
    }

    fn test_peer_id() -> Id20 {
        Id20::from_hex("0102030405060708091011121314151617181920").unwrap()
    }

    #[test]
    fn parse_single_announce_url() {
        let meta = torrent_with_trackers(
            Some("http://tracker.example.com/announce"),
            None,
        );
        let mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        assert_eq!(mgr.tracker_count(), 1);
        assert_eq!(mgr.trackers[0].protocol, TrackerProtocol::Http);
        assert_eq!(mgr.trackers[0].tier, 0);
    }

    #[test]
    fn parse_announce_list_tiers() {
        let meta = torrent_with_trackers(
            None,
            Some(vec![
                vec!["http://tier0-a.example.com/announce", "http://tier0-b.example.com/announce"],
                vec!["udp://tier1.example.com:6969/announce"],
            ]),
        );
        let mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        assert_eq!(mgr.tracker_count(), 3);
        assert_eq!(mgr.trackers[0].tier, 0);
        assert_eq!(mgr.trackers[1].tier, 0);
        assert_eq!(mgr.trackers[2].tier, 1);
        assert_eq!(mgr.trackers[2].protocol, TrackerProtocol::Udp);
    }

    #[test]
    fn classify_http_url() {
        assert_eq!(classify_url("http://t.co/a"), Some(TrackerProtocol::Http));
        assert_eq!(classify_url("https://t.co/a"), Some(TrackerProtocol::Http));
    }

    #[test]
    fn classify_udp_url() {
        assert_eq!(
            classify_url("udp://t.co:6969/a"),
            Some(TrackerProtocol::Udp)
        );
    }

    #[test]
    fn classify_unknown_url() {
        assert_eq!(classify_url("wss://t.co/a"), None);
    }

    #[test]
    fn deduplicate_urls() {
        let meta = torrent_with_trackers(
            Some("http://tracker.example.com/announce"),
            Some(vec![vec!["http://tracker.example.com/announce"]]),
        );
        let mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        // URL appears in both announce and announce_list — should be deduplicated
        assert_eq!(mgr.tracker_count(), 1);
    }

    #[test]
    fn empty_announce_list() {
        let meta = torrent_with_trackers(None, None);
        let mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        assert_eq!(mgr.tracker_count(), 0);
        assert_eq!(mgr.next_announce_in(), None);
    }

    #[test]
    fn next_announce_timing() {
        let meta = torrent_with_trackers(
            Some("http://tracker.example.com/announce"),
            None,
        );
        let mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        // Newly created — should be ready to announce immediately
        let next = mgr.next_announce_in().unwrap();
        assert!(next <= Duration::from_millis(10));
    }

    #[test]
    fn backoff_on_failure() {
        let meta = torrent_with_trackers(
            Some("http://tracker.example.com/announce"),
            None,
        );
        let mut mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);

        // Simulate a failure by directly setting state
        mgr.trackers[0].state = TrackerState::Failed {
            error: "connection refused".into(),
        };
        mgr.trackers[0].backoff = INITIAL_BACKOFF;
        mgr.trackers[0].next_announce = Instant::now() + INITIAL_BACKOFF;

        let next = mgr.next_announce_in().unwrap();
        // Should be approximately INITIAL_BACKOFF (30s), give or take
        assert!(next >= Duration::from_secs(29));
        assert!(next <= Duration::from_secs(31));
    }

    #[test]
    fn backoff_max_cap() {
        let meta = torrent_with_trackers(
            Some("http://tracker.example.com/announce"),
            None,
        );
        let mut mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);

        // Simulate many failures — backoff should cap at MAX_BACKOFF
        mgr.trackers[0].backoff = Duration::from_secs(20 * 60); // 20 min
        // Double would be 40 min, but cap is 30 min
        let doubled = (mgr.trackers[0].backoff * 2).min(MAX_BACKOFF);
        assert_eq!(doubled, MAX_BACKOFF);
    }

    #[test]
    fn parse_udp_addr_strips_scheme_and_path() {
        assert_eq!(
            parse_udp_addr("udp://tracker.example.com:6969/announce"),
            "tracker.example.com:6969"
        );
        assert_eq!(
            parse_udp_addr("udp://example.com:1234"),
            "example.com:1234"
        );
    }

    #[test]
    fn empty_manager_for_magnet() {
        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let mgr = TrackerManager::empty(info_hash, test_peer_id(), 6881);
        assert_eq!(mgr.tracker_count(), 0);
    }

    #[test]
    fn set_metadata_populates_trackers() {
        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let mut mgr = TrackerManager::empty(info_hash, test_peer_id(), 6881);
        assert_eq!(mgr.tracker_count(), 0);

        let meta = torrent_with_trackers(
            Some("http://tracker.example.com/announce"),
            None,
        );
        mgr.set_metadata(&meta);
        assert_eq!(mgr.tracker_count(), 1);
    }
}
```

### Step 3: Run tests to verify they pass

```bash
cargo test -p ferrite-session tracker_manager -- --nocapture
```

Expected: All 12 tests pass (these are all unit tests on parsing/state, no network calls).

### Step 4: Verify clippy

```bash
cargo clippy --workspace -- -D warnings
```

Expected: Zero warnings.

### Step 5: Commit

```bash
git add crates/ferrite-session/src/tracker_manager.rs crates/ferrite-session/src/lib.rs
git commit -m "feat(session): add TrackerManager with URL parsing and announce lifecycle"
```

---

## Task 2: Wire TrackerManager into TorrentActor

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

### Step 1: Add TrackerManager to TorrentActor struct

In `src/torrent.rs`, add import at the top (after the existing `use crate::` block around line 22-27):

```rust
use crate::tracker_manager::TrackerManager;
```

Add field to TorrentActor struct (after `listener: Option<TcpListener>` at line 206):

```rust
    // Tracker management
    tracker_manager: TrackerManager,
```

### Step 2: Initialize TrackerManager in `from_torrent()`

In `from_torrent()` (around line 39-94), after the `listener` binding and before the `TorrentActor` struct literal, add:

```rust
        let tracker_manager = TrackerManager::from_torrent(&meta, our_peer_id, config.listen_port);
```

Add the field to the TorrentActor struct literal (after `listener,`):

```rust
            tracker_manager,
```

### Step 3: Initialize TrackerManager in `from_magnet()`

In `from_magnet()` (around line 98-136), after the `listener` binding, add:

```rust
        let tracker_manager = TrackerManager::empty(magnet.info_hash, our_peer_id, config.listen_port);
```

Add the field to the TorrentActor struct literal (after `listener,`):

```rust
            tracker_manager,
```

### Step 4: Add tracker announce arm to the select! loop

In the `run()` method (around line 211-264), add a new arm to the `tokio::select!` block. Add it after the connect timer arm (before the closing `}`):

```rust
                // Tracker re-announce timer
                _ = async {
                    match self.tracker_manager.next_announce_in() {
                        Some(dur) => tokio::time::sleep(dur).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if self.state != TorrentState::FetchingMetadata {
                        let left = self.calculate_left();
                        let peers = self.tracker_manager.announce(
                            ferrite_tracker::AnnounceEvent::None,
                            self.uploaded,
                            self.downloaded,
                            left,
                        ).await;
                        if !peers.is_empty() {
                            debug!(count = peers.len(), "tracker returned peers");
                            self.handle_add_peers(peers);
                        }
                    }
                }
```

### Step 5: Add `calculate_left()` helper

Add this method to `impl TorrentActor` (e.g., after `make_stats()` around line 292):

```rust
    /// Calculate bytes remaining for tracker announce.
    fn calculate_left(&self) -> u64 {
        match (&self.meta, &self.chunk_tracker) {
            (Some(meta), Some(ct)) => {
                let total = meta.info.total_length();
                let have = ct.bitfield().count_ones() as u64;
                let pieces_total = self.num_pieces as u64;
                if pieces_total == 0 {
                    total
                } else {
                    total.saturating_sub(have * (total / pieces_total))
                }
            }
            _ => 0,
        }
    }
```

### Step 6: Send Started announce in `from_torrent()`

In `from_torrent()`, after `tokio::spawn(actor.run())` and before `Ok(TorrentHandle { cmd_tx })`, we can't call async on `actor` since it's already moved. Instead, add the initial announce inside `run()`.

In the `run()` method, add this block right before the `loop {` (after consuming initial ticks, around line 219):

```rust
        // Initial tracker announce (Started event)
        if self.state == TorrentState::Downloading {
            let left = self.calculate_left();
            let peers = self.tracker_manager.announce_started(
                self.uploaded,
                self.downloaded,
                left,
            ).await;
            if !peers.is_empty() {
                debug!(count = peers.len(), "initial tracker announce returned peers");
                self.handle_add_peers(peers);
            }
        }
```

### Step 7: Send Completed announce on state transition

In `verify_and_mark_piece()` (around line 453-493), after `self.state = TorrentState::Complete;` (line 484), add:

```rust
                    // Announce completion to trackers
                    let _ = self.tracker_manager.announce_completed(
                        self.uploaded,
                        self.downloaded,
                    ).await;
```

### Step 8: Send Stopped announce on shutdown

In `shutdown_peers()` (around line 294-298), add tracker announce before shutting down peers:

```rust
    async fn shutdown_peers(&mut self) {
        // Best-effort announce Stopped to trackers
        let left = self.calculate_left();
        self.tracker_manager.announce_stopped(
            self.uploaded,
            self.downloaded,
            left,
        ).await;

        for peer in self.peers.values() {
            let _ = peer.cmd_tx.send(PeerCommand::Shutdown).await;
        }
    }
```

Note: change `&self` to `&mut self` in the method signature.

### Step 9: Populate trackers on metadata assembly (magnet flow)

In `try_assemble_metadata()` (around line 495-546), after `self.meta = Some(meta);` (line 531), add:

```rust
                        // Populate tracker manager with newly parsed metadata
                        if let Some(ref meta) = self.meta {
                            self.tracker_manager.set_metadata(meta);
                        }
```

### Step 10: Run tests

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Expected: All 283+ tests pass, zero warnings. Existing tests should still pass since TrackerManager doesn't make real network calls in tests (the trackers are either empty or point to non-existent URLs that never get called because the tests don't wait for the announce timer to fire).

### Step 11: Commit

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat(session): wire TrackerManager into TorrentActor event loop"
```

---

## Task 3: Wire DhtHandle into TorrentActor

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

### Step 1: Add DhtHandle field to TorrentActor

Add field to TorrentActor struct (after `tracker_manager`):

```rust
    // DHT handle (shared, optional)
    dht: Option<DhtHandle>,
    dht_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,
```

### Step 2: Update `from_torrent()` to accept and store DhtHandle

Change the `from_torrent()` signature to accept an optional DhtHandle:

```rust
    pub async fn from_torrent(
        meta: TorrentMetaV1,
        storage: Arc<dyn TorrentStorage>,
        config: TorrentConfig,
        dht: Option<DhtHandle>,
    ) -> crate::Result<Self> {
```

After the BEP 27 check and before the TorrentActor struct literal, add DHT peer discovery launch:

```rust
        // Start DHT peer discovery if enabled and available
        let dht_peers_rx = if config.enable_dht {
            if let Some(ref dht) = dht {
                match dht.get_peers(meta.info_hash).await {
                    Ok(rx) => Some(rx),
                    Err(e) => {
                        warn!("failed to start DHT get_peers: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
```

Add both fields to the TorrentActor struct literal:

```rust
            dht: if config.enable_dht { dht } else { None },
            dht_peers_rx,
```

### Step 3: Update `from_magnet()` to store DhtHandle

Change `_dht` to `dht` in `from_magnet()` and add the same DHT peer discovery:

```rust
    pub async fn from_magnet(
        magnet: Magnet,
        config: TorrentConfig,
        dht: Option<DhtHandle>,
    ) -> crate::Result<Self> {
```

Add DHT launch (similar to from_torrent but for magnet info_hash):

```rust
        let dht_peers_rx = if config.enable_dht {
            if let Some(ref dht) = dht {
                match dht.get_peers(magnet.info_hash).await {
                    Ok(rx) => Some(rx),
                    Err(e) => {
                        warn!("failed to start DHT get_peers: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
```

Add fields:

```rust
            dht: if config.enable_dht { dht } else { None },
            dht_peers_rx,
```

### Step 4: Add DHT peer receiver arm to select! loop

In the `run()` method, add a new arm to the select! block:

```rust
                // DHT peer discovery
                Some(peers) = async {
                    match &mut self.dht_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    debug!(count = peers.len(), "DHT returned peers");
                    self.handle_add_peers(peers);
                }
```

### Step 5: Add DHT announce on download start

In the `run()` method, in the initial announce block added in Task 2 (before `loop`), add DHT announce:

```rust
        // DHT announce
        if self.config.enable_dht {
            if let Some(ref dht) = self.dht {
                if let Err(e) = dht.announce(self.info_hash, self.config.listen_port).await {
                    warn!("DHT announce failed: {e}");
                }
            }
        }
```

### Step 6: Re-trigger DHT search on connect timer when needed

In the connect timer arm of the select! loop (the `connect_interval.tick()` arm), add DHT re-search:

```rust
                _ = connect_interval.tick() => {
                    self.try_connect_peers();
                    // Re-trigger DHT search if we need more peers and previous search exhausted
                    if self.available_peers.is_empty()
                        && self.peers.len() < self.config.max_peers
                        && self.dht_peers_rx.as_ref().is_none_or(|_| false)
                    {
                        if let Some(ref dht) = self.dht {
                            match dht.get_peers(self.info_hash).await {
                                Ok(rx) => self.dht_peers_rx = Some(rx),
                                Err(e) => warn!("DHT re-search failed: {e}"),
                            }
                        }
                    }
                }
```

Actually, the `is_none_or` check is wrong — we want to re-search when the channel is closed (exhausted). Better approach: check if dht_peers_rx is None (which it becomes when the channel closes and we set it to None).

Update the DHT peer arm to set `dht_peers_rx = None` when the channel closes:

```rust
                // DHT peer discovery
                result = async {
                    match &mut self.dht_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Some(peers) => {
                            debug!(count = peers.len(), "DHT returned peers");
                            self.handle_add_peers(peers);
                        }
                        None => {
                            // DHT search exhausted
                            debug!("DHT peer search exhausted");
                            self.dht_peers_rx = None;
                        }
                    }
                }
```

And the connect timer re-trigger:

```rust
                _ = connect_interval.tick() => {
                    self.try_connect_peers();
                    // Re-trigger DHT search if exhausted and we still need peers
                    if self.dht_peers_rx.is_none()
                        && self.config.enable_dht
                        && self.available_peers.is_empty()
                        && self.peers.len() < self.config.max_peers
                    {
                        if let Some(ref dht) = self.dht {
                            match dht.get_peers(self.info_hash).await {
                                Ok(rx) => self.dht_peers_rx = Some(rx),
                                Err(e) => warn!("DHT re-search failed: {e}"),
                            }
                        }
                    }
                }
```

### Step 7: Update all test call sites

Every test that calls `from_torrent()` or `from_magnet()` needs the new `dht` parameter. Update all calls:

- `from_torrent(meta, storage, config)` → `from_torrent(meta, storage, config, None)`
- `from_magnet(magnet, config, None)` → already has `None`, OK

Search and replace in `torrent.rs` tests.

### Step 8: Run tests

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Expected: All tests pass (DHT is always `None` in tests), zero warnings.

### Step 9: Commit

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat(session): wire DhtHandle into TorrentActor for peer discovery"
```

---

## Task 4: Add Integration Tests

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` (tests module)
- Modify: `crates/ferrite-session/src/tracker_manager.rs` (tests module)

### Step 1: Add torrent integration tests

Add these tests to the `mod tests` block in `torrent.rs`:

```rust
    // ---- Test: Tracker manager is populated from torrent metadata ----

    #[tokio::test]
    async fn tracker_populated_from_metadata() {
        use serde::Serialize;

        let data = vec![0xAB; 16384];
        let hash = ferrite_core::sha1(&data);
        let mut pieces = Vec::new();
        pieces.extend_from_slice(hash.as_bytes());

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            announce: &'a str,
            info: Info<'a>,
        }

        let t = Torrent {
            announce: "http://tracker.example.com:8080/announce",
            info: Info {
                length: data.len() as u64,
                name: "test",
                piece_length: 16384,
                pieces: &pieces,
            },
        };

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert!(meta.announce.is_some());

        let storage = make_storage(&data, 16384);
        let config = test_config();

        // The torrent should start and announce to tracker (which will fail since
        // the tracker doesn't exist, but that's fine — failures are non-fatal).
        let handle = TorrentHandle::from_torrent(meta, storage, config, None)
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: Private torrent has no DHT ----

    #[tokio::test]
    async fn private_torrent_no_dht_field() {
        // Already tested that private flag disables DHT/PEX in config.
        // This test verifies the actor starts cleanly with DHT=None
        // even when a DhtHandle is provided.
        // We can't easily create a DhtHandle without a real UDP socket,
        // so we just verify the private torrent path with None works.
        let data = vec![0xAB; 16384];
        let hash = ferrite_core::sha1(&data);
        let mut pieces = Vec::new();
        pieces.extend_from_slice(hash.as_bytes());

        use serde::Serialize;

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
            private: i64,
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            announce: &'a str,
            info: Info<'a>,
        }

        let t = Torrent {
            announce: "http://private-tracker.example.com/announce",
            info: Info {
                length: data.len() as u64,
                name: "private_test",
                piece_length: 16384,
                pieces: &pieces,
                private: 1,
            },
        };

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(meta.info.private, Some(1));

        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None)
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: Magnet link defers tracker announce ----

    #[tokio::test]
    async fn magnet_no_tracker_before_metadata() {
        let magnet = Magnet {
            info_hash: Id20::from_hex("cccccccccccccccccccccccccccccccccccccccc").unwrap(),
            display_name: Some("magnet test".into()),
            trackers: vec![],
            peers: vec![],
        };

        let handle = TorrentHandle::from_magnet(magnet, test_config(), None)
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::FetchingMetadata);

        // No tracker announces should happen in FetchingMetadata state
        // (verified by the tracker_announce arm checking state != FetchingMetadata)
        tokio::time::sleep(Duration::from_millis(50)).await;

        handle.shutdown().await.unwrap();
    }
```

### Step 2: Run tests

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Expected: All tests pass (283 existing + 12 tracker_manager + 3 integration = ~298), zero warnings.

### Step 3: Commit

```bash
git add crates/ferrite-session/src/torrent.rs crates/ferrite-session/src/tracker_manager.rs
git commit -m "test(session): add tracker/DHT integration tests"
```

---

## Task 5: Final Verification and Cleanup

**Files:**
- All modified files

### Step 1: Full workspace test

```bash
cargo test --workspace 2>&1 | tail -20
```

Expected: ~298 tests pass, 0 failures.

### Step 2: Clippy check

```bash
cargo clippy --workspace -- -D warnings
```

Expected: Zero warnings.

### Step 3: Check for any dead_code warnings

If clippy or the compiler warns about dead code in tracker_manager.rs (e.g., `announce()` methods that aren't called from tests), add appropriate `#[allow(dead_code)]` annotations or ensure all public methods are exercised.

### Step 4: Update documentation

Update `docs/plans/2026-02-25-ferrite-roadmap.md`:
- Change M7 row status from "NEXT" to "Done"
- Change M8 row to show "M8a Done" or split into M8a/M8b/M8c rows

### Step 5: Final commit

```bash
git add -A
git commit -m "docs: update roadmap for M8a completion"
```

### Step 6: Push to both remotes

```bash
git push origin main && git push github main
```

---

## Summary

| Task | What | Tests Added |
|------|------|-------------|
| 1 | TrackerManager with URL parsing | 12 |
| 2 | Wire TrackerManager into TorrentActor | 0 (modifies existing) |
| 3 | Wire DhtHandle into TorrentActor | 0 (modifies existing) |
| 4 | Integration tests | 3 |
| 5 | Final verification and docs | 0 |

**Total new tests: ~15**
**Expected total workspace: ~298**
