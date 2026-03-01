# M46: Peer Turnover — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Periodically replace underperforming peers with fresh candidates from the available peer pool, improving download throughput by cycling out slow/stalled connections. Matches libtorrent-rasterbar's `peer_turnover`, `peer_turnover_cutoff`, and `peer_turnover_interval` settings.

**Architecture:** A new timer in the `TorrentActor::run()` select loop fires every `peer_turnover_interval` seconds. When the current download rate falls below `peer_turnover_cutoff * peak_download_rate`, the bottom `peer_turnover` fraction of peers (ranked by download rate) are disconnected — subject to exemptions (seeds when leeching, peers with outstanding requests, parole peers, peers connected < 30s). After disconnection, `try_connect_peers()` fills the freed slots with fresh candidates from `available_peers`.

**Tech Stack:** Rust edition 2024, tokio, ferrite-session crate only

---

## Task 1: Add `connected_at` Timestamp to PeerState

**Files:**
- Modify: `crates/ferrite-session/src/peer_state.rs`

**Step 1: Write test for connected_at field**

Add a test to the existing `tests` module in `crates/ferrite-session/src/peer_state.rs`:

```rust
#[test]
fn peer_state_has_connected_at() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let peer = PeerState::new(
        "127.0.0.1:6881".parse().unwrap(),
        100,
        tx,
        PeerSource::Tracker,
    );
    // connected_at should be set to now (within 1 second)
    assert!(peer.connected_at.elapsed().as_secs() < 1);
}
```

**Step 2: Add `connected_at` field to PeerState**

In `crates/ferrite-session/src/peer_state.rs`, add the field to the struct (after `last_data_received`):

```rust
    /// When this peer connection was established.
    pub connected_at: std::time::Instant,
```

And initialize it in `PeerState::new()`:

```rust
            connected_at: std::time::Instant::now(),
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session peer_state -- --nocapture`
Expected: all existing tests + new test PASS

---

## Task 2: Add Peer Turnover Settings

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs`

**Step 1: Write tests for new settings fields and validation**

Add these tests to the existing `tests` module in `crates/ferrite-session/src/settings.rs`:

```rust
#[test]
fn peer_turnover_defaults() {
    let s = Settings::default();
    assert!((s.peer_turnover - 0.04).abs() < f64::EPSILON);
    assert!((s.peer_turnover_cutoff - 0.9).abs() < f64::EPSILON);
    assert_eq!(s.peer_turnover_interval, 300);
}

#[test]
fn peer_turnover_json_round_trip() {
    let mut s = Settings::default();
    s.peer_turnover = 0.1;
    s.peer_turnover_cutoff = 0.8;
    s.peer_turnover_interval = 120;
    let json = serde_json::to_string(&s).unwrap();
    let decoded: Settings = serde_json::from_str(&json).unwrap();
    assert!((decoded.peer_turnover - 0.1).abs() < f64::EPSILON);
    assert!((decoded.peer_turnover_cutoff - 0.8).abs() < f64::EPSILON);
    assert_eq!(decoded.peer_turnover_interval, 120);
}

#[test]
fn peer_turnover_validation() {
    // Turnover fraction out of range
    let mut s = Settings::default();
    s.peer_turnover = 1.5;
    let err = s.validate().unwrap_err();
    assert!(err.to_string().contains("peer_turnover"));

    let mut s = Settings::default();
    s.peer_turnover = -0.1;
    let err = s.validate().unwrap_err();
    assert!(err.to_string().contains("peer_turnover"));

    // Cutoff out of range
    let mut s = Settings::default();
    s.peer_turnover_cutoff = 1.5;
    let err = s.validate().unwrap_err();
    assert!(err.to_string().contains("peer_turnover_cutoff"));

    // Zero interval is valid (disables turnover)
    let mut s = Settings::default();
    s.peer_turnover_interval = 0;
    s.validate().unwrap();
}
```

**Step 2: Add serde default helpers**

Add these default helper functions in the serde defaults section of `crates/ferrite-session/src/settings.rs` (after `default_utp_max_conns`):

```rust
fn default_peer_turnover() -> f64 {
    0.04
}
fn default_peer_turnover_cutoff() -> f64 {
    0.9
}
fn default_peer_turnover_interval() -> u64 {
    300
}
```

**Step 3: Add fields to Settings struct**

Add a new `// ── Peer turnover ──` section to the `Settings` struct, after the `// ── uTP tuning ──` section:

