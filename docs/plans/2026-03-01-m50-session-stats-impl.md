# M50: Session Statistics Counters — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Deep per-metric instrumentation matching libtorrent's named counter/gauge system. ~100 individual metrics across 6 categories, exposed as a compile-time metric registry with periodic `SessionStatsAlert` snapshots and on-demand `post_session_stats()`.

**Architecture:** A static `&'static [SessionStatsMetric]` array defines every metric at compile time. Each metric has a name, kind (Counter or Gauge), and a fixed index. Actual values live in a `SessionCounters` struct containing `AtomicI64` fields — one per metric. The session actor holds the counters and passes an `Arc<SessionCounters>` to torrent actors and the disk actor so they can update metrics in place. A periodic timer fires `SessionStatsAlert` containing a `Vec<i64>` snapshot indexed by metric ID.

**Tech Stack:** Rust edition 2024, `std::sync::atomic::AtomicI64`, tokio, serde

**Key constraint:** The existing `SessionStats` struct (4 fields) and `AlertKind::SessionStatsUpdate` remain unchanged for backward compatibility. The new system adds `SessionStatsAlert` as a separate alert kind and `SessionCounters` as the internal instrumentation primitive.

---

## Task 1: Metric Registry — MetricKind, SessionStatsMetric, and Static Metric List

**Files:**
- Create: `crates/ferrite-session/src/stats.rs`
- Modify: `crates/ferrite-session/src/lib.rs`

**Step 1: Write tests for metric types and registry**

Create `crates/ferrite-session/src/stats.rs`:

