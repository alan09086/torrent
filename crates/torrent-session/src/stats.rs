//! Session statistics metric registry and atomic counter array.
//!
//! Provides 70 atomic metrics across 7 categories (network, disk, DHT, peers,
//! protocol, bandwidth, session). [`MetricKind`] distinguishes monotonic
//! counters from point-in-time gauges. [`SessionStatsMetric`] provides static
//! metadata for each metric, and [`SessionCounters`] holds the atomic values.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// MetricKind
// ---------------------------------------------------------------------------

/// Whether a metric is a monotonically increasing counter or a point-in-time gauge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MetricKind {
    /// Monotonically increasing (bytes sent, pieces downloaded, etc.).
    Counter,
    /// Current value that can go up or down (connections, DHT nodes, etc.).
    Gauge,
}

// ---------------------------------------------------------------------------
// SessionStatsMetric
// ---------------------------------------------------------------------------

/// Static metadata for a single session metric.
#[derive(Debug, Clone, Copy)]
pub struct SessionStatsMetric {
    /// Human-readable dotted name (e.g. `"net.bytes_sent"`).
    pub name: &'static str,
    /// Whether this metric is a counter or gauge.
    pub kind: MetricKind,
}

// ---------------------------------------------------------------------------
// Metric index constants (70 total)
// ---------------------------------------------------------------------------

// -- Network (0..11) --

/// Metric index: total bytes sent across all connections (counter).
pub const NET_BYTES_SENT: usize = 0;
/// Metric index: total bytes received across all connections (counter).
pub const NET_BYTES_RECV: usize = 1;
/// Metric index: current number of established connections (gauge).
pub const NET_NUM_CONNECTIONS: usize = 2;
/// Metric index: current number of half-open (connecting) sockets (gauge).
pub const NET_NUM_HALF_OPEN: usize = 3;
/// Metric index: current number of TCP peers (gauge).
pub const NET_NUM_TCP_PEERS: usize = 4;
/// Metric index: current number of uTP peers (gauge).
pub const NET_NUM_UTP_PEERS: usize = 5;
/// Metric index: current number of TCP connections (gauge).
pub const NET_NUM_TCP_CONNECTIONS: usize = 6;
/// Metric index: current number of uTP connections (gauge).
pub const NET_NUM_UTP_CONNECTIONS: usize = 7;
/// Metric index: total bytes sent over TCP (counter).
pub const NET_TCP_BYTES_SENT: usize = 8;
/// Metric index: total bytes received over TCP (counter).
pub const NET_TCP_BYTES_RECV: usize = 9;
/// Metric index: total bytes sent over uTP (counter).
pub const NET_UTP_BYTES_SENT: usize = 10;
/// Metric index: total bytes received over uTP (counter).
pub const NET_UTP_BYTES_RECV: usize = 11;

// -- Disk (12..21) --

/// Metric index: total disk read operations (counter).
pub const DISK_READ_COUNT: usize = 12;
/// Metric index: total disk write operations (counter).
pub const DISK_WRITE_COUNT: usize = 13;
/// Metric index: total bytes read from disk (counter).
pub const DISK_READ_BYTES: usize = 14;
/// Metric index: total bytes written to disk (counter).
pub const DISK_WRITE_BYTES: usize = 15;
/// Metric index: total disk cache hits (counter).
pub const DISK_CACHE_HITS: usize = 16;
/// Metric index: total disk cache misses (counter).
pub const DISK_CACHE_MISSES: usize = 17;
/// Metric index: current disk job queue depth (gauge).
pub const DISK_QUEUE_DEPTH: usize = 18;
/// Metric index: cumulative disk job time in microseconds (counter).
pub const DISK_JOB_TIME_US: usize = 19;
/// Metric index: current write buffer size in bytes (gauge).
pub const DISK_WRITE_BUFFER_BYTES: usize = 20;
/// Metric index: total piece hash operations (counter).
pub const DISK_HASH_COUNT: usize = 21;

// -- DHT (22..28) --