```rust
    // ── Peer turnover ──
    /// Fraction of peers to disconnect per turnover interval (0.0–1.0, default: 0.04).
    #[serde(default = "default_peer_turnover")]
    pub peer_turnover: f64,
    /// Only trigger turnover if download rate < this fraction of peak rate (0.0–1.0, default: 0.9).
    #[serde(default = "default_peer_turnover_cutoff")]
    pub peer_turnover_cutoff: f64,
    /// Seconds between turnover checks (default: 300, 0 = disabled).
    #[serde(default = "default_peer_turnover_interval")]
    pub peer_turnover_interval: u64,
```

**Step 4: Update Default impl**

Add to the `Default::default()` impl in the `Settings` struct, after `utp_max_connections`:

```rust
            // Peer turnover
            peer_turnover: 0.04,
            peer_turnover_cutoff: 0.9,
            peer_turnover_interval: 300,
```

**Step 5: Add validation checks**

Add to the `validate()` method, before the final `Ok(())`:

```rust
        if !(0.0..=1.0).contains(&self.peer_turnover) {
            return Err(crate::Error::InvalidSettings(
                "peer_turnover must be between 0.0 and 1.0".into(),
            ));
        }

        if !(0.0..=1.0).contains(&self.peer_turnover_cutoff) {
            return Err(crate::Error::InvalidSettings(
                "peer_turnover_cutoff must be between 0.0 and 1.0".into(),
            ));
        }
```

**Step 6: Update PartialEq impl**

Add to the manual `PartialEq` impl, after the `utp_max_connections` comparison:

```rust
            && self.peer_turnover.to_bits() == other.peer_turnover.to_bits()
            && self.peer_turnover_cutoff.to_bits() == other.peer_turnover_cutoff.to_bits()
            && self.peer_turnover_interval == other.peer_turnover_interval
```

**Step 7: Run tests**

Run: `cargo test -p ferrite-session settings -- --nocapture`
Expected: all existing tests + 3 new tests PASS

---

## Task 3: Propagate Settings to TorrentConfig

**Files:**
- Modify: `crates/ferrite-session/src/types.rs`

**Step 1: Write test for TorrentConfig turnover defaults**

Add to the `tests` module in `crates/ferrite-session/src/types.rs`:

```rust
#[test]
fn torrent_config_peer_turnover_defaults() {
    let cfg = TorrentConfig::default();
    assert!((cfg.peer_turnover - 0.04).abs() < f64::EPSILON);
    assert!((cfg.peer_turnover_cutoff - 0.9).abs() < f64::EPSILON);
    assert_eq!(cfg.peer_turnover_interval, 300);
}

#[test]
fn torrent_config_from_settings_peer_turnover() {
    let mut s = crate::settings::Settings::default();
    s.peer_turnover = 0.1;
    s.peer_turnover_cutoff = 0.75;
    s.peer_turnover_interval = 120;
    let cfg = TorrentConfig::from(&s);
    assert!((cfg.peer_turnover - 0.1).abs() < f64::EPSILON);
    assert!((cfg.peer_turnover_cutoff - 0.75).abs() < f64::EPSILON);
    assert_eq!(cfg.peer_turnover_interval, 120);
}
```

**Step 2: Add fields to TorrentConfig struct**

Add to `TorrentConfig` struct (after `share_mode`):

```rust
    /// Fraction of peers to disconnect per turnover interval (0.0–1.0).
    pub peer_turnover: f64,
    /// Only trigger turnover if download rate < this fraction of peak rate (0.0–1.0).
    pub peer_turnover_cutoff: f64,
    /// Seconds between turnover checks (0 = disabled).
    pub peer_turnover_interval: u64,
```

**Step 3: Update TorrentConfig Default impl**

Add to `Default::default()` (after `share_mode: false`):

