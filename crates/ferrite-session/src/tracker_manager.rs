//! Per-torrent tracker announce lifecycle management.
//!
//! Parses tracker URLs from torrent metadata, manages announce intervals,
//! and handles exponential backoff on failure.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use ferrite_core::{Id20, TorrentMetaV1};
use ferrite_tracker::{AnnounceEvent, AnnounceRequest, HttpTracker, UdpTracker};

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
    Failed { _error: String },
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
            if !url.is_empty()
                && seen_urls.insert(url.clone())
                && let Some(protocol) = classify_url(&url)
            {
                trackers.push(TrackerEntry {
                    url,
                    tier: if trackers.is_empty() {
                        0
                    } else {
                        trackers.last().unwrap().tier + 1
                    },
                    protocol,
                    state: TrackerState::NeedsAnnounce,
                    tracker_id: None,
                    next_announce: Instant::now(),
                    interval: DEFAULT_INTERVAL,
                    backoff: Duration::ZERO,
                });
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
    #[cfg(test)]
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
                        _error: e.to_string(),
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
    #[allow(dead_code)]
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
    let without_scheme = url.strip_prefix("udp://").unwrap_or(url);
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
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
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
                vec![
                    "http://tier0-a.example.com/announce",
                    "http://tier0-b.example.com/announce",
                ],
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
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        // Newly created — should be ready to announce immediately
        let next = mgr.next_announce_in().unwrap();
        assert!(next <= Duration::from_millis(10));
    }

    #[test]
    fn backoff_on_failure() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let mut mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);

        // Simulate a failure by directly setting state
        mgr.trackers[0].state = TrackerState::Failed {
            _error: "connection refused".into(),
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
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
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

        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        mgr.set_metadata(&meta);
        assert_eq!(mgr.tracker_count(), 1);
    }
}
