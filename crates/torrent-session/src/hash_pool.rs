//! Dedicated thread pool for CPU-bound piece hash verification (M96).
//!
//! Workers receive `HashJob`s via `tokio::sync::mpsc` (bridged to `std::sync::mpsc`),
//! compute SHA1 hashes with `catch_unwind` panic recovery, and send results back
//! via per-torrent `tokio::sync::mpsc::Sender` carried in each job.

use std::thread;

use tracing::{debug, error};

/// Internal enum threaded through the std mpsc to workers.
/// `Shutdown` is sent during `Drop` to unblock workers without requiring the
/// async bridge task to have flushed first.
enum WorkerMsg {
    Job(HashJob),
    Shutdown,
}

/// Job submitted to the hash pool.
#[derive(Debug)]
pub struct HashJob {
    /// Piece index to verify.
    pub piece: u32,
    /// Expected SHA1 hash.
    pub expected: torrent_core::Id20,
    /// Generation counter for staleness detection.
    pub generation: u64,
    /// Pre-extracted piece data.
    pub data: Vec<u8>,
    /// Per-torrent result sender.
    pub result_tx: tokio::sync::mpsc::Sender<HashResult>,
}

/// Result of a hash verification.
#[derive(Debug)]
pub struct HashResult {
    /// Piece index that was verified.
    pub piece: u32,
    /// Whether the hash matched.
    pub passed: bool,
    /// Generation counter (for staleness check by caller).
    pub generation: u64,
}

/// A thread pool dedicated to CPU-bound piece hash verification.
///
/// Uses `tokio::sync::mpsc` for async job submission and `std::sync::mpsc`
/// internally for the blocking worker threads. Results are sent back via
/// per-torrent `tokio::sync::mpsc::Sender` carried in each `HashJob`.
pub struct HashPool {
    /// Async sender for submitting jobs.
    job_tx: tokio::sync::mpsc::Sender<HashJob>,
    /// Direct std sender for sending `Shutdown` sentinels in `Drop`.
    worker_tx: std::sync::mpsc::SyncSender<WorkerMsg>,
    /// Worker thread join handles (for clean shutdown).
    workers: Vec<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for HashPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashPool")
            .field("workers", &self.workers.len())
            .finish()
    }
}

impl HashPool {
    /// Create a new hash pool with `num_workers` threads.
    ///
    /// `job_capacity` bounds the submission channel (backpressure if full).
    ///
    /// **Must be called from within a tokio runtime** — spawns a bridge task.
    pub fn new(num_workers: usize, job_capacity: usize) -> Self {
        let (job_tx, mut job_async_rx) = tokio::sync::mpsc::channel::<HashJob>(job_capacity);
        // +num_workers headroom so Drop can always enqueue shutdown sentinels.
        let (worker_tx, worker_rx) =
            std::sync::mpsc::sync_channel::<WorkerMsg>(job_capacity + num_workers);

        // Bridge task: forwards jobs from tokio channel to std channel.
        let bridge_tx = worker_tx.clone();
        tokio::spawn(async move {
            while let Some(job) = job_async_rx.recv().await {
                if bridge_tx.send(WorkerMsg::Job(job)).is_err() {
                    break;
                }
            }
        });

        // Spawn worker threads
        let worker_rx = std::sync::Arc::new(std::sync::Mutex::new(worker_rx));
        let mut workers = Vec::with_capacity(num_workers);
        for id in 0..num_workers {
            let rx = worker_rx.clone();
            let handle = thread::Builder::new()
                .name(format!("hash-worker-{id}"))
                .spawn(move || {
                    Self::worker_loop(id, rx);
                })
                .expect("failed to spawn hash worker thread");
            workers.push(handle);
        }

        HashPool {
            job_tx,
            worker_tx,
            workers,
        }
    }

    /// Submit a hash job. Returns `Err` if the pool has been shut down.
    pub async fn submit(&self, job: HashJob) -> Result<(), HashJob> {
        self.job_tx.send(job).await.map_err(|e| e.0)
    }

    fn worker_loop(
        id: usize,
        rx: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<WorkerMsg>>>,
    ) {
        loop {
            let msg = {
                let rx = rx.lock().unwrap();
                match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => break, // Channel closed
                }
            };

            let job = match msg {
                WorkerMsg::Shutdown => break,
                WorkerMsg::Job(job) => job,
            };

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let actual = torrent_core::sha1(&job.data);
                actual == job.expected
            }));

            let passed = match result {
                Ok(passed) => passed,
                Err(panic) => {
                    error!(
                        worker = id,
                        piece = job.piece,
                        "hash worker panicked: {:?}",
                        panic.downcast_ref::<String>()
                    );
                    false
                }
            };

            if job
                .result_tx
                .blocking_send(HashResult {
                    piece: job.piece,
                    passed,
                    generation: job.generation,
                })
                .is_err()
            {
                // Torrent removed — result dropped, worker continues
                continue;
            }
        }
        debug!(worker = id, "hash worker exiting");
    }
}

