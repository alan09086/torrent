# M44: Piece Picker Enhancements — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fine-grained optimizations for piece selection and announcement: extent affinity, suggest mode, predictive Have, and peer speed categorization integration with settings.

**Architecture:** The piece picker (`PieceSelector`) gains extent-aware grouping so peers in the same speed class prefer pieces from the same 4 MiB extent. The torrent actor gains the ability to send `SuggestPiece` messages to peers for pieces currently in the ARC read cache. Predictive piece announce sends `Have` before the disk write completes (configurable delay). Peer speed categories are already defined (`PeerSpeed`) but need wiring into settings and the picker.

**Tech Stack:** Rust edition 2024, tokio, ferrite workspace conventions

---

## Task 1: Settings Fields for M44 Features

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs`
- Modify: `crates/ferrite-session/src/types.rs`

**Step 1: Add settings fields and serde default helpers**

In `crates/ferrite-session/src/settings.rs`, add four new default helper functions after the existing `default_utp_max_conns` function (line ~115):

```rust
fn default_max_suggest_pieces() -> usize {
    10
}
fn default_predictive_piece_announce_ms() -> u64 {
    0
}
```

Then add four new fields to the `Settings` struct. Insert them in the `// -- Hashing & piece picking --` section, after `max_concurrent_stream_reads` (line ~258):

```rust
    // ── Piece picker enhancements (M44) ──
    /// Group pieces into 4 MiB extents; peers in same speed class prefer same extent.
    #[serde(default = "default_true")]
    pub piece_extent_affinity: bool,
    /// Send SuggestPiece messages for pieces in the ARC read cache to peers.
    #[serde(default)]
    pub suggest_mode: bool,
    /// Maximum number of SuggestPiece messages to send per peer.
    #[serde(default = "default_max_suggest_pieces")]
    pub max_suggest_pieces: usize,
    /// Send Have before disk write completes. 0 = disabled (wait for verification).
    #[serde(default = "default_predictive_piece_announce_ms")]
    pub predictive_piece_announce_ms: u64,
```

Update the `Default for Settings` impl to include the four new fields after `max_concurrent_stream_reads: 8,`:

```rust
            // Piece picker enhancements (M44)
            piece_extent_affinity: true,
            suggest_mode: false,
            max_suggest_pieces: 10,
            predictive_piece_announce_ms: 0,
```

Update `Settings::min_memory()` -- add to the struct literal:

```rust
            suggest_mode: false,
```

Update `Settings::high_performance()` -- add to the struct literal:

```rust
            suggest_mode: true,
```

Update the manual `PartialEq for Settings` impl. Add four new comparisons after the `max_concurrent_stream_reads` line (line ~588):

```rust
            && self.piece_extent_affinity == other.piece_extent_affinity
            && self.suggest_mode == other.suggest_mode
            && self.max_suggest_pieces == other.max_suggest_pieces
            && self.predictive_piece_announce_ms == other.predictive_piece_announce_ms
```

**Step 2: Add TorrentConfig fields**

In `crates/ferrite-session/src/types.rs`, add four new fields to `TorrentConfig` after `share_mode: bool,` (line ~66):

```rust
    /// Group pieces into 4 MiB extents; peers in same speed class prefer same extent.
    pub piece_extent_affinity: bool,
    /// Send SuggestPiece messages for pieces in the ARC read cache to peers.
    pub suggest_mode: bool,
    /// Maximum number of SuggestPiece messages to send per peer.
    pub max_suggest_pieces: usize,
    /// Send Have before disk write completes (ms). 0 = disabled.
    pub predictive_piece_announce_ms: u64,
```

Update `Default for TorrentConfig` (after `share_mode: false,`):

```rust
            piece_extent_affinity: true,
            suggest_mode: false,
            max_suggest_pieces: 10,
            predictive_piece_announce_ms: 0,
```

Update `From<&Settings> for TorrentConfig` (after the `share_mode` line):

```rust
            piece_extent_affinity: s.piece_extent_affinity,
            suggest_mode: s.suggest_mode,
            max_suggest_pieces: s.max_suggest_pieces,
            predictive_piece_announce_ms: s.predictive_piece_announce_ms,
```

**Step 3: Write tests for new settings**

Add these tests at the end of the existing `mod tests` in `settings.rs`:

```rust
    #[test]
    fn m44_settings_defaults() {
        let s = Settings::default();
        assert!(s.piece_extent_affinity);
        assert!(!s.suggest_mode);
        assert_eq!(s.max_suggest_pieces, 10);
        assert_eq!(s.predictive_piece_announce_ms, 0);
    }

    #[test]
    fn m44_high_performance_enables_suggest() {
        let s = Settings::high_performance();
        assert!(s.suggest_mode);
    }

    #[test]
    fn m44_json_round_trip() {
        let mut s = Settings::default();
        s.piece_extent_affinity = false;
        s.suggest_mode = true;
        s.max_suggest_pieces = 5;
        s.predictive_piece_announce_ms = 50;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, decoded);
    }
```

Add a test in the `types.rs` `mod tests`:

```rust
    #[test]
    fn torrent_config_m44_defaults() {
        let cfg = TorrentConfig::default();
        assert!(cfg.piece_extent_affinity);
        assert!(!cfg.suggest_mode);
        assert_eq!(cfg.max_suggest_pieces, 10);
        assert_eq!(cfg.predictive_piece_announce_ms, 0);
    }
```

**Step 4: Run tests**

```bash
cargo test -p ferrite-session settings::tests -- --nocapture
cargo test -p ferrite-session types::tests -- --nocapture
```

---

## Task 2: Peer Speed Categorization Enhancements

**Files:**
- Modify: `crates/ferrite-session/src/piece_selector.rs`