```rust
            peer_turnover: 0.04,
            peer_turnover_cutoff: 0.9,
            peer_turnover_interval: 300,
```

**Step 4: Update From<&Settings> impl**

Add to the `From<&Settings> for TorrentConfig` impl (after `share_mode`):

```rust
            peer_turnover: s.peer_turnover,
            peer_turnover_cutoff: s.peer_turnover_cutoff,
            peer_turnover_interval: s.peer_turnover_interval,
```

**Step 5: Run tests**

Run: `cargo test -p ferrite-session types -- --nocapture`
Expected: all existing tests + 2 new tests PASS

---

## Task 4: Add PeerTurnover Alert

**Files:**
- Modify: `crates/ferrite-session/src/alert.rs`

**Step 1: Write test for PeerTurnover alert**

Add to the `tests` module in `crates/ferrite-session/src/alert.rs`:

```rust
#[test]
fn peer_turnover_alert_has_peer_category() {
    let alert = Alert::new(AlertKind::PeerTurnover {
        info_hash: Id20::from([0u8; 20]),
        disconnected: 2,
        replaced: 1,
    });
    assert!(alert.category().contains(AlertCategory::PEER));
}
```

**Step 2: Add PeerTurnover variant to AlertKind**

Add to the `AlertKind` enum, after the `PeerBlocked` variant (in the `// ── IP filtering (PEER) ──` section):

```rust
    // ── Peer turnover (PEER) ──
    /// Periodic turnover disconnected underperforming peers and connected replacements.
    PeerTurnover { info_hash: Id20, disconnected: usize, replaced: usize },
```

**Step 3: Add category mapping**

Add to the `category()` match in `AlertKind`, after `PeerBlocked { .. } => AlertCategory::PEER`:

```rust
            PeerTurnover { .. } => AlertCategory::PEER,
```

**Step 4: Run tests**

Run: `cargo test -p ferrite-session alert -- --nocapture`
Expected: all existing tests + 1 new test PASS

---

## Task 5: Implement Peer Turnover Logic in TorrentActor

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

This is the core implementation task. It adds:
1. Peak download rate tracking on `TorrentActor`
2. The `run_peer_turnover()` method
3. A new timer in the `run()` select loop

**Step 1: Write unit tests for turnover logic**

Add these tests to the `#[cfg(test)] mod tests` section in `crates/ferrite-session/src/torrent.rs` (at the end of the test module):