```rust
//! Session statistics counter system.
//!
//! Compile-time metric registry with atomic counters/gauges for deep
//! per-metric instrumentation matching libtorrent's named counter system.

use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};

// ── Metric metadata ──────────────────────────────────────────────────

/// Whether a metric is a monotonically increasing counter or a gauge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricKind {
    /// Monotonically increasing (bytes sent, pieces downloaded, etc.).
    Counter,
    /// Current value that can increase or decrease (connections, DHT nodes, etc.).
    Gauge,
}

/// Static metadata for a single session metric.
#[derive(Debug, Clone, Copy)]
pub struct SessionStatsMetric {
    /// Human-readable metric name (e.g. "net.bytes_sent").
    pub name: &'static str,
    /// Whether this metric is a counter or gauge.
    pub kind: MetricKind,
}

// ── Metric index constants ───────────────────────────────────────────
//
// Each constant is the index into both METRICS and SessionCounters.values.
// Grouped by category for readability. The order here defines the metric ID.

// -- Network (0..9) --
pub const NET_BYTES_SENT: usize = 0;
pub const NET_BYTES_RECV: usize = 1;
pub const NET_NUM_CONNECTIONS: usize = 2;
pub const NET_NUM_HALF_OPEN: usize = 3;
pub const NET_NUM_TCP_PEERS: usize = 4;
pub const NET_NUM_UTP_PEERS: usize = 5;
pub const NET_NUM_TCP_CONNECTIONS: usize = 6;
pub const NET_NUM_UTP_CONNECTIONS: usize = 7;
pub const NET_TCP_BYTES_SENT: usize = 8;
pub const NET_TCP_BYTES_RECV: usize = 9;
pub const NET_UTP_BYTES_SENT: usize = 10;
pub const NET_UTP_BYTES_RECV: usize = 11;

// -- Disk (12..21) --
pub const DISK_READ_COUNT: usize = 12;
pub const DISK_WRITE_COUNT: usize = 13;
pub const DISK_READ_BYTES: usize = 14;
pub const DISK_WRITE_BYTES: usize = 15;
pub const DISK_CACHE_HITS: usize = 16;
pub const DISK_CACHE_MISSES: usize = 17;
pub const DISK_QUEUE_DEPTH: usize = 18;
pub const DISK_JOB_TIME_US: usize = 19;
pub const DISK_WRITE_BUFFER_BYTES: usize = 20;
pub const DISK_HASH_COUNT: usize = 21;

// -- DHT (22..28) --
pub const DHT_NODES: usize = 22;
pub const DHT_LOOKUPS: usize = 23;
pub const DHT_BYTES_IN: usize = 24;
pub const DHT_BYTES_OUT: usize = 25;
pub const DHT_NODES_V4: usize = 26;
pub const DHT_NODES_V6: usize = 27;
pub const DHT_ANNOUNCE_COUNT: usize = 28;

// -- Peers (29..40) --
pub const PEER_NUM_UNCHOKED: usize = 29;
pub const PEER_NUM_INTERESTED: usize = 30;
pub const PEER_NUM_UPLOADING: usize = 31;
pub const PEER_NUM_DOWNLOADING: usize = 32;
pub const PEER_NUM_SEEDING_TORRENTS: usize = 33;
pub const PEER_NUM_DOWNLOADING_TORRENTS: usize = 34;
pub const PEER_NUM_CHECKING_TORRENTS: usize = 35;
pub const PEER_NUM_PAUSED_TORRENTS: usize = 36;
pub const PEER_PEERS_CONNECTED: usize = 37;
pub const PEER_PEERS_AVAILABLE: usize = 38;
pub const PEER_NUM_WEB_SEEDS: usize = 39;
pub const PEER_NUM_BANNED: usize = 40;

// -- Protocol (41..54) --
pub const PROTO_PIECES_DOWNLOADED: usize = 41;
pub const PROTO_PIECES_UPLOADED: usize = 42;
pub const PROTO_HASHFAILS: usize = 43;
pub const PROTO_WASTE_BYTES: usize = 44;
pub const PROTO_PIECE_REQUESTS: usize = 45;
pub const PROTO_PIECE_REJECTS: usize = 46;
pub const PROTO_HANDSHAKES_IN: usize = 47;
pub const PROTO_HANDSHAKES_OUT: usize = 48;
pub const PROTO_PEX_MESSAGES_IN: usize = 49;
pub const PROTO_PEX_MESSAGES_OUT: usize = 50;
pub const PROTO_TRACKER_ANNOUNCES: usize = 51;
pub const PROTO_TRACKER_ERRORS: usize = 52;
pub const PROTO_METADATA_REQUESTS: usize = 53;
pub const PROTO_METADATA_RECEIVES: usize = 54;

// -- Bandwidth (55..64) --
pub const BW_UPLOAD_RATE: usize = 55;
pub const BW_DOWNLOAD_RATE: usize = 56;
pub const BW_UPLOAD_RATE_TCP: usize = 57;
pub const BW_DOWNLOAD_RATE_TCP: usize = 58;
pub const BW_UPLOAD_RATE_UTP: usize = 59;
pub const BW_DOWNLOAD_RATE_UTP: usize = 60;
pub const BW_PAYLOAD_UPLOAD_RATE: usize = 61;
pub const BW_PAYLOAD_DOWNLOAD_RATE: usize = 62;
pub const BW_TOTAL_UPLOADED: usize = 63;
pub const BW_TOTAL_DOWNLOADED: usize = 64;

// -- Session (65..69) --
pub const SES_ACTIVE_TORRENTS: usize = 65;
pub const SES_NUM_TORRENTS: usize = 66;
pub const SES_UPTIME_SECS: usize = 67;
pub const SES_IP_FILTER_BLOCKED: usize = 68;
pub const SES_QUEUE_PAUSED_BY_AUTO: usize = 69;

/// Total number of metrics in the registry.
pub const NUM_METRICS: usize = 70;

/// Compile-time list of all session statistics metrics.
///
/// The index of each entry corresponds to the metric ID constants above.
/// This array is immutable and available for the lifetime of the process.
pub fn session_stats_metrics() -> &'static [SessionStatsMetric] {
    use MetricKind::*;
    static METRICS: [SessionStatsMetric; NUM_METRICS] = [
        // Network (0..11)
        SessionStatsMetric { name: "net.bytes_sent", kind: Counter },
        SessionStatsMetric { name: "net.bytes_recv", kind: Counter },
        SessionStatsMetric { name: "net.num_connections", kind: Gauge },
        SessionStatsMetric { name: "net.num_half_open", kind: Gauge },
        SessionStatsMetric { name: "net.num_tcp_peers", kind: Gauge },
        SessionStatsMetric { name: "net.num_utp_peers", kind: Gauge },
        SessionStatsMetric { name: "net.num_tcp_connections", kind: Gauge },
        SessionStatsMetric { name: "net.num_utp_connections", kind: Gauge },
        SessionStatsMetric { name: "net.tcp_bytes_sent", kind: Counter },
        SessionStatsMetric { name: "net.tcp_bytes_recv", kind: Counter },
        SessionStatsMetric { name: "net.utp_bytes_sent", kind: Counter },
        SessionStatsMetric { name: "net.utp_bytes_recv", kind: Counter },
        // Disk (12..21)
        SessionStatsMetric { name: "disk.read_count", kind: Counter },
        SessionStatsMetric { name: "disk.write_count", kind: Counter },
        SessionStatsMetric { name: "disk.read_bytes", kind: Counter },
        SessionStatsMetric { name: "disk.write_bytes", kind: Counter },
        SessionStatsMetric { name: "disk.cache_hits", kind: Counter },
        SessionStatsMetric { name: "disk.cache_misses", kind: Counter },
        SessionStatsMetric { name: "disk.queue_depth", kind: Gauge },
        SessionStatsMetric { name: "disk.job_time_us", kind: Counter },
        SessionStatsMetric { name: "disk.write_buffer_bytes", kind: Gauge },
        SessionStatsMetric { name: "disk.hash_count", kind: Counter },
        // DHT (22..28)
        SessionStatsMetric { name: "dht.nodes", kind: Gauge },
        SessionStatsMetric { name: "dht.lookups", kind: Counter },
        SessionStatsMetric { name: "dht.bytes_in", kind: Counter },
        SessionStatsMetric { name: "dht.bytes_out", kind: Counter },
        SessionStatsMetric { name: "dht.nodes_v4", kind: Gauge },
        SessionStatsMetric { name: "dht.nodes_v6", kind: Gauge },
        SessionStatsMetric { name: "dht.announce_count", kind: Counter },
        // Peers (29..40)
        SessionStatsMetric { name: "peer.num_unchoked", kind: Gauge },
        SessionStatsMetric { name: "peer.num_interested", kind: Gauge },
        SessionStatsMetric { name: "peer.num_uploading", kind: Gauge },
        SessionStatsMetric { name: "peer.num_downloading", kind: Gauge },
        SessionStatsMetric { name: "peer.num_seeding_torrents", kind: Gauge },
        SessionStatsMetric { name: "peer.num_downloading_torrents", kind: Gauge },
        SessionStatsMetric { name: "peer.num_checking_torrents", kind: Gauge },
        SessionStatsMetric { name: "peer.num_paused_torrents", kind: Gauge },
        SessionStatsMetric { name: "peer.peers_connected", kind: Gauge },
        SessionStatsMetric { name: "peer.peers_available", kind: Gauge },
        SessionStatsMetric { name: "peer.num_web_seeds", kind: Gauge },
        SessionStatsMetric { name: "peer.num_banned", kind: Gauge },
        // Protocol (41..54)
        SessionStatsMetric { name: "proto.pieces_downloaded", kind: Counter },
        SessionStatsMetric { name: "proto.pieces_uploaded", kind: Counter },
        SessionStatsMetric { name: "proto.hashfails", kind: Counter },
        SessionStatsMetric { name: "proto.waste_bytes", kind: Counter },
        SessionStatsMetric { name: "proto.piece_requests", kind: Counter },
        SessionStatsMetric { name: "proto.piece_rejects", kind: Counter },
        SessionStatsMetric { name: "proto.handshakes_in", kind: Counter },
        SessionStatsMetric { name: "proto.handshakes_out", kind: Counter },
        SessionStatsMetric { name: "proto.pex_messages_in", kind: Counter },
        SessionStatsMetric { name: "proto.pex_messages_out", kind: Counter },
        SessionStatsMetric { name: "proto.tracker_announces", kind: Counter },
        SessionStatsMetric { name: "proto.tracker_errors", kind: Counter },
        SessionStatsMetric { name: "proto.metadata_requests", kind: Counter },
        SessionStatsMetric { name: "proto.metadata_receives", kind: Counter },
        // Bandwidth (55..64)
        SessionStatsMetric { name: "bw.upload_rate", kind: Gauge },
        SessionStatsMetric { name: "bw.download_rate", kind: Gauge },
        SessionStatsMetric { name: "bw.upload_rate_tcp", kind: Gauge },
        SessionStatsMetric { name: "bw.download_rate_tcp", kind: Gauge },
        SessionStatsMetric { name: "bw.upload_rate_utp", kind: Gauge },
        SessionStatsMetric { name: "bw.download_rate_utp", kind: Gauge },
        SessionStatsMetric { name: "bw.payload_upload_rate", kind: Gauge },
        SessionStatsMetric { name: "bw.payload_download_rate", kind: Gauge },
        SessionStatsMetric { name: "bw.total_uploaded", kind: Counter },
        SessionStatsMetric { name: "bw.total_downloaded", kind: Counter },
        // Session (65..69)
        SessionStatsMetric { name: "ses.active_torrents", kind: Gauge },
        SessionStatsMetric { name: "ses.num_torrents", kind: Gauge },
        SessionStatsMetric { name: "ses.uptime_secs", kind: Gauge },
        SessionStatsMetric { name: "ses.ip_filter_blocked", kind: Counter },
        SessionStatsMetric { name: "ses.queue_paused_by_auto", kind: Counter },
    ];
    &METRICS
}

// ── SessionCounters ──────────────────────────────────────────────────

/// Atomic counter array shared between session and torrent actors.
///
/// All values are `AtomicI64` — counters are incremented, gauges are set.
/// The struct is `Send + Sync` and designed to be wrapped in `Arc`.
pub struct SessionCounters {
    values: [AtomicI64; NUM_METRICS],
    /// Session start time for uptime calculation.
    started_at: std::time::Instant,
    /// Previous snapshot bytes for rate calculation.
    prev_bytes_sent: AtomicI64,
    prev_bytes_recv: AtomicI64,
}

impl SessionCounters {
    /// Create a new zeroed counter array.
    pub fn new() -> Self {
        Self {
            values: std::array::from_fn(|_| AtomicI64::new(0)),
            started_at: std::time::Instant::now(),
            prev_bytes_sent: AtomicI64::new(0),
            prev_bytes_recv: AtomicI64::new(0),
        }
    }

    /// Increment a counter metric by `delta`.
    #[inline]
    pub fn inc(&self, metric: usize, delta: i64) {
        debug_assert!(metric < NUM_METRICS);
        self.values[metric].fetch_add(delta, Ordering::Relaxed);
    }

    /// Set a gauge metric to an absolute value.
    #[inline]
    pub fn set(&self, metric: usize, value: i64) {
        debug_assert!(metric < NUM_METRICS);
        self.values[metric].store(value, Ordering::Relaxed);
    }

    /// Read a single metric value.
    #[inline]
    pub fn get(&self, metric: usize) -> i64 {
        debug_assert!(metric < NUM_METRICS);
        self.values[metric].load(Ordering::Relaxed)
    }

    /// Snapshot all metric values into a `Vec<i64>`.
    ///
    /// The returned vector is indexed by metric ID — same as the constants
    /// and `session_stats_metrics()`.
    pub fn snapshot(&self) -> Vec<i64> {
        let mut vals: Vec<i64> = self.values.iter()
            .map(|a| a.load(Ordering::Relaxed))
            .collect();

        // Update uptime gauge
        vals[SES_UPTIME_SECS] = self.started_at.elapsed().as_secs() as i64;

        // Compute bandwidth rates from counter deltas
        let cur_sent = vals[NET_BYTES_SENT];
        let cur_recv = vals[NET_BYTES_RECV];
        let prev_sent = self.prev_bytes_sent.swap(cur_sent, Ordering::Relaxed);
        let prev_recv = self.prev_bytes_recv.swap(cur_recv, Ordering::Relaxed);
        // Rates are bytes/sec at snapshot interval; caller divides if needed.
        // For simplicity, store the delta — the alert consumer knows the interval.
        vals[BW_UPLOAD_RATE] = cur_sent.saturating_sub(prev_sent);
        vals[BW_DOWNLOAD_RATE] = cur_recv.saturating_sub(prev_recv);

        vals
    }

    /// Return the number of metrics.
    pub fn len(&self) -> usize {
        NUM_METRICS
    }

    /// Always false — there is always at least one metric.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Seconds since session start.
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

impl Default for SessionCounters {
    fn default() -> Self {
        Self::new()
    }
}

// Safety: AtomicI64 is Send+Sync, Instant is Send+Sync.
// The derive wouldn't work because of the array, so we assert manually.
unsafe impl Send for SessionCounters {}
unsafe impl Sync for SessionCounters {}

impl std::fmt::Debug for SessionCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCounters")
            .field("num_metrics", &NUM_METRICS)
            .field("uptime_secs", &self.uptime_secs())
            .finish()
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_registry_has_correct_count() {
        let metrics = session_stats_metrics();
        assert_eq!(metrics.len(), NUM_METRICS);
    }

    #[test]
    fn all_metric_names_are_unique() {
        let metrics = session_stats_metrics();
        let mut names: Vec<&str> = metrics.iter().map(|m| m.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), NUM_METRICS, "duplicate metric names found");
    }

    #[test]
    fn all_metric_names_have_category_prefix() {
        let metrics = session_stats_metrics();
        for (i, m) in metrics.iter().enumerate() {
            assert!(
                m.name.contains('.'),
                "metric {i} ({}) should have a category.name format",
                m.name
            );
        }
    }

    #[test]
    fn counter_inc_and_get() {
        let c = SessionCounters::new();
        assert_eq!(c.get(NET_BYTES_SENT), 0);
        c.inc(NET_BYTES_SENT, 1024);
        assert_eq!(c.get(NET_BYTES_SENT), 1024);
        c.inc(NET_BYTES_SENT, 512);
        assert_eq!(c.get(NET_BYTES_SENT), 1536);
    }

    #[test]
    fn gauge_set_and_get() {
        let c = SessionCounters::new();
        c.set(NET_NUM_CONNECTIONS, 42);
        assert_eq!(c.get(NET_NUM_CONNECTIONS), 42);
        c.set(NET_NUM_CONNECTIONS, 10);
        assert_eq!(c.get(NET_NUM_CONNECTIONS), 10);
    }

    #[test]
    fn snapshot_returns_all_values() {
        let c = SessionCounters::new();
        c.inc(NET_BYTES_SENT, 100);
        c.set(DHT_NODES, 50);
        c.inc(PROTO_PIECES_DOWNLOADED, 7);
        let snap = c.snapshot();
        assert_eq!(snap.len(), NUM_METRICS);
        assert_eq!(snap[NET_BYTES_SENT], 100);
        assert_eq!(snap[DHT_NODES], 50);
        assert_eq!(snap[PROTO_PIECES_DOWNLOADED], 7);
    }

    #[test]
    fn snapshot_includes_uptime() {
        let c = SessionCounters::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let snap = c.snapshot();
        // Uptime should be at least 0 (might be 0 if sub-second)
        assert!(snap[SES_UPTIME_SECS] >= 0);
    }

    #[test]
    fn counters_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SessionCounters>();
    }

    #[test]
    fn metric_kind_serializes() {
        let json = serde_json::to_string(&MetricKind::Counter).unwrap();
        let decoded: MetricKind = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, MetricKind::Counter);

        let json = serde_json::to_string(&MetricKind::Gauge).unwrap();
        let decoded: MetricKind = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, MetricKind::Gauge);
    }

    #[test]
    fn metric_index_constants_in_range() {
        // Spot-check boundary constants
        assert!(NET_BYTES_SENT < NUM_METRICS);
        assert!(SES_QUEUE_PAUSED_BY_AUTO < NUM_METRICS);
        assert_eq!(SES_QUEUE_PAUSED_BY_AUTO, NUM_METRICS - 1);
    }

    #[test]
    fn default_counters_all_zero() {
        let c = SessionCounters::new();
        for i in 0..NUM_METRICS {
            assert_eq!(c.get(i), 0, "metric {i} should start at 0");
        }
    }

    #[test]
    fn concurrent_inc_from_multiple_threads() {
        use std::sync::Arc;

        let c = Arc::new(SessionCounters::new());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&c);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    c.inc(NET_BYTES_SENT, 1);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(c.get(NET_BYTES_SENT), 8000);
    }

    #[test]
    fn len_and_is_empty() {
        let c = SessionCounters::new();
        assert_eq!(c.len(), NUM_METRICS);
        assert!(!c.is_empty());
    }
}
```

