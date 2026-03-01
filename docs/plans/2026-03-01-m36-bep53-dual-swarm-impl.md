# M36: BEP 53 + Dual-Swarm + Pure v2 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Complete the v2 ecosystem by adding BEP 53 magnet file selection (`&so=`), dual-swarm tracker/DHT announces for hybrid torrents, and removing the pure v2-only session guard.

**Architecture:** Three features span two crates. Feature 1 (BEP 53 `so=`) adds a `FileSelection` enum and `selected_files` field to `Magnet` in ferrite-core, with file priority mapping applied in ferrite-session's `try_assemble_metadata()`. Feature 2 (dual-swarm) stores `InfoHashes` on `TorrentActor` and `TrackerManager`, issuing parallel announces for both v1 and v2 hashes when hybrid. Feature 3 (pure v2) removes the `Error::Config` guard in `session.rs`, synthesizes a minimal `TorrentMetaV1` from `TorrentMetaV2` data for session compatibility, and uses SHA-256-truncated-to-20-bytes for the handshake info hash field.

**Tech Stack:** Rust (edition 2024), tokio async, serde, bytes, thiserror, tracing

---

## Feature 1: BEP 53 Magnet `&so=` Parameter

### Task 1: Add `FileSelection` enum to ferrite-core

**Files:**
- Create: `crates/ferrite-core/src/file_selection.rs`
- Modify: `crates/ferrite-core/src/lib.rs:3-41` (add module + re-export)

**Step 1: Write the failing test**

Create `crates/ferrite-core/src/file_selection.rs` with the enum and tests:

```rust
//! BEP 53 file selection for magnet URIs.
//!
//! The `so=` parameter selects specific files by index or range.
//! Example: `so=0,2,4-6` selects files 0, 2, 4, 5, and 6.

use crate::FilePriority;

/// A file selection entry from a BEP 53 `so=` parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSelection {
    /// A single file index.
    Single(usize),
    /// An inclusive range of file indices.
    Range(usize, usize),
}

impl FileSelection {
    /// Parse a `so=` parameter value into a list of file selections.
    ///
    /// Format: comma-separated entries, each either a single index or `start-end` range.
    /// Example: `"0,2,4-6"` -> `[Single(0), Single(2), Range(4, 6)]`
    pub fn parse(value: &str) -> Result<Vec<FileSelection>, String> {
        if value.is_empty() {
            return Err("empty so= value".into());
        }

        let mut selections = Vec::new();
        for part in value.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((start_str, end_str)) = part.split_once('-') {
                let start: usize = start_str
                    .parse()
                    .map_err(|_| format!("invalid range start: {start_str}"))?;
                let end: usize = end_str
                    .parse()
                    .map_err(|_| format!("invalid range end: {end_str}"))?;
                if start > end {
                    return Err(format!("invalid range: {start}-{end} (start > end)"));
                }
                selections.push(FileSelection::Range(start, end));
            } else {
                let index: usize = part
                    .parse()
                    .map_err(|_| format!("invalid file index: {part}"))?;
                selections.push(FileSelection::Single(index));
            }
        }

        if selections.is_empty() {
            return Err("no valid entries in so= value".into());
        }

        Ok(selections)
    }

    /// Convert file selections into a `FilePriority` vector.
    ///
    /// Selected files get `Normal` priority, unselected get `Skip`.
    /// `num_files` is the total number of files in the torrent.
    /// Out-of-range selections are silently ignored.
    pub fn to_priorities(selections: &[FileSelection], num_files: usize) -> Vec<FilePriority> {
        let mut priorities = vec![FilePriority::Skip; num_files];
        for sel in selections {
            match *sel {
                FileSelection::Single(i) => {
                    if i < num_files {
                        priorities[i] = FilePriority::Normal;
                    }
                }
                FileSelection::Range(start, end) => {
                    let end = end.min(num_files.saturating_sub(1));
                    for i in start..=end {
                        if i < num_files {
                            priorities[i] = FilePriority::Normal;
                        }
                    }
                }
            }
        }
        priorities
    }

    /// Serialize file selections back to a `so=` parameter value string.
    pub fn to_so_value(selections: &[FileSelection]) -> String {
        selections
            .iter()
            .map(|s| match s {
                FileSelection::Single(i) => i.to_string(),
                FileSelection::Range(start, end) => format!("{start}-{end}"),
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_index() {
        let sels = FileSelection::parse("3").unwrap();
        assert_eq!(sels, vec![FileSelection::Single(3)]);
    }

    #[test]
    fn parse_multiple_singles() {
        let sels = FileSelection::parse("0,2,5").unwrap();
        assert_eq!(
            sels,
            vec![
                FileSelection::Single(0),
                FileSelection::Single(2),
                FileSelection::Single(5),
            ]
        );
    }

    #[test]
    fn parse_range() {
        let sels = FileSelection::parse("4-6").unwrap();
        assert_eq!(sels, vec![FileSelection::Range(4, 6)]);
    }

    #[test]
    fn parse_mixed() {
        let sels = FileSelection::parse("0,2,4-6").unwrap();
        assert_eq!(
            sels,
            vec![
                FileSelection::Single(0),
                FileSelection::Single(2),
                FileSelection::Range(4, 6),
            ]
        );
    }

    #[test]
    fn parse_empty_rejected() {
        assert!(FileSelection::parse("").is_err());
    }

    #[test]
    fn parse_invalid_index() {
        assert!(FileSelection::parse("abc").is_err());
    }

    #[test]
    fn parse_invalid_range() {
        assert!(FileSelection::parse("6-4").is_err()); // start > end
    }

    #[test]
    fn to_priorities_basic() {
        let sels = vec![
            FileSelection::Single(0),
            FileSelection::Single(2),
            FileSelection::Range(4, 6),
        ];
        let prios = FileSelection::to_priorities(&sels, 8);
        assert_eq!(prios[0], FilePriority::Normal);
        assert_eq!(prios[1], FilePriority::Skip);
        assert_eq!(prios[2], FilePriority::Normal);
        assert_eq!(prios[3], FilePriority::Skip);
        assert_eq!(prios[4], FilePriority::Normal);
        assert_eq!(prios[5], FilePriority::Normal);
        assert_eq!(prios[6], FilePriority::Normal);
        assert_eq!(prios[7], FilePriority::Skip);
    }

    #[test]
    fn to_priorities_out_of_range_ignored() {
        let sels = vec![FileSelection::Single(10), FileSelection::Range(8, 12)];
        let prios = FileSelection::to_priorities(&sels, 5);
        // All should remain Skip since indices are beyond num_files
        assert!(prios.iter().all(|&p| p == FilePriority::Skip));
    }

    #[test]
    fn to_priorities_partial_range() {
        let sels = vec![FileSelection::Range(3, 100)];
        let prios = FileSelection::to_priorities(&sels, 5);
        assert_eq!(prios[0], FilePriority::Skip);
        assert_eq!(prios[1], FilePriority::Skip);
        assert_eq!(prios[2], FilePriority::Skip);
        assert_eq!(prios[3], FilePriority::Normal);
        assert_eq!(prios[4], FilePriority::Normal);
    }

    #[test]
    fn so_value_round_trip() {
        let sels = vec![
            FileSelection::Single(0),
            FileSelection::Single(2),
            FileSelection::Range(4, 6),
        ];
        let value = FileSelection::to_so_value(&sels);
        assert_eq!(value, "0,2,4-6");
        let parsed = FileSelection::parse(&value).unwrap();
        assert_eq!(parsed, sels);
    }
}
```