/// Metric index: current number of DHT routing table nodes (gauge).
pub const DHT_NODES: usize = 22;
/// Metric index: total DHT lookup operations (counter).
pub const DHT_LOOKUPS: usize = 23;
/// Metric index: total bytes received via DHT (counter).
pub const DHT_BYTES_IN: usize = 24;
/// Metric index: total bytes sent via DHT (counter).
pub const DHT_BYTES_OUT: usize = 25;
/// Metric index: current number of IPv4 DHT nodes (gauge).
pub const DHT_NODES_V4: usize = 26;
/// Metric index: current number of IPv6 DHT nodes (gauge).
pub const DHT_NODES_V6: usize = 27;
/// Metric index: total DHT announce operations (counter).
pub const DHT_ANNOUNCE_COUNT: usize = 28;

// -- Peers (29..40) --

/// Metric index: current number of unchoked peers (gauge).
pub const PEER_NUM_UNCHOKED: usize = 29;
/// Metric index: current number of interested peers (gauge).
pub const PEER_NUM_INTERESTED: usize = 30;
/// Metric index: current number of peers we are uploading to (gauge).
pub const PEER_NUM_UPLOADING: usize = 31;
/// Metric index: current number of peers we are downloading from (gauge).
pub const PEER_NUM_DOWNLOADING: usize = 32;
/// Metric index: current number of torrents in seeding state (gauge).
pub const PEER_NUM_SEEDING_TORRENTS: usize = 33;
/// Metric index: current number of torrents in downloading state (gauge).
pub const PEER_NUM_DOWNLOADING_TORRENTS: usize = 34;
/// Metric index: current number of torrents being checked (gauge).
pub const PEER_NUM_CHECKING_TORRENTS: usize = 35;
/// Metric index: current number of paused torrents (gauge).
pub const PEER_NUM_PAUSED_TORRENTS: usize = 36;
/// Metric index: current total number of connected peers (gauge).
pub const PEER_PEERS_CONNECTED: usize = 37;
/// Metric index: current total number of known available peers (gauge).
pub const PEER_PEERS_AVAILABLE: usize = 38;
/// Metric index: current number of active web seeds (gauge).
pub const PEER_NUM_WEB_SEEDS: usize = 39;
/// Metric index: current number of banned peers (gauge).
pub const PEER_NUM_BANNED: usize = 40;

// -- Protocol (41..54) --

/// Metric index: total pieces downloaded across all torrents (counter).
pub const PROTO_PIECES_DOWNLOADED: usize = 41;
/// Metric index: total pieces uploaded across all torrents (counter).
pub const PROTO_PIECES_UPLOADED: usize = 42;
/// Metric index: total piece hash verification failures (counter).
pub const PROTO_HASHFAILS: usize = 43;
/// Metric index: total wasted bytes (duplicate/rejected data) (counter).
pub const PROTO_WASTE_BYTES: usize = 44;
/// Metric index: total piece request messages sent (counter).
pub const PROTO_PIECE_REQUESTS: usize = 45;
/// Metric index: total piece reject messages received (counter).
pub const PROTO_PIECE_REJECTS: usize = 46;
/// Metric index: total incoming handshakes received (counter).
pub const PROTO_HANDSHAKES_IN: usize = 47;
/// Metric index: total outgoing handshakes sent (counter).
pub const PROTO_HANDSHAKES_OUT: usize = 48;
/// Metric index: total PEX messages received (counter).
pub const PROTO_PEX_MESSAGES_IN: usize = 49;
/// Metric index: total PEX messages sent (counter).
pub const PROTO_PEX_MESSAGES_OUT: usize = 50;
/// Metric index: total tracker announce requests (counter).
pub const PROTO_TRACKER_ANNOUNCES: usize = 51;
/// Metric index: total tracker announce errors (counter).
pub const PROTO_TRACKER_ERRORS: usize = 52;
/// Metric index: total BEP 9 metadata requests sent (counter).
pub const PROTO_METADATA_REQUESTS: usize = 53;
/// Metric index: total BEP 9 metadata pieces received (counter).
pub const PROTO_METADATA_RECEIVES: usize = 54;

// -- Bandwidth (55..64) --