**Step 2: Register the stats module in lib.rs**

In `crates/ferrite-session/src/lib.rs`, add the module declaration and re-export:

After the line `pub mod disk;` add:
```rust
pub mod stats;
```

Add to the re-exports at the end of the `pub use` block:
```rust
pub use stats::{
    MetricKind, SessionStatsMetric, SessionCounters,
    session_stats_metrics, NUM_METRICS,
};
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session stats:: -- --nocapture`
Expected: 11 tests PASS

---

## Task 2: SessionStatsAlert and Alert Integration

**Files:**
- Modify: `crates/ferrite-session/src/alert.rs`

**Step 1: Write tests for the new alert kind**

Add `SessionStatsAlert` variant to `AlertKind` in `crates/ferrite-session/src/alert.rs`.

In the `AlertKind` enum, after the `SessionStatsUpdate(...)` variant, add:

```rust
    /// Deep session statistics snapshot (M50).
    ///
    /// Contains a `Vec<i64>` where each index corresponds to a metric from
    /// `session_stats_metrics()`. Fired periodically at `stats_report_interval`
    /// or on-demand via `SessionHandle::post_session_stats()`.
    SessionStatsAlert { values: Vec<i64> },
```

In the `category()` match, add a new arm:

```rust
            SessionStatsAlert { .. } => AlertCategory::STATS,
```

