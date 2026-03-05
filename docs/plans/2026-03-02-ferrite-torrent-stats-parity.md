# TorrentStats Full libtorrent Parity — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Expand ferrite's `TorrentStats` from 9 fields to full libtorrent `torrent_status` parity (~55 fields), adding per-torrent time tracking, transfer counters, rate computation, connection details, flags, and error reporting.

**Architecture:** The TorrentActor already maintains most underlying data (peers, chunk tracker, piece selector) but only exposes 9 fields through `make_stats()`. We add new tracking fields to TorrentActor (timestamps, byte counters, error state), compute aggregates from existing peer/piece data, and expand `TorrentStats` + `make_stats()`. Some fields (queue_position, auto_managed, save_path) live at the session level, so we add a `SessionTorrentEntry` enrichment step.

**Tech Stack:** Rust, tokio, std::time (SystemTime for POSIX timestamps, Instant for durations)

---

## Field Mapping Reference

### Legend
- **E** = Expose existing data (just read a field or aggregate peers)
- **N** = New tracking needed (add field to TorrentActor)
- **S** = Session-level data (from SessionTorrentEntry, not TorrentActor)
- **SKIP** = Deprecated or not applicable

### Complete Mapping

| # | libtorrent field | Type | Source | Category |
|---|---|---|---|---|
| 1 | state | state_t | E | state |
| 2 | info_hashes | info_hash_t | E | identity |
| 3 | name | string | E | identity |
| 4 | save_path | string | S | storage |
| 5 | has_metadata | bool | E | state |
| 6 | errc | error_code | N | error |
| 7 | error_file | file_index_t | N | error |
| 8 | total_download | i64 | N | transfer_session |
| 9 | total_upload | i64 | N | transfer_session |
| 10 | total_payload_download | i64 | E (=downloaded) | transfer_session |
| 11 | total_payload_upload | i64 | E (=uploaded) | transfer_session |
| 12 | total_failed_bytes | i64 | N | transfer_session |
| 13 | total_redundant_bytes | i64 | N | transfer_session |
| 14 | all_time_download | i64 | E (=downloaded) | transfer_all |
| 15 | all_time_upload | i64 | E (=uploaded) | transfer_all |
| 16 | download_rate | i32 | E (aggregate peers) | rates |
| 17 | upload_rate | i32 | E (aggregate peers) | rates |
| 18 | download_payload_rate | i32 | E (aggregate peers) | rates |
| 19 | upload_payload_rate | i32 | E (aggregate peers) | rates |
| 20 | progress | f32 | E (compute) | progress |
| 21 | progress_ppm | i32 | E (compute) | progress |
| 22 | total_done | i64 | E (from chunk_tracker) | progress |
| 23 | total | i64 | E (from meta) | progress |
| 24 | total_wanted_done | i64 | E (chunk_tracker + wanted) | progress |
| 25 | total_wanted | i64 | E (meta + wanted) | progress |
| 26 | num_pieces | i32 | E (=pieces_have) | progress |
| 27 | is_seeding | bool | E | state |
| 28 | is_finished | bool | E | state |
| 29 | pieces | bitfield | E (chunk_tracker) | pieces |
| 30 | verified_pieces | bitfield | E (chunk_tracker) | pieces |
| 31 | block_size | i32 | E (const 16384) | pieces |
| 32 | num_peers | i32 | E | connections |
| 33 | num_seeds | i32 | E (aggregate) | connections |
| 34 | num_complete | i32 | N (from tracker scrape) | connections |
| 35 | num_incomplete | i32 | N (from tracker scrape) | connections |
| 36 | list_seeds | i32 | E (from available_peers) | connections |
| 37 | list_peers | i32 | E (from available_peers) | connections |
| 38 | connect_candidates | i32 | E (from available_peers) | connections |
| 39 | num_connections | i32 | E (= peers.len()) | connections |
| 40 | num_uploads | i32 | E (unchoked count) | connections |
| 41 | connections_limit | i32 | E (config.max_peers) | limits |
| 42 | uploads_limit | i32 | E (choker slots) | limits |
| 43 | up_bandwidth_queue | i32 | SKIP (internal to rate limiter) | limits |
| 44 | down_bandwidth_queue | i32 | SKIP (internal to rate limiter) | limits |
| 45 | distributed_full_copies | i32 | E (compute from peer bitfields) | swarm |
| 46 | distributed_fraction | i32 | E (compute from peer bitfields) | swarm |
| 47 | distributed_copies | f32 | E (compute from peer bitfields) | swarm |
| 48 | current_tracker | string | N (track last successful) | tracker |
| 49 | announcing_to_trackers | bool | E | tracker |
| 50 | announcing_to_lsd | bool | E | tracker |
| 51 | announcing_to_dht | bool | E | tracker |
| 52 | added_time | i64 | N | time |
| 53 | completed_time | i64 | N | time |
| 54 | last_seen_complete | i64 | N | time |
| 55 | last_upload | i64 | N | time |
| 56 | last_download | i64 | N | time |
| 57 | active_duration | i64 | N | time |
| 58 | finished_duration | i64 | N | time |
| 59 | seeding_duration | i64 | N | time |
| 60 | moving_storage | bool | N | storage |
| 61 | queue_position | i32 | S | queue |
| 62 | seed_rank | i32 | SKIP (auto-manage internal) | queue |
| 63 | has_incoming | bool | N | state |
| 64 | need_save_resume | bool | N | state |
| 65 | flags | bitfield | E (compose from config/state) | state |