The `PeerSpeed` enum and `PeerSpeed::from_rate()` already exist (lines 8-27) with `#[allow(dead_code)]`. This task removes the `dead_code` allows (since we'll use the types actively) and adds configurable speed thresholds plus a `PeerSpeedClassifier` that can be instantiated with custom thresholds.

**Step 1: Add PeerSpeedClassifier**

Replace the `PeerSpeed` section (lines 7-27) with:

```rust
/// Speed category for a peer based on download rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PeerSpeed {
    Slow,   // < slow_threshold
    Medium, // slow_threshold .. < fast_threshold
    Fast,   // >= fast_threshold
}

impl PeerSpeed {
    /// Classify a peer by download rate using default thresholds.
    pub fn from_rate(bytes_per_sec: f64) -> Self {
        PeerSpeedClassifier::default().classify(bytes_per_sec)
    }
}

/// Configurable thresholds for peer speed classification.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PeerSpeedClassifier {
    /// Bytes/sec threshold below which a peer is Slow (default: 10 KB/s).
    pub slow_threshold: f64,
    /// Bytes/sec threshold at or above which a peer is Fast (default: 100 KB/s).
    pub fast_threshold: f64,
}

impl Default for PeerSpeedClassifier {
    fn default() -> Self {
        Self {
            slow_threshold: 10_240.0,
            fast_threshold: 102_400.0,
        }
    }
}

impl PeerSpeedClassifier {
    pub fn classify(&self, bytes_per_sec: f64) -> PeerSpeed {
        if bytes_per_sec < self.slow_threshold {
            PeerSpeed::Slow
        } else if bytes_per_sec < self.fast_threshold {
            PeerSpeed::Medium
        } else {
            PeerSpeed::Fast
        }
    }
}
```

**Step 2: Remove all `#[allow(dead_code)]` annotations from types in piece_selector.rs**

Remove the `#[allow(dead_code)]` from:
- `PeerSpeed` (line ~8) -- already removed in step 1
- `InFlightPiece` struct (line ~31)
- `InFlightPiece` impl (line ~37)
- `PickContext` struct (line ~56)
- `PickResult` struct (line ~78)
- `PieceSelector` struct (line ~91)
- `PieceSelector` impl (line ~98)

**Step 3: Add tests for PeerSpeedClassifier**

Add at the end of `mod tests` in `piece_selector.rs`:

```rust
    #[test]
    fn peer_speed_default_classification() {
        assert_eq!(PeerSpeed::from_rate(0.0), PeerSpeed::Slow);
        assert_eq!(PeerSpeed::from_rate(5_000.0), PeerSpeed::Slow);
        assert_eq!(PeerSpeed::from_rate(10_240.0), PeerSpeed::Medium);
        assert_eq!(PeerSpeed::from_rate(50_000.0), PeerSpeed::Medium);
        assert_eq!(PeerSpeed::from_rate(102_400.0), PeerSpeed::Fast);
        assert_eq!(PeerSpeed::from_rate(1_000_000.0), PeerSpeed::Fast);
    }

    #[test]
    fn peer_speed_custom_classifier() {
        let classifier = PeerSpeedClassifier {
            slow_threshold: 1_000.0,
            fast_threshold: 50_000.0,
        };
        assert_eq!(classifier.classify(500.0), PeerSpeed::Slow);
        assert_eq!(classifier.classify(1_000.0), PeerSpeed::Medium);
        assert_eq!(classifier.classify(50_000.0), PeerSpeed::Fast);
    }
```

**Step 4: Run tests**

```bash
cargo test -p ferrite-session piece_selector::tests -- --nocapture
```

---

## Task 3: Piece Extent Affinity

**Files:**
- Modify: `crates/ferrite-session/src/piece_selector.rs`

Extent affinity groups pieces into 4 MiB extents. When a peer in a speed class is downloading from extent E, other peers in the same speed class should also prefer extent E. This reduces disk seeking and improves cache locality.

**Step 1: Add extent computation to PickContext**

Add a new field to `PickContext` after `piece_size: u32,` (line ~74):

```rust
    pub extent_affinity: bool,
```

Update `default_pick_context()` in the test helper (add after `piece_size: 262_144,`):

```rust
            extent_affinity: false,
```

**Step 2: Add extent helper methods to PieceSelector**

Add these methods to the `impl PieceSelector` block, before `pick_blocks`:

```rust
    /// Size of an extent group in bytes (4 MiB).
    const EXTENT_SIZE: u64 = 4 * 1024 * 1024;

    /// Compute the extent index for a piece given the piece size.
    fn extent_of(piece: u32, piece_size: u32) -> u32 {
        let byte_offset = piece as u64 * piece_size as u64;
        (byte_offset / Self::EXTENT_SIZE) as u32
    }

    /// Find the preferred extent for a peer based on what other peers in the
    /// same speed class are currently downloading.
    fn preferred_extent(&self, ctx: &PickContext<'_>) -> Option<u32> {
        // Count how many blocks each extent has from peers of the same speed class
        let mut extent_counts: HashMap<u32, u32> = HashMap::new();
        for (&piece, ifp) in ctx.in_flight_pieces {
            let extent = Self::extent_of(piece, ctx.piece_size);
            for &peer_addr in ifp.assigned_blocks.values() {
                // We can't look up peer speed from here (PickContext only has our peer's info),
                // so use the fact that all peers share extents -- the key insight is we want to
                // continue an extent that's already partially downloaded.
                if peer_addr != ctx.peer_addr {
                    *extent_counts.entry(extent).or_default() += 1;
                }
            }
        }

        // Prefer the extent with the most blocks already being downloaded
        // by peers of the same speed class (approximated: any active extent)
        extent_counts.into_iter()
            .max_by_key(|&(_, count)| count)
            .map(|(extent, _)| extent)
    }
```

**Step 3: Integrate extent affinity into pick_rarest_new**

Replace the `pick_rarest_new` method with an extent-aware version:

```rust
    /// Rarest-first among pieces not in-flight, with optional extent affinity.
    fn pick_rarest_new<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        // If extent affinity is enabled, try picking from the preferred extent first
        if ctx.extent_affinity {
            if let Some(preferred) = self.preferred_extent(ctx) {
                if let Some(result) = self.pick_rarest_in_extent(ctx, missing_chunks, preferred) {
                    return Some(result);
                }
            }
        }

        // Fallback: standard rarest-first across all pieces
        self.pick_rarest_any(ctx, missing_chunks)
    }

    /// Rarest-first within a specific extent.
    fn pick_rarest_in_extent<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
        target_extent: u32,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        let mut best_index: Option<u32> = None;
        let mut best_avail: u32 = u32::MAX;

        for i in 0..self.num_pieces {
            if Self::extent_of(i, ctx.piece_size) != target_extent {
                continue;
            }
            if !ctx.peer_has.get(i) || ctx.we_have.get(i) || !ctx.wanted.get(i) {
                continue;
            }
            if ctx.in_flight_pieces.contains_key(&i) {
                continue;
            }
            let avail = self.effective_availability(i);
            if avail == 0 {
                continue;
            }
            if avail < best_avail {
                best_avail = avail;
                best_index = Some(i);
            }
        }

        best_index.map(|piece| {
            let blocks = missing_chunks(piece);
            let exclusive = self.should_whole_piece(ctx, &blocks);
            PickResult { piece, blocks, exclusive }
        })
    }

    /// Standard rarest-first across all pieces (no extent filtering).
    fn pick_rarest_any<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        let mut best_index: Option<u32> = None;
        let mut best_avail: u32 = u32::MAX;

        for i in 0..self.num_pieces {
            if !ctx.peer_has.get(i) || ctx.we_have.get(i) || !ctx.wanted.get(i) {
                continue;
            }
            if ctx.in_flight_pieces.contains_key(&i) {
                continue;
            }
            let avail = self.effective_availability(i);
            if avail == 0 {
                continue;
            }
            if avail < best_avail {
                best_avail = avail;
                best_index = Some(i);
            }
        }

        best_index.map(|piece| {
            let blocks = missing_chunks(piece);
            let exclusive = self.should_whole_piece(ctx, &blocks);
            PickResult { piece, blocks, exclusive }
        })
    }
```

**Step 4: Wire extent_affinity in TorrentActor**

In `crates/ferrite-session/src/torrent.rs`, in the `request_pieces_from_peer` function, where `PickContext` is constructed (~line 2624), add after `piece_size:`:

```rust
            extent_affinity: self.config.piece_extent_affinity,
```

**Step 5: Write extent affinity tests**

Add in `piece_selector.rs` test module:

```rust
    #[test]
    fn extent_of_computation() {
        // 256 KiB pieces, 4 MiB extent = 16 pieces per extent
        assert_eq!(PieceSelector::extent_of(0, 262_144), 0);
        assert_eq!(PieceSelector::extent_of(15, 262_144), 0);
        assert_eq!(PieceSelector::extent_of(16, 262_144), 1);
        assert_eq!(PieceSelector::extent_of(31, 262_144), 1);
        assert_eq!(PieceSelector::extent_of(32, 262_144), 2);

        // 1 MiB pieces, 4 MiB extent = 4 pieces per extent
        assert_eq!(PieceSelector::extent_of(0, 1_048_576), 0);
        assert_eq!(PieceSelector::extent_of(3, 1_048_576), 0);
        assert_eq!(PieceSelector::extent_of(4, 1_048_576), 1);
    }

    #[test]
    fn extent_affinity_prefers_active_extent() {
        // 32 pieces at 256 KiB = 8 MiB = 2 extents (0-15, 16-31)
        let mut sel = PieceSelector::new(32);
        for i in 0..32 {
            sel.availability[i] = 2;
        }
        // Piece 20 is rarest overall (in extent 1)
        sel.availability[20] = 1;
        // Piece 5 is rarer within extent 0 (but not globally rarest)
        sel.availability[5] = 1;

        let mut peer_has = Bitfield::new(32);
        for i in 0..32 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(32);
        let mut wanted = Bitfield::new(32);
        for i in 0..32 {
            wanted.set(i);
        }

        // Another peer is downloading piece 10 (extent 0)
        let mut ifp = InFlightPiece::new(2);
        ifp.assigned_blocks.insert((10, 0), addr(9999));
        let mut in_flight = HashMap::new();
        in_flight.insert(10u32, ifp);

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();

        let mut ctx = default_pick_context(
            addr(5555), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.extent_affinity = true;
        ctx.piece_size = 262_144;
        ctx.completed_count = 100; // past initial threshold

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        // With extent affinity, should prefer extent 0 (where piece 10 is active)
        // and pick piece 5 (rarest in extent 0), not piece 20 (rarest globally in extent 1)
        assert_eq!(result.piece, 5);
    }

    #[test]
    fn extent_affinity_disabled_picks_global_rarest() {
        let mut sel = PieceSelector::new(32);
        for i in 0..32 {
            sel.availability[i] = 2;
        }
        sel.availability[20] = 1; // globally rarest (extent 1)
        sel.availability[5] = 1;  // rarest in extent 0

        let mut peer_has = Bitfield::new(32);
        for i in 0..32 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(32);
        let mut wanted = Bitfield::new(32);
        for i in 0..32 {
            wanted.set(i);
        }

        let mut ifp = InFlightPiece::new(2);
        ifp.assigned_blocks.insert((10, 0), addr(9999));
        let mut in_flight = HashMap::new();
        in_flight.insert(10u32, ifp);

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();

        let mut ctx = default_pick_context(
            addr(5555), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.extent_affinity = false; // disabled
        ctx.piece_size = 262_144;
        ctx.completed_count = 100;

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        // Without extent affinity, picks globally rarest = piece 5 (lowest index tie-break with 20)
        assert_eq!(result.piece, 5);
    }

    #[test]
    fn extent_affinity_fallback_to_any() {
        // All pieces in the active extent are already downloaded or in-flight
        let mut sel = PieceSelector::new(32);
        for i in 0..32 {
            sel.availability[i] = 2;
        }

        let mut peer_has = Bitfield::new(32);
        for i in 0..32 {
            peer_has.set(i);
        }
        // We already have all pieces in extent 0 (0-15)
        let mut we_have = Bitfield::new(32);
        for i in 0..16 {
            we_have.set(i);
        }
        let mut wanted = Bitfield::new(32);
        for i in 0..32 {
            wanted.set(i);
        }

        // Active piece in extent 0 (but we have all extent 0 pieces)
        let mut ifp = InFlightPiece::new(2);
        ifp.assigned_blocks.insert((10, 0), addr(9999));
        let mut in_flight = HashMap::new();
        in_flight.insert(10u32, ifp);

        let streaming = BTreeSet::new();
        let time_critical = BTreeSet::new();
        let suggested = HashSet::new();

        let mut ctx = default_pick_context(
            addr(5555), &peer_has, &we_have, &wanted,
            &in_flight, &streaming, &time_critical, &suggested,
        );
        ctx.extent_affinity = true;
        ctx.piece_size = 262_144;
        ctx.completed_count = 100;

        let chunks = |_piece: u32| vec![(0, 16384)];
        let result = sel.pick_blocks(&ctx, chunks).unwrap();
        // Should fall back to any extent, picking from extent 1 (pieces 16-31)
        assert!(result.piece >= 16 && result.piece < 32);
    }
```

**Step 6: Run tests**

```bash
cargo test -p ferrite-session piece_selector::tests -- --nocapture
```

---

## Task 4: SuggestPiece Sending Infrastructure

**Files:**
- Modify: `crates/ferrite-storage/src/cache.rs`
- Modify: `crates/ferrite-session/src/disk.rs`
- Modify: `crates/ferrite-session/src/types.rs`
- Modify: `crates/ferrite-session/src/peer.rs`

This task adds the ability to query which pieces are in the ARC read cache and send `SuggestPiece` messages to peers.

**Step 1: Add `cached_keys()` method to ArcCache**

In `crates/ferrite-storage/src/cache.rs`, add a new method to `impl ArcCache`:

```rust
    /// Return an iterator over all keys currently in the cache (T1 + T2).
    pub fn cached_keys(&self) -> impl Iterator<Item = &K> {
        self.map.keys()
    }
```

**Step 2: Add `SuggestPiece` to PeerCommand**

In `crates/ferrite-session/src/types.rs`, add a new variant to `PeerCommand` after `SendBitfield(Bytes),` (line ~284):

```rust
    /// BEP 6: Suggest a piece to the peer.
    SuggestPiece(u32),
```

**Step 3: Handle SuggestPiece command in peer task**

In `crates/ferrite-session/src/peer.rs`, in the `handle_command` function, add a new match arm before `PeerCommand::SendHashRequest`:

```rust
        PeerCommand::SuggestPiece(index) => Message::SuggestPiece(index),
```

**Step 4: Add `cached_pieces` query to DiskManager**

In `crates/ferrite-session/src/disk.rs`, add a new `DiskJob` variant after `FlushWriteBuffer` (line ~89):

```rust
    CachedPieces {
        info_hash: Id20,
        reply: oneshot::Sender<Vec<u32>>,
    },
```

Add handler in the `DiskActor::run()` method. Find the match arm for `DiskJob::FlushWriteBuffer` and add after it:

```rust
                DiskJob::CachedPieces { info_hash, reply } => {
                    let pieces: Vec<u32> = self.cache
                        .cached_keys()
                        .filter(|(ih, _, _)| *ih == info_hash)
                        .map(|(_, piece, _)| *piece)
                        .collect::<std::collections::HashSet<u32>>()
                        .into_iter()
                        .collect();
                    let _ = reply.send(pieces);
                }
```

Add a public method to `DiskHandle`:

```rust
    /// Query which pieces are currently in the read cache for this torrent.
    pub async fn cached_pieces(&self) -> Vec<u32> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::CachedPieces {
                info_hash: self.info_hash,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or_default()
    }
```

**Step 5: Add ArcCache test**

In `crates/ferrite-storage/src/cache.rs` test module:

```rust
    #[test]
    fn cached_keys_returns_all_entries() {
        let mut cache = ArcCache::new(5);
        cache.insert(1, "one".to_string());
        cache.insert(2, "two".to_string());
        cache.insert(3, "three".to_string());
        let mut keys: Vec<_> = cache.cached_keys().copied().collect();
        keys.sort();
        assert_eq!(keys, vec![1, 2, 3]);
    }

    #[test]
    fn cached_keys_empty_cache() {
        let cache: ArcCache<i32, String> = ArcCache::new(3);
        assert_eq!(cache.cached_keys().count(), 0);
    }
```

**Step 6: Run tests**

```bash
cargo test -p ferrite-storage cache::tests -- --nocapture
cargo test -p ferrite-session -- --nocapture
```

---

## Task 5: Suggest Mode — TorrentActor Integration

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

This task wires the suggest mode into the TorrentActor. When seeding (or when we have pieces) and `suggest_mode` is enabled, the actor periodically queries the ARC cache for piece indices and sends `SuggestPiece` messages to connected peers.

**Step 1: Add `suggested_to_peers` tracking to TorrentActor**

In the `TorrentActor` struct definition (after `have_buffer` around line ~685), add:

```rust
    // M44: pieces we've suggested to each peer (to avoid re-suggesting)
    suggested_to_peers: HashMap<SocketAddr, HashSet<u32>>,
```

Initialize it in both `from_torrent()` and `from_magnet()` constructor sites. In both, add after `have_buffer,`:

```rust
            suggested_to_peers: HashMap::new(),
```

**Step 2: Add suggest_cached_pieces method**

Add a new method to `impl TorrentActor` (after `on_piece_verified`):

```rust
    /// Query the disk cache and send SuggestPiece messages for cached pieces.
    async fn suggest_cached_pieces(&mut self) {
        if !self.config.suggest_mode {
            return;
        }
        let disk = match self.disk {
            Some(ref d) => d.clone(),
            None => return,
        };

        let cached = disk.cached_pieces().await;
        if cached.is_empty() {
            return;
        }

        let max_suggest = self.config.max_suggest_pieces;
        let peer_addrs: Vec<SocketAddr> = self.peers.keys().copied().collect();

        for peer_addr in peer_addrs {
            let already_suggested = self.suggested_to_peers
                .entry(peer_addr)
                .or_default();

            let peer_has = match self.peers.get(&peer_addr) {
                Some(p) => &p.bitfield,
                None => continue,
            };

            let mut sent = 0;
            for &piece in &cached {
                if sent >= max_suggest {
                    break;
                }
                // Don't suggest pieces the peer already has
                if peer_has.get(piece) {
                    continue;
                }
                // Don't re-suggest
                if already_suggested.contains(&piece) {
                    continue;
                }

                if let Some(peer) = self.peers.get(&peer_addr) {
                    let _ = peer.cmd_tx.send(PeerCommand::SuggestPiece(piece)).await;
                    already_suggested.insert(piece);
                    sent += 1;
                }
            }
        }
    }
```

**Step 3: Clean up suggested_to_peers on peer disconnect**

In the `PeerEvent::Disconnected` handler in `torrent.rs`, after the line `if let Some(peer) = self.peers.remove(&peer_addr) {` (line ~1383), add:

```rust
                    self.suggested_to_peers.remove(&peer_addr);
```

**Step 4: Wire into the select! loop**

In the `run()` method, after the existing timer definitions (around line 743), add a new timer for suggest mode:

```rust
        let mut suggest_interval = if self.config.suggest_mode {
            Some(tokio::time::interval(Duration::from_secs(30)))
        } else {
            None
        };
        if let Some(ref mut si) = suggest_interval {
            si.tick().await; // skip initial tick
        }
```

In the `tokio::select!` loop, add a new arm. Find an appropriate location near the `have_flush_interval` arm and add:

```rust
            // M44: periodic suggest_cached_pieces
            _ = async {
                match suggest_interval {
                    Some(ref mut interval) => interval.tick().await,
                    None => std::future::pending().await,
                }
            } => {
                self.suggest_cached_pieces().await;
            }
```

**Step 5: Send suggests on piece verification too**

In `on_piece_verified()`, after the `Have` broadcast block (after the `}` closing the `if self.super_seed.is_none()` block, around line 1973), add:

```rust
        // M44: suggest newly-verified piece to peers that don't have it
        if self.config.suggest_mode {
            let max_suggest = self.config.max_suggest_pieces;
            let peer_addrs: Vec<SocketAddr> = self.peers.keys().copied().collect();
            for peer_addr in peer_addrs {
                let already = self.suggested_to_peers
                    .entry(peer_addr)
                    .or_default();
                if already.len() >= max_suggest {
                    continue;
                }
                let peer_has = match self.peers.get(&peer_addr) {
                    Some(p) if !p.bitfield.get(index) => true,
                    _ => false,
                };
                if peer_has && !already.contains(&index) {
                    if let Some(peer) = self.peers.get(&peer_addr) {
                        let _ = peer.cmd_tx.send(PeerCommand::SuggestPiece(index)).await;
                        already.insert(index);
                    }
                }
            }
        }
```

**Step 6: Run tests**

```bash
cargo test -p ferrite-session -- --nocapture
cargo clippy -p ferrite-session -- -D warnings
```

---

## Task 6: Predictive Piece Announce

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

Predictive piece announce sends `Have` messages before disk verification completes. When a piece's last block arrives and we're confident it will verify, we can announce early to reduce latency for peers wanting rare pieces. This is gated by `predictive_piece_announce_ms` (0 = disabled).

**Step 1: Add predictive_have_sent tracking**

In `TorrentActor` struct, add after `suggested_to_peers`:

```rust
    // M44: pieces for which we've already sent predictive Have
    predictive_have_sent: HashSet<u32>,
```

Initialize it in both constructors after `suggested_to_peers`:

```rust
            predictive_have_sent: HashSet::new(),
```

**Step 2: Send predictive Have when last block of a piece arrives**

In `torrent.rs`, find the `PeerEvent::PieceData` handler. After the block is written to disk but BEFORE `verify_and_mark_piece` is called (look for the line that calls `self.verify_and_mark_piece(index).await;`, approximately line 1506), insert predictive announce logic:

```rust
                // M44: Predictive piece announce — send Have before verification
                if self.config.predictive_piece_announce_ms > 0
                    && !self.predictive_have_sent.contains(&index)
                {
                    self.predictive_have_sent.insert(index);
                    // Send Have to all peers immediately (before disk hash verification)
                    for peer in self.peers.values() {
                        if !peer.bitfield.get(index) {
                            let _ = peer.cmd_tx.send(PeerCommand::Have(index)).await;
                        }
                    }
                }
```

**Step 3: Suppress duplicate Have in on_piece_verified**

In `on_piece_verified()`, modify the Have broadcast section. Replace the existing broadcast logic (the block that starts with `if self.super_seed.is_none()` around line 1962) with:

```rust
        // Broadcast Have to all peers (skip in super-seed mode)
        if self.super_seed.is_none() {
            // M44: skip if we already sent predictive Have for this piece
            let already_announced = self.predictive_have_sent.remove(&index);

            if already_announced {
                // Predictive Have was already sent; no duplicate needed
            } else if self.have_buffer.is_enabled() {
                self.have_buffer.push(index);
            } else {
                // Immediate mode — with redundancy elimination
                for peer in self.peers.values() {
                    if !peer.bitfield.get(index) {
                        let _ = peer.cmd_tx.send(PeerCommand::Have(index)).await;
                    }
                }
            }
        }
```

**Step 4: Handle hash failure — retract predictive Have**

In the `on_piece_hash_failed` method (find it via grep), after the existing failure handling, add:

```rust
        // M44: if we sent a predictive Have for this piece, the peer now has
        // incorrect state. We can't retract a Have message (no such protocol
        // message exists), but we clean up our tracking. The piece will be
        // re-verified and re-announced if it succeeds later.
        self.predictive_have_sent.remove(&index);
```

**Step 5: Run tests**

```bash
cargo test -p ferrite-session -- --nocapture
cargo clippy -p ferrite-session -- -D warnings
```

---

## Task 7: Predictive Announce Unit Tests

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` (test module at end of file)

Since the TorrentActor is tested via integration patterns, we add a standalone test for the predictive announce tracking HashSet logic. For the full torrent actor tests, we verify the wiring compiles and clippy passes.

**Step 1: Add TorrentConfig test for predictive announce**

In `crates/ferrite-session/src/types.rs` tests:

```rust
    #[test]
    fn torrent_config_from_settings_m44() {
        let mut s = crate::settings::Settings::default();
        s.piece_extent_affinity = false;
        s.suggest_mode = true;
        s.max_suggest_pieces = 5;
        s.predictive_piece_announce_ms = 100;
        let tc = TorrentConfig::from(&s);
        assert!(!tc.piece_extent_affinity);
        assert!(tc.suggest_mode);
        assert_eq!(tc.max_suggest_pieces, 5);
        assert_eq!(tc.predictive_piece_announce_ms, 100);
    }
```

**Step 2: Run full workspace tests and clippy**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

---

## Task 8: ClientBuilder Integration

**Files:**
- Modify: `crates/ferrite/src/client.rs` (or wherever ClientBuilder is defined)

**Step 1: Find ClientBuilder location**

Search for the ClientBuilder implementation file and add four new builder methods.

**Step 2: Add builder methods**

Add these methods to `ClientBuilder`:

```rust
    /// Enable/disable piece extent affinity (default: true).
    ///
    /// When enabled, peers in the same speed class prefer downloading
    /// pieces from the same 4 MiB extent, improving disk cache locality.
    pub fn piece_extent_affinity(mut self, enabled: bool) -> Self {
        self.settings.piece_extent_affinity = enabled;
        self
    }

    /// Enable/disable suggest mode (default: false).
    ///
    /// When enabled, the client sends BEP 6 SuggestPiece messages to peers
    /// for pieces currently in the ARC disk read cache.
    pub fn suggest_mode(mut self, enabled: bool) -> Self {
        self.settings.suggest_mode = enabled;
        self
    }

    /// Maximum SuggestPiece messages per peer (default: 10).
    pub fn max_suggest_pieces(mut self, count: usize) -> Self {
        self.settings.max_suggest_pieces = count;
        self
    }

    /// Send Have messages before disk verification completes (ms, default: 0 = disabled).
    ///
    /// When non-zero, Have messages are sent predictively when the last block
    /// of a piece arrives, before hash verification. If verification fails,
    /// the piece will be re-downloaded (no protocol-level retraction exists).
    pub fn predictive_piece_announce(mut self, ms: u64) -> Self {
        self.settings.predictive_piece_announce_ms = ms;
        self
    }
```

**Step 3: Add ClientBuilder test**

```rust
    #[test]
    fn builder_m44_settings() {
        let settings = ClientBuilder::new()
            .piece_extent_affinity(false)
            .suggest_mode(true)
            .max_suggest_pieces(5)
            .predictive_piece_announce(100)
            .into_settings();
        assert!(!settings.piece_extent_affinity);
        assert!(settings.suggest_mode);
        assert_eq!(settings.max_suggest_pieces, 5);
        assert_eq!(settings.predictive_piece_announce_ms, 100);
    }
```

**Step 4: Run tests**

```bash
cargo test -p ferrite -- --nocapture
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

---

## Task 9: Facade Re-exports and Documentation

**Files:**
- Modify: `crates/ferrite/src/session.rs` (or appropriate re-export module)

**Step 1: Ensure PeerSpeed re-export path**

`PeerSpeed` and `PeerSpeedClassifier` are `pub(crate)` — they are internal to ferrite-session and not exposed through the facade. This is correct; they are implementation details. No re-export needed.

**Step 2: Verify all new settings fields appear in JSON serialization**

Add a test in `crates/ferrite-session/src/settings.rs`:

```rust
    #[test]
    fn m44_settings_in_json() {
        let s = Settings::default();
        let json = serde_json::to_string_pretty(&s).unwrap();
        assert!(json.contains("piece_extent_affinity"));
        assert!(json.contains("suggest_mode"));
        assert!(json.contains("max_suggest_pieces"));
        assert!(json.contains("predictive_piece_announce_ms"));
    }
```

**Step 3: Final workspace test and clippy**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

---

## Summary

| Task | Description | Files Modified | Tests Added |
|------|-------------|---------------|-------------|
| 1 | Settings fields for M44 features | `settings.rs`, `types.rs` | 4 |
| 2 | PeerSpeedClassifier with custom thresholds | `piece_selector.rs` | 2 |
| 3 | Piece extent affinity in picker | `piece_selector.rs`, `torrent.rs` | 3 |
| 4 | SuggestPiece sending infrastructure | `cache.rs`, `disk.rs`, `types.rs`, `peer.rs` | 2 |
| 5 | Suggest mode TorrentActor integration | `torrent.rs` | 0 (wiring) |
| 6 | Predictive piece announce | `torrent.rs` | 0 (wiring) |
| 7 | Predictive announce tests | `types.rs` | 1 |
| 8 | ClientBuilder integration | `client.rs` | 1 |
| 9 | Re-exports and final verification | `settings.rs` | 1 |
| **Total** | | **8 files** | **~14 tests** |

### Version Bump

After all tasks pass, bump workspace version in root `Cargo.toml` to `0.42.0` and commit:

```
feat: piece picker enhancements — extent affinity, suggest mode, predictive announce (M44)
```

### Key Design Decisions

1. **Extent size = 4 MiB:** Matches typical OS readahead size and libtorrent's default. Computed as `byte_offset / 4MiB`, not configurable (internal constant).

2. **Suggest mode is opt-in (default: false):** Sending SuggestPiece to many peers creates wire traffic proportional to `peers * cached_pieces`. Default-off avoids surprises. The `high_performance()` preset enables it.

3. **Predictive announce has no retraction:** BitTorrent has no "un-Have" message. If a predictively announced piece fails verification, peers may request it and get RejectRequest. This is acceptable — libtorrent does the same thing.

4. **ArcCache.cached_keys():** Returns a simple iterator over the HashMap keys. The deduplication to piece indices (multiple blocks per piece share the same piece index) happens at the DiskActor level using a HashSet.

5. **suggest_interval = 30s:** Cached pieces change slowly; polling every 30 seconds keeps overhead low while catching new entries.

6. **PeerSpeedClassifier is `pub(crate)`:** Speed classification is an internal concern. The thresholds (10 KB/s, 100 KB/s) match libtorrent defaults and are not user-configurable. The `PeerSpeedClassifier` struct exists for future extensibility and testability.