```rust
#[test]
fn peer_turnover_identifies_worst_peers() {
    // Test the sorting and selection logic: given peers with various download rates,
    // verify the bottom fraction is correctly identified, and exemptions are honored.
    use std::net::SocketAddr;
    use std::time::Instant;

    let now = Instant::now();
    let thirty_one_secs_ago = now - Duration::from_secs(31);

    // Helper: create a mock peer info for turnover ranking
    struct TurnoverCandidate {
        addr: SocketAddr,
        download_rate: u64,
        is_seed: bool,
        has_outstanding_requests: bool,
        in_parole: bool,
        connected_at: Instant,
    }

    let candidates = vec![
        TurnoverCandidate {
            addr: "10.0.0.1:6881".parse().unwrap(),
            download_rate: 100,
            is_seed: false,
            has_outstanding_requests: false,
            in_parole: false,
            connected_at: thirty_one_secs_ago, // old enough
        },
        TurnoverCandidate {
            addr: "10.0.0.2:6881".parse().unwrap(),
            download_rate: 5000,
            is_seed: false,
            has_outstanding_requests: false,
            in_parole: false,
            connected_at: thirty_one_secs_ago,
        },
        TurnoverCandidate {
            addr: "10.0.0.3:6881".parse().unwrap(),
            download_rate: 50,
            is_seed: false,
            has_outstanding_requests: false,
            in_parole: false,
            connected_at: thirty_one_secs_ago,
        },
        // Exempt: seed
        TurnoverCandidate {
            addr: "10.0.0.4:6881".parse().unwrap(),
            download_rate: 0,
            is_seed: true,
            has_outstanding_requests: false,
            in_parole: false,
            connected_at: thirty_one_secs_ago,
        },
        // Exempt: outstanding requests
        TurnoverCandidate {
            addr: "10.0.0.5:6881".parse().unwrap(),
            download_rate: 10,
            is_seed: false,
            has_outstanding_requests: true,
            in_parole: false,
            connected_at: thirty_one_secs_ago,
        },
        // Exempt: recently connected
        TurnoverCandidate {
            addr: "10.0.0.6:6881".parse().unwrap(),
            download_rate: 0,
            is_seed: false,
            has_outstanding_requests: false,
            in_parole: false,
            connected_at: now, // just connected
        },
        // Exempt: parole
        TurnoverCandidate {
            addr: "10.0.0.7:6881".parse().unwrap(),
            download_rate: 5,
            is_seed: false,
            has_outstanding_requests: false,
            in_parole: true,
            connected_at: thirty_one_secs_ago,
        },
    ];

    // Filter to eligible candidates (not exempt)
    let mut eligible: Vec<_> = candidates
        .iter()
        .filter(|c| {
            !c.is_seed
                && !c.has_outstanding_requests
                && !c.in_parole
                && c.connected_at.elapsed() >= Duration::from_secs(30)
        })
        .collect();

    // Should have 3 eligible: 10.0.0.1 (100), 10.0.0.2 (5000), 10.0.0.3 (50)
    assert_eq!(eligible.len(), 3);

    // Sort by download rate ascending (worst first)
    eligible.sort_by_key(|c| c.download_rate);

    // With turnover fraction 0.04, floor(3 * 0.04) = 0 → clamp to 1
    let turnover_count = (eligible.len() as f64 * 0.04).floor() as usize;
    let turnover_count = turnover_count.max(1); // always at least 1 if eligible exist
    assert_eq!(turnover_count, 1);

    // The worst peer should be 10.0.0.3 (rate 50)
    assert_eq!(eligible[0].addr, "10.0.0.3:6881".parse::<SocketAddr>().unwrap());
}

#[test]
fn peer_turnover_cutoff_suppresses_at_peak() {
    // When download rate >= cutoff * peak, turnover should not fire
    let peak_rate: u64 = 10_000;
    let current_rate: u64 = 9_500;
    let cutoff = 0.9;

    // 9500 >= 0.9 * 10000 = 9000 → suppress
    let should_suppress = current_rate as f64 >= cutoff * peak_rate as f64;
    assert!(should_suppress);

    // Below cutoff
    let current_rate: u64 = 8_000;
    let should_suppress = current_rate as f64 >= cutoff * peak_rate as f64;
    assert!(!should_suppress);
}

#[test]
fn peer_turnover_disabled_when_interval_zero() {
    let interval = 0u64;
    assert_eq!(interval, 0, "interval of 0 disables peer turnover");
}

#[test]
fn peer_turnover_no_action_when_seeding() {
    // When state is Seeding, turnover should not disconnect any peers.
    // Turnover only applies during active downloading.
    let state = TorrentState::Seeding;
    assert_ne!(state, TorrentState::Downloading);
}
```

**Step 2: Add peak download rate tracking to TorrentActor**

In `crates/ferrite-session/src/torrent.rs`, add a new field to the `TorrentActor` struct, after `upload_bytes_interval`:

```rust
    /// Peak aggregate download rate observed (bytes/sec), for peer turnover cutoff.
    peak_download_rate: u64,
```

Initialize it to `0` in both `TorrentHandle::from_torrent()` and `TorrentHandle::from_magnet()` constructor bodies, after the `upload_bytes_interval: 0` line:

```rust
            peak_download_rate: 0,
```

**Step 3: Update peak download rate in update_peer_rates()**

In the `update_peer_rates()` method (around line 2826), after the existing loop that computes per-peer rates, add peak rate tracking:

```rust
        // Update aggregate download rate and peak for turnover cutoff
        let aggregate_download: u64 = self.peers.values().map(|p| p.download_rate).sum();
        if aggregate_download > self.peak_download_rate {
            self.peak_download_rate = aggregate_download;
        }
```

**Step 4: Implement `run_peer_turnover()` method**

