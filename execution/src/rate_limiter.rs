use std::collections::VecDeque;
use std::time::Instant;
use tokio::sync::Mutex;
use std::sync::Arc;

/// Sliding-window rate limiter. Community reports suggest Polymarket throttles
/// above ~60–100 cancel/place requests per minute. We stay under 60/min by default.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<RateLimiterInner>>,
}

struct RateLimiterInner {
    max_per_minute: usize,
    calls: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(max_per_minute: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RateLimiterInner {
                max_per_minute,
                calls: VecDeque::new(),
            })),
        }
    }

    /// Wait until a request slot is available, then record the call.
    pub async fn acquire(&self) {
        loop {
            let sleep_ms = {
                let mut inner = self.inner.lock().await;
                let now = Instant::now();

                // Drop calls older than 60 seconds.
                while let Some(&front) = inner.calls.front() {
                    if now.duration_since(front).as_secs() >= 60 {
                        inner.calls.pop_front();
                    } else {
                        break;
                    }
                }

                if inner.calls.len() < inner.max_per_minute {
                    inner.calls.push_back(now);
                    0 // no wait needed
                } else {
                    // Wait until the oldest call falls out of the window.
                    let oldest = *inner.calls.front().unwrap();
                    let elapsed = now.duration_since(oldest).as_millis() as u64;
                    60_000u64.saturating_sub(elapsed) + 50 // +50ms buffer
                }
            };

            if sleep_ms == 0 {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(sleep_ms)).await;
        }
    }

    /// Current call count in the last 60 seconds.
    pub async fn current_rate(&self) -> usize {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        while let Some(&front) = inner.calls.front() {
            if now.duration_since(front).as_secs() >= 60 {
                inner.calls.pop_front();
            } else {
                break;
            }
        }
        inner.calls.len()
    }
}
