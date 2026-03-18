//! M107: Semaphore-paced peer admission.
//!
//! Provides a dedicated async task that receives discovered peer addresses from
//! an unbounded channel, validates them (dedup, ban, IP filter, backoff), acquires
//! a semaphore permit (blocking when at connection capacity), and forwards
//! validated `(addr, source, permit)` triples to the actor for connection spawning.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::{Semaphore, mpsc};
use tracing::trace;

use crate::peer_state::PeerSource;
use crate::session::{SharedBanManager, SharedIpFilter};

/// Message sent from the adder to the actor when a connection slot is available.
pub(crate) struct ConnectPeer {
    /// The peer address to connect to.
    pub addr: SocketAddr,
    /// How this peer was discovered.
    pub source: PeerSource,
    /// Semaphore permit — held for the peer's entire connection lifetime.
    pub permit: tokio::sync::OwnedSemaphorePermit,
}

/// Dedicated async task that paces outbound peer connections via semaphore.
///
/// Receives peer addresses from an unbounded channel (fed by `handle_add_peers`),
/// validates them, acquires a semaphore permit (blocking if at capacity), and
/// sends the validated `(addr, source, permit)` to the actor for connection spawning.
///
/// # Shutdown
///
/// Returns when:
/// - The input channel closes (all senders dropped = torrent stopped).
/// - The semaphore is closed (session shutting down).
/// - The `connect_tx` channel is closed (actor gone).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn peer_adder_task(
    mut rx: mpsc::UnboundedReceiver<(SocketAddr, PeerSource)>,
    semaphore: Arc<Semaphore>,
    peers_connected: Arc<DashMap<SocketAddr, ()>>,
    connect_backoff: Arc<DashMap<SocketAddr, (Instant, u32)>>,
    ban_manager: SharedBanManager,
    ip_filter: SharedIpFilter,
    connect_tx: mpsc::Sender<ConnectPeer>,
    mut clear_seen_rx: tokio::sync::watch::Receiver<u64>,
) {
    let mut seen = HashSet::new();
    loop {
        let (addr, source) = {
            tokio::select! {
                biased;
                // M107: Clear seen set when DHT re-query triggers
                Ok(()) = clear_seen_rx.changed() => {
                    seen.clear();
                    continue;
                }
                result = rx.recv() => {
                    match result {
                        Some(pair) => pair,
                        None => return, // channel closed = torrent stopped
                    }
                }
            }
        };

        // Pre-permit validation (cheap checks first)
        if !seen.insert(addr) {
            trace!(%addr, "peer_adder: duplicate, skipping");
            continue;
        }
        if addr.port() == 0 {
            trace!(%addr, "peer_adder: port zero, skipping");
            continue;
        }
        if ban_manager
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_banned(&addr.ip())
        {
            trace!(%addr, "peer_adder: banned, skipping");
            continue;
        }
        if ip_filter
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_blocked(addr.ip())
        {
            trace!(%addr, "peer_adder: IP-filtered, skipping");
            continue;
        }
        if peers_connected.contains_key(&addr) {
            trace!(%addr, "peer_adder: already connected, skipping");
            continue;
        }
        if let Some(entry) = connect_backoff.get(&addr)
            && Instant::now() < entry.0
        {
            trace!(%addr, "peer_adder: in backoff, skipping");
            continue;
        }

        // Block until a connection slot is available
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return, // semaphore closed = shutting down
        };

        // Post-permit revalidation (state may have changed while blocked)
        if peers_connected.contains_key(&addr) {
            trace!(%addr, "peer_adder: connected while waiting for permit, skipping");
            drop(permit);
            continue;
        }

        // Send to actor for connection spawning
        if connect_tx
            .send(ConnectPeer {
                addr,
                source,
                permit,
            })
            .await
            .is_err()
        {
            return; // actor gone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use crate::ban::{BanConfig, BanManager};
    use crate::ip_filter::IpFilter;

    fn test_ban_manager() -> SharedBanManager {
        Arc::new(std::sync::RwLock::new(
            BanManager::new(BanConfig::default()),
        ))
    }

    fn test_ip_filter() -> SharedIpFilter {
        Arc::new(std::sync::RwLock::new(IpFilter::new()))
    }

    fn test_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)), port)
    }

    fn test_addr_ip(last_octet: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last_octet)), port)
    }

    /// Helper to spawn the adder task and return the handles.
    fn spawn_adder(
        semaphore: Arc<Semaphore>,
    ) -> (
        mpsc::UnboundedSender<(SocketAddr, PeerSource)>,
        mpsc::Receiver<ConnectPeer>,
        Arc<DashMap<SocketAddr, ()>>,
        Arc<DashMap<SocketAddr, (Instant, u32)>>,
        SharedBanManager,
        SharedIpFilter,
        tokio::task::JoinHandle<()>,
        tokio::sync::watch::Sender<u64>,
    ) {
        let (peer_tx, peer_rx) = mpsc::unbounded_channel();
        let (connect_tx, connect_rx) = mpsc::channel(64);
        let peers_connected = Arc::new(DashMap::new());
        let connect_backoff = Arc::new(DashMap::new());
        let ban_manager = test_ban_manager();
        let ip_filter = test_ip_filter();
        let (clear_seen_tx, clear_seen_rx) = tokio::sync::watch::channel(0u64);

        let handle = tokio::spawn(peer_adder_task(
            peer_rx,
            semaphore,
            Arc::clone(&peers_connected),
            Arc::clone(&connect_backoff),
            Arc::clone(&ban_manager),
            Arc::clone(&ip_filter),
            connect_tx,
            clear_seen_rx,
        ));

        (
            peer_tx,
            connect_rx,
            peers_connected,
            connect_backoff,
            ban_manager,
            ip_filter,
            handle,
            clear_seen_tx,
        )
    }

    #[tokio::test]
    async fn adder_connects_on_permit_available() {
        let sem = Arc::new(Semaphore::new(10));
        let (peer_tx, mut connect_rx, ..) = spawn_adder(sem);

        let addr = test_addr(6881);
        peer_tx.send((addr, PeerSource::Tracker)).unwrap();

        let cp = tokio::time::timeout(Duration::from_secs(1), connect_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(cp.addr, addr);
        assert_eq!(cp.source, PeerSource::Tracker);
    }

    #[tokio::test]
    async fn adder_blocks_at_capacity() {
        let sem = Arc::new(Semaphore::new(1));
        let (peer_tx, mut connect_rx, ..) = spawn_adder(sem);

        // First peer should go through immediately
        let addr1 = test_addr_ip(1, 6881);
        let addr2 = test_addr_ip(2, 6882);
        peer_tx.send((addr1, PeerSource::Dht)).unwrap();
        let cp1 = tokio::time::timeout(Duration::from_secs(1), connect_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(cp1.addr, addr1);

        // Second peer should block (semaphore exhausted)
        peer_tx.send((addr2, PeerSource::Dht)).unwrap();
        let result = tokio::time::timeout(Duration::from_millis(100), connect_rx.recv()).await;
        assert!(result.is_err(), "should have timed out");

        // Drop first permit → second should unblock
        drop(cp1.permit);
        let cp2 = tokio::time::timeout(Duration::from_secs(1), connect_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(cp2.addr, addr2);
    }

    #[tokio::test]
    async fn adder_deduplicates_peers() {
        let sem = Arc::new(Semaphore::new(10));
        let (peer_tx, mut connect_rx, ..) = spawn_adder(sem);

        let addr = test_addr(6881);
        peer_tx.send((addr, PeerSource::Tracker)).unwrap();
        peer_tx.send((addr, PeerSource::Dht)).unwrap(); // duplicate

        // First should arrive
        let cp = tokio::time::timeout(Duration::from_secs(1), connect_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(cp.addr, addr);

        // Second should NOT arrive (dedup)
        let result = tokio::time::timeout(Duration::from_millis(100), connect_rx.recv()).await;
        assert!(result.is_err(), "duplicate should not produce ConnectPeer");
    }

    #[tokio::test]
    async fn adder_skips_banned_peers() {
        let sem = Arc::new(Semaphore::new(10));
        let (peer_tx, mut connect_rx, _, _, ban_manager, ..) = spawn_adder(sem);

        let addr = test_addr(6881);
        ban_manager.write().unwrap().ban(addr.ip());
        peer_tx.send((addr, PeerSource::Tracker)).unwrap();

        let result = tokio::time::timeout(Duration::from_millis(100), connect_rx.recv()).await;
        assert!(result.is_err(), "banned peer should be skipped");
    }

    #[tokio::test]
    async fn adder_skips_peers_in_backoff() {
        let sem = Arc::new(Semaphore::new(10));
        let (peer_tx, mut connect_rx, _, connect_backoff, ..) = spawn_adder(sem);

        let addr = test_addr(6881);
        // Set backoff 10 seconds in the future
        connect_backoff.insert(addr, (Instant::now() + Duration::from_secs(10), 1));
        peer_tx.send((addr, PeerSource::Tracker)).unwrap();

        let result = tokio::time::timeout(Duration::from_millis(100), connect_rx.recv()).await;
        assert!(result.is_err(), "peer in backoff should be skipped");
    }

    #[tokio::test]
    async fn adder_skips_port_zero() {
        let sem = Arc::new(Semaphore::new(10));
        let (peer_tx, mut connect_rx, ..) = spawn_adder(sem);

        let addr = test_addr(0); // port 0
        peer_tx.send((addr, PeerSource::Tracker)).unwrap();

        let result = tokio::time::timeout(Duration::from_millis(100), connect_rx.recv()).await;
        assert!(result.is_err(), "port-zero peer should be skipped");
    }

    #[tokio::test]
    async fn adder_skips_ip_filtered() {
        let sem = Arc::new(Semaphore::new(10));
        let (peer_tx, mut connect_rx, _, _, _, ip_filter, ..) = spawn_adder(sem);

        let addr = test_addr(6881);
        // Block the IP range
        ip_filter.write().unwrap().add_rule(
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 0)),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 255)),
            1,
        );
        peer_tx.send((addr, PeerSource::Tracker)).unwrap();

        let result = tokio::time::timeout(Duration::from_millis(100), connect_rx.recv()).await;
        assert!(result.is_err(), "IP-filtered peer should be skipped");
    }

    #[tokio::test]
    async fn adder_revalidates_after_permit_wait() {
        let sem = Arc::new(Semaphore::new(1));
        let (peer_tx, mut connect_rx, peers_connected, ..) = spawn_adder(sem);

        // First peer takes the only permit
        let addr1 = test_addr_ip(1, 6881);
        let addr2 = test_addr_ip(2, 6882);
        peer_tx.send((addr1, PeerSource::Dht)).unwrap();
        let cp1 = tokio::time::timeout(Duration::from_secs(1), connect_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");

        // Second peer queues behind the semaphore
        peer_tx.send((addr2, PeerSource::Dht)).unwrap();
        // While it's waiting, mark addr2 as already connected
        peers_connected.insert(addr2, ());

        // Release permit — adder should acquire it, then skip addr2 due to revalidation
        drop(cp1.permit);

        // addr2 should NOT come through
        let result = tokio::time::timeout(Duration::from_millis(200), connect_rx.recv()).await;
        assert!(
            result.is_err(),
            "peer connected during wait should be skipped after revalidation"
        );
    }

    #[tokio::test]
    async fn adder_exits_on_channel_close() {
        let sem = Arc::new(Semaphore::new(10));
        let (peer_tx, _connect_rx, _, _, _, _, handle, _) = spawn_adder(sem);

        drop(peer_tx);

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("timed out")
            .expect("task panicked");
    }

    #[tokio::test]
    async fn adder_exits_on_semaphore_close() {
        let sem = Arc::new(Semaphore::new(10));
        let sem_clone = Arc::clone(&sem);
        let (peer_tx, _connect_rx, _, _, _, _, handle, _) = spawn_adder(sem);

        // Close semaphore — adder should exit on next acquire
        sem_clone.close();

        // Send a peer to trigger the acquire path
        let addr = test_addr(6881);
        let _ = peer_tx.send((addr, PeerSource::Tracker));

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("timed out")
            .expect("task panicked");
    }

    #[tokio::test]
    async fn clear_seen_allows_resubmission() {
        let sem = Arc::new(Semaphore::new(10));
        let (peer_tx, mut connect_rx, _, _, _, _, _, clear_seen_tx) = spawn_adder(sem);

        let addr = test_addr(6881);
        peer_tx.send((addr, PeerSource::Dht)).unwrap();

        // First submission should go through
        let cp = tokio::time::timeout(Duration::from_secs(1), connect_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(cp.addr, addr);

        // Second submission of same addr should be deduped
        peer_tx.send((addr, PeerSource::Dht)).unwrap();
        let result = tokio::time::timeout(Duration::from_millis(100), connect_rx.recv()).await;
        assert!(result.is_err(), "duplicate should not produce ConnectPeer");

        // Signal clear_seen
        let _ = clear_seen_tx.send(1);
        // Give the adder task a moment to process the clear signal
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now the same addr should go through again
        peer_tx.send((addr, PeerSource::Dht)).unwrap();
        let cp2 = tokio::time::timeout(Duration::from_secs(1), connect_rx.recv())
            .await
            .expect("timed out after clear_seen")
            .expect("channel closed after clear_seen");
        assert_eq!(cp2.addr, addr);
    }
}
