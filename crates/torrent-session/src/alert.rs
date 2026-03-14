//! Alert/event system for push-based notifications.
//!
//! Consumers subscribe to a broadcast channel of [`Alert`] events, optionally
//! filtered by [`AlertCategory`] bitmask at both session and per-subscriber level.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::types::TorrentState;
use torrent_core::Id20;

// ── AlertCategory (bitflags) ──────────────────────────────────────────

bitflags::bitflags! {
    /// Bitmask categories for filtering alerts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AlertCategory: u32 {
        /// Torrent lifecycle: added, removed, paused, resumed, finished, state changes.
        const STATUS       = 0x001;
        /// Errors from torrents, trackers, storage.
        const ERROR        = 0x002;
        /// Peer connect/disconnect/ban events.
        const PEER         = 0x004;
        /// Tracker announce replies and errors.
        const TRACKER      = 0x008;
        /// Storage/file operations.
        const STORAGE      = 0x010;
        /// DHT bootstrap and peer discovery.
        const DHT          = 0x020;
        /// Periodic session/torrent statistics.
        const STATS        = 0x040;
        /// Piece-level events (verified, hash-failed).
        const PIECE        = 0x080;
        /// Block-level events (high volume).
        const BLOCK        = 0x100;
        /// Performance warnings.
        const PERFORMANCE  = 0x200;
        /// Port mapping (UPnP/NAT-PMP).
        const PORT_MAPPING = 0x400;
        /// I2P session events.
        const I2P          = 0x800;
        /// All categories enabled.
        const ALL          = 0xFFF;
    }
}

// ── AlertKind ─────────────────────────────────────────────────────────