**Step 2: Register the module in lib.rs**

In `crates/ferrite-core/src/lib.rs`, add `mod file_selection;` after line 16 (`mod file_priority;`) and add `pub use file_selection::FileSelection;` after line 28 (`pub use file_priority::FilePriority;`).

**Step 3: Run test to verify it passes**
Run: `cargo test -p ferrite-core file_selection`
Expected: PASS (all 11 tests)

**Step 4: Run clippy**
Run: `cargo clippy -p ferrite-core -- -D warnings`
Expected: PASS

**Step 5: Commit**
```
feat: add FileSelection enum for BEP 53 so= parsing (M36)
```

---

### Task 2: Add `selected_files` field to `Magnet` and parse `so=` parameter

**Files:**
- Modify: `crates/ferrite-core/src/magnet.rs:1-284`

**Step 1: Add the field and parse logic**

In `crates/ferrite-core/src/magnet.rs`:

1. Add import at top (after existing `use` lines, line 5):
```rust
use crate::file_selection::FileSelection;
```

2. Add field to `Magnet` struct (after `peers` field, line 19):
```rust
    /// BEP 53 file selection (optional `so=` parameter).
    pub selected_files: Option<Vec<FileSelection>>,
```

3. In `Magnet::parse()`, add `so=` parsing. After the `let mut peers = Vec::new();` line (57), add:
```rust
        let mut selected_files = None;
```

4. In the `match key.as_ref()` block, before the `_ => {}` arm (line 77), add a new arm:
```rust
                "so" => {
                    match FileSelection::parse(&value) {
                        Ok(sels) => selected_files = Some(sels),
                        Err(_) => {} // Ignore malformed so= values per BEP 53
                    }
                }
```

5. In the `Ok(Magnet { ... })` block (line 92-97), add the field:
```rust
            selected_files,
```

6. In `Magnet::to_uri()`, after the tracker loop (line 131), add:
```rust
        if let Some(ref sels) = self.selected_files {
            parts.push(format!("so={}", FileSelection::to_so_value(sels)));
        }
```

7. Add `selected_files: None,` to every test that constructs a `Magnet` if needed. Actually, the tests use `Magnet::parse()` which will produce the field automatically. No changes needed to existing tests.