Add the following method to `TorrentActor`, in the `// ----- Peer connectivity -----` section (after `try_connect_peers()`):

```rust
    /// Peer turnover: disconnect the worst-performing fraction of peers and
    /// connect fresh replacements. Only fires when download rate is below
    /// `peer_turnover_cutoff * peak_download_rate`.
    async fn run_peer_turnover(&mut self) {
        // Only run during active downloading
        if self.state != TorrentState::Downloading {
            return;
        }

        // Check cutoff: if current rate >= cutoff * peak, don't churn
        let aggregate_download: u64 = self.peers.values().map(|p| p.download_rate).sum();
        if self.peak_download_rate > 0 {
            let threshold = self.config.peer_turnover_cutoff * self.peak_download_rate as f64;
            if aggregate_download as f64 >= threshold {
                return;
            }
        }

        // Collect eligible peers (not exempt from turnover)
        let parole_ips: HashSet<std::net::IpAddr> = self
            .parole_pieces
            .values()
            .filter_map(|p| p.parole_peer)
            .collect();

        let mut eligible: Vec<(SocketAddr, u64)> = self
            .peers
            .values()
            .filter(|p| {
                // Exempt: seeds (when we are leeching — bitfield is full)
                if p.bitfield.count_ones() == self.num_pieces as usize {
                    return false;
                }
                // Exempt: peers with outstanding requests
                if !p.pending_requests.is_empty() {
                    return false;
                }
                // Exempt: peers in parole
                if parole_ips.contains(&p.addr.ip()) {
                    return false;
                }
                // Exempt: recently connected (< 30 seconds)
                if p.connected_at.elapsed() < Duration::from_secs(30) {
                    return false;
                }
                true
            })
            .map(|p| (p.addr, p.download_rate))
            .collect();

        if eligible.is_empty() {
            return;
        }

        // Sort by download rate ascending (worst performers first)
        eligible.sort_by_key(|&(_, rate)| rate);

        // Calculate how many to disconnect
        let turnover_count = (eligible.len() as f64 * self.config.peer_turnover)
            .floor() as usize;
        // Always at least 1 if we have eligible peers and turnover > 0
        let turnover_count = if self.config.peer_turnover > 0.0 {
            turnover_count.max(1)
        } else {
            0
        };

        if turnover_count == 0 {
            return;
        }

        // Disconnect the worst peers
        let to_disconnect: Vec<SocketAddr> = eligible
            .iter()
            .take(turnover_count)
            .map(|&(addr, _)| addr)
            .collect();

        for &addr in &to_disconnect {
            if let Some(peer) = self.peers.remove(&addr) {
                self.piece_selector.remove_peer_bitfield(&peer.bitfield);
                // Clean up in-flight pieces
                let peer_pieces: HashSet<u32> = peer
                    .pending_requests
                    .iter()
                    .map(|&(idx, _, _)| idx)
                    .collect();
                for piece_idx in peer_pieces {
                    let other_has = self.peers.values().any(|p| {
                        p.pending_requests.iter().any(|&(i, _, _)| i == piece_idx)
                    });
                    if !other_has {
                        self.in_flight_pieces.remove(&piece_idx);
                    }
                }
                for ifp in self.in_flight_pieces.values_mut() {
                    ifp.assigned_blocks.retain(|_, a| *a != addr);
                }
                if self.end_game.is_active() {
                    self.end_game.peer_disconnected(addr);
                }
                let _ = peer.cmd_tx.send(PeerCommand::Shutdown).await;
                post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerDisconnected {
                    info_hash: self.info_hash,
                    addr,
                    reason: Some("peer turnover".into()),
                });
            }
        }

        // Connect replacements
        let peers_before = self.peers.len();
        self.try_connect_peers();
        let replaced = self.peers.len() - peers_before;

        // Fire turnover alert
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerTurnover {
            info_hash: self.info_hash,
            disconnected: to_disconnect.len(),
            replaced,
        });

        info!(
            info_hash = %self.info_hash,
            disconnected = to_disconnect.len(),
            replaced,
            aggregate_rate = aggregate_download,
            peak_rate = self.peak_download_rate,
            "peer turnover executed"
        );
    }
```

