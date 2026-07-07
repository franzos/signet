use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Bounded, short-TTL set of session ids recently found not-yet-fulfillable, so
/// repeated polls/floods of the same id cost at most one Stripe round-trip.
pub struct NegCache {
    ttl: Duration,
    cap: usize,
    inner: Mutex<HashMap<String, Instant>>,
}

impl NegCache {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            inner: Mutex::new(HashMap::new()),
        }
    }

    // std Mutex held only for the map op (no await inside): fine and idiomatic.
    pub fn contains(&self, key: &str) -> bool {
        let mut map = self.inner.lock().unwrap();
        match map.get(key) {
            Some(&t) if Instant::now().duration_since(t) < self.ttl => true,
            Some(_) => {
                map.remove(key);
                false
            }
            None => false,
        }
    }

    pub fn insert(&self, key: String) {
        let mut map = self.inner.lock().unwrap();
        if map.len() >= self.cap {
            let now = Instant::now();
            map.retain(|_, &mut t| now.duration_since(t) < self.ttl);
            if map.len() >= self.cap {
                map.clear();
            }
        }
        map.insert(key, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_contains() {
        let c = NegCache::new(Duration::from_secs(30), 10);
        c.insert("cs_1".into());
        assert!(c.contains("cs_1"));
        assert!(!c.contains("cs_missing"));
    }

    #[test]
    fn expired_entry_not_contained() {
        let c = NegCache::new(Duration::from_millis(0), 10);
        c.insert("cs_1".into());
        std::thread::sleep(Duration::from_millis(1));
        assert!(!c.contains("cs_1"));
    }

    #[test]
    fn cap_eviction_does_not_panic() {
        let c = NegCache::new(Duration::from_secs(30), 4);
        for i in 0..100 {
            c.insert(format!("cs_{i}"));
        }
    }
}
