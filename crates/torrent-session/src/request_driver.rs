//! Per-peer request driver: acquires semaphore permits and signals the torrent
//! actor to pick and dispatch blocks.
//!
//! The driver does NOT directly call the picker or send requests to peers.
//! Instead, it acquires a semaphore permit (blocking until capacity is
//! available), then sends a [`DriverMessage::NeedBlocks`] to the torrent actor
//! via an mpsc channel. The torrent actor handles the actual pick + dispatch.
//! The driver's sole job is **timing** — acquiring permits and signalling
//! readiness.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

/// Messages sent from the per-peer request driver to the torrent actor.
#[derive(Debug)]
pub(crate) enum DriverMessage {
    /// The driver acquired a permit and needs the torrent actor to pick +
    /// dispatch a block.
    NeedBlocks,
}

/// Per-peer async loop that acquires semaphore permits and signals the torrent
/// actor to pick and dispatch blocks.
///
/// The driver acquires one permit at a time from the peer's semaphore. Each
/// acquired permit is [`forget`](tokio::sync::SemaphorePermit::forget)ten —
/// ownership is conceptually transferred to the torrent actor, which calls
/// [`release_permit()`](crate::pipeline::PeerPipelineState::release_permit)
/// when the corresponding block arrives.
///
/// When the peer is snubbed, the driver parks on [`Notify`] instead of
/// acquiring permits, avoiding wasted requests to unresponsive peers.
pub(crate) async fn request_driver(
    semaphore: Arc<Semaphore>,
    notify: Arc<Notify>,
    snubbed: Arc<AtomicBool>,
    driver_tx: mpsc::Sender<(SocketAddr, DriverMessage)>,
    peer_addr: SocketAddr,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }

        // Snub probe mode: wait for notification before attempting requests.
        if snubbed.load(Ordering::Acquire) {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = notify.notified() => continue, // snub cleared or new pieces
            }
        }

        // Acquire a permit (blocks until capacity is available).
        let permit = tokio::select! {
            () = cancel.cancelled() => return,
            result = semaphore.acquire() => {
                match result {
                    Ok(permit) => permit,
                    Err(_) => return, // semaphore closed
                }
            }
        };

        // Transfer permit ownership to the torrent actor. It will call
        // release_permit() when the block is received.
        permit.forget();

        // Signal the torrent actor to pick and dispatch a block for this peer.
        if driver_tx.send((peer_addr, DriverMessage::NeedBlocks)).await.is_err() {
            return; // torrent actor dropped the receiver
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_addr(n: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, n as u8)), 6881)
    }

    #[tokio::test]
    async fn driver_sends_need_blocks_per_permit() {
        let sem = Arc::new(Semaphore::new(3));
        let notify = Arc::new(Notify::new());
        let snubbed = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(16);
        let addr = test_addr(1);

        let cancel2 = cancel.clone();
        let handle = tokio::spawn(request_driver(sem, notify, snubbed, tx, addr, cancel2));

        // Should receive exactly 3 NeedBlocks (one per permit)
        for _ in 0..3 {
            let (peer, msg) = rx.recv().await.unwrap();
            assert_eq!(peer, addr);
            assert!(matches!(msg, DriverMessage::NeedBlocks));
        }

        // Driver should now be blocked on acquire (0 permits left).
        // Cancel to clean up.
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn driver_stops_on_cancel() {
        let sem = Arc::new(Semaphore::new(0)); // no permits — driver will block
        let notify = Arc::new(Notify::new());
        let snubbed = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(16);

        let cancel2 = cancel.clone();
        let handle = tokio::spawn(request_driver(sem, notify, snubbed, tx, test_addr(1), cancel2));

        cancel.cancel();
        // Should return promptly, not hang
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("driver should stop within 1s")
            .unwrap();
    }

    #[tokio::test]
    async fn driver_stops_on_semaphore_close() {
        let sem = Arc::new(Semaphore::new(0));
        let notify = Arc::new(Notify::new());
        let snubbed = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(16);

        let handle = tokio::spawn(request_driver(
            Arc::clone(&sem),
            notify,
            snubbed,
            tx,
            test_addr(1),
            cancel,
        ));

        sem.close(); // closing the semaphore should make acquire return Err
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("driver should stop within 1s")
            .unwrap();
    }

    #[tokio::test]
    async fn driver_snub_mode_waits_for_notify() {
        let sem = Arc::new(Semaphore::new(10)); // plenty of permits
        let notify = Arc::new(Notify::new());
        let snubbed = Arc::new(AtomicBool::new(true)); // start snubbed
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(16);

        let notify2 = Arc::clone(&notify);
        let snubbed2 = Arc::clone(&snubbed);
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(request_driver(
            sem, notify2, snubbed2, tx, test_addr(1), cancel2,
        ));

        // Give driver time to enter snub wait
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Should NOT have sent any messages (snubbed)
        assert!(rx.try_recv().is_err());

        // Clear snub and wake
        snubbed.store(false, Ordering::Release);
        notify.notify_one();

        // Now should get a NeedBlocks
        let (_, msg) = rx.recv().await.unwrap();
        assert!(matches!(msg, DriverMessage::NeedBlocks));

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn driver_stops_when_channel_dropped() {
        let sem = Arc::new(Semaphore::new(5));
        let notify = Arc::new(Notify::new());
        let snubbed = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::channel(16);

        let handle = tokio::spawn(request_driver(
            sem, notify, snubbed, tx, test_addr(1), cancel,
        ));

        // Drop the receiver — driver's send should fail
        drop(rx);

        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("driver should stop within 1s")
            .unwrap();
    }
}