/// Metric index: current aggregate upload rate in bytes/sec (gauge).
pub const BW_UPLOAD_RATE: usize = 55;
/// Metric index: current aggregate download rate in bytes/sec (gauge).
pub const BW_DOWNLOAD_RATE: usize = 56;
/// Metric index: current TCP upload rate in bytes/sec (gauge).
pub const BW_UPLOAD_RATE_TCP: usize = 57;
/// Metric index: current TCP download rate in bytes/sec (gauge).
pub const BW_DOWNLOAD_RATE_TCP: usize = 58;
/// Metric index: current uTP upload rate in bytes/sec (gauge).
pub const BW_UPLOAD_RATE_UTP: usize = 59;
/// Metric index: current uTP download rate in bytes/sec (gauge).
pub const BW_DOWNLOAD_RATE_UTP: usize = 60;
/// Metric index: current payload-only upload rate in bytes/sec (gauge).
pub const BW_PAYLOAD_UPLOAD_RATE: usize = 61;
/// Metric index: current payload-only download rate in bytes/sec (gauge).
pub const BW_PAYLOAD_DOWNLOAD_RATE: usize = 62;
/// Metric index: total bytes uploaded since session start (counter).
pub const BW_TOTAL_UPLOADED: usize = 63;
/// Metric index: total bytes downloaded since session start (counter).
pub const BW_TOTAL_DOWNLOADED: usize = 64;

// -- Session (65..69) --

/// Metric index: current number of active (non-paused) torrents (gauge).
pub const SES_ACTIVE_TORRENTS: usize = 65;
/// Metric index: total number of torrents in the session (gauge).
pub const SES_NUM_TORRENTS: usize = 66;
/// Metric index: session uptime in seconds (gauge).
pub const SES_UPTIME_SECS: usize = 67;
/// Metric index: total connections blocked by the IP filter (counter).
pub const SES_IP_FILTER_BLOCKED: usize = 68;
/// Metric index: total torrents paused by auto-management (counter).
pub const SES_QUEUE_PAUSED_BY_AUTO: usize = 69;

/// Total number of metrics tracked by the session.
pub const NUM_METRICS: usize = 70;

// ---------------------------------------------------------------------------
// session_stats_metrics()
// ---------------------------------------------------------------------------

/// Return static metadata for all session metrics.
///
/// The returned slice is indexed by metric constant (e.g. [`NET_BYTES_SENT`]).
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

// ---------------------------------------------------------------------------
// SessionCounters
// ---------------------------------------------------------------------------

/// Atomic counter array shared between session and torrent actors.
///
/// All values are [`AtomicI64`] — counters are incremented, gauges are set.
/// The struct is `Send + Sync` (auto-derived from `AtomicI64`).
pub struct SessionCounters {
    values: [AtomicI64; NUM_METRICS],
    started_at: Instant,
    prev_bytes_sent: AtomicI64,
    prev_bytes_recv: AtomicI64,
}

impl SessionCounters {
    /// Create a new counter array with all values initialised to zero.
    pub fn new() -> Self {
        Self {
            values: std::array::from_fn(|_| AtomicI64::new(0)),
            started_at: Instant::now(),
            prev_bytes_sent: AtomicI64::new(0),
            prev_bytes_recv: AtomicI64::new(0),
        }
    }

    /// Atomically add `delta` to a counter metric.
    #[inline]
    pub fn inc(&self, metric: usize, delta: i64) {
        debug_assert!(metric < NUM_METRICS);
        self.values[metric].fetch_add(delta, Ordering::Relaxed);
    }

    /// Atomically set a gauge metric to `value`.
    #[inline]
    pub fn set(&self, metric: usize, value: i64) {
        debug_assert!(metric < NUM_METRICS);
        self.values[metric].store(value, Ordering::Relaxed);
    }

    /// Read the current value of a metric.
    #[inline]
    pub fn get(&self, metric: usize) -> i64 {
        debug_assert!(metric < NUM_METRICS);
        self.values[metric].load(Ordering::Relaxed)
    }

