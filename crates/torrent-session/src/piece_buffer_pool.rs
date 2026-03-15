//! Per-torrent buffer pool that limits concurrent in-flight pieces via a
//! semaphore and recycles `BytesMut` buffers to avoid repeated allocations.

use std::sync::{Arc, Mutex};

use bytes::BytesMut;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Statistics snapshot for a [`PieceBufferPool`].
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // Used in tests; will be wired to DiskStats in a future milestone.
pub(crate) struct PoolStats {
    /// Total number of permits (equal to pool size at construction).
    pub total: u32,
    /// Number of permits currently available (not held by callers).
    pub available_permits: u32,
    /// Number of `BytesMut` buffers currently cached (ready for reuse).
    pub buffers_cached: u32,
}

/// A bounded pool of reusable `BytesMut` buffers gated by a semaphore.
///
/// Each `acquire()` call obtains a semaphore permit (blocking if the pool is
/// exhausted) and returns a recycled buffer. When the caller finishes writing
/// the piece data to disk, it calls `release_buffer()` to return the buffer
/// to the cache *before* dropping the permit — this ensures the buffer is
/// available for the next waiter as soon as the permit is freed.
pub(crate) struct PieceBufferPool {
    semaphore: Arc<Semaphore>,
    buffers: Mutex<Vec<BytesMut>>,
    buf_capacity: usize,
    pool_size: u32,
}

impl std::fmt::Debug for PieceBufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PieceBufferPool")
            .field("buf_capacity", &self.buf_capacity)
            .field("pool_size", &self.pool_size)
            .finish_non_exhaustive()
    }
}

impl PieceBufferPool {
    /// Create a new pool with `pool_size` permits and pre-allocated buffers.
    ///
    /// Each buffer is created with `piece_size` bytes of capacity.
    pub(crate) fn new(pool_size: u32, piece_size: usize) -> Self {
        let mut buffers = Vec::with_capacity(pool_size as usize);
        for _ in 0..pool_size {
            buffers.push(BytesMut::with_capacity(piece_size));
        }
        Self {
            semaphore: Arc::new(Semaphore::new(pool_size as usize)),
            buffers: Mutex::new(buffers),
            buf_capacity: piece_size,
            pool_size,
        }
    }

    /// Acquire a buffer and semaphore permit.
    ///
    /// Blocks if all permits are held. Returns `Err` if the pool has been
    /// closed via [`close()`](Self::close).
    ///
    /// # Errors
    ///
    /// Returns [`tokio::sync::AcquireError`] if the semaphore is closed.
    pub(crate) async fn acquire(
        &self,
    ) -> Result<(BytesMut, OwnedSemaphorePermit), tokio::sync::AcquireError> {
        let permit = self.semaphore.clone().acquire_owned().await?;

        // Pop a cached buffer if available; otherwise allocate a fresh one.
        // The cache can be empty if a permit was freed by dropping it directly
        // (without calling `release_buffer` first).
        let buf = self
            .buffers
            .lock()
            .expect("buffer pool mutex poisoned")
            .pop()
            .unwrap_or_else(|| BytesMut::with_capacity(self.buf_capacity));

        Ok((buf, permit))
    }

    /// Return a buffer to the cache for reuse.
    ///
    /// The buffer is cleared (length reset to zero) but its allocated capacity
    /// is retained. This should be called *before* dropping the
    /// `OwnedSemaphorePermit` so that the next waiter finds a buffer ready.
    pub(crate) fn release_buffer(&self, mut buf: BytesMut) {
        buf.clear();
        self.buffers
            .lock()
            .expect("buffer pool mutex poisoned")
            .push(buf);
    }

    /// Close the pool, causing all pending and future `acquire()` calls to
    /// return `Err(AcquireError)`.
    #[allow(dead_code)] // Called from tests; will be wired to torrent shutdown.
    pub(crate) fn close(&self) {
        self.semaphore.close();
    }