**Step 5: Add turnover timer to the run() select loop**

In `crates/ferrite-session/src/torrent.rs`, in the `TorrentActor::run()` method:

After the existing interval declarations (around line 743, after `refill_interval`), add:

```rust
        let mut turnover_interval = if self.config.peer_turnover_interval > 0 {
            Some(tokio::time::interval(Duration::from_secs(self.config.peer_turnover_interval)))
        } else {
            None
        };
        // Don't fire immediately for the first tick
        if let Some(ref mut interval) = turnover_interval {
            interval.tick().await;
        }
```

Add a new select arm in the `loop { tokio::select! { ... } }` block. Place it after the `refill_interval` arm (after line ~970) and before the closing `}` of the `select!`:

```rust
                // Peer turnover timer
                _ = async {
                    match &mut turnover_interval {
                        Some(interval) => interval.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.run_peer_turnover().await;
                }
```

**Step 6: Run tests**

Run: `cargo test -p ferrite-session -- --nocapture`
Expected: all tests PASS (existing + 4 new turnover tests)

---

## Task 6: Compile and Lint Check

**Files:** None (verification only)

**Step 1: Full workspace build**

Run: `cargo test --workspace`
Expected: all 895+ tests PASS

**Step 2: Clippy lint check**

Run: `cargo clippy --workspace -- -D warnings`
Expected: zero warnings

---

## Task 7: Version Bump and Documentation

**Files:**
- Modify: `Cargo.toml` (workspace root — version bump)
- Modify: `CLAUDE.md` (update Settings field count)

**Step 1: Bump workspace version**

In the root `Cargo.toml`, bump the workspace version from the current version to the next patch version (e.g., `0.41.0` -> `0.42.0`).

**Step 2: Update CLAUDE.md**

Update the Settings description in `CLAUDE.md`:
- Change `56-field` to `59-field` (3 new fields: `peer_turnover`, `peer_turnover_cutoff`, `peer_turnover_interval`)
- Add to the Session Configuration section: mention peer turnover settings

**Step 3: Run tests one final time**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: all PASS, zero warnings

---

## Summary

| Task | What | Files Modified | Tests Added |
|------|------|---------------|-------------|
| 1 | `connected_at` on PeerState | `peer_state.rs` | 1 |
| 2 | Settings fields + validation | `settings.rs` | 3 |
| 3 | TorrentConfig propagation | `types.rs` | 2 |
| 4 | PeerTurnover alert variant | `alert.rs` | 1 |
| 5 | Core turnover logic + timer | `torrent.rs` | 4 |
| 6 | Compile + lint verification | — | — |
| 7 | Version bump + docs | `Cargo.toml`, `CLAUDE.md` | — |
| **Total** | | **6 files** | **11 tests** |

### Settings Added

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `peer_turnover` | `f64` | `0.04` | Fraction of peers to disconnect per interval |
| `peer_turnover_cutoff` | `f64` | `0.9` | Only trigger if download rate < fraction of peak |
| `peer_turnover_interval` | `u64` | `300` | Seconds between turnover checks (0 = disabled) |

### Exemptions (peers never disconnected by turnover)

1. **Seeds** — peers whose bitfield is complete (they are valuable when we are leeching)
2. **Outstanding requests** — peers with pending piece requests (data in flight)
3. **Parole peers** — peers being tested for smart ban verification
4. **Recently connected** — peers connected less than 30 seconds ago (haven't had time to ramp up)

### Algorithm

1. Timer fires every `peer_turnover_interval` seconds
2. Skip if not in `Downloading` state
3. Compute aggregate download rate across all peers
4. Skip if `aggregate_rate >= peer_turnover_cutoff * peak_rate` (performing well)
5. Filter peers to eligible candidates (apply 4 exemptions)
6. Sort eligible by download rate ascending (worst first)
7. Disconnect bottom `floor(eligible * peer_turnover)` peers (minimum 1 if turnover > 0)
8. Call `try_connect_peers()` to fill freed slots
9. Fire `PeerTurnover` alert with disconnected/replaced counts