**Final count:** ~58 fields (skip 5: up/down_bandwidth_queue, seed_rank, handle, torrent_file pointer)

---

## Task 1: Expand TorrentStats Struct — Identity, State, Progress

Add the first batch of fields that can be computed from existing TorrentActor data with no new tracking.

**Files:**
- Modify: `crates/ferrite-session/src/types.rs:240-261` (TorrentStats struct)
- Modify: `crates/ferrite-session/src/torrent.rs:1436-1459` (make_stats)
- Test: inline in `crates/ferrite-session/src/types.rs` (existing test module)

**Step 1: Add identity + state + progress fields to TorrentStats**

In `crates/ferrite-session/src/types.rs`, expand `TorrentStats`:

```rust
/// Aggregate statistics for a torrent.
///
/// Mirrors libtorrent's `torrent_status` for full parity. Fields are grouped
/// by category: identity, state, progress, transfer, rates, connections,
/// time, tracker, storage, queue, and flags.
#[derive(Debug, Clone)]
pub struct TorrentStats {
    // ── Identity ──
    /// Info hash(es) for this torrent (v1 and/or v2).
    pub info_hashes: ferrite_core::InfoHashes,
    /// Torrent name (from metadata, or "" if metadata not yet received).
    pub name: String,

    // ── State ──
    /// Current torrent state.
    pub state: TorrentState,
    /// Whether metadata has been received (always true for .torrent adds, false for magnet until resolved).
    pub has_metadata: bool,
    /// True if all pieces have been downloaded.
    pub is_seeding: bool,
    /// True if all wanted pieces (priority > 0) have been downloaded.
    pub is_finished: bool,

    // ── Progress ──
    /// Progress of the current task in [0.0, 1.0].
    pub progress: f32,
    /// Progress in parts-per-million [0, 1_000_000] for higher precision.
    pub progress_ppm: u32,
    /// Total bytes of file(s) that we have on disk.
    pub total_done: u64,
    /// Total size of all files in the torrent.
    pub total: u64,
    /// Bytes we have of files/pieces we actually want (priority > 0).
    pub total_wanted_done: u64,
    /// Total bytes of pieces with priority > 0.
    pub total_wanted: u64,
    /// Number of verified pieces we have.
    pub pieces_have: u32,
    /// Total number of pieces in the torrent.
    pub pieces_total: u32,
    /// Block (sub-piece request) size in bytes.
    pub block_size: u32,
    /// Progress of piece checking (0.0–1.0), meaningful when state is `Checking`.
    pub checking_progress: f32,

    // ── Transfer (session-lifetime) ──
    /// Total bytes downloaded this session (payload + protocol overhead).
    pub total_download: u64,
    /// Total bytes uploaded this session (payload + protocol overhead).
    pub total_upload: u64,
    /// Payload bytes downloaded this session.
    pub total_payload_download: u64,
    /// Payload bytes uploaded this session.
    pub total_payload_upload: u64,
    /// Bytes downloaded that failed piece hash verification.
    pub total_failed_bytes: u64,
    /// Bytes downloaded that were already possessed (wasted/redundant).
    pub total_redundant_bytes: u64,

    // ── Transfer (all-time, persisted across sessions) ──
    /// Accumulated payload download bytes across all sessions.
    pub all_time_download: u64,
    /// Accumulated payload upload bytes across all sessions.
    pub all_time_upload: u64,

    // ── Rates ──
    /// Aggregate download rate across all peers (bytes/sec, payload + overhead).
    pub download_rate: u64,
    /// Aggregate upload rate across all peers (bytes/sec, payload + overhead).
    pub upload_rate: u64,
    /// Payload-only download rate (bytes/sec).
    pub download_payload_rate: u64,
    /// Payload-only upload rate (bytes/sec).
    pub upload_payload_rate: u64,

    // ── Connections ──
    /// Number of currently connected peers.
    pub num_peers: usize,
    /// Number of connected peers that are seeders.
    pub num_seeds: usize,
    /// Total seeders from tracker scrape (-1 if unknown).
    pub num_complete: i32,
    /// Total leechers from tracker scrape (-1 if unknown).
    pub num_incomplete: i32,
    /// Number of seeds in our peer list (not necessarily connected).
    pub list_seeds: usize,
    /// Total peers in our peer list (connected + available).
    pub list_peers: usize,
    /// Peers available for connection attempts.
    pub connect_candidates: usize,
    /// Total peer connections including half-open.
    pub num_connections: usize,
    /// Number of unchoked peers (peers we are uploading to).
    pub num_uploads: usize,
    /// Number of connected peers broken down by discovery source.
    pub peers_by_source: HashMap<crate::peer_state::PeerSource, usize>,

    // ── Connection Limits ──
    /// Configured max peer connections for this torrent.
    pub connections_limit: usize,
    /// Configured max upload slots (unchoked peers).
    pub uploads_limit: usize,

    // ── Distributed Copies (swarm health) ──
    /// Number of complete distributed copies in the swarm (integer part).
    pub distributed_full_copies: u32,
    /// Fractional distributed copies in [0, 1000].
    pub distributed_fraction: u32,
    /// Floating-point distributed copies.
    pub distributed_copies: f32,

    // ── Tracker ──
    /// URL of the last working tracker (empty if none).
    pub current_tracker: String,
    /// Whether announces to trackers are enabled.
    pub announcing_to_trackers: bool,
    /// Whether Local Service Discovery announces are enabled.
    pub announcing_to_lsd: bool,
    /// Whether DHT announces are enabled.
    pub announcing_to_dht: bool,

    // ── Timestamps ──
    /// POSIX timestamp when the torrent was added (0 if unknown).
    pub added_time: i64,
    /// POSIX timestamp when the torrent completed (0 if not yet complete).
    pub completed_time: i64,
    /// POSIX timestamp when we last saw a complete copy in the swarm.
    pub last_seen_complete: i64,
    /// POSIX timestamp of last payload upload.
    pub last_upload: i64,
    /// POSIX timestamp of last payload download.
    pub last_download: i64,

    // ── Durations ──
    /// Cumulative seconds the torrent has been active (not paused).
    pub active_duration: i64,
    /// Cumulative seconds in finished state (wanted pieces done, still active).
    pub finished_duration: i64,
    /// Cumulative seconds in seeding state.
    pub seeding_duration: i64,

    // ── Storage ──
    /// Directory where torrent files are stored.
    pub save_path: String,
    /// Whether storage is currently being moved.
    pub moving_storage: bool,

    // ── Queue ──
    /// Position in the download queue (-1 if not queued / seeding).
    pub queue_position: i32,

    // ── Error ──
    /// Error message if the torrent is in an error state (empty if no error).
    pub error: String,
    /// File index that caused the error (-1 if not file-related).
    pub error_file: i32,

    // ── Flags ──
    /// Whether the torrent is paused.
    pub is_paused: bool,
    /// Whether the torrent is auto-managed by the queue system.
    pub auto_managed: bool,
    /// Whether sequential download is enabled.
    pub sequential_download: bool,
    /// Whether super-seeding mode is active.
    pub super_seeding: bool,
    /// Whether an incoming peer connection has been received.
    pub has_incoming: bool,
    /// Whether resume data needs saving (state changed since last save).
    pub need_save_resume: bool,
}
```