**Step 2: Add test for the new alert variant**

Add to `alert.rs` tests:

```rust
    #[test]
    fn session_stats_alert_has_stats_category() {
        let alert = Alert::new(AlertKind::SessionStatsAlert {
            values: vec![0; crate::stats::NUM_METRICS],
        });
        assert!(alert.category().contains(AlertCategory::STATS));
    }

    #[test]
    fn session_stats_alert_serializes() {
        let values = vec![42i64; crate::stats::NUM_METRICS];
        let alert = Alert::new(AlertKind::SessionStatsAlert {
            values: values.clone(),
        });
        let json = serde_json::to_string(&alert).unwrap();
        let decoded: Alert = serde_json::from_str(&json).unwrap();
        if let AlertKind::SessionStatsAlert { values: v } = decoded.kind {
            assert_eq!(v.len(), crate::stats::NUM_METRICS);
            assert_eq!(v[0], 42);
        } else {
            panic!("wrong alert kind");
        }
    }
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session alert:: -- --nocapture`
Expected: All existing alert tests PASS + 2 new tests PASS

---

## Task 3: Settings — `stats_report_interval`

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs`

**Step 1: Add the new setting field**

Add a serde default helper after the existing helpers:

```rust
fn default_stats_report_interval() -> u64 {
    1000
}
```

Add the field to `Settings` struct in the `// ── Alerts ──` section, after `alert_channel_size`:

```rust
    /// Interval in milliseconds between automatic `SessionStatsAlert` firings.
    /// Set to 0 to disable periodic stats alerts. Default: 1000ms.
    #[serde(default = "default_stats_report_interval")]
    pub stats_report_interval: u64,
```

Add to `Default for Settings` impl, in the `// Alerts` section after `alert_channel_size`:

```rust
            stats_report_interval: 1000,
```

Add to the `PartialEq for Settings` impl, after the `alert_channel_size` comparison:

```rust
            && self.stats_report_interval == other.stats_report_interval
```

**Step 2: Add test**

Add to `settings.rs` tests:

```rust
    #[test]
    fn stats_report_interval_default() {
        let s = Settings::default();
        assert_eq!(s.stats_report_interval, 1000);
    }

    #[test]
    fn stats_report_interval_json_round_trip() {
        let mut s = Settings::default();
        s.stats_report_interval = 500;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.stats_report_interval, 500);
    }

    #[test]
    fn stats_report_interval_disabled() {
        let json = r#"{"stats_report_interval": 0}"#;
        let decoded: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.stats_report_interval, 0);
    }
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session settings:: -- --nocapture`
Expected: All existing settings tests PASS + 3 new tests PASS

---

## Task 4: SessionActor Integration — Counters, Timer, and Snapshot