/// The specific event that occurred.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertKind {
    // ── Torrent lifecycle (STATUS) ──
    /// A torrent was added to the session.
    TorrentAdded {
        /// Info hash of the added torrent.
        info_hash: Id20,
        /// Display name of the torrent.
        name: String,
    },
    /// A torrent was removed from the session.
    TorrentRemoved {
        /// Info hash of the removed torrent.
        info_hash: Id20,
    },
    /// A torrent was paused.
    TorrentPaused {
        /// Info hash of the paused torrent.
        info_hash: Id20,
    },
    /// A torrent was resumed.
    TorrentResumed {
        /// Info hash of the resumed torrent.
        info_hash: Id20,
    },
    /// A torrent finished downloading all pieces.
    TorrentFinished {
        /// Info hash of the finished torrent.
        info_hash: Id20,
    },
    /// A torrent changed state.
    StateChanged {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Previous torrent state.
        prev_state: TorrentState,
        /// New torrent state.
        new_state: TorrentState,
    },
    /// Torrent metadata received via BEP 9 extension protocol.
    MetadataReceived {
        /// Info hash of the torrent whose metadata was received.
        info_hash: Id20,
        /// Display name from the received metadata.
        name: String,
    },
    /// Metadata download failed for a magnet link torrent.
    MetadataFailed {
        /// Info hash of the torrent whose metadata download failed.
        info_hash: Id20,
    },

    // ── Checking (STATUS) ──
    /// A torrent finished checking (verifying existing data on disk).
    TorrentChecked {
        /// Info hash of the checked torrent.
        info_hash: Id20,
        /// Number of pieces that passed hash verification.
        pieces_have: u32,
        /// Total number of pieces in the torrent.
        pieces_total: u32,
    },
    /// Progress update during piece hash checking.
    CheckingProgress {
        /// Info hash of the torrent being checked.
        info_hash: Id20,
        /// Fraction complete (0.0 to 1.0).
        progress: f32,
    },

    // ── Transfer (PIECE / BLOCK) ──
    /// A piece passed hash verification and is now complete.
    PieceFinished {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Zero-based piece index.
        piece: u32,
    },
    /// A block (sub-piece chunk) was received and written.
    BlockFinished {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Zero-based piece index containing the block.
        piece: u32,
        /// Byte offset of the block within the piece.
        offset: u32,
    },
    /// A piece failed hash verification.
    HashFailed {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Zero-based piece index that failed.
        piece: u32,
        /// IP addresses of peers that contributed data to this piece.
        contributors: Vec<std::net::IpAddr>,
    },

    // ── Peers (PEER) ──
    /// A new peer connection was established.
    PeerConnected {
        /// Info hash of the torrent swarm.
        info_hash: Id20,
        /// Socket address of the connected peer.
        addr: SocketAddr,
    },
    /// A peer disconnected.
    PeerDisconnected {
        /// Info hash of the torrent swarm.
        info_hash: Id20,
        /// Socket address of the disconnected peer.
        addr: SocketAddr,
        /// Human-readable reason for disconnection, if available.
        reason: Option<String>,
    },
    /// A peer was banned (e.g. for sending corrupt data).
    PeerBanned {
        /// Info hash of the torrent swarm.
        info_hash: Id20,
        /// Socket address of the banned peer.
        addr: SocketAddr,
    },

    // ── Tracker (TRACKER) ──
    /// A tracker announce completed successfully.
    TrackerReply {
        /// Info hash announced to the tracker.
        info_hash: Id20,
        /// Tracker URL.
        url: String,
        /// Number of peers returned by the tracker.
        num_peers: usize,
    },
    /// A tracker returned a warning message.
    TrackerWarning {
        /// Info hash announced to the tracker.
        info_hash: Id20,
        /// Tracker URL.
        url: String,
        /// Warning message from the tracker.
        message: String,
    },
    /// A tracker announce failed.
    TrackerError {
        /// Info hash announced to the tracker.
        info_hash: Id20,
        /// Tracker URL.
        url: String,
        /// Error description.
        message: String,
    },
    /// A tracker scrape completed successfully.
    ScrapeReply {
        /// Info hash that was scraped.
        info_hash: Id20,
        /// Tracker URL.
        url: String,
        /// Number of complete peers (seeders).
        complete: u32,
        /// Number of incomplete peers (leechers).
        incomplete: u32,
        /// Total number of completed downloads reported by the tracker.
        downloaded: u32,
    },
    /// A tracker scrape failed.
    ScrapeError {
        /// Info hash that was scraped.
        info_hash: Id20,
        /// Tracker URL.
        url: String,
        /// Error description.
        message: String,
    },

    // ── DHT ──
    /// DHT routing table bootstrapping finished.
    DhtBootstrapComplete,
    /// DHT get_peers query returned peers for a torrent.
    DhtGetPeers {
        /// Info hash that was queried.
        info_hash: Id20,
        /// Number of peers returned.
        num_peers: usize,
    },
    /// BEP 42: A DHT node was rejected because its ID doesn't match its IP.
    DhtNodeIdViolation {
        /// Node ID that violated the BEP 42 constraint.
        node_id: Id20,
        /// Socket address of the violating node.
        addr: SocketAddr,
    },
    /// BEP 51: sample_infohashes response received from a DHT node.
    DhtSampleInfohashes {
        /// Number of sampled info hashes received.
        num_samples: usize,
        /// The remote node's estimate of its total stored info hashes.
        total_estimate: i64,
    },

    // ── Session (STATUS) ──
    /// The session successfully started listening on a port.
    ListenSucceeded {
        /// The port number now being listened on.
        port: u16,
    },
    /// The session failed to listen on a port.
    ListenFailed {
        /// The port number that failed to bind.
        port: u16,
        /// Error description.
        message: String,
    },
    /// Periodic session statistics snapshot.
    SessionStatsUpdate(crate::types::SessionStats),

    // ── Storage / Disk ──
    /// A file within a torrent was renamed.
    FileRenamed {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Zero-based file index within the torrent.
        index: usize,
        /// The new filesystem path of the file.
        new_path: std::path::PathBuf,
    },
    /// All pieces belonging to a file have been downloaded and verified.
    FileCompleted {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Zero-based file index that is now complete.
        file_index: usize,
    },
    /// A torrent's storage was moved to a new directory.
    StorageMoved {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// The new root directory path.
        new_path: std::path::PathBuf,
    },
    /// A file I/O error occurred.
    FileError {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Filesystem path where the error occurred.
        path: std::path::PathBuf,
        /// Error description.
        message: String,
    },
    /// Periodic disk I/O statistics snapshot.
    DiskStatsUpdate(crate::disk::DiskStats),

    // ── Resume (STATUS) ──
    /// Fast resume data was saved for a torrent.
    ResumeDataSaved {
        /// Info hash of the torrent whose resume data was saved.
        info_hash: Id20,
    },

    // ── Error ──
    /// A torrent encountered a fatal error.
    TorrentError {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Error description.
        message: String,
    },

    // ── Performance ──
    /// A performance warning was detected (e.g. too many hash failures).
    PerformanceWarning {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Warning description.
        message: String,
    },

    // ── Queue management (STATUS) ──
    /// Queue position changed (manual move or torrent removal shifted positions).
    TorrentQueuePositionChanged {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Previous queue position.
        old_pos: i32,
        /// New queue position.
        new_pos: i32,
    },
    /// Torrent was paused or resumed by the auto-manage system.
    TorrentAutoManaged {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Whether the torrent was paused (`true`) or resumed (`false`).
        paused: bool,
    },

    // ── IP filtering (PEER) ──
    /// An incoming connection was blocked by the IP filter.
    PeerBlocked {
        /// Socket address that was blocked.
        addr: SocketAddr,
    },

    /// Periodic turnover disconnected underperforming peers and connected replacements.
    PeerTurnover {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Number of peers disconnected during turnover.
        disconnected: usize,
        /// Number of replacement peers connected.
        replaced: usize,
    },

    // ── Web seeding (STATUS) ──
    /// A web seed was banned (e.g. for serving corrupt data).
    WebSeedBanned {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// URL of the banned web seed.
        url: String,
    },

    // ── Port mapping ──
    /// A UPnP/NAT-PMP port mapping succeeded.
    PortMappingSucceeded {
        /// The mapped external port.
        port: u16,
        /// Protocol name (e.g. "TCP" or "UDP").
        protocol: String,
    },
    /// A UPnP/NAT-PMP port mapping failed.
    PortMappingFailed {
        /// The port that failed to map.
        port: u16,
        /// Error description.
        message: String,
    },

    // ── Hybrid hash conflict (M35) ──
    /// v1 and v2 hashes disagree on the same piece — the .torrent data is inconsistent.
    InconsistentHashes {
        /// Info hash of the affected torrent.
        info_hash: Id20,
        /// Zero-based piece index with inconsistent hashes.
        piece: u32,
    },

    // ── BEP 44 DHT storage (M38) ──
    /// An immutable DHT put completed.
    DhtPutComplete {
        /// SHA-1 target hash of the stored item.
        target: Id20,
    },
    /// A mutable DHT put completed.
    DhtMutablePutComplete {
        /// SHA-1 target hash derived from the public key and salt.
        target: Id20,
        /// Sequence number of the stored value.
        seq: i64,
    },
    /// Result of a DHT get (immutable). `value` is None if not found.
    DhtGetResult {
        /// SHA-1 target hash that was queried.
        target: Id20,
        /// The retrieved value, or `None` if not found.
        value: Option<Vec<u8>>,
    },
    /// Result of a DHT mutable get. `value` is None if not found.
    DhtMutableGetResult {
        /// SHA-1 target hash derived from the public key and salt.
        target: Id20,
        /// The retrieved value, or `None` if not found.
        value: Option<Vec<u8>>,
        /// Sequence number of the value, if found.
        seq: Option<i64>,
        /// Ed25519 public key of the item author.
        public_key: [u8; 32],
    },
    /// A BEP 44 DHT operation failed.
    DhtItemError {
        /// SHA-1 target hash of the failed operation.
        target: Id20,
        /// Error description.
        message: String,
    },

    // ── BEP 55 Holepunch (M40) ──
    /// A holepunch connection attempt succeeded.
    HolepunchSucceeded {
        /// Info hash of the torrent swarm.
        info_hash: Id20,
        /// Socket address of the peer reached via holepunch.
        addr: SocketAddr,
    },
    /// A holepunch connection attempt failed.
    HolepunchFailed {
        /// Info hash of the torrent swarm.
        info_hash: Id20,
        /// Socket address of the peer we attempted to reach.
        addr: SocketAddr,
        /// BEP 55 error code, if provided by the relay.
        error_code: Option<u32>,
        /// Human-readable error description.
        message: String,
    },

    // ── I2P (M41) ──
    /// SAM session successfully created with an ephemeral destination.
    I2pSessionCreated {
        /// The b32 address of our I2P destination.
        b32_address: String,
    },
    /// I2P error (SAM bridge unreachable, tunnel failure, etc.)
    I2pError {
        /// Error description.
        message: String,
    },

    // ── SSL (M42) ──
    /// SSL/TLS error for an SSL torrent (handshake failure, cert validation, etc.).
    SslTorrentError {
        /// Info hash of the affected SSL torrent.
        info_hash: Id20,
        /// Error description.
        message: String,
    },

    // ── Session Stats (M50) ──
    /// Session-level statistics counters snapshot (one value per metric).
    SessionStatsAlert {
        /// Counter values indexed by [`MetricKind`](crate::MetricKind) ordinal.
        values: Vec<i64>,
    },

    // ── Network (STATUS) ──
    /// An external IP address was detected or updated (e.g. from tracker/NAT/DHT).
    ExternalIpDetected {
        /// The detected external IP address.
        ip: std::net::IpAddr,
    },

    // ── Settings (M31) ──
    /// Session settings were changed via `apply_settings()`.
    SettingsChanged,
}