**Step 2: Write new tests**

Add these tests to the `mod tests` block in `magnet.rs`:

```rust
    #[test]
    fn parse_so_parameter() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&so=0,2,4-6";
        let m = Magnet::parse(uri).unwrap();
        let sels = m.selected_files.unwrap();
        assert_eq!(
            sels,
            vec![
                crate::file_selection::FileSelection::Single(0),
                crate::file_selection::FileSelection::Single(2),
                crate::file_selection::FileSelection::Range(4, 6),
            ]
        );
    }

    #[test]
    fn so_parameter_round_trip() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&so=0,2,4-6";
        let m = Magnet::parse(uri).unwrap();
        let rebuilt = m.to_uri();
        let m2 = Magnet::parse(&rebuilt).unwrap();
        assert_eq!(m.selected_files, m2.selected_files);
    }

    #[test]
    fn no_so_parameter_is_none() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";
        let m = Magnet::parse(uri).unwrap();
        assert!(m.selected_files.is_none());
    }

    #[test]
    fn invalid_so_parameter_ignored() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&so=abc";
        let m = Magnet::parse(uri).unwrap();
        assert!(m.selected_files.is_none());
    }
```

**Step 3: Run tests**
Run: `cargo test -p ferrite-core magnet`
Expected: PASS

**Step 4: Commit**
```
feat: parse BEP 53 so= parameter in magnet URIs (M36)
```

---

### Task 3: Apply `selected_files` after metadata assembly in session

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:375-440` (store selected_files on actor from magnet)
- Modify: `crates/ferrite-session/src/torrent.rs:2254-2332` (apply in try_assemble_metadata)

**Step 1: Add `selected_files` field to `TorrentActor`**

In `crates/ferrite-session/src/torrent.rs`, add a new field to the `TorrentActor` struct (after `meta_v2` field, around line 712):
```rust
    /// BEP 53: deferred file selection from magnet `so=` parameter.
    /// Applied after metadata is received to set file priorities.
    magnet_selected_files: Option<Vec<ferrite_core::FileSelection>>,
```

**Step 2: Set the field in `from_magnet()`**

In the `from_magnet()` constructor, before the `let actor = TorrentActor { ... }` block (around line 375), capture the selected files:
```rust
        let magnet_selected_files = magnet.selected_files.clone();
```

In the `TorrentActor { ... }` initializer (around line 438), add:
```rust
            magnet_selected_files,
```

**Step 3: Set the field to `None` in `from_torrent()`**

In the `from_torrent()` constructor's `TorrentActor { ... }` initializer (around line 284), add:
```rust
            magnet_selected_files: None,
```

**Step 4: Apply selected_files in `try_assemble_metadata()`**

In `try_assemble_metadata()`, after the line that sets `self.file_priorities` (line 2297):
```rust
                        self.file_priorities = vec![FilePriority::Normal; file_lengths.len()];
```

Add this block right after it, before the `self.wanted_pieces = ...` line:
```rust
                        // BEP 53: apply magnet so= file selection
                        if let Some(ref selections) = self.magnet_selected_files {
                            self.file_priorities = ferrite_core::FileSelection::to_priorities(
                                selections,
                                file_lengths.len(),
                            );
                            self.magnet_selected_files = None;
                        }
```

**Step 5: Run all tests**
Run: `cargo test -p ferrite-session`
Expected: PASS (existing tests should still pass; the new field is None for non-magnet flows)

**Step 6: Commit**
```
feat: apply BEP 53 so= file selection after metadata received (M36)
```

---

## Feature 2: Dual-Swarm Tracker/DHT Announces

### Task 4: Store `InfoHashes` on `TrackerManager` and support dual announces

**Files:**
- Modify: `crates/ferrite-session/src/tracker_manager.rs:91-191` (add info_hashes field, dual announce)

**Step 1: Add `InfoHashes` to `TrackerManager`**

In `crates/ferrite-session/src/tracker_manager.rs`:

1. Add import at top (line 11):
```rust
use ferrite_core::{Id20, InfoHashes, TorrentMetaV1};
```
Remove the existing `use ferrite_core::{Id20, TorrentMetaV1};` line.

2. Add `info_hashes` field to `TrackerManager` struct (after `info_hash` on line 97):
```rust
    info_hashes: InfoHashes,
```

3. In `from_torrent()` (line 109), after setting `info_hash: meta.info_hash` (line 167), add:
```rust
            info_hashes: InfoHashes::v1_only(meta.info_hash),
```

4. In `empty()` (line 176), after setting `info_hash` (line 179), add:
```rust
            info_hashes: InfoHashes::v1_only(info_hash),
```

5. Add a new method to set info_hashes (after `set_metadata`, around line 191):
```rust
    /// Set the full info hashes for dual-swarm support (hybrid torrents).
    pub fn set_info_hashes(&mut self, info_hashes: InfoHashes) {
        self.info_hashes = info_hashes;
    }