    /// Take a consistent snapshot of all metric values.
    ///
    /// Also updates the uptime gauge and computes bandwidth rate deltas
    /// (upload/download rate = bytes since last snapshot).
    pub fn snapshot(&self) -> Vec<i64> {
        let mut vals: Vec<i64> = self
            .values
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .collect();

        // Update uptime gauge.
        vals[SES_UPTIME_SECS] = self.started_at.elapsed().as_secs() as i64;

        // Compute bandwidth rate deltas.
        let cur_sent = vals[NET_BYTES_SENT];
        let cur_recv = vals[NET_BYTES_RECV];
        let prev_sent = self.prev_bytes_sent.swap(cur_sent, Ordering::Relaxed);
        let prev_recv = self.prev_bytes_recv.swap(cur_recv, Ordering::Relaxed);
        vals[BW_UPLOAD_RATE] = cur_sent.saturating_sub(prev_sent);
        vals[BW_DOWNLOAD_RATE] = cur_recv.saturating_sub(prev_recv);

        vals
    }

    /// Number of metrics tracked.
    pub fn len(&self) -> usize {
        NUM_METRICS
    }

    /// Always returns `false` — there are always metrics.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Seconds elapsed since the session was created.
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

impl Default for SessionCounters {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn metrics_registry_has_correct_count() {
        assert_eq!(session_stats_metrics().len(), NUM_METRICS);
    }

    #[test]
    fn all_metric_names_are_unique() {
        let names: HashSet<&str> = session_stats_metrics().iter().map(|m| m.name).collect();
        assert_eq!(names.len(), NUM_METRICS);
    }

    #[test]
    fn all_metric_names_have_category_prefix() {
        for m in session_stats_metrics() {
            assert!(
                m.name.contains('.'),
                "metric name {:?} has no category prefix",
                m.name
            );
        }
    }

    #[test]
    fn counter_inc_and_get() {
        let c = SessionCounters::new();
        c.inc(NET_BYTES_SENT, 5);
        assert_eq!(c.get(NET_BYTES_SENT), 5);
        c.inc(NET_BYTES_SENT, 3);
        assert_eq!(c.get(NET_BYTES_SENT), 8);
    }

    #[test]
    fn gauge_set_and_get() {
        let c = SessionCounters::new();
        c.set(NET_NUM_CONNECTIONS, 42);
        assert_eq!(c.get(NET_NUM_CONNECTIONS), 42);
        c.set(NET_NUM_CONNECTIONS, 0);
        assert_eq!(c.get(NET_NUM_CONNECTIONS), 0);
    }

    #[test]
    fn snapshot_returns_all_values() {
        let c = SessionCounters::new();
        c.inc(NET_BYTES_SENT, 100);
        c.set(DHT_NODES, 50);
        c.inc(PROTO_HASHFAILS, 3);
        let snap = c.snapshot();
        assert_eq!(snap.len(), NUM_METRICS);
        assert_eq!(snap[NET_BYTES_SENT], 100);
        assert_eq!(snap[DHT_NODES], 50);
        assert_eq!(snap[PROTO_HASHFAILS], 3);
    }

    #[test]
    fn snapshot_includes_uptime() {
        let c = SessionCounters::new();
        // Even without sleeping, uptime should be >= 0.
        let snap = c.snapshot();
        assert!(snap[SES_UPTIME_SECS] >= 0);
    }

    #[test]
    fn counters_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SessionCounters>();
    }

    #[test]
    fn metric_kind_serializes() {
        let counter_json = serde_json::to_string(&MetricKind::Counter).unwrap();
        let gauge_json = serde_json::to_string(&MetricKind::Gauge).unwrap();
        assert_eq!(
            serde_json::from_str::<MetricKind>(&counter_json).unwrap(),
            MetricKind::Counter
        );
        assert_eq!(
            serde_json::from_str::<MetricKind>(&gauge_json).unwrap(),
            MetricKind::Gauge
        );
    }

