//! Smart banning: track peers that contribute to hash failures and ban repeat offenders.
//!
//! When a piece fails hash verification, the peers that sent data for it are
//! recorded as "striked". After a configurable number of strikes, the peer's
//! IP is session-wide banned. An optional parole mechanism re-downloads a
//! failed piece from a single uninvolved peer to definitively attribute fault.

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::IpAddr;

/// Configuration for smart banning behaviour.
#[derive(Debug, Clone)]
pub struct BanConfig {
    /// Number of hash-failure involvements before auto-ban (default: 3).
    pub max_failures: u32,
    /// If true, use parole to isolate the offending peer before striking (default: true).
    pub use_parole: bool,
}

impl Default for BanConfig {
    fn default() -> Self {
        Self {
            max_failures: 3,
            use_parole: true,
        }
    }
}

/// Session-wide ban manager shared across all torrents via `Arc<RwLock<_>>`.
///
/// Tracks per-IP strike counts and maintains a set of banned IPs.
#[derive(Debug)]
pub struct BanManager {
    config: BanConfig,
    banned: HashSet<IpAddr>,
    strikes: HashMap<IpAddr, u32>,
}

impl BanManager {
    /// Create a new manager with the given configuration.
    pub fn new(config: BanConfig) -> Self {
        Self {
            config,
            banned: HashSet::new(),
            strikes: HashMap::new(),
        }
    }

    /// Returns `true` if the IP is currently banned.
    pub fn is_banned(&self, ip: &IpAddr) -> bool {
        self.banned.contains(ip)
    }

    /// Increment the strike count for `ip`. Auto-bans at threshold.
    ///
    /// Returns `true` if the peer was just banned by this call.
    pub fn record_strike(&mut self, ip: IpAddr) -> bool {
        if self.banned.contains(&ip) {
            return false; // already banned
        }
        let count = self.strikes.entry(ip).or_insert(0);
        *count += 1;
        if *count >= self.config.max_failures {
            self.banned.insert(ip);
            true
        } else {
            false
        }
    }

    /// Manually ban an IP (e.g. via API).
    pub fn ban(&mut self, ip: IpAddr) {
        self.banned.insert(ip);
    }

    /// Remove an IP from the ban list and clear its strikes.
    ///
    /// Returns `true` if the IP was previously banned.
    pub fn unban(&mut self, ip: &IpAddr) -> bool {
        self.strikes.remove(ip);
        self.banned.remove(ip)
    }

    /// View the current set of banned IPs.
    pub fn banned_list(&self) -> &HashSet<IpAddr> {
        &self.banned
    }

    /// View the current strike counts.
    pub fn strikes_map(&self) -> &HashMap<IpAddr, u32> {
        &self.strikes
    }

    /// Whether parole mode is enabled.
    pub fn use_parole(&self) -> bool {
        self.config.use_parole
    }

    /// Rebuild a BanManager from persisted state.
    #[allow(dead_code)] // Used when restoring session state from disk
    pub fn restore(
        config: BanConfig,
        banned: HashSet<IpAddr>,
        strikes: HashMap<IpAddr, u32>,
    ) -> Self {
        Self {
            config,
            banned,
            strikes,
        }
    }
}

/// Per-piece parole state tracked by `TorrentActor`.
///
/// When a piece fails hash verification, we save the original contributors and
/// re-download from a single uninvolved peer. The result determines who gets striked.
#[derive(Debug, Clone)]
pub(crate) struct ParoleState {
    /// Peers that contributed data to the original (failed) download.
    pub original_contributors: HashSet<IpAddr>,
    /// The single peer assigned to re-download the piece on parole.
    pub parole_peer: Option<IpAddr>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ban_manager_empty() {
        let mgr = BanManager::new(BanConfig::default());
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(!mgr.is_banned(&ip));
        assert!(mgr.banned_list().is_empty());
        assert!(mgr.strikes_map().is_empty());
    }

    #[test]
    fn record_strike_below_threshold() {
        let mut mgr = BanManager::new(BanConfig {
            max_failures: 3,
            use_parole: true,
        });
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        assert!(!mgr.record_strike(ip)); // strike 1
        assert!(!mgr.record_strike(ip)); // strike 2
        assert!(!mgr.is_banned(&ip));
        assert_eq!(*mgr.strikes_map().get(&ip).unwrap(), 2);
    }

    #[test]
    fn record_strike_hits_threshold() {
        let mut mgr = BanManager::new(BanConfig {
            max_failures: 3,
            use_parole: true,
        });
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        assert!(!mgr.record_strike(ip)); // 1
        assert!(!mgr.record_strike(ip)); // 2
        assert!(mgr.record_strike(ip)); // 3 → banned
        assert!(mgr.is_banned(&ip));

        // Further strikes don't re-trigger
        assert!(!mgr.record_strike(ip));
    }

    #[test]
    fn manual_ban_unban() {
        let mut mgr = BanManager::new(BanConfig::default());
        let ip: IpAddr = "192.168.1.1".parse().unwrap();

        mgr.ban(ip);
        assert!(mgr.is_banned(&ip));

        assert!(mgr.unban(&ip));
        assert!(!mgr.is_banned(&ip));

        // Unban of non-banned returns false
        assert!(!mgr.unban(&ip));
    }

    #[test]
    fn unban_clears_strikes() {
        let mut mgr = BanManager::new(BanConfig {
            max_failures: 3,
            use_parole: true,
        });
        let ip: IpAddr = "10.0.0.5".parse().unwrap();

        mgr.record_strike(ip);
        mgr.record_strike(ip);
        assert_eq!(*mgr.strikes_map().get(&ip).unwrap(), 2);

        mgr.ban(ip);
        mgr.unban(&ip);
        assert!(mgr.strikes_map().get(&ip).is_none());
        assert!(!mgr.is_banned(&ip));
    }

    #[test]
    fn restore_preserves_state() {
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();

        let banned = HashSet::from([ip1]);
        let strikes = HashMap::from([(ip1, 3), (ip2, 1)]);

        let mgr = BanManager::restore(BanConfig::default(), banned, strikes);
        assert!(mgr.is_banned(&ip1));
        assert!(!mgr.is_banned(&ip2));
        assert_eq!(*mgr.strikes_map().get(&ip1).unwrap(), 3);
        assert_eq!(*mgr.strikes_map().get(&ip2).unwrap(), 1);
    }
}