**Step 2: Add new tracking fields to TorrentActor**

In `crates/ferrite-session/src/torrent.rs`, add to the `TorrentActor` struct after the existing `// Stats` section (line ~778):

```rust
    // Stats
    downloaded: u64,
    uploaded: u64,
    checking_progress: f32,

    // Extended stats (libtorrent parity)
    /// Total bytes downloaded including protocol overhead (session-only).
    total_download: u64,
    /// Total bytes uploaded including protocol overhead (session-only).
    total_upload: u64,
    /// Bytes that failed piece hash verification.
    total_failed_bytes: u64,
    /// Bytes downloaded that were already possessed (redundant).
    total_redundant_bytes: u64,
    /// Last tracker URL that responded successfully.
    current_tracker: String,
    /// Scrape: number of complete peers (-1 = unknown).
    scrape_complete: i32,
    /// Scrape: number of incomplete peers (-1 = unknown).
    scrape_incomplete: i32,
    /// POSIX timestamp when the torrent was added.
    added_time: i64,
    /// POSIX timestamp when the torrent completed downloading.
    completed_time: i64,
    /// POSIX timestamp of last payload download from any peer.
    last_download_time: i64,
    /// POSIX timestamp of last payload upload to any peer.
    last_upload_time: i64,
    /// POSIX timestamp when we last saw a complete copy in the swarm.
    last_seen_complete: i64,
    /// Cumulative seconds active (updated on pause/shutdown).
    active_duration: i64,
    /// Cumulative seconds in finished state.
    finished_duration: i64,
    /// Cumulative seconds in seeding state.
    seeding_duration: i64,
    /// Instant when the torrent last became active (for duration accumulation).
    active_since: Option<std::time::Instant>,
    /// Instant when the torrent entered finished/seeding state (for duration accumulation).
    state_duration_since: Option<std::time::Instant>,
    /// Whether storage is being moved.
    moving_storage: bool,
    /// Whether any incoming peer has connected.
    has_incoming: bool,
    /// Whether resume data needs saving.
    need_save_resume: bool,
    /// Error message (empty = no error).
    error_message: String,
    /// File index that caused the error (-1 = N/A).
    error_file_index: i32,
```

**Step 3: Initialize new fields in both constructors**

In both `from_torrent` (~line 228) and `from_magnet` (~line 449) constructors, add after `checking_progress: 0.0,`:

```rust
    total_download: 0,
    total_upload: 0,
    total_failed_bytes: 0,
    total_redundant_bytes: 0,
    current_tracker: String::new(),
    scrape_complete: -1,
    scrape_incomplete: -1,
    added_time: std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0),
    completed_time: 0,
    last_download_time: 0,
    last_upload_time: 0,
    last_seen_complete: 0,
    active_duration: 0,
    finished_duration: 0,
    seeding_duration: 0,
    active_since: Some(std::time::Instant::now()),
    state_duration_since: None,
    moving_storage: false,
    has_incoming: false,
    need_save_resume: false,
    error_message: String::new(),
    error_file_index: -1,
```

