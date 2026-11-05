//! Sliding-window rate limiter for messages sent to Twitch.

use std::{collections::VecDeque, time::Instant};

use crate::config::RateLimitSpec;

#[derive(Debug)]
pub struct RateLimiter {
    spec: RateLimitSpec,
    sent: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(spec: RateLimitSpec) -> Self {
        Self {
            spec,
            sent: VecDeque::new(),
        }
    }

    /// Records a send and returns true if it is within the limit; returns false (and
    /// records nothing) otherwise.
    pub fn try_acquire(&mut self, now: Instant) -> bool {
        while self
            .sent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= self.spec.per)
        {
            self.sent.pop_front();
        }
        if self.sent.len() < self.spec.count as usize {
            self.sent.push_back(now);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn allows_count_per_window() {
        let mut rl = RateLimiter::new(RateLimitSpec {
            count: 2,
            per: Duration::from_secs(30),
        });
        let t0 = Instant::now();
        assert!(rl.try_acquire(t0));
        assert!(rl.try_acquire(t0 + Duration::from_secs(1)));
        assert!(!rl.try_acquire(t0 + Duration::from_secs(2)));
        assert!(rl.try_acquire(t0 + Duration::from_secs(30)));
    }
}
