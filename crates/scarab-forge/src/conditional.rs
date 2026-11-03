//! Conditional GETs for the forge adapters (git-bug 5d5fc8a).
//!
//! An adapter keeps the last `ETag` and body it got for each URL and sends
//! the tag back as `If-None-Match`. When nothing changed the forge answers
//! `304 Not Modified` with no body, and the adapter serves the body it kept.
//! On GitHub a 304 does not count against the rate limit, so a cron fire on a
//! repo whose default branch did not move costs close to nothing.
//!
//! The cache is per adapter, so per connection: every URL in it was fetched
//! with that connection's credential. Pure: no I/O.

use std::collections::HashMap;
use std::sync::Mutex;

/// How many URLs one adapter remembers. Past it, an entry is dropped to make
/// room; a dropped entry costs one full GET.
pub const DEFAULT_CAPACITY: usize = 4096;

/// URL (and accept type) → the last `ETag` and body for it.
pub struct EtagCache {
    capacity: usize,
    entries: Mutex<HashMap<String, (String, Vec<u8>)>>,
}

impl Default for EtagCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl EtagCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The `ETag` and body kept for `key`, read together: a request sends the
    /// tag and, on a `304`, serves the body it read with it, so another
    /// request replacing or forgetting the entry meanwhile changes nothing.
    pub fn get(&self, key: &str) -> Option<(String, Vec<u8>)> {
        self.entries.lock().unwrap().get(key).cloned()
    }

    /// The `ETag` to send as `If-None-Match` for `key`, if one is kept.
    pub fn etag(&self, key: &str) -> Option<String> {
        self.entries
            .lock()
            .unwrap()
            .get(key)
            .map(|(etag, _)| etag.clone())
    }

    /// The body kept for `key`: what a `304` stands for.
    pub fn body(&self, key: &str) -> Option<Vec<u8>> {
        self.entries
            .lock()
            .unwrap()
            .get(key)
            .map(|(_, body)| body.clone())
    }

    /// Keep `body` under `key` with the `etag` the forge sent with it.
    pub fn store(&self, key: &str, etag: &str, body: &[u8]) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.capacity && !entries.contains_key(key) {
            if let Some(evict) = entries.keys().next().cloned() {
                entries.remove(&evict);
            }
        }
        entries.insert(key.to_string(), (etag.to_string(), body.to_vec()));
    }

    /// Forget `key`, for a response that came without an `ETag`.
    pub fn forget(&self, key: &str) {
        self.entries.lock().unwrap().remove(key);
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_cache_drops_one_entry_to_keep_a_new_one() {
        let cache = EtagCache::new(2);
        cache.store("a", "\"1\"", b"A");
        cache.store("b", "\"2\"", b"B");
        cache.store("a", "\"3\"", b"A2"); // an update, not a new key
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.etag("a").as_deref(), Some("\"3\""));
        cache.store("c", "\"4\"", b"C");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.body("c").as_deref(), Some(&b"C"[..]));
        cache.forget("c");
        assert!(cache.etag("c").is_none());
    }
}