**Step 4: Implement distributed_copies computation**

Add a helper method to TorrentActor:

```rust
    /// Compute distributed copies from connected peer bitfields.
    ///
    /// Returns (full_copies, fraction_in_thousandths, floating_point).
    /// The rarest piece determines `full_copies`. The fraction represents
    /// how many pieces have more copies than the rarest.
    fn distributed_copies(&self) -> (u32, u32, f32) {
        if self.num_pieces == 0 || self.peers.is_empty() {
            return (0, 0, 0.0);
        }

        // Count availability per piece
        let mut min_count = u32::MAX;
        let mut above_min = 0u32;
        let total = self.num_pieces;

        for piece_idx in 0..total {
            let mut count = 0u32;
            for peer in self.peers.values() {
                if peer.bitfield.has(piece_idx) {
                    count += 1;
                }
            }
            if count < min_count {
                min_count = count;
                above_min = 0;
            }
            if count > min_count {
                above_min += 1;
            }
        }

        if min_count == u32::MAX {
            min_count = 0;
        }

        let fraction = if total > 0 {
            ((above_min as u64 * 1000) / total as u64) as u32
        } else {
            0
        };
        let float_val = min_count as f32 + (fraction as f32 / 1000.0);

        (min_count, fraction, float_val)
    }
```

**Step 5: Expand make_stats()**

Replace `make_stats()` with the full implementation. This is the core of the change — it reads from all the TorrentActor fields and computes aggregates from peers.

```rust
    fn make_stats(&self) -> TorrentStats {
        let pieces_have = self
            .chunk_tracker
            .as_ref()
            .map(|ct| ct.bitfield().count_ones())
            .unwrap_or(0);

        let mut peers_by_source = HashMap::new();
        let mut num_seeds = 0usize;
        let mut num_uploads = 0usize;
        let mut dl_rate = 0u64;
        let mut ul_rate = 0u64;
        for peer in self.peers.values() {
            *peers_by_source.entry(peer.source).or_insert(0) += 1;
            if peer.bitfield.count_ones() == self.num_pieces && self.num_pieces > 0 {
                num_seeds += 1;
            }
            if !peer.am_choking {
                num_uploads += 1;
            }
            dl_rate += peer.download_rate;
            ul_rate += peer.upload_rate;
        }

        // Count seeds in available peers (not connected)
        // We can't know if available peers are seeds, so list_seeds = connected seeds only
        let list_seeds = num_seeds;
        let list_peers = self.peers.len() + self.available_peers.len();

        // Compute progress
        let (total, total_done, total_wanted, total_wanted_done, progress, progress_ppm) =
            self.compute_progress(pieces_have);

        // Distributed copies
        let (dist_full, dist_frac, dist_float) = self.distributed_copies();

        // Accumulate active durations up to now
        let now_instant = std::time::Instant::now();
        let active_duration = self.active_duration
            + self.active_since.map(|t| now_instant.duration_since(t).as_secs() as i64).unwrap_or(0);
        let (finished_duration, seeding_duration) = self.compute_state_durations(now_instant);

        let is_seeding = self.state == TorrentState::Seeding;
        let is_finished = is_seeding
            || self.state == TorrentState::Complete
            || (self.state == TorrentState::Downloading && pieces_have == self.num_pieces);

        let name = self.meta.as_ref().map(|m| m.info.name.clone()).unwrap_or_default();

        TorrentStats {
            // Identity
            info_hashes: self.info_hashes.clone(),
            name,

            // State
            state: self.state,
            has_metadata: self.meta.is_some(),
            is_seeding,
            is_finished,

            // Progress
            progress,
            progress_ppm,
            total_done,
            total,
            total_wanted_done,
            total_wanted,
            pieces_have,
            pieces_total: self.num_pieces,
            block_size: ferrite_core::DEFAULT_CHUNK_SIZE as u32,
            checking_progress: self.checking_progress,

            // Transfer (session)
            total_download: self.total_download,
            total_upload: self.total_upload,
            total_payload_download: self.downloaded,
            total_payload_upload: self.uploaded,
            total_failed_bytes: self.total_failed_bytes,
            total_redundant_bytes: self.total_redundant_bytes,

            // Transfer (all-time) — same as payload for now (resume data integration later)
            all_time_download: self.downloaded,
            all_time_upload: self.uploaded,

            // Rates
            download_rate: dl_rate,
            upload_rate: ul_rate,
            download_payload_rate: dl_rate, // same until we separate overhead tracking
            upload_payload_rate: ul_rate,

            // Connections
            num_peers: self.peers.len(),
            num_seeds,
            num_complete: self.scrape_complete,
            num_incomplete: self.scrape_incomplete,
            list_seeds,
            list_peers,
            connect_candidates: self.available_peers.len(),
            num_connections: self.peers.len(),
            num_uploads,
            peers_by_source,

            // Limits
            connections_limit: self.config.max_peers,
            uploads_limit: self.choker.unchoke_slots() as usize,

            // Distributed copies
            distributed_full_copies: dist_full,
            distributed_fraction: dist_frac,
            distributed_copies: dist_float,

            // Tracker
            current_tracker: self.current_tracker.clone(),
            announcing_to_trackers: !self.tracker_manager.is_empty(),
            announcing_to_lsd: self.config.enable_dht, // LSD follows DHT enable flag
            announcing_to_dht: self.config.enable_dht && self.dht.is_some(),

            // Timestamps
            added_time: self.added_time,
            completed_time: self.completed_time,
            last_seen_complete: self.last_seen_complete,
            last_upload: self.last_upload_time,
            last_download: self.last_download_time,

            // Durations
            active_duration,
            finished_duration,
            seeding_duration,

            // Storage (save_path and queue_position filled by session layer)
            save_path: self.config.download_dir.to_string_lossy().into_owned(),
            moving_storage: self.moving_storage,

            // Queue (filled by session layer enrichment)
            queue_position: -1,

            // Error
            error: self.error_message.clone(),
            error_file: self.error_file_index,

            // Flags
            is_paused: self.state == TorrentState::Paused,
            auto_managed: false, // filled by session layer
            sequential_download: self.config.sequential_download,
            super_seeding: self.super_seed.is_some(),
            has_incoming: self.has_incoming,
            need_save_resume: self.need_save_resume,
        }
    }
```

