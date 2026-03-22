//! Per-torrent tracker announce lifecycle management.
//!
//! Parses tracker URLs from torrent metadata, manages announce intervals,
//! and handles exponential backoff on failure.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::{debug, warn};

use torrent_core::{Id20, InfoHashes, TorrentMetaV1};
use torrent_tracker::{AnnounceEvent, AnnounceRequest, HttpTracker, UdpTracker};

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
    scrape_info: Option<torrent_tracker::ScrapeInfo>,
    consecutive_failures: u32,
}

/// Public tracker status (simplified view of internal TrackerState).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TrackerStatus {
    /// Tracker has not been contacted yet.
    NotContacted,
    /// Last announce succeeded.
    Working,
    /// Last announce failed.
    Error,
}

/// Public info about a single tracker.
#[derive(Debug, Clone, Serialize)]
pub struct TrackerInfo {
    /// Tracker announce URL.
    pub url: String,
    /// Tier index (lower = higher priority).
    pub tier: usize,
    /// Current status of this tracker.
    pub status: TrackerStatus,
    /// Number of seeders reported by the tracker (from scrape).
    pub seeders: Option<u32>,
    /// Number of leechers reported by the tracker (from scrape).
    pub leechers: Option<u32>,
    /// Total completed downloads reported by the tracker (from scrape).
    pub downloaded: Option<u32>,
    /// Seconds until the next scheduled announce.
    pub next_announce_secs: u64,
    /// Number of consecutive announce failures.
    pub consecutive_failures: u32,
}

/// Per-tracker announce outcome (success with peer count, or error message).
#[derive(Debug, Clone)]
pub(crate) struct TrackerOutcome {
    pub url: String,
    pub result: Result<usize, String>,
}

/// Result of announcing to all trackers: aggregated peers + per-tracker outcomes.
#[derive(Debug, Clone)]
pub(crate) struct AnnounceResult {
    pub peers: Vec<SocketAddr>,
    pub outcomes: Vec<TrackerOutcome>,
}

/// Per-torrent tracker manager.
///
/// Handles the announce lifecycle for all trackers associated with a torrent:
/// parsing URLs from metadata, scheduling announces, and managing backoff.
pub(crate) struct TrackerManager {
    trackers: Vec<TrackerEntry>,
    info_hash: Id20,
    info_hashes: InfoHashes,
    peer_id: Id20,
    port: u16,
    http_client: HttpTracker,
    udp_client: UdpTracker,
    anonymous_mode: bool,
    dscp: u8,
    /// I2P destination Base64 for BEP 7 tracker announces.
    i2p_destination: Option<String>,
}