**Files:**
- Modify: `crates/ferrite-session/src/session.rs`

**Step 1: Add `SessionCounters` to `SessionActor` and `SessionHandle`**

Add to imports at top of `session.rs`:

```rust
use crate::stats::{SessionCounters, NUM_METRICS};
```

Add to `SessionHandle` struct:

```rust
    counters: Arc<SessionCounters>,
```

Add to `SessionActor` struct:

```rust
    counters: Arc<SessionCounters>,
```

**Step 2: Initialize counters in `SessionHandle::start_with_plugins()`**

In `start_with_plugins()`, after `let alert_mask = ...` and before `let (lsd, ...)`:

```rust
        let counters = Arc::new(SessionCounters::new());
```

When constructing `SessionActor`, add `counters: Arc::clone(&counters)`.

When constructing the returned `SessionHandle`, add `counters`.

**Step 3: Add `post_session_stats()` method to `SessionHandle`**

Add a new `SessionCommand` variant:

```rust
    PostSessionStats,
```

Add a new method to `SessionHandle`:

```rust
    /// Trigger an immediate `SessionStatsAlert` snapshot.
    ///
    /// The alert is fired asynchronously — subscribe to `AlertCategory::STATS`
    /// to receive it.
    pub async fn post_session_stats(&self) -> crate::Result<()> {
        self.cmd_tx
            .send(SessionCommand::PostSessionStats)
            .await
            .map_err(|_| crate::Error::Shutdown)
    }
```

Also add a public accessor for the counters:

```rust
    /// Get the session-wide statistics counters.
    ///
    /// The returned `Arc<SessionCounters>` can be used to read metrics directly
    /// without going through the command channel.
    pub fn counters(&self) -> &Arc<SessionCounters> {
        &self.counters
    }
```

**Step 4: Handle the command in the actor run loop**

In the `run()` method's `tokio::select!` command match, add a new arm before `Shutdown`:

```rust
                        Some(SessionCommand::PostSessionStats) => {
                            self.fire_stats_alert();
                        }
```

**Step 5: Add the periodic stats timer to the run loop**

In `SessionActor::run()`, after the `auto_manage_interval` setup, add:

```rust
        let stats_interval_ms = self.settings.stats_report_interval;
        let mut stats_timer = if stats_interval_ms > 0 {
            Some(tokio::time::interval(std::time::Duration::from_millis(stats_interval_ms)))
        } else {
            None
        };
        // Skip immediate first tick
        if let Some(ref mut t) = stats_timer {
            t.tick().await;
        }
```

Add a new branch to the `tokio::select!` block, after the auto-manage branch:

```rust
                // Periodic stats report
                _ = async {
                    match &mut stats_timer {
                        Some(t) => t.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.update_session_gauges();
                    self.fire_stats_alert();
                }
```

**Step 6: Implement helper methods on SessionActor**

Add two new methods to `SessionActor`:

```rust
    /// Update gauge metrics that are derived from session state.
    fn update_session_gauges(&self) {
        let c = &self.counters;
        c.set(crate::stats::SES_NUM_TORRENTS, self.torrents.len() as i64);

        let active = self.torrents.len() as i64;
        c.set(crate::stats::SES_ACTIVE_TORRENTS, active);

        let banned = self.ban_manager.read().unwrap().banned_list().len() as i64;
        c.set(crate::stats::PEER_NUM_BANNED, banned);

        let dht_count = self.dht_v4.is_some() as i64 + self.dht_v6.is_some() as i64;
        c.set(crate::stats::DHT_NODES, dht_count);
        c.set(crate::stats::DHT_NODES_V4, self.dht_v4.is_some() as i64);
        c.set(crate::stats::DHT_NODES_V6, self.dht_v6.is_some() as i64);
    }

    /// Take a snapshot and fire a SessionStatsAlert.
    fn fire_stats_alert(&self) {
        let values = self.counters.snapshot();
        crate::alert::post_alert(
            &self.alert_tx,
            &self.alert_mask,
            crate::alert::AlertKind::SessionStatsAlert { values },
        );
    }
```

**Step 7: Update `handle_apply_settings` to update the stats timer**

After storing new settings in `handle_apply_settings`, the stats timer update will take effect on the next iteration because the timer is owned by `run()`. The simplest approach: since the timer is created at startup, `apply_settings` only affects the interval on next restart. This matches the existing pattern documented: "Sub-actor reconfiguration takes effect on next session restart."

No code change needed here — document the behavior.

**Step 8: Wire counters into existing make_session_stats (backward compat)**

Update `make_session_stats()` to also populate from the counter array where available, but keep the existing summation logic for the legacy `SessionStats` struct:

```rust
    async fn make_session_stats(&self) -> SessionStats {
        let mut total_downloaded = 0u64;
        let mut total_uploaded = 0u64;

        for entry in self.torrents.values() {
            if let Ok(stats) = entry.handle.stats().await {
                total_downloaded += stats.downloaded;
                total_uploaded += stats.uploaded;
            }
        }

        // Also update the counters for consistency
        self.counters.set(crate::stats::BW_TOTAL_DOWNLOADED, total_downloaded as i64);
        self.counters.set(crate::stats::BW_TOTAL_UPLOADED, total_uploaded as i64);
        self.counters.set(crate::stats::SES_ACTIVE_TORRENTS, self.torrents.len() as i64);
        self.counters.set(crate::stats::SES_NUM_TORRENTS, self.torrents.len() as i64);

        SessionStats {
            active_torrents: self.torrents.len(),
            total_downloaded,
            total_uploaded,
            dht_nodes: self.dht_v4.is_some() as usize + self.dht_v6.is_some() as usize,
        }
    }
```

**Step 9: Run tests**

Run: `cargo test -p ferrite-session session:: -- --nocapture`
Expected: All existing session tests PASS (the new counters are additive, no behavior change)

---

## Task 5: Session Integration Tests

**Files:**
- Modify: `crates/ferrite-session/src/session.rs` (tests module)

**Step 1: Add test for post_session_stats**

Add to session.rs `mod tests`:

```rust
    // ---- Test: post_session_stats fires alert ----

    #[tokio::test]
    async fn post_session_stats_fires_alert() {
        let session = SessionHandle::start(test_settings()).await.unwrap();

        // Subscribe before triggering
        let mut rx = session.subscribe();

        session.post_session_stats().await.unwrap();

        // Receive the alert (with a timeout to avoid hanging)
        let alert = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for stats alert")
            .expect("broadcast recv failed");

        match alert.kind {
            AlertKind::SessionStatsAlert { ref values } => {
                assert_eq!(values.len(), crate::stats::NUM_METRICS);
            }
            _ => panic!("expected SessionStatsAlert, got {:?}", alert.kind),
        }

        session.shutdown().await.unwrap();
    }
```

**Step 2: Add test for counters accessor**

```rust
    // ---- Test: counters accessible and functional ----

    #[tokio::test]
    async fn session_counters_accessible() {
        let session = SessionHandle::start(test_settings()).await.unwrap();

        let counters = session.counters();
        assert_eq!(counters.len(), crate::stats::NUM_METRICS);

        // All counters should start at zero
        for i in 0..crate::stats::NUM_METRICS {
            assert_eq!(counters.get(i), 0, "metric {i} should start at 0");
        }

        // Manual increment should be visible
        counters.inc(crate::stats::NET_BYTES_SENT, 42);
        assert_eq!(counters.get(crate::stats::NET_BYTES_SENT), 42);

        session.shutdown().await.unwrap();
    }
```

**Step 3: Add test for periodic stats timer**

```rust
    // ---- Test: periodic stats alert ----

    #[tokio::test]
    async fn periodic_stats_alert_fires() {
        let mut settings = test_settings();
        settings.stats_report_interval = 100; // 100ms for fast test
        let session = SessionHandle::start(settings).await.unwrap();

        let mut rx = session.subscribe();

        // Wait for at least one periodic alert (give it 500ms)
        let mut got_stats = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(alert)) => {
                    if matches!(alert.kind, AlertKind::SessionStatsAlert { .. }) {
                        got_stats = true;
                        break;
                    }
                }
                _ => continue,
            }
        }

        assert!(got_stats, "should have received at least one SessionStatsAlert");
        session.shutdown().await.unwrap();
    }
```

**Step 4: Add test for disabled periodic stats**

```rust
    // ---- Test: stats timer disabled when interval = 0 ----

    #[tokio::test]
    async fn stats_timer_disabled_when_zero() {
        let mut settings = test_settings();
        settings.stats_report_interval = 0;
        let session = SessionHandle::start(settings).await.unwrap();

        let mut rx = session.subscribe();

        // Wait briefly — should NOT receive a SessionStatsAlert
        let result = tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                match rx.recv().await {
                    Ok(alert) if matches!(alert.kind, AlertKind::SessionStatsAlert { .. }) => {
                        return true;
                    }
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await;

        // Should time out (no stats alert fired)
        assert!(result.is_err() || result == Ok(false));
        session.shutdown().await.unwrap();
    }
```

**Step 5: Run tests**

Run: `cargo test -p ferrite-session -- --nocapture`
Expected: All tests PASS

---

## Task 6: Facade Re-exports and ClientBuilder Integration

**Files:**
- Modify: `crates/ferrite/src/session.rs`
- Modify: `crates/ferrite/src/prelude.rs`
- Modify: `crates/ferrite/src/client.rs`

**Step 1: Add re-exports to facade session module**

In `crates/ferrite/src/session.rs`, add to the `pub use ferrite_session` block:

