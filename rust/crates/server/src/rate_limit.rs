//! `RateLimit`: an in-process counter per key with a TTL from the first hit
//! (`ActiveSupport::Cache::MemoryStore#increment` with `expires_in`). Like the Rails
//! memory store, limits are per process.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Default)]
pub struct RateLimit {
    counters: Arc<Mutex<HashMap<String, (u32, Instant)>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("Too many requests")]
pub struct LimitApproached;

impl RateLimit {
    /// Counts a hit; fails once `key` was hit more than `limit` times within `ttl`.
    pub fn call(&self, key: &str, limit: u32, ttl: Duration) -> Result<(), LimitApproached> {
        let now = Instant::now();
        let mut counters = self.counters.lock().expect("rate limit lock");

        if counters.len() > 10_000 {
            counters.retain(|_, (_, expires)| *expires > now);
        }

        let entry = counters
            .entry(key.to_owned())
            .and_modify(|(count, expires)| {
                if *expires <= now {
                    *count = 0;
                    *expires = now + ttl;
                }
            })
            .or_insert((0, now + ttl));
        entry.0 += 1;

        if entry.0 > limit {
            Err(LimitApproached)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_within_ttl() {
        let limit = RateLimit::default();
        for _ in 0..5 {
            limit.call("k", 5, Duration::from_mins(5)).unwrap();
        }
        assert!(limit.call("k", 5, Duration::from_mins(5)).is_err());
        assert!(limit.call("other", 5, Duration::from_mins(5)).is_ok());

        limit.call("short", 1, Duration::ZERO).unwrap();
        limit.call("short", 1, Duration::ZERO).unwrap();
    }
}