```

**Step 2: Add dual-announce to `build_request` and `announce`**

The `build_request` method currently uses `self.info_hash`. For dual-swarm, when `self.info_hashes.is_hybrid()`, we need to announce twice: once with the v1 hash and once with the v2 hash (truncated to 20 bytes for the protocol).

Replace the `announce` method to support dual announces. The key change is in the `announce()` method. After the existing announce loop, add a second loop if hybrid, using the v2 hash truncated to Id20:

After the existing `for tracker in &mut self.trackers { ... }` loop in `announce()` (line 246-317), wrap it and add:

Actually, the cleanest approach is to make `announce()` call an internal helper with a specific `Id20`, and loop over hashes. Modify `announce()`:

```rust
    /// Announce to all trackers that are due.
    ///
    /// For hybrid torrents, announces both v1 and v2 info hashes separately.
    /// Returns all discovered peer addresses (deduplicated).
    pub async fn announce(
        &mut self,
        event: AnnounceEvent,
        uploaded: u64,
        downloaded: u64,
        left: u64,
    ) -> AnnounceResult {
        let mut all_peers = Vec::new();
        let mut seen_peers = std::collections::HashSet::new();
        let mut all_outcomes = Vec::new();

        // Always announce with the primary (v1) info hash
        let result = self.announce_with_hash(self.info_hash, event, uploaded, downloaded, left).await;
        for peer in result.peers {
            if seen_peers.insert(peer) {
                all_peers.push(peer);
            }
        }
        all_outcomes.extend(result.outcomes);

        // Dual-swarm: also announce with v2 hash (truncated) if hybrid
        if self.info_hashes.is_hybrid() {
            if let Some(v2) = self.info_hashes.v2 {
                let v2_as_v1 = {
                    let mut truncated = [0u8; 20];
                    truncated.copy_from_slice(&v2.0[..20]);
                    Id20(truncated)
                };
                // Only announce the v2 hash if it differs from the v1 hash
                if v2_as_v1 != self.info_hash {
                    let result = self.announce_with_hash(v2_as_v1, event, uploaded, downloaded, left).await;
                    for peer in result.peers {
                        if seen_peers.insert(peer) {
                            all_peers.push(peer);
                        }
                    }
                    all_outcomes.extend(result.outcomes);
                }
            }
        }

        AnnounceResult { peers: all_peers, outcomes: all_outcomes }
    }
```

Extract the current announce loop body into a private `announce_with_hash` method:

```rust
    /// Internal: announce with a specific info hash.
    async fn announce_with_hash(
        &mut self,
        hash: Id20,
        event: AnnounceEvent,
        uploaded: u64,
        downloaded: u64,
        left: u64,
    ) -> AnnounceResult {
        let req = AnnounceRequest {
            info_hash: hash,
            peer_id: self.peer_id,
            port: self.port,
            uploaded,
            downloaded,
            left,
            event,
            num_want: Some(50),
            compact: true,
        };
        let now = Instant::now();
        let mut all_peers = Vec::new();
        let mut seen_peers = std::collections::HashSet::new();
        let mut outcomes = Vec::new();

        for tracker in &mut self.trackers {
            // For the secondary (v2) hash, always announce (don't skip based on next_announce,
            // since the timer tracks the primary hash). For the primary hash, respect the timer.
            if hash == self.info_hash && tracker.next_announce > now {
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
                Ok((peers, interval, tracker_id, seeders, leechers)) => {
                    let num_peers = peers.len();
                    debug!(
                        url = %tracker.url,
                        peer_count = num_peers,
                        interval,
                        %hash,
                        "tracker announce success"
                    );
                    // Only update tracker state for the primary hash
                    if hash == self.info_hash {
                        tracker.state = TrackerState::Active;
                        tracker.interval = Duration::from_secs(interval as u64);
                        tracker.next_announce = now + tracker.interval;
                        tracker.backoff = Duration::ZERO;
                        tracker.consecutive_failures = 0;
                        if let Some(id) = tracker_id {
                            tracker.tracker_id = Some(id);
                        }
                        if seeders.is_some() || leechers.is_some() {
                            let prev_downloaded = tracker.scrape_info.map(|s| s.downloaded).unwrap_or(0);
                            tracker.scrape_info = Some(ferrite_tracker::ScrapeInfo {
                                complete: seeders.unwrap_or(0),
                                incomplete: leechers.unwrap_or(0),
                                downloaded: prev_downloaded,
                            });
                        }
                    }

                    for peer in peers {
                        if seen_peers.insert(peer) {
                            all_peers.push(peer);
                        }
                    }
                    outcomes.push(TrackerOutcome {
                        url: tracker.url.clone(),
                        result: Ok(num_peers),
                    });
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!(url = %tracker.url, error = %msg, %hash, "tracker announce failed");
                    // Only update failure state for the primary hash
                    if hash == self.info_hash {
                        tracker.state = TrackerState::Failed {
                            _error: msg.clone(),
                        };
                        tracker.consecutive_failures += 1;
                        tracker.backoff = if tracker.backoff.is_zero() {
                            INITIAL_BACKOFF
                        } else {
                            (tracker.backoff * 2).min(MAX_BACKOFF)
                        };
                        tracker.next_announce = now + tracker.backoff;
                    }
                    outcomes.push(TrackerOutcome {
                        url: tracker.url.clone(),
                        result: Err(msg),
                    });
                }
            }
        }

        AnnounceResult { peers: all_peers, outcomes }
    }
