use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Client-side token bucket, sized below Bybit's published limits.
///
/// Enforcing the limit ourselves means a retry storm degrades into waiting
/// rather than escalating into an IP ban, which would take the bot offline
/// while positions are open.
#[derive(Debug)]
pub struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    state: Mutex<BucketState>,
}

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(capacity: u32, refill_per_sec: u32) -> Self {
        assert!(capacity > 0 && refill_per_sec > 0);
        RateLimiter {
            capacity: capacity as f64,
            refill_per_sec: refill_per_sec as f64,
            state: Mutex::new(BucketState {
                tokens: capacity as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Consume one token, waiting if the bucket is empty.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut st = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(st.last_refill).as_secs_f64();
                st.tokens = (st.tokens + elapsed * self.refill_per_sec).min(self.capacity);
                st.last_refill = now;

                if st.tokens >= 1.0 {
                    st.tokens -= 1.0;
                    return;
                }
                // Time until one whole token is available.
                Duration::from_secs_f64((1.0 - st.tokens) / self.refill_per_sec)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn tokens_within_capacity_are_immediate() {
        let limiter = RateLimiter::new(5, 5);
        let start = Instant::now();
        for _ in 0..5 {
            limiter.acquire().await;
        }
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "burst was throttled"
        );
    }

    #[tokio::test]
    async fn exceeding_capacity_waits_for_refill() {
        // 2 tokens capacity, refilling 2 per second: the third acquire must
        // wait roughly half a second.
        let limiter = RateLimiter::new(2, 2);
        limiter.acquire().await;
        limiter.acquire().await;
        let start = Instant::now();
        limiter.acquire().await;
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(400),
            "did not throttle: {waited:?}"
        );
        assert!(
            waited < Duration::from_millis(900),
            "throttled too long: {waited:?}"
        );
    }
}