impl Drop for HashPool {
    fn drop(&mut self) {
        // Send one Shutdown sentinel per worker so each unblocks from recv().
        // This works even if the async bridge task hasn't flushed yet, because
        // we hold a direct std sender (`worker_tx`) that bypasses the bridge.
        for _ in 0..self.workers.len() {
            let _ = self.worker_tx.try_send(WorkerMsg::Shutdown);
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_pool_parallel_correctness() {
        let pool = HashPool::new(2, 16);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);

        // Submit 10 jobs: 5 matching, 5 mismatched
        for i in 0u32..10 {
            let data = format!("piece-data-{i}").into_bytes();
            let expected = if i < 5 {
                torrent_core::sha1(&data)
            } else {
                torrent_core::Id20([0xff; 20])
            };
            pool.submit(HashJob {
                piece: i,
                expected,
                generation: 0,
                data,
                result_tx: tx.clone(),
            })
            .await
            .unwrap();
        }

        let mut results = Vec::new();
        for _ in 0..10 {
            results.push(rx.recv().await.unwrap());
        }

        results.sort_by_key(|r| r.piece);
        assert_eq!(results.len(), 10);
        for r in &results[..5] {
            assert!(r.passed, "piece {} should pass", r.piece);
        }
        for r in &results[5..] {
            assert!(!r.passed, "piece {} should fail", r.piece);
        }
    }

    #[tokio::test]
    async fn hash_pool_stale_generation_discard() {
        let pool = HashPool::new(1, 8);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        let data = b"piece five data".to_vec();
        let expected = torrent_core::sha1(&data);
        pool.submit(HashJob {
            piece: 5,
            expected,
            generation: 1,
            data,
            result_tx: tx,
        })
        .await
        .unwrap();

        let r = rx.recv().await.unwrap();
        assert_eq!(r.piece, 5);
        assert!(r.passed);
        assert_eq!(r.generation, 1);

        let current_gen = 2u64;
        assert!(r.generation != current_gen, "generation 1 result should be stale when current is 2");
    }

    #[tokio::test]
    async fn hash_pool_concurrent_cancel_resubmit() {
        let pool = HashPool::new(2, 16);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);

        let data1 = b"piece42-attempt1".to_vec();
        let expected1 = torrent_core::sha1(&data1);
        pool.submit(HashJob {
            piece: 42,
            expected: expected1,
            generation: 1,
            data: data1,
            result_tx: tx.clone(),
        })
        .await
        .unwrap();

        let data2 = b"piece42-attempt2".to_vec();
        let expected2 = torrent_core::sha1(&data2);
        pool.submit(HashJob {
            piece: 42,
            expected: expected2,
            generation: 2,
            data: data2,
            result_tx: tx,
        })
        .await
        .unwrap();

        let r1 = rx.recv().await.unwrap();
        let r2 = rx.recv().await.unwrap();
        let mut results = vec![r1, r2];
        results.sort_by_key(|r| r.generation);

        assert_eq!(results[0].generation, 1);
        assert!(results[0].passed);
        assert_eq!(results[1].generation, 2);
        assert!(results[1].passed);

        let current_gen = 2u64;
        assert!(results[0].generation != current_gen);
        assert!(results[1].generation == current_gen);
    }

    #[tokio::test]
    async fn hash_pool_shutdown() {
        let pool = HashPool::new(2, 8);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        // Submit a few jobs
        for i in 0u32..3 {
            let data = format!("data-{i}").into_bytes();
            let expected = torrent_core::sha1(&data);
            pool.submit(HashJob {
                piece: i,
                expected,
                generation: 0,
                data,
                result_tx: tx.clone(),
            })
            .await
            .unwrap();
        }

        // Collect results
        for _ in 0..3 {
            let r = rx.recv().await.unwrap();
            assert!(r.passed);
        }

        // Drop pool — should join worker threads cleanly
        drop(pool);
        // Drop our tx clone so channel can close
        drop(tx);

        // After drop, recv should return None (channel closed)
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn hash_pool_failure_recovery() {
        let pool = HashPool::new(1, 8);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        // Submit a job with mismatched hash
        let data = b"corrupt data".to_vec();
        let expected = torrent_core::Id20([0xAA; 20]); // Wrong hash
        pool.submit(HashJob {
            piece: 0,
            expected,
            generation: 0,
            data,
            result_tx: tx.clone(),
        })
        .await
        .unwrap();

        let r = rx.recv().await.unwrap();
        assert!(!r.passed, "hash mismatch should report failure");
        assert_eq!(r.piece, 0);

        // Worker should continue — submit a correct job
        let data2 = b"good data".to_vec();
        let expected2 = torrent_core::sha1(&data2);
        pool.submit(HashJob {
            piece: 1,
            expected: expected2,
            generation: 0,
            data: data2,
            result_tx: tx,
        })
        .await
        .unwrap();

        let r2 = rx.recv().await.unwrap();
        assert!(r2.passed, "correct hash should pass");
        assert_eq!(r2.piece, 1);
    }
}
