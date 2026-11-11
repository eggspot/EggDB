use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MILLI: u64 = 1000;
const MAX_CLIENTS: usize = 16_384;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    milli_tokens: u64,
    refilled: Instant,
}

/// A token bucket per client address: each address may send `requests_per_second` requests a
/// second on average, and up to `burst` at once.
#[derive(Debug)]
pub struct RateLimiter {
    requests_per_second: u64,
    burst: u64,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

impl RateLimiter {
    /// A limiter of `requests_per_second` (at least 1) per address, with bursts of `burst`
    /// (at least `requests_per_second`).
    pub fn new(requests_per_second: u32, burst: u32) -> Self {
        let requests_per_second = u64::from(requests_per_second.max(1));
        Self {
            requests_per_second,
            burst: u64::from(burst).max(requests_per_second),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// The requests a second each address may send on average.
    pub fn requests_per_second(&self) -> u64 {
        self.requests_per_second
    }

    /// Takes one request from `client`'s bucket at `now`, or says how long until it holds one.
    pub fn admit_at(&self, client: IpAddr, now: Instant) -> Result<(), Duration> {
        let Ok(mut buckets) = self.buckets.lock() else {
            return Ok(());
        };
        let full = self.burst * MILLI;
        if buckets.len() >= MAX_CLIENTS && !buckets.contains_key(&client) {
            let rate = self.requests_per_second;
            buckets.retain(|_, bucket| {
                let refill = refill(bucket, now, rate);
                refill < full
            });
            if buckets.len() >= MAX_CLIENTS {
                return Err(Duration::from_secs(1));
            }
        }
        let bucket = buckets.entry(client).or_insert(Bucket {
            milli_tokens: full,
            refilled: now,
        });
        bucket.milli_tokens = refill(bucket, now, self.requests_per_second).min(full);
        bucket.refilled = now;
        if bucket.milli_tokens >= MILLI {
            bucket.milli_tokens -= MILLI;
            return Ok(());
        }
        let missing = MILLI - bucket.milli_tokens;
        let millis = missing.div_ceil(self.requests_per_second).max(1);
        Err(Duration::from_millis(millis))
    }

    /// Takes one request from `client`'s bucket now.
    pub fn admit(&self, client: IpAddr) -> Result<(), Duration> {
        self.admit_at(client, Instant::now())
    }
}

fn refill(bucket: &Bucket, now: Instant, requests_per_second: u64) -> u64 {
    let elapsed = now.saturating_duration_since(bucket.refilled);
    let gained = u64::try_from(elapsed.as_millis())
        .unwrap_or(u64::MAX)
        .saturating_mul(requests_per_second);
    bucket.milli_tokens.saturating_add(gained)
}

/// The answer to a request over its address's rate: `429 Too Many Requests` with
/// `Retry-After` in whole seconds.
pub fn too_many_requests_response(wait: Duration) -> crate::Response {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    crate::Response::text(429, "too many requests")
        .with_header("Retry-After", &seconds.max(1).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_admitted_then_the_rate_refills_it() {
        let limiter = RateLimiter::new(2, 3);
        let client: IpAddr = [127, 0, 0, 1].into();
        let start = Instant::now();
        for _ in 0..3 {
            assert_eq!(limiter.admit_at(client, start), Ok(()));
        }
        assert_eq!(
            limiter.admit_at(client, start),
            Err(Duration::from_millis(500))
        );
        assert_eq!(
            limiter.admit_at(client, start + Duration::from_millis(500)),
            Ok(())
        );
        let other: IpAddr = [127, 0, 0, 2].into();
        assert_eq!(limiter.admit_at(other, start), Ok(()));
    }

    #[test]
    fn the_answer_says_when_to_retry() {
        let response = too_many_requests_response(Duration::from_millis(1500));
        assert_eq!(response.status, 429);
        assert!(
            response
                .headers
                .iter()
                .any(|(name, value)| name == "Retry-After" && value == "2")
        );
    }
}
