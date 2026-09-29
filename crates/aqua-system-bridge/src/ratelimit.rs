//! Per-recipient sliding-window send limit. In memory: a daemon restart resets
//! it, which is acceptable for a guard against runaway loops (a restart takes
//! seconds and is visible in the journal).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub struct RateLimiter {
    max: usize,
    window: Duration,
    sends: HashMap<String, VecDeque<Instant>>,
}

impl RateLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        Self { max, window, sends: HashMap::new() }
    }

    fn trim(&mut self, key: &str, now: Instant) -> &mut VecDeque<Instant> {
        let q = self.sends.entry(key.to_ascii_lowercase()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= self.window) {
            q.pop_front();
        }
        q
    }

    /// Reserve one send for `key` at `now`. `Err(wait)` = over budget; `wait`
    /// is how long until the oldest send leaves the window.
    pub fn try_acquire(&mut self, key: &str, now: Instant) -> Result<(), Duration> {
        let (max, window) = (self.max, self.window);
        let q = self.trim(key, now);
        if q.len() >= max {
            let oldest = *q.front().expect("len >= max > 0");
            return Err(window.saturating_sub(now.duration_since(oldest)));
        }
        q.push_back(now);
        Ok(())
    }

    /// Give back the most recent reservation (the send failed before anything
    /// reached the recipient).
    pub fn release(&mut self, key: &str) {
        if let Some(q) = self.sends.get_mut(&key.to_ascii_lowercase()) {
            q.pop_back();
        }
    }

    /// Sends still available to `key` in the current window.
    pub fn remaining(&mut self, key: &str, now: Instant) -> usize {
        let max = self.max;
        max.saturating_sub(self.trim(key, now).len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_key_and_recovers() {
        let mut rl = RateLimiter::new(2, Duration::from_secs(10));
        let t0 = Instant::now();
        assert!(rl.try_acquire("@a:x", t0).is_ok());
        assert!(rl.try_acquire("@A:x", t0 + Duration::from_secs(1)).is_ok());
        let wait = rl.try_acquire("@a:x", t0 + Duration::from_secs(2)).unwrap_err();
        assert_eq!(wait, Duration::from_secs(8));
        // another key is independent
        assert!(rl.try_acquire("@b:x", t0).is_ok());
        // after the window the oldest slot frees
        assert!(rl.try_acquire("@a:x", t0 + Duration::from_secs(10)).is_ok());
        assert_eq!(rl.remaining("@a:x", t0 + Duration::from_secs(10)), 0);
    }

    #[test]
    fn release_returns_a_slot() {
        let mut rl = RateLimiter::new(1, Duration::from_secs(10));
        let t0 = Instant::now();
        rl.try_acquire("@a:x", t0).unwrap();
        assert!(rl.try_acquire("@a:x", t0).is_err());
        rl.release("@a:x");
        assert!(rl.try_acquire("@a:x", t0).is_ok());
    }
}