```

Remove the existing `build_request` method (it's replaced by inline construction in `announce_with_hash`).

**Step 3: Write tests**

Add to the `mod tests` block:

```rust
    #[test]
    fn tracker_manager_stores_info_hashes() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let mut mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        assert!(!mgr.info_hashes.is_hybrid());

        let v2 = ferrite_core::Id32::from_hex(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .unwrap();
        mgr.set_info_hashes(InfoHashes::hybrid(meta.info_hash, v2));
        assert!(mgr.info_hashes.is_hybrid());
    }
```

**Step 4: Run tests**
Run: `cargo test -p ferrite-session tracker_manager`
Expected: PASS

**Step 5: Commit**
```
feat: dual-swarm tracker announces for hybrid torrents (M36)
```

---

### Task 5: Wire dual-swarm DHT announces in `TorrentActor`

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:598-713` (add info_hashes to TorrentActor)
- Modify: `crates/ferrite-session/src/torrent.rs:112-289` (from_torrent: set info_hashes, pass to tracker_manager)
- Modify: `crates/ferrite-session/src/torrent.rs:290-444` (from_magnet: set info_hashes)
- Modify: `crates/ferrite-session/src/torrent.rs:755-770` (DHT announce loop)
- Modify: `crates/ferrite-session/src/torrent.rs:870-896` (DHT get_peers re-search)

**Step 1: Add `info_hashes` field to `TorrentActor`**

In `TorrentActor` struct (after `version` field, around line 710):
```rust
    /// Full info hashes for dual-swarm support (v1 + v2 for hybrid).
    info_hashes: ferrite_core::InfoHashes,
```

**Step 2: Set `info_hashes` in `from_torrent()`**

In `from_torrent()`, compute the info_hashes from the version and metadata. After line 139 (`let num_pieces = ...`), add:
```rust
        let info_hashes = match (&version, &meta_v2) {
            (ferrite_core::TorrentVersion::Hybrid, Some(v2_meta)) => {
                if let Some(v2_hash) = v2_meta.info_hashes.v2 {
                    ferrite_core::InfoHashes::hybrid(meta.info_hash, v2_hash)
                } else {
                    ferrite_core::InfoHashes::v1_only(meta.info_hash)
                }
            }
            (ferrite_core::TorrentVersion::V2Only, Some(v2_meta)) => {
                v2_meta.info_hashes.clone()
            }
            _ => ferrite_core::InfoHashes::v1_only(meta.info_hash),
        };
```

In the `TorrentActor { ... }` initializer, add:
```rust
            info_hashes: info_hashes.clone(),
```

Also pass info_hashes to the tracker manager. After `TrackerManager::from_torrent()` (line 164), add:
```rust
        tracker_manager.set_info_hashes(info_hashes.clone());
```

**Step 3: Set `info_hashes` in `from_magnet()`**

In `from_magnet()`, compute from the magnet link. Before the `let actor = TorrentActor { ... }` block (around line 375):
```rust
        let info_hashes = magnet.info_hashes.clone();
```

In the `TorrentActor { ... }` initializer:
```rust
            info_hashes,
```

**Step 4: Dual-swarm DHT announces**

In the initial DHT announce block (lines 757-769), replace with:

```rust
        // DHT announce (v4 + v6) — dual-swarm for hybrid torrents
        if self.state == TorrentState::Downloading && self.config.enable_dht {
            // Primary hash (v1 or best_v1)
            if let Some(ref dht) = self.dht
                && let Err(e) = dht.announce(self.info_hash, self.config.listen_port).await
            {
                warn!("DHT v4 announce failed: {e}");
            }
            if let Some(ref dht6) = self.dht_v6
                && let Err(e) = dht6.announce(self.info_hash, self.config.listen_port).await
            {
                debug!("DHT v6 announce failed: {e}");
            }
            // Dual-swarm: also announce v2 hash (truncated) for hybrid torrents
            if self.info_hashes.is_hybrid() {
                let v2_hash = self.info_hashes.best_v1(); // best_v1 returns v1 when present
                // Use the v2 hash truncated to 20 bytes
                if let Some(v2) = self.info_hashes.v2 {
                    let v2_as_v1 = {
                        let mut truncated = [0u8; 20];
                        truncated.copy_from_slice(&v2.0[..20]);
                        ferrite_core::Id20(truncated)
                    };
                    if v2_as_v1 != self.info_hash {
                        if let Some(ref dht) = self.dht
                            && let Err(e) = dht.announce(v2_as_v1, self.config.listen_port).await
                        {
                            debug!("DHT v4 dual-swarm announce failed: {e}");
                        }
                        if let Some(ref dht6) = self.dht_v6
                            && let Err(e) = dht6.announce(v2_as_v1, self.config.listen_port).await
                        {
                            debug!("DHT v6 dual-swarm announce failed: {e}");
                        }
                    }
                }
            }
        }
```

