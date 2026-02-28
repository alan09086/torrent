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
use ferrite_core::Id20;

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
        /// All categories enabled.
        const ALL          = 0x7FF;
    }
}

// ── AlertKind ─────────────────────────────────────────────────────────

/// The specific event that occurred.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertKind {
    // ── Torrent lifecycle (STATUS) ──
    TorrentAdded { info_hash: Id20, name: String },
    TorrentRemoved { info_hash: Id20 },
    TorrentPaused { info_hash: Id20 },
    TorrentResumed { info_hash: Id20 },
    TorrentFinished { info_hash: Id20 },
    StateChanged { info_hash: Id20, prev_state: TorrentState, new_state: TorrentState },
    MetadataReceived { info_hash: Id20, name: String },

    // ── Checking (STATUS) ──
    TorrentChecked { info_hash: Id20, pieces_have: u32, pieces_total: u32 },
    CheckingProgress { info_hash: Id20, progress: f32 },

    // ── Transfer (PIECE / BLOCK) ──
    PieceFinished { info_hash: Id20, piece: u32 },
    BlockFinished { info_hash: Id20, piece: u32, offset: u32 },
    HashFailed { info_hash: Id20, piece: u32, contributors: Vec<std::net::IpAddr> },

    // ── Peers (PEER) ──
    PeerConnected { info_hash: Id20, addr: SocketAddr },
    PeerDisconnected { info_hash: Id20, addr: SocketAddr, reason: Option<String> },
    PeerBanned { info_hash: Id20, addr: SocketAddr },

    // ── Tracker (TRACKER) ──
    TrackerReply { info_hash: Id20, url: String, num_peers: usize },
    TrackerWarning { info_hash: Id20, url: String, message: String },
    TrackerError { info_hash: Id20, url: String, message: String },
    ScrapeReply { info_hash: Id20, url: String, complete: u32, incomplete: u32, downloaded: u32 },
    ScrapeError { info_hash: Id20, url: String, message: String },

    // ── DHT ──
    DhtBootstrapComplete,
    DhtGetPeers { info_hash: Id20, num_peers: usize },

    // ── Session (STATUS) ──
    ListenSucceeded { port: u16 },
    ListenFailed { port: u16, message: String },
    SessionStatsUpdate(crate::types::SessionStats),

    // ── Storage / Disk ──
    FileRenamed { info_hash: Id20, index: usize, new_path: std::path::PathBuf },
    StorageMoved { info_hash: Id20, new_path: std::path::PathBuf },
    FileError { info_hash: Id20, path: std::path::PathBuf, message: String },
    DiskStatsUpdate(crate::disk::DiskStats),

    // ── Resume (STATUS) ──
    ResumeDataSaved { info_hash: Id20 },

    // ── Error ──
    TorrentError { info_hash: Id20, message: String },

    // ── Performance ──
    PerformanceWarning { info_hash: Id20, message: String },

    // ── Queue management (STATUS) ──
    /// Queue position changed (manual move or torrent removal shifted positions).
    TorrentQueuePositionChanged {
        info_hash: Id20,
        old_pos: i32,
        new_pos: i32,
    },
    /// Torrent was paused or resumed by the auto-manage system.
    TorrentAutoManaged {
        info_hash: Id20,
        paused: bool,
    },

    // ── IP filtering (PEER) ──
    PeerBlocked { addr: SocketAddr },

    // ── Web seeding (STATUS) ──
    WebSeedBanned { info_hash: Id20, url: String },

    // ── Port mapping ──
    PortMappingSucceeded { port: u16, protocol: String },
    PortMappingFailed { port: u16, message: String },

    // ── Settings (M31) ──
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

            SessionStatsUpdate(_) => AlertCategory::STATS,

            // PIECE
            PieceFinished { .. } => AlertCategory::PIECE,
            HashFailed { .. } => AlertCategory::PIECE | AlertCategory::ERROR,

            // BLOCK
            BlockFinished { .. } => AlertCategory::BLOCK,

            // PEER
            PeerConnected { .. }
            | PeerDisconnected { .. }
            | PeerBanned { .. } => AlertCategory::PEER,

            // TRACKER
            TrackerReply { .. } => AlertCategory::TRACKER,
            TrackerWarning { .. } => AlertCategory::TRACKER,
            TrackerError { .. } => AlertCategory::TRACKER | AlertCategory::ERROR,
            ScrapeReply { .. } => AlertCategory::TRACKER,
            ScrapeError { .. } => AlertCategory::TRACKER | AlertCategory::ERROR,

            // DHT
            DhtBootstrapComplete | DhtGetPeers { .. } => AlertCategory::DHT,

            // STORAGE
            FileRenamed { .. }
            | StorageMoved { .. } => AlertCategory::STORAGE,
            FileError { .. } => AlertCategory::STORAGE | AlertCategory::ERROR,
            DiskStatsUpdate(_) => AlertCategory::STATS | AlertCategory::STORAGE,

            // ERROR
            TorrentError { .. } => AlertCategory::ERROR,

            // PERFORMANCE
            PerformanceWarning { .. } => AlertCategory::PERFORMANCE,

            // QUEUE MANAGEMENT (STATUS)
            TorrentQueuePositionChanged { .. } => AlertCategory::STATUS,
            TorrentAutoManaged { .. } => AlertCategory::STATUS,

            // IP FILTER
            PeerBlocked { .. } => AlertCategory::PEER,

            // WEB SEED
            WebSeedBanned { .. } => AlertCategory::STATUS,

            // PORT_MAPPING
            PortMappingSucceeded { .. } => AlertCategory::PORT_MAPPING,
            PortMappingFailed { .. } => AlertCategory::PORT_MAPPING | AlertCategory::ERROR,

            // SETTINGS
            SettingsChanged => AlertCategory::STATUS,
        }
    }
}

// ── Alert ─────────────────────────────────────────────────────────────

/// A timestamped event from the session or a torrent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub timestamp: SystemTime,
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
pub(crate) fn post_alert(
    tx: &broadcast::Sender<Alert>,
    mask: &AtomicU32,
    kind: AlertKind,
) {
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
    }

    #[test]
    fn alert_category_mapping() {
        use AlertKind::*;
        let info_hash = Id20::from_bytes(&[0u8; 20]).unwrap();

        let a = Alert::new(TorrentAdded { info_hash, name: String::new() });
        assert!(a.category().contains(AlertCategory::STATUS));

        let a = Alert::new(PieceFinished { info_hash, piece: 0 });
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
        post_alert(&tx, &mask, AlertKind::TorrentAdded {
            info_hash: Id20::from_bytes(&[0u8; 20]).unwrap(),
            name: "test".into(),
        });
        assert!(rx.try_recv().is_ok());

        // PIECE alert should be filtered out
        post_alert(&tx, &mask, AlertKind::PieceFinished {
            info_hash: Id20::from_bytes(&[0u8; 20]).unwrap(),
            piece: 0,
        });
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn post_alert_empty_mask_blocks_all() {
        let (tx, mut rx) = broadcast::channel(16);
        let mask = AtomicU32::new(AlertCategory::empty().bits());

        post_alert(&tx, &mask, AlertKind::TorrentAdded {
            info_hash: Id20::from_bytes(&[0u8; 20]).unwrap(),
            name: "test".into(),
        });
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
}