    /// Return a snapshot of the pool's current state.
    #[allow(dead_code)] // Called from tests; will be wired to DiskStats in a future milestone.
    pub(crate) fn stats(&self) -> PoolStats {
        let available_permits =
            u32::try_from(self.semaphore.available_permits()).unwrap_or(u32::MAX);
        let buffers_cached = self
            .buffers
            .lock()
            .expect("buffer pool mutex poisoned")
            .len();
        let buffers_cached = u32::try_from(buffers_cached).unwrap_or(u32::MAX);

        PoolStats {
            total: self.pool_size,
            available_permits,
            buffers_cached,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    const PIECE_SIZE: usize = 512 * 1024; // 512 KiB
    const POOL_SIZE: u32 = 4;

    #[tokio::test]
    async fn acquire_release_cycle() {
        let pool = PieceBufferPool::new(POOL_SIZE, PIECE_SIZE);

        let (buf, permit) = pool.acquire().await.expect("acquire failed");
        assert!(buf.capacity() >= PIECE_SIZE);
        assert_eq!(buf.len(), 0);

        // After acquiring one, stats should reflect it.
        let s = pool.stats();
        assert_eq!(s.total, POOL_SIZE);
        assert_eq!(s.available_permits, POOL_SIZE - 1);
        assert_eq!(s.buffers_cached, POOL_SIZE - 1);

        // Release buffer, then drop permit.
        pool.release_buffer(buf);
        drop(permit);

        let s = pool.stats();
        assert_eq!(s.available_permits, POOL_SIZE);
        assert_eq!(s.buffers_cached, POOL_SIZE);
    }

    #[tokio::test]
    async fn acquire_blocks_when_exhausted() {
        let pool = Arc::new(PieceBufferPool::new(2, PIECE_SIZE));
        let mut permits = Vec::new();

        // Exhaust all permits.
        for _ in 0..2 {
            let (buf, permit) = pool.acquire().await.expect("acquire failed");
            permits.push((buf, permit));
        }

        // Spawn a task that tries to acquire — it should block.
        let pool2 = Arc::clone(&pool);
        let handle = tokio::spawn(async move { pool2.acquire().await });

        // Give the spawned task time to reach the semaphore wait.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished());

        // Release one permit — the waiter should unblock.
        let (buf, permit) = permits.pop().expect("no permits to release");
        pool.release_buffer(buf);
        drop(permit);

        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("timed out waiting for task")
            .expect("task panicked");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn permit_drop_unblocks_waiter() {
        let pool = Arc::new(PieceBufferPool::new(1, PIECE_SIZE));

        // Acquire the only permit.
        let (buf, permit) = pool.acquire().await.expect("acquire failed");

        // Spawn a waiter.
        let pool2 = Arc::clone(&pool);
        let handle = tokio::spawn(async move { pool2.acquire().await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished());

        // Drop permit WITHOUT calling release_buffer — simulates the buffer
        // being consumed (e.g., sent to disk). The waiter should still unblock
        // but will get a freshly allocated buffer (cache is empty).
        drop(buf);
        drop(permit);

        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("timed out waiting for task")
            .expect("task panicked");
        let (new_buf, _permit) = result.expect("acquire should succeed");
        // The buffer should still have the correct capacity (fresh allocation).
        assert!(new_buf.capacity() >= PIECE_SIZE);
    }

    #[tokio::test]
    async fn concurrent_acquire_release() {
        let pool = Arc::new(PieceBufferPool::new(4, PIECE_SIZE));
        let mut handles = Vec::new();

        for _ in 0..16 {
            let pool = Arc::clone(&pool);
            handles.push(tokio::spawn(async move {
                for _ in 0..10 {
                    let (buf, permit) = pool.acquire().await.expect("acquire failed");
                    // Simulate some work.
                    tokio::task::yield_now().await;
                    pool.release_buffer(buf);
                    drop(permit);
                }
            }));
        }

        for h in handles {
            timeout(Duration::from_secs(5), h)
                .await
                .expect("task timed out")
                .expect("task panicked");
        }

        // All permits should be back.
        let s = pool.stats();
        assert_eq!(s.available_permits, 4);
    }

    #[tokio::test]
    async fn stats_accuracy() {
        let pool = PieceBufferPool::new(4, PIECE_SIZE);

        let s = pool.stats();
        assert_eq!(s.total, 4);
        assert_eq!(s.available_permits, 4);
        assert_eq!(s.buffers_cached, 4);

        let (buf1, permit1) = pool.acquire().await.expect("acquire 1 failed");
        let (buf2, permit2) = pool.acquire().await.expect("acquire 2 failed");

        let s = pool.stats();
        assert_eq!(s.total, 4);
        assert_eq!(s.available_permits, 2);
        assert_eq!(s.buffers_cached, 2);

        pool.release_buffer(buf1);
        drop(permit1);

        let s = pool.stats();
        assert_eq!(s.total, 4);
        assert_eq!(s.available_permits, 3);
        assert_eq!(s.buffers_cached, 3);

        pool.release_buffer(buf2);
        drop(permit2);

        let s = pool.stats();
        assert_eq!(s.total, 4);
        assert_eq!(s.available_permits, 4);
        assert_eq!(s.buffers_cached, 4);
    }

    #[tokio::test]
    async fn close_returns_error() {
        let pool = PieceBufferPool::new(2, PIECE_SIZE);
        pool.close();

        let result = pool.acquire().await;
        assert!(result.is_err());
    }
}