**Step 5: Dual-swarm DHT get_peers**

In the `from_torrent()` DHT get_peers calls (lines 170-199), after the v4 and v6 `get_peers` calls, add dual-swarm receivers. Add two new fields to `TorrentActor`:

```rust
    // Dual-swarm DHT peer receivers (for v2 hash in hybrid torrents)
    dht_v2_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,
    dht_v6_v2_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,
```

In `from_torrent()`, after the v6 peers rx setup (line 198), start dual-swarm get_peers if hybrid:
```rust
        // Dual-swarm: also search for v2 hash peers
        let (dht_v2_peers_rx, dht_v6_v2_peers_rx) = if enable_dht && info_hashes.is_hybrid() {
            let v2_as_v1 = if let Some(v2) = info_hashes.v2 {
                let mut truncated = [0u8; 20];
                truncated.copy_from_slice(&v2.0[..20]);
                Some(Id20(truncated))
            } else {
                None
            };
            let rx4 = if let (Some(ref dht), Some(v2_id)) = (&dht, v2_as_v1) {
                dht.get_peers(v2_id).await.ok()
            } else {
                None
            };
            let rx6 = if let (Some(ref dht6), Some(v2_id)) = (&dht_v6, v2_as_v1) {
                dht6.get_peers(v2_id).await.ok()
            } else {
                None
            };
            (rx4, rx6)
        } else {
            (None, None)
        };
```

Set these in the actor initializer and do the same for `from_magnet()` (set both to `None` since magnet resolves to v1 hash initially).

In the `select!` loop, add arms for the v2 peer receivers (after the existing DHT v6 peer arm, around line 955, before the "Batched Have flush timer" arm):
```rust
                // Dual-swarm: DHT v4 v2-hash peer discovery (hybrid)
                result = async {
                    match &mut self.dht_v2_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Some(peers) => {
                            debug!(count = peers.len(), "DHT v4 v2-swarm returned peers");
                            self.handle_add_peers(peers, PeerSource::Dht);
                        }
                        None => {
                            debug!("DHT v4 v2-swarm peer search exhausted");
                            self.dht_v2_peers_rx = None;
                        }
                    }
                }
                // Dual-swarm: DHT v6 v2-hash peer discovery (hybrid)
                result = async {
                    match &mut self.dht_v6_v2_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Some(peers) => {
                            debug!(count = peers.len(), "DHT v6 v2-swarm returned peers");
                            self.handle_add_peers(peers, PeerSource::Dht);
                        }
                        None => {
                            debug!("DHT v6 v2-swarm peer search exhausted");
                            self.dht_v6_v2_peers_rx = None;
                        }
                    }
                }
```

**Step 6: Add v2 DHT re-search in connect timer**

In the connect timer arm (around line 876), after the existing v6 re-search block (line 894), add dual-swarm re-search:

```rust
                        // Dual-swarm: re-search v2 hash
                        if self.info_hashes.is_hybrid() {
                            if let Some(v2) = self.info_hashes.v2 {
                                let v2_as_v1 = {
                                    let mut truncated = [0u8; 20];
                                    truncated.copy_from_slice(&v2.0[..20]);
                                    ferrite_core::Id20(truncated)
                                };
                                if self.dht_v2_peers_rx.is_none()
                                    && let Some(ref dht) = self.dht
                                {
                                    match dht.get_peers(v2_as_v1).await {
                                        Ok(rx) => self.dht_v2_peers_rx = Some(rx),
                                        Err(e) => debug!("DHT v4 v2-swarm re-search failed: {e}"),
                                    }
                                }
                                if self.dht_v6_v2_peers_rx.is_none()
                                    && let Some(ref dht6) = self.dht_v6
                                {
                                    match dht6.get_peers(v2_as_v1).await {
                                        Ok(rx) => self.dht_v6_v2_peers_rx = Some(rx),
                                        Err(e) => debug!("DHT v6 v2-swarm re-search failed: {e}"),
                                    }
                                }
                            }
                        }
```

**Step 7: Run tests**
Run: `cargo test -p ferrite-session`
Expected: PASS

**Step 8: Commit**
```
feat: dual-swarm DHT announces and get_peers for hybrid torrents (M36)
```

---

## Feature 3: Pure v2-Only Session Support

### Task 6: Remove `Error::Config` guard and synthesize v1 metadata for v2-only torrents