impl AlertKind {
    /// Returns the category bitmask for this alert kind.
    pub fn category(&self) -> AlertCategory {
        use AlertKind::*;
        match self {
            // STATUS
            TorrentAdded { .. }
            | TorrentRemoved { .. }
            | TorrentPaused { .. }
            | TorrentResumed { .. }
            | TorrentFinished { .. }
            | StateChanged { .. }
            | MetadataReceived { .. }
            | ListenSucceeded { .. }
            | ListenFailed { .. }
            | ResumeDataSaved { .. }
            | TorrentChecked { .. }
            | CheckingProgress { .. } => AlertCategory::STATUS,

            MetadataFailed { .. } => AlertCategory::STATUS | AlertCategory::ERROR,

            SessionStatsUpdate(_) => AlertCategory::STATS,

            // PIECE
            PieceFinished { .. } => AlertCategory::PIECE,
            HashFailed { .. } => AlertCategory::PIECE | AlertCategory::ERROR,

            // BLOCK
            BlockFinished { .. } => AlertCategory::BLOCK,

            // PEER
            PeerConnected { .. } | PeerDisconnected { .. } | PeerBanned { .. } => {
                AlertCategory::PEER
            }

            // TRACKER
            TrackerReply { .. } => AlertCategory::TRACKER,
            TrackerWarning { .. } => AlertCategory::TRACKER,
            TrackerError { .. } => AlertCategory::TRACKER | AlertCategory::ERROR,
            ScrapeReply { .. } => AlertCategory::TRACKER,
            ScrapeError { .. } => AlertCategory::TRACKER | AlertCategory::ERROR,

            // DHT
            DhtBootstrapComplete | DhtGetPeers { .. } | DhtSampleInfohashes { .. } => {
                AlertCategory::DHT
            }
            DhtNodeIdViolation { .. } => AlertCategory::DHT | AlertCategory::ERROR,
            DhtPutComplete { .. }
            | DhtMutablePutComplete { .. }
            | DhtGetResult { .. }
            | DhtMutableGetResult { .. } => AlertCategory::DHT,
            DhtItemError { .. } => AlertCategory::DHT | AlertCategory::ERROR,

            // STORAGE
            FileRenamed { .. } | StorageMoved { .. } | FileCompleted { .. } => {
                AlertCategory::STORAGE
            }
            FileError { .. } => AlertCategory::STORAGE | AlertCategory::ERROR,
            DiskStatsUpdate(_) => AlertCategory::STATS | AlertCategory::STORAGE,

            // ERROR
            TorrentError { .. } => AlertCategory::ERROR,
            InconsistentHashes { .. } => AlertCategory::ERROR,

            // PERFORMANCE
            PerformanceWarning { .. } => AlertCategory::PERFORMANCE,

            // QUEUE MANAGEMENT (STATUS)
            TorrentQueuePositionChanged { .. } => AlertCategory::STATUS,
            TorrentAutoManaged { .. } => AlertCategory::STATUS,

            // IP FILTER
            PeerBlocked { .. } => AlertCategory::PEER,
            PeerTurnover { .. } => AlertCategory::PEER,

            // WEB SEED
            WebSeedBanned { .. } => AlertCategory::STATUS,

            // PORT_MAPPING
            PortMappingSucceeded { .. } => AlertCategory::PORT_MAPPING,
            PortMappingFailed { .. } => AlertCategory::PORT_MAPPING | AlertCategory::ERROR,

            // HOLEPUNCH (BEP 55)
            HolepunchSucceeded { .. } => AlertCategory::PEER,
            HolepunchFailed { .. } => AlertCategory::PEER | AlertCategory::ERROR,

            // I2P
            I2pSessionCreated { .. } => AlertCategory::I2P | AlertCategory::STATUS,
            I2pError { .. } => AlertCategory::I2P | AlertCategory::ERROR,

            // SSL
            SslTorrentError { .. } => AlertCategory::ERROR,

            // SESSION STATS (M50)
            SessionStatsAlert { .. } => AlertCategory::STATS,

            // NETWORK
            ExternalIpDetected { .. } => AlertCategory::STATUS,

            // SETTINGS
            SettingsChanged => AlertCategory::STATUS,
        }
    }
}