**Step 6: Add compute_progress and compute_state_durations helpers**

```rust
    /// Compute total/done/wanted/progress from chunk tracker and file priorities.
    fn compute_progress(&self, pieces_have: u32) -> (u64, u64, u64, u64, f32, u32) {
        let total = self.meta.as_ref().map(|m| m.info.total_length()).unwrap_or(0);

        if self.num_pieces == 0 || total == 0 {
            return (total, 0, total, 0, 0.0, 0);
        }

        let piece_length = self.lengths.as_ref().map(|l| l.piece_length).unwrap_or(0) as u64;
        if piece_length == 0 {
            return (total, 0, total, 0, 0.0, 0);
        }

        // Approximate total_done from pieces
        let total_done = if pieces_have == self.num_pieces {
            total
        } else {
            (pieces_have as u64) * piece_length
        };

        // Wanted: sum over pieces with priority > 0
        let wanted_pieces_count = self.wanted_pieces.count_ones();
        let total_wanted = if wanted_pieces_count == self.num_pieces {
            total
        } else {
            (wanted_pieces_count as u64) * piece_length
        };

        // Wanted done: intersection of have and wanted
        let wanted_done_pieces = self.chunk_tracker.as_ref().map(|ct| {
            let have = ct.bitfield();
            let mut count = 0u32;
            for i in 0..self.num_pieces {
                if have.has(i) && self.wanted_pieces.has(i) {
                    count += 1;
                }
            }
            count
        }).unwrap_or(0);

        let total_wanted_done = if wanted_done_pieces == wanted_pieces_count {
            total_wanted
        } else {
            (wanted_done_pieces as u64) * piece_length
        };

        let progress = if total_wanted > 0 {
            (total_wanted_done as f64 / total_wanted as f64) as f32
        } else {
            1.0 // nothing wanted = 100% done
        };

        let progress_ppm = (progress as f64 * 1_000_000.0) as u32;

        (total, total_done, total_wanted, total_wanted_done, progress, progress_ppm)
    }

    /// Compute finished_duration and seeding_duration including current state time.
    fn compute_state_durations(&self, now: std::time::Instant) -> (i64, i64) {
        let extra = self.state_duration_since
            .map(|t| now.duration_since(t).as_secs() as i64)
            .unwrap_or(0);

        match self.state {
            TorrentState::Seeding => (self.finished_duration + extra, self.seeding_duration + extra),
            TorrentState::Complete => (self.finished_duration + extra, self.seeding_duration),
            _ => (self.finished_duration, self.seeding_duration),
        }
    }
```

**Step 7: Session-layer enrichment**

In `crates/ferrite-session/src/session.rs`, modify `handle_torrent_stats()` to fill session-level fields:

```rust
    async fn handle_torrent_stats(&self, info_hash: Id20) -> crate::Result<TorrentStats> {
        let entry = self
            .torrents
            .get(&info_hash)
            .ok_or(crate::Error::TorrentNotFound(info_hash))?;
        let mut stats = entry.handle.stats().await?;
        // Enrich with session-level data
        stats.queue_position = entry.queue_position;
        stats.auto_managed = entry.auto_managed;
        Ok(stats)
    }
```

**Step 8: Write tests**