**Files:**
- Modify: `crates/ferrite-session/src/session.rs:955-1048` (handle_add_torrent)
- Modify: `crates/ferrite-core/src/detect.rs:66-82` (add TorrentMeta accessors)

**Step 1: Add `TorrentMeta` helper for v2-only info hash**

In `crates/ferrite-core/src/detect.rs`, add a method to `TorrentMeta`:

```rust
    /// Get the best v1-compatible info hash for session identification.
    ///
    /// For v1 and hybrid: returns the v1 SHA-1 info hash.
    /// For v2-only: returns the v2 SHA-256 hash truncated to 20 bytes.
    pub fn best_v1_info_hash(&self) -> crate::hash::Id20 {
        self.info_hashes().best_v1()
    }
```

**Step 2: Synthesize v1 metadata from v2 in session**

In `crates/ferrite-session/src/session.rs`, modify `handle_add_torrent()`. Replace the v2-only guard (lines 960-967):

```rust
        // Extract the v1 component — required for session logic.
        // Pure v2 torrents are not yet supported through the session path.
        let version = torrent_meta.version();
        let meta_v2 = torrent_meta.as_v2().cloned();
        let meta = torrent_meta
            .as_v1()
            .cloned()
            .ok_or_else(|| crate::Error::Config("pure v2 torrents not yet supported in session".into()))?;

        let info_hash = meta.info_hash;
```

With:

```rust
        let version = torrent_meta.version();
        let meta_v2 = torrent_meta.as_v2().cloned();

        // For v2-only torrents, synthesize a minimal v1 metadata wrapper.
        // The session uses info_hash (Id20) as the primary key, so we use
        // the SHA-256 truncated to 20 bytes (as per BEP 52 tracker/DHT compat).
        let meta = match torrent_meta.as_v1() {
            Some(v1) => v1.clone(),
            None => {
                // v2-only: synthesize v1 metadata from v2
                let v2 = torrent_meta.as_v2().unwrap();
                synthesize_v1_from_v2(v2)?
            }
        };

        let info_hash = meta.info_hash;
```

Add the synthesis function as a free function in the same file (before `handle_add_torrent` or at the bottom of the impl block):

```rust
/// Synthesize a minimal `TorrentMetaV1` from a `TorrentMetaV2` for session compatibility.
///
/// The session engine uses v1 structures internally (info hash as Id20, InfoDict for
/// piece hashing, etc.). For v2-only torrents, we create a "virtual" v1 representation
/// with the truncated SHA-256 hash as the info_hash.
fn synthesize_v1_from_v2(v2: &ferrite_core::TorrentMetaV2) -> crate::Result<ferrite_core::TorrentMetaV1> {
    use ferrite_core::{InfoDict, FileEntry, Id20};

    let info_hash = v2.info_hashes.best_v1();

    // Build file entries from v2 file tree
    let v2_files = v2.info.files();
    let file_entries: Vec<FileEntry> = v2_files
        .iter()
        .map(|f| FileEntry {
            length: f.attr.length,
            path: f.path.iter().map(|s| s.to_string()).collect(),
            attr: None,
            mtime: None,
            symlink_path: None,
        })
        .collect();

    // v2-only torrents have no v1 piece hashes — use empty pieces field.
    // Verification is done via v2 Merkle trees, not v1 SHA-1 hashes.
    let num_pieces = v2.info.num_pieces() as usize;
    let pieces = vec![0u8; num_pieces * 20]; // placeholder, not used for verification

    let info = InfoDict {
        name: v2.info.name.clone(),
        piece_length: v2.info.piece_length,
        pieces: pieces.into(),
        length: if file_entries.len() == 1 {
            Some(file_entries[0].length)
        } else {
            None
        },
        files: if file_entries.len() > 1 {
            Some(file_entries)
        } else {
            None
        },
        private: None,
        source: None,
    };

    Ok(ferrite_core::TorrentMetaV1 {
        info_hash,
        announce: None,
        announce_list: None,
        comment: None,
        created_by: None,
        creation_date: None,
        info,
        info_bytes: None,
        url_list: Vec::new(),
        httpseeds: Vec::new(),
    })
}
```

Note: The `InfoDict`, `FileEntry`, and `TorrentMetaV1` fields need to match what's in `crates/ferrite-core/src/metainfo.rs`. Let me verify these are public.

**Step 3: Run tests**
Run: `cargo test -p ferrite-session`
Expected: PASS

**Step 4: Commit**
```
feat: enable pure v2-only torrent session support (M36)
```

---