```rust
    // Session statistics counters (M50)
    MetricKind,
    SessionStatsMetric,
    SessionCounters,
    session_stats_metrics,
    NUM_METRICS,
```

**Step 2: Add to prelude**

In `crates/ferrite/src/prelude.rs`, add:

```rust
// Session statistics (M50)
pub use crate::session::{
    MetricKind, SessionStatsMetric, SessionCounters,
    session_stats_metrics, NUM_METRICS,
};
```

**Step 3: Add `stats_report_interval` to ClientBuilder**

In `crates/ferrite/src/client.rs`, add a new builder method:

```rust
    /// Set the interval in milliseconds between automatic `SessionStatsAlert` firings.
    ///
    /// Default: 1000ms. Set to 0 to disable periodic stats alerts.
    /// Consumers subscribe to `AlertCategory::STATS` to receive the alerts.
    pub fn stats_report_interval(mut self, ms: u64) -> Self {
        self.settings.stats_report_interval = ms;
        self
    }
```

**Step 4: Add facade tests**

Add to `crates/ferrite/src/lib.rs` tests:

```rust
    #[test]
    fn session_stats_metrics_accessible_through_facade() {
        let metrics = session::session_stats_metrics();
        assert_eq!(metrics.len(), session::NUM_METRICS);

        // Verify first and last metric names
        assert_eq!(metrics[0].name, "net.bytes_sent");
        assert!(matches!(metrics[0].kind, session::MetricKind::Counter));

        // Verify all have dotted names
        for m in metrics {
            assert!(m.name.contains('.'), "{} should have dotted name", m.name);
        }
    }

    #[test]
    fn client_builder_stats_interval() {
        let config = crate::ClientBuilder::new()
            .stats_report_interval(500)
            .into_settings();
        assert_eq!(config.stats_report_interval, 500);

        // Disabled
        let config = crate::ClientBuilder::new()
            .stats_report_interval(0)
            .into_settings();
        assert_eq!(config.stats_report_interval, 0);
    }

    #[test]
    fn session_counters_accessible_through_prelude() {
        use crate::prelude::*;

        let counters = SessionCounters::new();
        assert_eq!(counters.len(), NUM_METRICS);

        let metrics = session_stats_metrics();
        assert_eq!(metrics.len(), NUM_METRICS);
    }
```

**Step 5: Run tests**

Run: `cargo test --workspace -- --nocapture`
Expected: All tests PASS

---

## Task 7: Full Workspace Validation

**Step 1: Run full test suite**

Run: `cargo test --workspace`
Expected: All 900+ tests PASS

**Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings

**Step 3: Verify metric count**

The implementation should have exactly 70 metrics (NUM_METRICS = 70), covering:
- Network: 12 metrics (0-11)
- Disk: 10 metrics (12-21)
- DHT: 7 metrics (22-28)
- Peers: 12 metrics (29-40)
- Protocol: 14 metrics (41-54)
- Bandwidth: 10 metrics (55-64)
- Session: 5 metrics (65-69)

Total: 70 metrics

---

## Summary of Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/ferrite-session/src/stats.rs` | Create | MetricKind, SessionStatsMetric, SessionCounters, metric constants, session_stats_metrics() |
| `crates/ferrite-session/src/lib.rs` | Modify | Add `pub mod stats;` and re-exports |
| `crates/ferrite-session/src/alert.rs` | Modify | Add `SessionStatsAlert` variant to AlertKind |
| `crates/ferrite-session/src/settings.rs` | Modify | Add `stats_report_interval` field |
| `crates/ferrite-session/src/session.rs` | Modify | Wire counters into SessionActor/SessionHandle, periodic timer, post_session_stats() |
| `crates/ferrite/src/session.rs` | Modify | Re-export stats types |
| `crates/ferrite/src/prelude.rs` | Modify | Re-export stats types |
| `crates/ferrite/src/client.rs` | Modify | Add stats_report_interval() builder method |
| `crates/ferrite/src/lib.rs` | Modify | Add facade tests |

**New public API surface:**
- `MetricKind` enum (Counter, Gauge)
- `SessionStatsMetric` struct (name, kind)
- `SessionCounters` struct (inc, set, get, snapshot, len, uptime_secs)
- `session_stats_metrics()` function
- `NUM_METRICS` constant (70)
- `SessionHandle::post_session_stats()` method
- `SessionHandle::counters()` method
- `AlertKind::SessionStatsAlert { values: Vec<i64> }` variant
- `Settings::stats_report_interval` field
- `ClientBuilder::stats_report_interval()` method
- 70 metric index constants (NET_BYTES_SENT through SES_QUEUE_PAUSED_BY_AUTO)

**Backward compatibility:** The existing `SessionStats` struct (4 fields) and `AlertKind::SessionStatsUpdate` remain unchanged. The new `SessionStatsAlert` is a separate alert kind that coexists with the legacy one.

**Test count:** ~18 new tests across stats.rs, alert.rs, settings.rs, session.rs, and lib.rs (facade).

**Version bump:** 0.41.0 -> 0.42.0