impl TrackerManager {
    /// Create a TrackerManager from torrent metadata (unfiltered, for tests only).
    ///
    /// Parses `announce` and `announce_list` (BEP 12) fields, deduplicates URLs,
    /// and classifies each as HTTP or UDP.
    #[cfg(test)]
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
                            scrape_info: None,
                            consecutive_failures: 0,
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
                    scrape_info: None,
                    consecutive_failures: 0,
                });
            }
        }

        TrackerManager {
            trackers,
            info_hash: meta.info_hash,
            info_hashes: InfoHashes::v1_only(meta.info_hash),
            peer_id,
            port,
            http_client: HttpTracker::new(),
            udp_client: UdpTracker::new(),
            anonymous_mode: false,
            dscp: 0,
            i2p_destination: None,
        }
    }

    /// Create a TrackerManager from torrent metadata with URL security filtering.
    ///
    /// Same as [`from_torrent`](Self::from_torrent), but each URL is validated
    /// through [`validate_tracker_url`](crate::url_guard::validate_tracker_url).
    /// URLs that fail validation are logged at warn level and skipped.
    #[allow(dead_code)] // Wired in during Task 3 (TorrentActor integration).
    pub fn from_torrent_filtered(
        meta: &TorrentMetaV1,
        peer_id: Id20,
        port: u16,
        security: &crate::url_guard::UrlSecurityConfig,
        dscp: u8,
        anonymous_mode: bool,
    ) -> Self {
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
                    if let Err(e) = crate::url_guard::validate_tracker_url(&url, security) {
                        warn!(%url, %e, "tracker URL rejected by security policy");
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
                            scrape_info: None,
                            consecutive_failures: 0,
                        });
                    }
                }
            }
        }

        // Fallback: single announce URL (only if not already in announce_list)
        if let Some(ref url) = meta.announce {
            let url = url.trim().to_string();
            if !url.is_empty() && seen_urls.insert(url.clone()) {
                if let Err(e) = crate::url_guard::validate_tracker_url(&url, security) {
                    warn!(%url, %e, "tracker URL rejected by security policy");
                } else if let Some(protocol) = classify_url(&url) {
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
                        scrape_info: None,
                        consecutive_failures: 0,
                    });
                }
            }
        }

        TrackerManager {
            trackers,
            info_hash: meta.info_hash,
            info_hashes: InfoHashes::v1_only(meta.info_hash),
            peer_id,
            port,
            http_client: if anonymous_mode {
                HttpTracker::with_anonymous()
            } else {
                HttpTracker::new()
            },
            udp_client: UdpTracker::new().with_dscp(dscp),
            anonymous_mode,
            dscp,
            i2p_destination: None,
        }
    }

    /// Create an empty TrackerManager (for magnet links before metadata arrives).
    pub fn empty(
        info_hash: Id20,
        peer_id: Id20,
        port: u16,
        dscp: u8,
        anonymous_mode: bool,
    ) -> Self {
        TrackerManager {
            trackers: Vec::new(),
            info_hash,
            info_hashes: InfoHashes::v1_only(info_hash),
            peer_id,
            port,
            http_client: if anonymous_mode {
                HttpTracker::with_anonymous()
            } else {
                HttpTracker::new()
            },
            udp_client: UdpTracker::new().with_dscp(dscp),
            anonymous_mode,
            dscp,
            i2p_destination: None,
        }
    }

    /// Populate trackers from metadata once it's been fetched (unfiltered, for tests only).
    #[cfg(test)]
    pub fn set_metadata(&mut self, meta: &TorrentMetaV1) {
        let fresh = Self::from_torrent(meta, self.peer_id, self.port);
        self.trackers = fresh.trackers;
    }

    /// Populate trackers from metadata with URL security filtering (magnet link flow).
    pub fn set_metadata_filtered(
        &mut self,
        meta: &TorrentMetaV1,
        security: &crate::url_guard::UrlSecurityConfig,
    ) {
        let fresh = Self::from_torrent_filtered(
            meta,
            self.peer_id,
            self.port,
            security,
            self.dscp,
            self.anonymous_mode,
        );
        self.trackers = fresh.trackers;
    }

    /// Set the full info hashes for dual-swarm support (hybrid torrents).
    pub fn set_info_hashes(&mut self, info_hashes: InfoHashes) {
        self.info_hashes = info_hashes;
    }

    /// Set the I2P destination Base64 string for BEP 7 tracker announces.
    pub fn set_i2p_destination(&mut self, dest: Option<String>) {
        self.i2p_destination = dest;
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

    /// Announce to all trackers that are due.
    ///
    /// For hybrid torrents, announces both v1 and v2 info hashes separately
    /// to reach peers in both swarms. Returns all discovered peer addresses (deduplicated).
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
        let result = self
            .announce_with_hash(self.info_hash, event, uploaded, downloaded, left)
            .await;
        for peer in result.peers {
            if seen_peers.insert(peer) {
                all_peers.push(peer);
            }
        }
        all_outcomes.extend(result.outcomes);

        // Dual-swarm: also announce with v2 hash (truncated) if hybrid
        if self.info_hashes.is_hybrid()
            && let Some(v2) = self.info_hashes.v2
        {
            let v2_as_v1 = Id20(v2.0[..20].try_into().unwrap());
            // Only announce the v2 hash if it differs from the v1 hash
            if v2_as_v1 != self.info_hash {
                let result = self
                    .announce_with_hash(v2_as_v1, event, uploaded, downloaded, left)
                    .await;
                for peer in result.peers {
                    if seen_peers.insert(peer) {
                        all_peers.push(peer);
                    }
                }
                all_outcomes.extend(result.outcomes);
            }
        }

        AnnounceResult {
            peers: all_peers,
            outcomes: all_outcomes,
        }
    }

    /// Returns the port to announce (0 when anonymous mode is active).
    fn announce_port(&self) -> u16 {
        if self.anonymous_mode { 0 } else { self.port }
    }

    /// Internal: announce with a specific info hash to all due trackers.
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
            port: self.announce_port(),
            uploaded,
            downloaded,
            left,
            event,
            num_want: None,
            compact: true,
            i2p_destination: self.i2p_destination.clone(),
        };
        let now = Instant::now();

        // Spawn all eligible tracker announces in parallel
        type AnnounceOk = (
            Vec<SocketAddr>,
            u32,
            Option<String>,
            Option<u32>,
            Option<u32>,
        );
        let mut join_set =
            tokio::task::JoinSet::<(usize, Result<AnnounceOk, torrent_tracker::Error>)>::new();

        for (idx, tracker) in self.trackers.iter().enumerate() {
            // For the secondary (v2) hash, always announce (don't skip based on next_announce,
            // since the timer tracks the primary hash). For the primary hash, respect the timer.
            if hash == self.info_hash && tracker.next_announce > now {
                continue;
            }

            let http_client = self.http_client.clone();
            let udp_client = self.udp_client.clone();
            let url = tracker.url.clone();
            let protocol = tracker.protocol;
            let req = req.clone();

            join_set.spawn(async move {
                let result = match protocol {
                    TrackerProtocol::Http => {
                        Self::announce_http(&http_client, &url, &req).await
                    }
                    TrackerProtocol::Udp => {
                        Self::announce_udp(&udp_client, &url, &req).await
                    }
                };
                (idx, result)
            });
        }

        // Collect results and update tracker state
        let mut all_peers = Vec::new();
        let mut seen_peers = std::collections::HashSet::new();
        let mut outcomes = Vec::new();

        while let Some(Ok((idx, result))) = join_set.join_next().await {
            let tracker = &mut self.trackers[idx];
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
                            let prev_downloaded =
                                tracker.scrape_info.map(|s| s.downloaded).unwrap_or(0);
                            tracker.scrape_info = Some(torrent_tracker::ScrapeInfo {
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

        AnnounceResult {
            peers: all_peers,
            outcomes,
        }
    }

    /// Convenience: announce with Started event.
    #[allow(dead_code)]
    pub async fn announce_started(
        &mut self,
        uploaded: u64,
        downloaded: u64,
        left: u64,
    ) -> AnnounceResult {
        self.announce(AnnounceEvent::Started, uploaded, downloaded, left)
            .await
    }

    /// Convenience: announce with Completed event.
    pub async fn announce_completed(&mut self, uploaded: u64, downloaded: u64) -> AnnounceResult {
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
    ) -> std::result::Result<
        (
            Vec<SocketAddr>,
            u32,
            Option<String>,
            Option<u32>,
            Option<u32>,
        ),
        torrent_tracker::Error,
    > {
        let resp = client.announce(url, req).await?;
        Ok((
            resp.response.peers,
            resp.response.interval,
            resp.tracker_id,
            resp.response.seeders,
            resp.response.leechers,
        ))
    }

    async fn announce_udp(
        client: &UdpTracker,
        url: &str,
        req: &AnnounceRequest,
    ) -> std::result::Result<
        (
            Vec<SocketAddr>,
            u32,
            Option<String>,
            Option<u32>,
            Option<u32>,
        ),
        torrent_tracker::Error,
    > {
        // UDP tracker URLs are like "udp://tracker.example.com:6969/announce"
        // UdpTracker::announce expects "host:port"
        let addr = parse_udp_addr(url);
        let resp = client.announce(&addr, req).await?;
        Ok((
            resp.response.peers,
            resp.response.interval,
            None,
            resp.response.seeders,
            resp.response.leechers,
        ))
    }

    // ---- New public methods ----

    /// Get a list of all configured trackers with their status.
    pub fn tracker_list(&self) -> Vec<TrackerInfo> {
        self.trackers
            .iter()
            .map(|t| {
                let status = match t.state {
                    TrackerState::NeedsAnnounce => TrackerStatus::NotContacted,
                    TrackerState::Active => TrackerStatus::Working,
                    TrackerState::Failed { .. } => TrackerStatus::Error,
                };
                TrackerInfo {
                    url: t.url.clone(),
                    tier: t.tier,
                    status,
                    seeders: t.scrape_info.map(|s| s.complete),
                    leechers: t.scrape_info.map(|s| s.incomplete),
                    downloaded: t.scrape_info.map(|s| s.downloaded),
                    next_announce_secs: t
                        .next_announce
                        .saturating_duration_since(Instant::now())
                        .as_secs(),
                    consecutive_failures: t.consecutive_failures,
                }
            })
            .collect()
    }

    /// Force all trackers to re-announce immediately.
    pub fn force_reannounce(&mut self) {
        let now = Instant::now();
        for tracker in &mut self.trackers {
            tracker.next_announce = now;
        }
    }

    /// Add a new tracker URL (e.g. from lt_trackers exchange).
    ///
    /// Returns `true` if the URL was added, `false` if empty, unknown protocol, or duplicate.
    pub fn add_tracker_url(&mut self, url: &str) -> bool {
        let url = url.trim();
        if url.is_empty() {
            return false;
        }
        let Some(protocol) = classify_url(url) else {
            return false;
        };
        // Deduplicate
        if self.trackers.iter().any(|t| t.url == url) {
            return false;
        }
        let new_tier = self.trackers.last().map(|t| t.tier + 1).unwrap_or(0);
        self.trackers.push(TrackerEntry {
            url: url.to_string(),
            tier: new_tier,
            protocol,
            state: TrackerState::NeedsAnnounce,
            tracker_id: None,
            next_announce: Instant::now(),
            interval: DEFAULT_INTERVAL,
            backoff: Duration::ZERO,
            scrape_info: None,
            consecutive_failures: 0,
        });
        true
    }

    /// Replace all trackers with a new set of URLs.
    ///
    /// Clears the existing tracker list and adds each URL via `add_tracker_url`,
    /// which handles validation and deduplication.
    pub fn replace_all(&mut self, urls: Vec<String>) {
        self.trackers.clear();
        for url in &urls {
            self.add_tracker_url(url);
        }
    }

    /// Add a new tracker URL with URL security validation.
    ///
    /// Returns `true` if the URL was added, `false` if it failed validation,
    /// was empty, had an unknown protocol, or was a duplicate.
    #[allow(dead_code)] // Wired in during Task 3 (TorrentActor integration).
    pub fn add_tracker_url_validated(
        &mut self,
        url: &str,
        security: &crate::url_guard::UrlSecurityConfig,
    ) -> bool {
        let url = url.trim();
        if url.is_empty() {
            return false;
        }
        if let Err(e) = crate::url_guard::validate_tracker_url(url, security) {
            warn!(%url, %e, "tracker URL rejected by security policy");
            return false;
        }
        self.add_tracker_url(url)
    }

    /// Scrape trackers to get seeder/leecher counts.
    ///
    /// Tries each tracker until one succeeds. Returns `(url, ScrapeInfo)` from first success.
    pub async fn scrape(&self) -> Option<(String, torrent_tracker::ScrapeInfo)> {
        for tracker in &self.trackers {
            let result = match tracker.protocol {
                TrackerProtocol::Http => self
                    .http_client
                    .scrape(&tracker.url, &[self.info_hash])
                    .await
                    .ok()
                    .and_then(|resp| resp.files.get(&self.info_hash).copied()),
                TrackerProtocol::Udp => {
                    let addr = parse_udp_addr(&tracker.url);
                    self.udp_client
                        .scrape(&addr, &[self.info_hash])
                        .await
                        .ok()
                        .and_then(|resp| resp.results.into_iter().next())
                }
            };
            if let Some(info) = result {
                return Some((tracker.url.clone(), info));
            }
        }
        None
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
    use torrent_core::{Id20, InfoHashes};

    /// Helper to build a minimal TorrentMetaV1 with given tracker URLs.
    fn torrent_with_trackers(
        announce: Option<&str>,
        announce_list: Option<Vec<Vec<&str>>>,
    ) -> TorrentMetaV1 {
        use serde::Serialize;

        let data = vec![0u8; 16384];
        let hash = torrent_core::sha1(&data);
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

        let bytes = torrent_bencode::to_bytes(&t).unwrap();
        let mut meta = torrent_core::torrent_from_bytes(&bytes).unwrap();
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
        assert_eq!(parse_udp_addr("udp://example.com:1234"), "example.com:1234");
    }

    #[test]
    fn empty_manager_for_magnet() {
        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let mgr = TrackerManager::empty(info_hash, test_peer_id(), 6881, 0, false);
        assert_eq!(mgr.tracker_count(), 0);
    }

    #[test]
    fn set_metadata_populates_trackers() {
        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let mut mgr = TrackerManager::empty(info_hash, test_peer_id(), 6881, 0, false);
        assert_eq!(mgr.tracker_count(), 0);

        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        mgr.set_metadata(&meta);
        assert_eq!(mgr.tracker_count(), 1);
    }

    #[test]
    fn tracker_list_returns_info() {
        let meta = torrent_with_trackers(
            None,
            Some(vec![
                vec!["http://tracker1.example.com/announce"],
                vec!["udp://tracker2.example.com:6969/announce"],
            ]),
        );
        let mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        let list = mgr.tracker_list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].status, TrackerStatus::NotContacted);
        assert_eq!(list[0].seeders, None);
        assert_eq!(list[0].tier, 0);
        assert_eq!(list[1].tier, 1);
    }

    #[test]
    fn force_reannounce_resets_timers() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let mut mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        // Push next_announce far into the future
        mgr.trackers[0].next_announce = Instant::now() + Duration::from_secs(3600);
        assert!(mgr.next_announce_in().unwrap() > Duration::from_secs(3500));
        mgr.force_reannounce();
        let next = mgr.next_announce_in().unwrap();
        assert!(next <= Duration::from_millis(10));
    }

    #[test]
    fn add_tracker_url_new() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let mut mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        assert_eq!(mgr.tracker_count(), 1);
        let added = mgr.add_tracker_url("http://new-tracker.example.com/announce");
        assert!(added);
        assert_eq!(mgr.tracker_count(), 2);
        assert_eq!(mgr.trackers[1].tier, 1); // new tier
    }

    #[test]
    fn add_tracker_url_duplicate() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let mut mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        let added = mgr.add_tracker_url("http://tracker.example.com/announce");
        assert!(!added);
        assert_eq!(mgr.tracker_count(), 1);
    }

    #[test]
    fn tracker_manager_stores_info_hashes() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let mut mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        assert!(!mgr.info_hashes.is_hybrid());

        let v2 = torrent_core::Id32::from_hex(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .unwrap();
        mgr.set_info_hashes(InfoHashes::hybrid(meta.info_hash, v2));
        assert!(mgr.info_hashes.is_hybrid());
    }

    #[test]
    fn add_tracker_url_empty() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let mut mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        assert!(!mgr.add_tracker_url(""));
        assert!(!mgr.add_tracker_url("   "));
        assert_eq!(mgr.tracker_count(), 1);
    }

    fn ssrf_config() -> crate::url_guard::UrlSecurityConfig {
        crate::url_guard::UrlSecurityConfig {
            ssrf_mitigation: true,
            allow_idna: false,
            validate_https_trackers: true,
        }
    }

    #[test]
    fn localhost_tracker_announce_path_accepted() {
        // A localhost URL with /announce path should be accepted by the filter.
        let meta = torrent_with_trackers(Some("http://127.0.0.1:8080/announce"), None);
        let cfg = ssrf_config();
        let mgr =
            TrackerManager::from_torrent_filtered(&meta, test_peer_id(), 6881, &cfg, 0, false);
        assert_eq!(mgr.tracker_count(), 1);
        assert_eq!(mgr.trackers[0].url, "http://127.0.0.1:8080/announce");
    }

    #[test]
    fn localhost_tracker_bad_path_filtered() {
        // A localhost URL with a non-/announce path should be rejected,
        // while a global URL should pass.
        let meta = torrent_with_trackers(
            None,
            Some(vec![vec![
                "http://127.0.0.1:8080/api/admin",
                "http://tracker.example.com/announce",
            ]]),
        );
        let cfg = ssrf_config();
        let mgr =
            TrackerManager::from_torrent_filtered(&meta, test_peer_id(), 6881, &cfg, 0, false);
        assert_eq!(mgr.tracker_count(), 1);
        assert_eq!(mgr.trackers[0].url, "http://tracker.example.com/announce");
    }

    #[test]
    fn add_tracker_url_validates() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let cfg = ssrf_config();
        let mut mgr =
            TrackerManager::from_torrent_filtered(&meta, test_peer_id(), 6881, &cfg, 0, false);
        assert_eq!(mgr.tracker_count(), 1);

        // Valid URL should be added.
        assert!(mgr.add_tracker_url_validated("http://other.example.com/announce", &cfg));
        assert_eq!(mgr.tracker_count(), 2);

        // Localhost with bad path should be rejected.
        assert!(!mgr.add_tracker_url_validated("http://127.0.0.1:8080/api/admin", &cfg));
        assert_eq!(mgr.tracker_count(), 2);

        // UDP URL should pass (UDP skips SSRF checks).
        assert!(mgr.add_tracker_url_validated("udp://tracker.example.com:6969/announce", &cfg));
        assert_eq!(mgr.tracker_count(), 3);
    }

    #[test]
    fn anonymous_mode_zeroes_announce_port() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let cfg = ssrf_config();
        let mgr = TrackerManager::from_torrent_filtered(&meta, test_peer_id(), 6881, &cfg, 0, true);
        assert!(mgr.anonymous_mode);
        assert_eq!(mgr.announce_port(), 0);
    }

    #[test]
    fn normal_mode_includes_port() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let mgr = TrackerManager::from_torrent(&meta, test_peer_id(), 6881);
        assert!(!mgr.anonymous_mode);
        assert_eq!(mgr.announce_port(), 6881);
    }

    #[test]
    fn empty_manager_with_dscp_and_anonymous() {
        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let mgr = TrackerManager::empty(info_hash, test_peer_id(), 6881, 0x2E, true);
        assert!(mgr.anonymous_mode);
        assert_eq!(mgr.dscp, 0x2E);
        assert_eq!(mgr.announce_port(), 0);
    }
}