### Task 7: v2 handshake with truncated SHA-256 for info hash field

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:3220-3260` (spawn_peer_from_stream_with_mode, pass info_hash correctly)

**Step 1: Verify handshake uses correct hash**

The v2 handshake uses the truncated SHA-256 as the 20-byte info hash in the peer wire protocol. The `TorrentActor.info_hash` field is already set to `meta.info_hash` in `from_torrent()`, which for v2-only torrents is the truncated hash (via `best_v1()`). The handshake code in `run_peer()` uses this `info_hash` directly, so it already works correctly.

However, for v2-only torrents created via the synthesized path, we need to verify that `self.info_hash` is set to the truncated SHA-256. Looking at the code flow:
- `session.rs::handle_add_torrent()` sets `info_hash = meta.info_hash`
- For v2-only, `meta.info_hash` comes from `synthesize_v1_from_v2()` which sets `info_hash = v2.info_hashes.best_v1()` = truncated SHA-256

This is correct. The peer handshake will use the truncated SHA-256, which is the v2 protocol behavior.

**Step 2: Write integration test for v2-only torrent addition**

Add to `session.rs` test module:

```rust
    #[tokio::test]
    async fn add_v2_only_torrent() {
        use std::collections::BTreeMap;
        use ferrite_bencode::BencodeValue;

        let session = SessionHandle::start(test_settings()).await.unwrap();

        // Build a minimal v2-only torrent
        let mut info_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        let mut ft_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        let mut attr_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        attr_map.insert(b"length".to_vec(), BencodeValue::Integer(16384));
        let mut file_node: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        file_node.insert(b"".to_vec(), BencodeValue::Dict(attr_map));
        ft_map.insert(b"test.dat".to_vec(), BencodeValue::Dict(file_node));

        info_map.insert(b"file tree".to_vec(), BencodeValue::Dict(ft_map));
        info_map.insert(b"meta version".to_vec(), BencodeValue::Integer(2));
        info_map.insert(b"name".to_vec(), BencodeValue::Bytes(b"v2test".to_vec()));
        info_map.insert(b"piece length".to_vec(), BencodeValue::Integer(16384));

        let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

        let bytes = ferrite_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap();
        let meta = ferrite_core::torrent_from_bytes_any(&bytes).unwrap();
        assert!(meta.is_v2());

        // This should NOT return an error now (v2-only is supported)
        let info_hash = session.add_torrent(meta, None).await.unwrap();
        let list = session.list_torrents().await.unwrap();
        assert!(list.contains(&info_hash));

        session.shutdown().await.unwrap();
    }
```

**Step 3: Run test**
Run: `cargo test -p ferrite-session add_v2_only_torrent`
Expected: PASS

**Step 4: Commit**
```
feat: v2-only torrent session integration test (M36)
```

---

### Task 8: Update facade re-exports and bump version

**Files:**
- Modify: `crates/ferrite/src/core.rs` (add FileSelection re-export)
- Modify: `crates/ferrite/src/prelude.rs` (add FileSelection to prelude)
- Modify: `Cargo.toml:6` (bump version to 0.42.0)

**Step 1: Add re-exports**

In `crates/ferrite/src/core.rs`, add `FileSelection` to the re-export list (wherever `FilePriority` is re-exported).

In `crates/ferrite/src/prelude.rs`, add `FileSelection` to the prelude re-export list.

**Step 2: Bump workspace version**

In root `Cargo.toml`, change `version = "0.41.0"` to `version = "0.42.0"`.

**Step 3: Run full test suite and clippy**
Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: PASS

**Step 4: Commit**
```
feat: re-export FileSelection, bump to v0.42.0 (M36)
```

---

## Summary

| Task | Feature | Crate | Tests Added | Files |
|------|---------|-------|-------------|-------|
| 1 | BEP 53 `FileSelection` enum | ferrite-core | 11 | file_selection.rs (new), lib.rs |
| 2 | Magnet `so=` parsing | ferrite-core | 4 | magnet.rs |
| 3 | Apply `so=` after metadata | ferrite-session | 0 (behavioral) | torrent.rs |
| 4 | Dual-swarm tracker announces | ferrite-session | 1 | tracker_manager.rs |
| 5 | Dual-swarm DHT announces | ferrite-session | 0 (behavioral) | torrent.rs |
| 6 | Remove v2-only guard | ferrite-session | 0 (behavioral) | session.rs |
| 7 | v2-only integration test | ferrite-session | 1 | session.rs |
| 8 | Facade + version bump | ferrite | 0 | core.rs, prelude.rs, Cargo.toml |
| **Total** | | | **~17 new tests** | |

**Estimated total: 895 + 17 = ~912 tests**

### Commit sequence:
1. `feat: add FileSelection enum for BEP 53 so= parsing (M36)`
2. `feat: parse BEP 53 so= parameter in magnet URIs (M36)`
3. `feat: apply BEP 53 so= file selection after metadata received (M36)`
4. `feat: dual-swarm tracker announces for hybrid torrents (M36)`
5. `feat: dual-swarm DHT announces and get_peers for hybrid torrents (M36)`
6. `feat: enable pure v2-only torrent session support (M36)`
7. `feat: v2-only torrent session integration test (M36)`
8. `feat: re-export FileSelection, bump to v0.42.0 (M36)`