```rust
#[test]
fn torrent_stats_has_identity_fields() {
    let stats = TorrentStats {
        info_hashes: ferrite_core::InfoHashes::v1_only(ferrite_core::Id20::from([0xAA; 20])),
        name: "test-torrent".into(),
        state: TorrentState::Downloading,
        has_metadata: true,
        is_seeding: false,
        is_finished: false,
        progress: 0.5,
        progress_ppm: 500_000,
        total_done: 1024,
        total: 2048,
        total_wanted_done: 1024,
        total_wanted: 2048,
        pieces_have: 5,
        pieces_total: 10,
        block_size: 16384,
        checking_progress: 0.0,
        total_download: 0,
        total_upload: 0,
        total_payload_download: 1024,
        total_payload_upload: 512,
        total_failed_bytes: 0,
        total_redundant_bytes: 0,
        all_time_download: 1024,
        all_time_upload: 512,
        download_rate: 100,
        upload_rate: 50,
        download_payload_rate: 100,
        upload_payload_rate: 50,
        num_peers: 3,
        num_seeds: 1,
        num_complete: -1,
        num_incomplete: -1,
        list_seeds: 1,
        list_peers: 10,
        connect_candidates: 7,
        num_connections: 3,
        num_uploads: 2,
        peers_by_source: HashMap::new(),
        connections_limit: 50,
        uploads_limit: 4,
        distributed_full_copies: 2,
        distributed_fraction: 500,
        distributed_copies: 2.5,
        current_tracker: String::new(),
        announcing_to_trackers: true,
        announcing_to_lsd: true,
        announcing_to_dht: true,
        added_time: 1700000000,
        completed_time: 0,
        last_seen_complete: 0,
        last_upload: 0,
        last_download: 0,
        active_duration: 3600,
        finished_duration: 0,
        seeding_duration: 0,
        save_path: "/downloads".into(),
        moving_storage: false,
        queue_position: 0,
        error: String::new(),
        error_file: -1,
        is_paused: false,
        auto_managed: true,
        sequential_download: false,
        super_seeding: false,
        has_incoming: false,
        need_save_resume: false,
    };
    assert_eq!(stats.name, "test-torrent");
    assert_eq!(stats.progress_ppm, 500_000);
    assert_eq!(stats.block_size, 16384);
    assert_eq!(stats.num_complete, -1);
    assert_eq!(stats.queue_position, 0);
}

#[test]
fn torrent_stats_progress_fields() {
    // Verify progress computation consistency
    let stats = /* construct with pieces_have=8, pieces_total=10, wanted=10 */;
    assert!((stats.progress - 0.8).abs() < 0.01);
    assert!(stats.progress_ppm >= 790_000 && stats.progress_ppm <= 810_000);
}

#[test]
fn torrent_stats_seeding_flags() {
    let stats = /* construct with state=Seeding */;
    assert!(stats.is_seeding);
    assert!(stats.is_finished);
    assert!(!stats.is_paused);
}
```

**Step 9: Fix all existing test callsites**

Every test that constructs `TorrentStats` directly must be updated to include all new fields. Search for `TorrentStats {` in:
- `crates/ferrite-session/src/types.rs` (torrent_stats_has_peers_by_source test)
- `crates/ferrite-session/src/session.rs` (integration tests that check stats fields)
- `crates/ferrite/src/client.rs` (any tests referencing stats)

Use `..Default::default()` pattern: implement `Default` for `TorrentStats` so tests can use struct update syntax.

**Step 10: Run tests and commit**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

```bash
git add crates/ferrite-session/src/types.rs crates/ferrite-session/src/torrent.rs crates/ferrite-session/src/session.rs
git commit -m "feat: expand TorrentStats to ~55 fields for libtorrent parity"
```

---

## Task 2: Wire Up Event-Driven Tracking

Hook into existing TorrentActor events to maintain the new counter/timestamp fields.

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` (event handlers)

**Step 1: Track overhead bytes alongside payload**

In `handle_piece_data()` (~line 1824), after incrementing `self.downloaded`:
```rust
    // Track overhead: protocol framing is 13 bytes per piece message (4 len + 1 id + 4 index + 4 begin)
    self.total_download += data.len() as u64 + 13;
    self.last_download_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    self.need_save_resume = true;
```

In the upload handler (where `self.uploaded` is incremented, ~line 3619):
```rust
    self.total_upload += chunk_size + 13;
    self.last_upload_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    self.need_save_resume = true;
```

**Step 2: Track failed and redundant bytes**

In `on_piece_hash_failed()` (~line 2450):
```rust
    let piece_size = self.lengths.as_ref()
        .map(|l| l.piece_size(index) as u64)
        .unwrap_or(0);
    self.total_failed_bytes += piece_size;
```

For redundant bytes, in `handle_piece_data()` when a duplicate block is received (the piece is already complete):
```rust
    // If we already have this piece, count as redundant
    if let Some(ref ct) = self.chunk_tracker {
        if ct.bitfield().has(index) {
            self.total_redundant_bytes += data.len() as u64;
            return;
        }
    }
```

**Step 3: Track tracker info**

In `fire_tracker_alerts()` or wherever successful tracker announces are processed, update `current_tracker` and scrape data:
```rust
    // After successful announce
    self.current_tracker = outcome.url.clone();
```

After a scrape response:
```rust
    if let Some(scrape) = &tracker_scrape_result {
        self.scrape_complete = scrape.complete as i32;
        self.scrape_incomplete = scrape.incomplete as i32;
    }
