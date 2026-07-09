use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Why a session id was recently found not fulfillable. Kept apart so an
/// unpaid session keeps rendering "processing" while an unknown id renders
/// a not-found page instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NegEntry {
    /// Session exists on Stripe but is not paid yet.
    Unpaid,
    /// Stripe has no such session (unknown or long-expired id).
    NotFound,
}

/// Bounded, short-TTL map of session ids recently found not-yet-fulfillable,
/// so repeated polls/floods of the same id cost at most one Stripe round-trip.
pub struct NegCache {
    ttl: Duration,
    cap: usize,
    inner: Mutex<HashMap<String, (Instant, NegEntry)>>,
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
    pub fn get(&self, key: &str) -> Option<NegEntry> {
        let mut map = self.inner.lock().unwrap();
        match map.get(key) {
            Some(&(t, entry)) if Instant::now().duration_since(t) < self.ttl => Some(entry),
            Some(_) => {
                map.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn insert(&self, key: String, entry: NegEntry) {
        let mut map = self.inner.lock().unwrap();
        if map.len() >= self.cap {
            let now = Instant::now();
            map.retain(|_, &mut (t, _)| now.duration_since(t) < self.ttl);
            if map.len() >= self.cap {
                map.clear();
            }
        }
        map.insert(key, (Instant::now(), entry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_get() {
        let c = NegCache::new(Duration::from_secs(30), 10);
        c.insert("cs_1".into(), NegEntry::Unpaid);
        c.insert("cs_2".into(), NegEntry::NotFound);
        assert_eq!(c.get("cs_1"), Some(NegEntry::Unpaid));
        assert_eq!(c.get("cs_2"), Some(NegEntry::NotFound));
        assert_eq!(c.get("cs_missing"), None);
    }

    #[test]
    fn expired_entry_not_returned() {
        let c = NegCache::new(Duration::from_millis(0), 10);
        c.insert("cs_1".into(), NegEntry::Unpaid);
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(c.get("cs_1"), None);
    }

    #[test]
    fn cap_eviction_does_not_panic() {
        let c = NegCache::new(Duration::from_secs(30), 4);
        for i in 0..100 {
            c.insert(format!("cs_{i}"), NegEntry::Unpaid);
        }
    }
}
