//! Bounded blocking-task spawner that uses `block_in_place` instead of
//! `spawn_blocking`, eliminating per-call JoinHandle heap allocations.

use std::sync::Arc;

use tokio::sync::Semaphore;

/// Executes blocking closures on the current thread (via [`tokio::task::block_in_place`])
/// with bounded concurrency from a semaphore.
///
/// On a `CurrentThread` runtime, `block_in_place` panics, so the spawner falls
/// back to calling the closure directly.
#[derive(Clone, Debug)]
pub(crate) struct BlockingSpawner {
    allow_block_in_place: bool,
    semaphore: Arc<Semaphore>,
}

impl BlockingSpawner {
    /// Create a new spawner with the given concurrency limit.
    ///
    /// Detects the current tokio runtime flavor to decide whether
    /// `block_in_place` is safe.
    pub(crate) fn new(max_blocking: usize) -> Self {
        let flavor = tokio::runtime::Handle::current().runtime_flavor();
        let allow_block_in_place =
            matches!(flavor, tokio::runtime::RuntimeFlavor::MultiThread);

        Self {
            allow_block_in_place,
            semaphore: Arc::new(Semaphore::new(max_blocking)),
        }
    }

    /// Run a blocking closure, waiting for a semaphore permit first.
    ///
    /// On multi-thread runtimes this uses `block_in_place`; on single-thread
    /// runtimes it calls `f` directly.
    pub(crate) async fn block_in_place<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        // acquire_owned so the permit lives across the blocking call
        let _permit = self
            .semaphore
            .acquire()
            .await
            .expect("BlockingSpawner semaphore closed");

        if self.allow_block_in_place {
            tokio::task::block_in_place(f)
        } else {
            f()
        }
    }

    /// Synchronous variant for non-async contexts (e.g. deferred write paths).
    ///
    /// Does **not** acquire the semaphore — intended for fallback paths where
    /// blocking is unavoidable and already bounded by the caller.
    pub(crate) fn block_in_place_sync<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        if self.allow_block_in_place {
            tokio::task::block_in_place(f)
        } else {
            f()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn blocking_spawner_limits_concurrency() {
        let spawner = BlockingSpawner::new(2);
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_observed = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = spawner.clone();
            let c = Arc::clone(&concurrent);
            let m = Arc::clone(&max_observed);
            handles.push(tokio::spawn(async move {
                s.block_in_place(|| {
                    let prev = c.fetch_add(1, Ordering::SeqCst);
                    // Update max observed concurrency
                    let current = prev + 1;
                    m.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    c.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let max = max_observed.load(Ordering::SeqCst);
        assert!(
            max <= 2,
            "expected at most 2 concurrent ops, observed {max}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_spawner_semaphore_backpressure() {
        let spawner = BlockingSpawner::new(1);
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));

        let s1 = spawner.clone();
        let o1 = Arc::clone(&order);
        let h1 = tokio::spawn(async move {
            s1.block_in_place(|| {
                o1.lock().unwrap().push("first-start");
                std::thread::sleep(Duration::from_millis(80));
                o1.lock().unwrap().push("first-end");
            })
            .await;
        });

        // Give h1 a moment to acquire the permit
        tokio::time::sleep(Duration::from_millis(10)).await;

        let s2 = spawner.clone();
        let o2 = Arc::clone(&order);
        let h2 = tokio::spawn(async move {
            s2.block_in_place(|| {
                o2.lock().unwrap().push("second-start");
            })
            .await;
        });

        h1.await.unwrap();
        h2.await.unwrap();

        let log = order.lock().unwrap();
        // first-end must come before second-start (serialized by semaphore)
        let first_end = log.iter().position(|s| *s == "first-end").unwrap();
        let second_start = log.iter().position(|s| *s == "second-start").unwrap();
        assert!(
            first_end < second_start,
            "expected first-end before second-start, got: {log:?}"
        );
    }

    #[test]
    fn blocking_spawner_single_threaded_runtime() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let spawner = BlockingSpawner::new(2);
            // Must not panic on CurrentThread runtime
            let result = spawner.block_in_place(|| 42).await;
            assert_eq!(result, 42);

            // Sync variant also works
            let sync_result = spawner.block_in_place_sync(|| 99);
            assert_eq!(sync_result, 99);
        });
    }
}