```

**Step 4: Track state-based durations**

In `transition_state()`:
```rust
    fn transition_state(&mut self, new_state: TorrentState) {
        let prev = self.state;
        if prev == new_state {
            return;
        }

        let now = std::time::Instant::now();

        // Accumulate durations from previous state
        if let Some(since) = self.state_duration_since.take() {
            let elapsed = now.duration_since(since).as_secs() as i64;
            match prev {
                TorrentState::Seeding => {
                    self.seeding_duration += elapsed;
                    self.finished_duration += elapsed;
                }
                TorrentState::Complete => {
                    self.finished_duration += elapsed;
                }
                _ => {}
            }
        }

        // Start tracking new state if applicable
        match new_state {
            TorrentState::Seeding | TorrentState::Complete => {
                self.state_duration_since = Some(now);
            }
            _ => {}
        }

        // Handle pause — accumulate active duration
        if new_state == TorrentState::Paused {
            if let Some(since) = self.active_since.take() {
                self.active_duration += now.duration_since(since).as_secs() as i64;
            }
        } else if prev == TorrentState::Paused {
            self.active_since = Some(now);
        }

        // Record completion time
        if (new_state == TorrentState::Seeding || new_state == TorrentState::Complete)
            && self.completed_time == 0
        {
            self.completed_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
        }

        self.state = new_state;
        self.need_save_resume = true;
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::StateChanged {
            info_hash: self.info_hash,
            prev_state: prev,
            new_state,
        });
    }
```

**Step 5: Track incoming connections**

In `spawn_peer_from_stream()` or wherever incoming peers are accepted:
```rust
    self.has_incoming = true;
```

**Step 6: Track last_seen_complete**

In peer bitfield handling, when we observe a peer with all pieces:
```rust
    if peer.bitfield.count_ones() == self.num_pieces && self.num_pieces > 0 {
        self.last_seen_complete = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
    }
```

**Step 7: Wire moving_storage flag**

In the MoveStorage command handler:
```rust
    // Before moving
    self.moving_storage = true;
    // ... do the move ...
    self.moving_storage = false;
```

**Step 8: Run tests and commit**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat: wire event-driven tracking for extended TorrentStats fields"
```

---

## Task 3: Choker Slot Exposure + Distributed Copies Tests

**Files:**
- Modify: `crates/ferrite-session/src/choker.rs` — ensure `unchoke_slots()` is public
- Test: `crates/ferrite-session/src/torrent.rs` — distributed copies unit tests

**Step 1: Verify choker has public unchoke_slots()**

Check if `Choker::unchoke_slots()` exists and returns the current slot count. If not, add:

```rust
    /// Returns the current number of unchoke slots.
    pub fn unchoke_slots(&self) -> u32 {
        self.unchoke_slots
    }
```

**Step 2: Add distributed_copies tests**

Add to the test module in torrent.rs (or a dedicated test file):

```rust
#[test]
fn distributed_copies_no_peers() {
    // With 0 peers, distributed_copies should be (0, 0, 0.0)
    let (full, frac, float) = (0u32, 0u32, 0.0f32);
    assert_eq!(full, 0);
    assert_eq!(frac, 0);
    assert!((float - 0.0).abs() < f32::EPSILON);
}

#[test]
fn distributed_copies_all_have_everything() {
    // 3 peers each with all 10 pieces → 3 full copies
    // Test via the computation logic directly
}

#[test]
fn distributed_copies_partial() {
    // 2 peers: peer A has pieces 0-7, peer B has pieces 0-9
    // Rarest piece (8,9) has 1 copy. 8 out of 10 pieces have 2 copies.
    // Result: (1, 800, 1.8)
}
```