// ── Alert ─────────────────────────────────────────────────────────────

/// A timestamped event from the session or a torrent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    /// Wall-clock time when this alert was created.
    pub timestamp: SystemTime,
    /// The specific event that occurred.
    pub kind: AlertKind,
}

impl Alert {
    /// Create a new alert with the current wall-clock time.
    pub fn new(kind: AlertKind) -> Self {
        Self {
            timestamp: SystemTime::now(),
            kind,
        }
    }

    /// Shorthand: returns the category of `self.kind`.
    pub fn category(&self) -> AlertCategory {
        self.kind.category()
    }
}

// ── AlertStream (per-subscriber filter) ───────────────────────────────

/// A filtered view of the alert broadcast channel.
///
/// Drops alerts that don't match the subscriber's filter bitmask.
pub struct AlertStream {
    rx: broadcast::Receiver<Alert>,
    filter: AlertCategory,
}

impl AlertStream {
    /// Wrap a broadcast receiver with a category filter.
    pub fn new(rx: broadcast::Receiver<Alert>, filter: AlertCategory) -> Self {
        Self { rx, filter }
    }

    /// Receive the next alert matching this subscriber's filter.
    ///
    /// Alerts that don't match are silently dropped.
    pub async fn recv(&mut self) -> Result<Alert, broadcast::error::RecvError> {
        loop {
            let alert = self.rx.recv().await?;
            if alert.category().intersects(self.filter) {
                return Ok(alert);
            }
        }
    }
}