    #[test]
    fn metric_index_constants_in_range() {
        let indices = [
            NET_BYTES_SENT,
            NET_BYTES_RECV,
            NET_NUM_CONNECTIONS,
            NET_NUM_HALF_OPEN,
            NET_NUM_TCP_PEERS,
            NET_NUM_UTP_PEERS,
            NET_NUM_TCP_CONNECTIONS,
            NET_NUM_UTP_CONNECTIONS,
            NET_TCP_BYTES_SENT,
            NET_TCP_BYTES_RECV,
            NET_UTP_BYTES_SENT,
            NET_UTP_BYTES_RECV,
            DISK_READ_COUNT,
            DISK_WRITE_COUNT,
            DISK_READ_BYTES,
            DISK_WRITE_BYTES,
            DISK_CACHE_HITS,
            DISK_CACHE_MISSES,
            DISK_QUEUE_DEPTH,
            DISK_JOB_TIME_US,
            DISK_WRITE_BUFFER_BYTES,
            DISK_HASH_COUNT,
            DHT_NODES,
            DHT_LOOKUPS,
            DHT_BYTES_IN,
            DHT_BYTES_OUT,
            DHT_NODES_V4,
            DHT_NODES_V6,
            DHT_ANNOUNCE_COUNT,
            PEER_NUM_UNCHOKED,
            PEER_NUM_INTERESTED,
            PEER_NUM_UPLOADING,
            PEER_NUM_DOWNLOADING,
            PEER_NUM_SEEDING_TORRENTS,
            PEER_NUM_DOWNLOADING_TORRENTS,
            PEER_NUM_CHECKING_TORRENTS,
            PEER_NUM_PAUSED_TORRENTS,
            PEER_PEERS_CONNECTED,
            PEER_PEERS_AVAILABLE,
            PEER_NUM_WEB_SEEDS,
            PEER_NUM_BANNED,
            PROTO_PIECES_DOWNLOADED,
            PROTO_PIECES_UPLOADED,
            PROTO_HASHFAILS,
            PROTO_WASTE_BYTES,
            PROTO_PIECE_REQUESTS,
            PROTO_PIECE_REJECTS,
            PROTO_HANDSHAKES_IN,
            PROTO_HANDSHAKES_OUT,
            PROTO_PEX_MESSAGES_IN,
            PROTO_PEX_MESSAGES_OUT,
            PROTO_TRACKER_ANNOUNCES,
            PROTO_TRACKER_ERRORS,
            PROTO_METADATA_REQUESTS,
            PROTO_METADATA_RECEIVES,
            BW_UPLOAD_RATE,
            BW_DOWNLOAD_RATE,
            BW_UPLOAD_RATE_TCP,
            BW_DOWNLOAD_RATE_TCP,
            BW_UPLOAD_RATE_UTP,
            BW_DOWNLOAD_RATE_UTP,
            BW_PAYLOAD_UPLOAD_RATE,
            BW_PAYLOAD_DOWNLOAD_RATE,
            BW_TOTAL_UPLOADED,
            BW_TOTAL_DOWNLOADED,
            SES_ACTIVE_TORRENTS,
            SES_NUM_TORRENTS,
            SES_UPTIME_SECS,
            SES_IP_FILTER_BLOCKED,
            SES_QUEUE_PAUSED_BY_AUTO,
        ];
        assert_eq!(indices.len(), NUM_METRICS);
        for &idx in &indices {
            assert!(idx < NUM_METRICS, "index {idx} >= NUM_METRICS");
        }
    }

    #[test]
    fn default_counters_all_zero() {
        let c = SessionCounters::default();
        let snap = c.snapshot();
        for (i, &val) in snap.iter().enumerate() {
            if i == SES_UPTIME_SECS {
                continue; // uptime is computed dynamically
            }
            // BW_UPLOAD_RATE and BW_DOWNLOAD_RATE are computed from deltas
            // and will be 0 on first snapshot (prev == 0, cur == 0).
            assert_eq!(val, 0, "metric index {i} should be 0 but was {val}");
        }
    }

    #[test]
    fn concurrent_inc_from_multiple_threads() {
        use std::sync::Arc;

        let c = Arc::new(SessionCounters::new());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        c.inc(NET_BYTES_SENT, 1);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(c.get(NET_BYTES_SENT), 4000);
    }

    #[test]
    fn len_and_is_empty() {
        let c = SessionCounters::new();
        assert_eq!(c.len(), 70);
        assert!(!c.is_empty());
    }
}
