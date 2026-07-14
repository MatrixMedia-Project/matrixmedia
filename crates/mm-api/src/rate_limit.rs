//! Per-key rate-limit using `governor 0.8`.
//! Quota: N attempts per hour, sliding window, in-memory only.
//! No persistence — restart resets all counters (acceptable for the small
//! signup volume at pilot scale; revisit if abuse pattern appears).

use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter, clock::Clock, clock::DefaultClock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{num::NonZeroU32, sync::Arc};

/// How often (in calls) to sweep keys that have fully replenished.
///
/// A keyed governor allocates one entry per distinct key and NEVER evicts on its own, so
/// the map grew without bound — and the key here is a client IP taken from
/// `X-Forwarded-For`, i.e. entirely attacker-controlled. A spray from spoofed addresses
/// grew the map until the process died, which is a denial of service against the very
/// thing meant to prevent one.
///
/// `retain_recent()` drops keys whose quota has FULLY replenished — such a key is
/// indistinguishable from one never seen, so evicting it changes no behaviour. For a
/// per-hour quota a bucket refills completely one hour after its last use, so the map
/// goes from "every IP ever seen" to "IPs seen in the last hour": bounded by real
/// traffic instead of by uptime.
///
/// Sweeping every 1024 calls keeps the amortised cost negligible.
const RETAIN_EVERY: u64 = 1024;

#[derive(Clone)]
pub struct SignupRateLimiter {
    inner: Arc<DefaultKeyedRateLimiter<String>>,
    calls: Arc<AtomicU64>,
}

impl SignupRateLimiter {
    pub fn new(per_ip_per_hour: u32) -> Self {
        let q = NonZeroU32::new(per_ip_per_hour.max(1)).expect("max(1) ensures non-zero");
        Self {
            inner: Arc::new(RateLimiter::keyed(Quota::per_hour(q))),
            calls: Arc::new(AtomicU64::new(0)),
        }
    }

    /// `Ok(())` if allowed; `Err(retry_after_ms)` if rate-limited.
    pub fn allow(&self, ip: &str) -> Result<(), u64> {
        // Amortised eviction: no background task to own, and the sweep runs on whichever
        // request happens to land on the boundary.
        if self.calls.fetch_add(1, Ordering::Relaxed).is_multiple_of(RETAIN_EVERY) {
            self.inner.retain_recent();
        }

        match self.inner.check_key(&ip.to_string()) {
            Ok(()) => Ok(()),
            Err(notuntil) => {
                let wait = notuntil.wait_time_from(DefaultClock::default().now());
                Err(wait.as_millis() as u64)
            }
        }
    }

    /// Build from an explicit quota. Test-only: the production quota is per-hour, whose
    /// buckets take an hour to refill — far too long to observe eviction in a unit test.
    /// A short-period quota exercises the identical `allow()` / `retain_recent()` path.
    #[cfg(test)]
    fn with_quota(q: Quota) -> Self {
        Self {
            inner: Arc::new(RateLimiter::keyed(q)),
            calls: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Number of keys currently held. Test-only visibility into the map's growth.
    #[cfg(test)]
    fn tracked_keys(&self) -> usize {
        self.inner.len()
    }

    /// Force a sweep of fully-replenished keys.
    #[cfg(test)]
    fn sweep(&self) {
        self.inner.retain_recent();
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

#[cfg(test)]
mod eviction_tests {
    use super::*;

    /// The regression: a keyed governor never evicts, and the key is a client IP read
    /// from `X-Forwarded-For` — attacker-controlled. Without a sweep, spraying distinct
    /// addresses grows the map until the process dies.
    #[test]
    fn replenished_keys_are_evicted() {
        // A 50ms-period quota behaves exactly like the production per-hour one, just on a
        // timescale a test can wait out.
        let q = Quota::with_period(std::time::Duration::from_millis(50))
            .expect("50ms is a valid period");
        let rl = SignupRateLimiter::with_quota(q);

        for i in 0..500 {
            let _ = rl.allow(&format!("10.0.0.{i}"));
        }
        assert!(
            rl.tracked_keys() > 0,
            "keys must be tracked while they hold state"
        );

        // Let every bucket refill, then sweep.
        std::thread::sleep(std::time::Duration::from_millis(250));
        rl.sweep();

        assert_eq!(
            rl.tracked_keys(),
            0,
            "fully-replenished keys must be evicted; otherwise the map grows without bound"
        );
    }

    /// The sweep must actually be reachable from `allow()` — a `retain_recent` that no
    /// call site ever triggers leaves the map just as unbounded as before.
    #[test]
    fn allow_triggers_a_sweep_on_the_boundary() {
        let q = Quota::with_period(std::time::Duration::from_millis(20))
            .expect("20ms is a valid period");
        let rl = SignupRateLimiter::with_quota(q);

        // `fetch_add` returns the PREVIOUS count, so the sweep fires on calls
        // 1, 1025, 2049, ... — i.e. after exactly RETAIN_EVERY calls have been made.
        for i in 0..(RETAIN_EVERY as usize) {
            let _ = rl.allow(&format!("10.1.{}.{}", i / 256, i % 256));
        }
        let before = rl.tracked_keys();
        assert!(before > 0, "keys accumulated");

        // Everything above has long since replenished; the next call lands on the
        // boundary and must sweep them.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = rl.allow("10.9.9.9");

        assert_eq!(
            rl.tracked_keys(),
            1,
            "allow() must trigger the sweep: {before} stale keys should be gone, leaving \
             only the key from the sweeping call itself"
        );
    }

    /// Eviction must not hand out free quota: a key still inside its window keeps its
    /// state through a sweep.
    #[test]
    fn sweeping_does_not_reset_an_active_limit() {
        let rl = SignupRateLimiter::new(2);
        assert!(rl.allow("7.7.7.7").is_ok());
        assert!(rl.allow("7.7.7.7").is_ok());
        assert!(rl.allow("7.7.7.7").is_err(), "quota exhausted");

        rl.sweep();

        assert!(
            rl.allow("7.7.7.7").is_err(),
            "a sweep must never reset a limit that is still in force"
        );
    }
}
