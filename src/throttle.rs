use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// Serializes and spaces out Amazon requests so bursts do not trigger rate limiting.
///
/// Even if several MCP tool calls arrive at once, every request goes through a
/// single queue with a minimum interval and a little random jitter, plus a
/// capped exponential backoff for retries.
pub struct Throttler {
    gate: Mutex<Instant>,
    min_interval: Duration,
    jitter_ms: u64,
    max_retries: u32,
    backoff_base: Duration,
}

impl Throttler {
    pub fn new(
        min_interval_ms: u64,
        jitter_ms: u64,
        max_retries: u32,
        backoff_base_ms: u64,
    ) -> Self {
        Self {
            gate: Mutex::new(Instant::now()),
            min_interval: Duration::from_millis(min_interval_ms),
            jitter_ms,
            max_retries,
            backoff_base: Duration::from_millis(backoff_base_ms),
        }
    }

    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Runs the future after all previously scheduled work, honouring the
    /// minimum interval plus jitter.
    pub async fn run<F, T>(&self, future: F) -> T
    where
        F: Future<Output = T>,
    {
        let mut last = self.gate.lock().await;
        let now = Instant::now();
        let target = *last + self.min_interval + Duration::from_millis(jitter(self.jitter_ms));
        if target > now {
            tokio::time::sleep(target - now).await;
        }
        let output = future.await;
        *last = Instant::now();
        output
    }

    /// Delay (with jitter) before retry number attempt (1-based).
    pub fn backoff(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(10);
        self.backoff_base * (1u32 << exponent) + Duration::from_millis(jitter(self.jitter_ms))
    }
}

/// Cheap non-cryptographic jitter derived from the current clock.
fn jitter(max_ms: u64) -> u64 {
    if max_ms == 0 {
        return 0;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()) % (max_ms + 1))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn serializes_work_and_grows_backoff() {
        let throttler = Throttler::new(0, 0, 3, 100);
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = {
            let order = order.clone();
            throttler.run(async move {
                order.lock().await.push(1);
            })
        };
        let second = {
            let order = order.clone();
            throttler.run(async move {
                order.lock().await.push(2);
            })
        };
        tokio::join!(first, second);
        assert_eq!(*order.lock().await, vec![1, 2]);

        assert!(throttler.backoff(1) < throttler.backoff(3));
        assert_eq!(throttler.max_retries(), 3);
    }

    #[tokio::test]
    async fn runs_every_scheduled_task() {
        let throttler = Throttler::new(0, 0, 0, 0);
        let counter = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..5 {
            let counter = counter.clone();
            tasks.push(throttler.run(async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }));
        }
        futures::future::join_all(tasks).await;
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }
}