// ── post_alert (free function) ────────────────────────────────────────

/// Fire an alert if its category passes the session-level mask.
///
/// Called by both `SessionActor` and `TorrentActor`. The mask is an
/// `AtomicU32` shared between the handle and actors — no command roundtrip.
pub(crate) fn post_alert(tx: &broadcast::Sender<Alert>, mask: &AtomicU32, kind: AlertKind) {
    let alert = Alert::new(kind);
    let m = AlertCategory::from_bits_truncate(mask.load(Ordering::Relaxed));
    if alert.category().intersects(m) {
        let _ = tx.send(alert);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_category_all_includes_every_flag() {
        let all = AlertCategory::ALL;
        assert!(all.contains(AlertCategory::STATUS));
        assert!(all.contains(AlertCategory::ERROR));
        assert!(all.contains(AlertCategory::PEER));
        assert!(all.contains(AlertCategory::TRACKER));
        assert!(all.contains(AlertCategory::STORAGE));
        assert!(all.contains(AlertCategory::DHT));
        assert!(all.contains(AlertCategory::STATS));
        assert!(all.contains(AlertCategory::PIECE));
        assert!(all.contains(AlertCategory::BLOCK));
        assert!(all.contains(AlertCategory::PERFORMANCE));
        assert!(all.contains(AlertCategory::PORT_MAPPING));
        assert!(all.contains(AlertCategory::I2P));
    }

    #[test]
    fn alert_category_mapping() {
        use AlertKind::*;
        let info_hash = Id20::from_bytes(&[0u8; 20]).unwrap();

        let a = Alert::new(TorrentAdded {
            info_hash,
            name: String::new(),
        });
        assert!(a.category().contains(AlertCategory::STATUS));

        let a = Alert::new(PieceFinished {
            info_hash,
            piece: 0,
        });
        assert!(a.category().contains(AlertCategory::PIECE));

        let a = Alert::new(PeerConnected {
            info_hash,
            addr: "127.0.0.1:6881".parse().unwrap(),
        });
        assert!(a.category().contains(AlertCategory::PEER));

        // TrackerError maps to both TRACKER and ERROR
        let a = Alert::new(TrackerError {
            info_hash,
            url: String::new(),
            message: String::new(),
        });
        assert!(a.category().contains(AlertCategory::TRACKER));
        assert!(a.category().contains(AlertCategory::ERROR));
    }

    #[test]
    fn alert_has_timestamp() {
        let before = SystemTime::now();
        let alert = Alert::new(AlertKind::DhtBootstrapComplete);
        assert!(alert.timestamp >= before);
    }

    #[test]
    fn alert_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Alert>();
    }

    #[test]
    fn post_alert_respects_mask() {
        let (tx, mut rx) = broadcast::channel(16);
        let mask = AtomicU32::new(AlertCategory::STATUS.bits());

        // STATUS alert should pass
        post_alert(
            &tx,
            &mask,
            AlertKind::TorrentAdded {
                info_hash: Id20::from_bytes(&[0u8; 20]).unwrap(),
                name: "test".into(),
            },
        );
        assert!(rx.try_recv().is_ok());

        // PIECE alert should be filtered out
        post_alert(
            &tx,
            &mask,
            AlertKind::PieceFinished {
                info_hash: Id20::from_bytes(&[0u8; 20]).unwrap(),
                piece: 0,
            },
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn post_alert_empty_mask_blocks_all() {
        let (tx, mut rx) = broadcast::channel(16);
        let mask = AtomicU32::new(AlertCategory::empty().bits());

        post_alert(
            &tx,
            &mask,
            AlertKind::TorrentAdded {
                info_hash: Id20::from_bytes(&[0u8; 20]).unwrap(),
                name: "test".into(),
            },
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn alert_serializes_to_json() {
        let alert = Alert::new(AlertKind::TorrentAdded {
            info_hash: Id20::from_bytes(&[0u8; 20]).unwrap(),
            name: "test".into(),
        });
        let json = serde_json::to_string(&alert).unwrap();
        let decoded: Alert = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded.kind, AlertKind::TorrentAdded { .. }));
    }

    #[test]
    fn alert_category_serializes_as_u32() {
        let mask = AlertCategory::STATUS | AlertCategory::ERROR;
        let json = serde_json::to_string(&mask).unwrap();
        let decoded: AlertCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, mask);
    }

    #[test]
    fn queue_position_changed_alert_has_status_category() {
        let alert = Alert::new(AlertKind::TorrentQueuePositionChanged {
            info_hash: Id20::from([0u8; 20]),
            old_pos: 3,
            new_pos: 0,
        });
        assert!(alert.category().contains(AlertCategory::STATUS));
    }

    #[test]
    fn torrent_auto_managed_alert_has_status_category() {
        let alert = Alert::new(AlertKind::TorrentAutoManaged {
            info_hash: Id20::from([0u8; 20]),
            paused: true,
        });
        assert!(alert.category().contains(AlertCategory::STATUS));
    }

    #[test]
    fn scrape_reply_alert_has_tracker_category() {
        let alert = Alert::new(AlertKind::ScrapeReply {
            info_hash: Id20::from([0u8; 20]),
            url: "http://tracker.example.com/announce".into(),
            complete: 10,
            incomplete: 3,
            downloaded: 50,
        });
        assert!(alert.category().contains(AlertCategory::TRACKER));
    }

    #[test]
    fn scrape_error_alert_has_tracker_and_error_category() {
        let alert = Alert::new(AlertKind::ScrapeError {
            info_hash: Id20::from([0u8; 20]),
            url: "http://tracker.example.com/announce".into(),
            message: "connection refused".into(),
        });
        assert!(alert.category().contains(AlertCategory::TRACKER));
        assert!(alert.category().contains(AlertCategory::ERROR));
    }

    #[test]
    fn web_seed_banned_alert_has_status_category() {
        let alert = Alert::new(AlertKind::WebSeedBanned {
            info_hash: Id20::from([0u8; 20]),
            url: "http://example.com/files".into(),
        });
        assert!(alert.category().contains(AlertCategory::STATUS));
    }

    #[test]
    fn dht_node_id_violation_alert_category() {
        let alert = Alert::new(AlertKind::DhtNodeIdViolation {
            node_id: Id20::from([0u8; 20]),
            addr: "203.0.113.5:6881".parse().unwrap(),
        });
        assert!(alert.category().contains(AlertCategory::DHT));
        assert!(alert.category().contains(AlertCategory::ERROR));
    }

    #[test]
    fn dht_put_complete_alert_has_dht_category() {
        let alert = Alert::new(AlertKind::DhtPutComplete {
            target: Id20::from([0u8; 20]),
        });
        assert!(alert.category().contains(AlertCategory::DHT));
    }

    #[test]
    fn dht_sample_infohashes_alert_has_dht_category() {
        let alert = Alert::new(AlertKind::DhtSampleInfohashes {
            num_samples: 15,
            total_estimate: 500,
        });
        assert!(alert.category().contains(AlertCategory::DHT));
    }

    #[test]
    fn dht_item_error_alert_has_dht_and_error_category() {
        let alert = Alert::new(AlertKind::DhtItemError {
            target: Id20::from([0u8; 20]),
            message: "test".into(),
        });
        assert!(alert.category().contains(AlertCategory::DHT));
        assert!(alert.category().contains(AlertCategory::ERROR));
    }

    #[test]
    fn holepunch_succeeded_alert_has_peer_category() {
        let alert = Alert::new(AlertKind::HolepunchSucceeded {
            info_hash: Id20::from([0u8; 20]),
            addr: "203.0.113.5:6881".parse().unwrap(),
        });
        assert!(alert.category().contains(AlertCategory::PEER));
        assert!(!alert.category().contains(AlertCategory::ERROR));
    }

    #[test]
    fn holepunch_failed_alert_has_peer_and_error_category() {
        let alert = Alert::new(AlertKind::HolepunchFailed {
            info_hash: Id20::from([0u8; 20]),
            addr: "203.0.113.5:6881".parse().unwrap(),
            error_code: Some(1),
            message: "no support".into(),
        });
        assert!(alert.category().contains(AlertCategory::PEER));
        assert!(alert.category().contains(AlertCategory::ERROR));
    }

    #[test]
    fn i2p_session_created_alert_category() {
        let alert = Alert::new(AlertKind::I2pSessionCreated {
            b32_address: "abcdef1234567890abcdef1234567890abcdef1234567890abcd.b32.i2p".into(),
        });
        assert!(alert.category().contains(AlertCategory::I2P));
        assert!(alert.category().contains(AlertCategory::STATUS));
    }

    #[test]
    fn i2p_error_alert_category() {
        let alert = Alert::new(AlertKind::I2pError {
            message: "SAM bridge unreachable".into(),
        });
        assert!(alert.category().contains(AlertCategory::I2P));
        assert!(alert.category().contains(AlertCategory::ERROR));
    }

    #[test]
    fn alert_category_all_includes_i2p() {
        let all = AlertCategory::ALL;
        assert!(all.contains(AlertCategory::I2P));
    }

    #[test]
    fn ssl_torrent_error_alert_has_error_category() {
        let alert = Alert::new(AlertKind::SslTorrentError {
            info_hash: Id20::from([0u8; 20]),
            message: "handshake failed".into(),
        });
        assert!(alert.category().contains(AlertCategory::ERROR));
    }

    #[test]
    fn ssl_torrent_error_alert_serializes_to_json() {
        let alert = Alert::new(AlertKind::SslTorrentError {
            info_hash: Id20::from([0u8; 20]),
            message: "cert validation failed".into(),
        });
        let json = serde_json::to_string(&alert).unwrap();
        let decoded: Alert = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded.kind, AlertKind::SslTorrentError { .. }));
    }

    #[test]
    fn i2p_alert_serializes_to_json() {
        let alert = Alert::new(AlertKind::I2pSessionCreated {
            b32_address: "test.b32.i2p".into(),
        });
        let json = serde_json::to_string(&alert).unwrap();
        let decoded: Alert = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded.kind, AlertKind::I2pSessionCreated { .. }));
    }

    #[test]
    fn peer_turnover_alert_has_peer_category() {
        let alert = Alert::new(AlertKind::PeerTurnover {
            info_hash: Id20::from([0u8; 20]),
            disconnected: 2,
            replaced: 1,
        });
        assert!(alert.category().contains(AlertCategory::PEER));
    }

    #[test]
    fn session_stats_alert_has_stats_category() {
        let alert = Alert::new(AlertKind::SessionStatsAlert {
            values: vec![100, 200, 300],
        });
        assert!(alert.category().contains(AlertCategory::STATS));
    }

    #[test]
    fn session_stats_alert_serializes_to_json() {
        let alert = Alert::new(AlertKind::SessionStatsAlert {
            values: vec![1, 2, 3, 4, 5],
        });
        let json = serde_json::to_string(&alert).unwrap();
        let decoded: Alert = serde_json::from_str(&json).unwrap();
        match decoded.kind {
            AlertKind::SessionStatsAlert { values } => {
                assert_eq!(values, vec![1, 2, 3, 4, 5]);
            }
            _ => panic!("expected SessionStatsAlert"),
        }
    }
}
