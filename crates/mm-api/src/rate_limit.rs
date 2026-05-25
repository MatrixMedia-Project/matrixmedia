//! Per-IP signup rate-limit using `governor 0.8`.
//! Quota: N attempts per hour, sliding window, in-memory only.
//! No persistence — restart resets all counters (acceptable for the small
//! signup volume at pilot scale; revisit if abuse pattern appears).

use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter, clock::Clock, clock::DefaultClock};
use std::{num::NonZeroU32, sync::Arc};

#[derive(Clone)]
pub struct SignupRateLimiter {
    inner: Arc<DefaultKeyedRateLimiter<String>>,
}

impl SignupRateLimiter {
    pub fn new(per_ip_per_hour: u32) -> Self {
        let q = NonZeroU32::new(per_ip_per_hour.max(1)).expect("max(1) ensures non-zero");
        Self {
            inner: Arc::new(RateLimiter::keyed(Quota::per_hour(q))),
        }
    }

    /// `Ok(())` if allowed; `Err(retry_after_ms)` if rate-limited.
    pub fn allow(&self, ip: &str) -> Result<(), u64> {
        match self.inner.check_key(&ip.to_string()) {
            Ok(()) => Ok(()),
            Err(notuntil) => {
                let wait = notuntil.wait_time_from(DefaultClock::default().now());
                Err(wait.as_millis() as u64)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_within_quota() {
        let rl = SignupRateLimiter::new(3);
        for _ in 0..3 {
            assert!(rl.allow("1.2.3.4").is_ok(), "should allow within quota");
        }
    }

    #[test]
    fn blocks_when_quota_exceeded() {
        let rl = SignupRateLimiter::new(2);
        assert!(rl.allow("9.9.9.9").is_ok());
        assert!(rl.allow("9.9.9.9").is_ok());
        let r = rl.allow("9.9.9.9");
        assert!(r.is_err(), "third call should be blocked");
        let ms = r.unwrap_err();
        assert!(ms > 0 && ms <= 3_600_000, "retry_after_ms should be in (0, 1h]");
    }

    #[test]
    fn different_ips_independent() {
        let rl = SignupRateLimiter::new(1);
        assert!(rl.allow("1.1.1.1").is_ok());
        assert!(rl.allow("2.2.2.2").is_ok(), "different IPs should be independent");
    }
}
