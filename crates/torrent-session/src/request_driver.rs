//! Per-peer request driver for concurrent piece dispatch.
//!
//! Each unchoked peer gets a driver task that autonomously reserves pieces
//! from the shared [`PieceReservationState`] and dispatches block requests
//! via semaphore-gated flow control. The driver does NOT go through the
//! `TorrentActor` — it interacts directly with the shared state and the
//! peer's command channel.

use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::piece_reservation::PieceReservationState;
use crate::types::PeerCommand;

/// Spawn a per-peer request driver task.
///
/// Returns a cancellation token (to stop the driver on choke/disconnect)
/// and the task's join handle.
///
/// Each semaphore permit corresponds to one in-flight block request.
/// When the actor receives block data, it calls `semaphore.add_permits(1)`
/// to let the driver send another request.
pub(crate) fn spawn_driver(
    peer_addr: SocketAddr,
    semaphore: Arc<Semaphore>,
    state: Arc<RwLock<PieceReservationState>>,
    piece_notify: Arc<Notify>,
    cmd_tx: mpsc::Sender<PeerCommand>,
) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        run_driver(peer_addr, semaphore, state, piece_notify, cmd_tx, cancel_clone).await;
    });
    (cancel, handle)
}

/// Core driver loop.
///
/// Acquires a semaphore permit, reserves a block from the shared state,
/// and sends a `PeerCommand::Request` to the peer. If no block is available,
/// waits for a notification that new pieces are ready.
async fn run_driver(
    peer_addr: SocketAddr,
    semaphore: Arc<Semaphore>,
    state: Arc<RwLock<PieceReservationState>>,
    piece_notify: Arc<Notify>,
    cmd_tx: mpsc::Sender<PeerCommand>,
    cancel: CancellationToken,
) {
    debug!(%peer_addr, "request driver started");

    loop {
        // 1. Acquire a semaphore permit (one permit = one in-flight block).
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                debug!(%peer_addr, "request driver cancelled during permit acquire");
                return;
            }
            result = semaphore.acquire() => {
                match result {
                    Ok(permit) => permit,
                    Err(_) => {
                        // Semaphore closed — shut down.
                        debug!(%peer_addr, "semaphore closed, request driver exiting");
                        return;
                    }
                }
            }
        };

        // 2. Try to get a block from the shared reservation state.
        loop {
            if cancel.is_cancelled() {
                debug!(%peer_addr, "request driver cancelled before lock acquire");
                // Drop the permit — returns it to the semaphore.
                drop(permit);
                return;
            }

            let block = state.write().next_request(peer_addr);

            if let Some(block) = block {
                // 3a. Block available — consume the permit and send the request.
                // `forget()` prevents the permit from being returned to the semaphore
                // when dropped. The actor will call `semaphore.add_permits(1)` when
                // it receives the block data.
                permit.forget();

                trace!(
                    %peer_addr,
                    piece = block.piece,
                    begin = block.begin,
                    length = block.length,
                    "dispatching block request"
                );

                if cmd_tx
                    .try_send(PeerCommand::Request {
                        index: block.piece,
                        begin: block.begin,
                        length: block.length,
                    })
                    .is_err()
                {
                    // Channel full or closed — peer is overwhelmed or gone.
                    debug!(%peer_addr, "peer command channel full/closed, driver exiting");
                    return;
                }

                // Successfully dispatched — go back to step 1 (acquire next permit).
                break;
            }

            // 3b. No block available — wait for a notification that new pieces
            // are ready, keeping the permit held so we can use it immediately.
            trace!(%peer_addr, "no block available, waiting for piece notification");

            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    debug!(%peer_addr, "request driver cancelled during piece wait");
                    drop(permit);
                    return;
                }
                () = piece_notify.notified() => {
                    // Retry next_request with the same permit.
                    trace!(%peer_addr, "piece notification received, retrying");
                }
            }
        }
    }
}