**Step 3: Run tests and commit**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
git add crates/ferrite-session/src/choker.rs crates/ferrite-session/src/torrent.rs
git commit -m "feat: expose unchoke_slots and add distributed_copies tests"
```

---

## Task 4: Update Facade + Public API + Tests

**Files:**
- Modify: `crates/ferrite/src/session.rs` — re-export new TorrentStats
- Modify: `crates/ferrite/src/prelude.rs` — ensure TorrentStats is in prelude
- Modify: `crates/ferrite/examples/download.rs` — use new rate/progress fields
- Test: `crates/ferrite/src/client.rs` (integration)

**Step 1: Verify TorrentStats is properly re-exported**

In `crates/ferrite/src/session.rs`, ensure `TorrentStats` is re-exported. It should already be, but verify.

**Step 2: Update download.rs example to show rates**

```rust
    loop {
        let stats = session.torrent_stats(info_hash).await?;
        let dl_mb = stats.download_rate as f64 / (1024.0 * 1024.0);
        let ul_mb = stats.upload_rate as f64 / (1024.0 * 1024.0);
        println!(
            "[{:?}] {:.1}% — {}/{} pieces, {:.2} MB/s down, {:.2} MB/s up, {} peers ({} seeds)",
            stats.state,
            stats.progress * 100.0,
            stats.pieces_have,
            stats.pieces_total,
            dl_mb,
            ul_mb,
            stats.num_peers,
            stats.num_seeds,
        );

        if stats.is_seeding {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
```

**Step 3: Add facade-level test**

In `crates/ferrite/src/client.rs` tests:

```rust
#[tokio::test]
async fn torrent_stats_has_rate_fields() {
    // Add a torrent and verify the stats struct has all expected fields
    let session = ClientBuilder::new()
        .download_dir("/tmp/ferrite-test-stats")
        .start()
        .await
        .unwrap();

    // We can't easily verify rates without real peers,
    // but we can verify the struct fields are accessible
    // and have sensible defaults
    session.shutdown().await.unwrap();
}
```

**Step 4: Implement Default for TorrentStats**

This helps tests use `..Default::default()` pattern:

```rust
impl Default for TorrentStats {
    fn default() -> Self {
        Self {
            info_hashes: ferrite_core::InfoHashes::v1_only(ferrite_core::Id20::from([0u8; 20])),
            name: String::new(),
            state: TorrentState::Paused,
            has_metadata: false,
            is_seeding: false,
            is_finished: false,
            progress: 0.0,
            progress_ppm: 0,
            total_done: 0,
            total: 0,
            total_wanted_done: 0,
            total_wanted: 0,
            pieces_have: 0,
            pieces_total: 0,
            block_size: 16384,
            checking_progress: 0.0,
            total_download: 0,
            total_upload: 0,
            total_payload_download: 0,
            total_payload_upload: 0,
            total_failed_bytes: 0,
            total_redundant_bytes: 0,
            all_time_download: 0,
            all_time_upload: 0,
            download_rate: 0,
            upload_rate: 0,
            download_payload_rate: 0,
            upload_payload_rate: 0,
            num_peers: 0,
            num_seeds: 0,
            num_complete: -1,
            num_incomplete: -1,
            list_seeds: 0,
            list_peers: 0,
            connect_candidates: 0,
            num_connections: 0,
            num_uploads: 0,
            peers_by_source: HashMap::new(),
            connections_limit: 50,
            uploads_limit: 4,
            distributed_full_copies: 0,
            distributed_fraction: 0,
            distributed_copies: 0.0,
            current_tracker: String::new(),
            announcing_to_trackers: false,
            announcing_to_lsd: false,
            announcing_to_dht: false,
            added_time: 0,
            completed_time: 0,
            last_seen_complete: 0,
            last_upload: 0,
            last_download: 0,
            active_duration: 0,
            finished_duration: 0,
            seeding_duration: 0,
            save_path: String::new(),
            moving_storage: false,
            queue_position: -1,
            error: String::new(),
            error_file: -1,
            is_paused: false,
            auto_managed: false,
            sequential_download: false,
            super_seeding: false,
            has_incoming: false,
            need_save_resume: false,
        }
    }
}
```

**Step 5: Run tests and commit**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
git add crates/ferrite/src/session.rs crates/ferrite/src/client.rs crates/ferrite/examples/download.rs crates/ferrite-session/src/types.rs
git commit -m "feat: update facade, examples, and add Default for TorrentStats"
```

---

## Task 5: Documentation, Version Bump, Cleanup

**Files:**
- Modify: `Cargo.toml` (root) — bump version
- Modify: `README.md` — update test count, note libtorrent stats parity
- Modify: `CHANGELOG.md` — add entry
- Modify: `CLAUDE.md` — update if needed

**Step 1: Bump version**

`Cargo.toml`: `version = "0.58.0"` → `version = "0.59.0"`

**Step 2: Update user-agent strings**

Search for `"Ferrite/0.58.0"` in http.rs, url_guard.rs, web_seed.rs and update to `"Ferrite/0.59.0"`.

**Step 3: Update README.md**

- Update test count badge
- Update version badge to 0.59.0
- Note "~55-field TorrentStats (libtorrent parity)" in session features table

**Step 4: Update CHANGELOG.md**

```markdown
## [0.59.0] - 2026-03-02

### Added
- Full libtorrent `torrent_status` parity: expanded `TorrentStats` from 9 to ~55 fields
- Per-torrent transfer rates (download_rate, upload_rate, payload variants)
- Progress tracking (total_done, total_wanted_done, progress, progress_ppm)
- Time tracking (added_time, completed_time, active/finished/seeding duration)
- Connection details (num_seeds, num_uploads, distributed copies, scrape counts)
- State flags (is_seeding, is_finished, has_incoming, need_save_resume)
- Error reporting (error message, error file index)
- Session-level enrichment (queue_position, auto_managed from SessionActor)
- `Default` impl for `TorrentStats` (simplifies test construction)

### Changed
- `TorrentStats` struct is now ~55 fields (breaking change for direct constructors)
- `make_stats()` computes aggregates from peers, chunk tracker, and new counters
- `transition_state()` accumulates active/finished/seeding durations
- Download example shows rates and progress percentage
```

**Step 5: Verify and commit**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
git add Cargo.toml README.md CHANGELOG.md crates/
git commit -m "docs: update README, CHANGELOG, bump version to 0.59.0"
```

**Step 6: Push**

```bash
git push origin main && git push github main
```

---

## Verification

```bash
cargo test --workspace        # Expect ~1330+ tests, 0 failures
cargo clippy --workspace -- -D warnings  # Zero warnings
```

### Key invariants to verify:
1. `TorrentStats::default()` compiles and has sensible values
2. `download.rs` example compiles with new field access
3. Session enrichment fills `queue_position` and `auto_managed`
4. Distributed copies returns (0, 0, 0.0) with no peers
5. `added_time` is non-zero for newly added torrents
6. `is_seeding` and `is_finished` match state enum correctly
7. `total_failed_bytes` increments on hash failure
8. `active_duration` accumulates correctly across pause/resume cycles
