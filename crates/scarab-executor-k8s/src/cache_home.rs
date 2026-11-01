//! Each Cache's **home** Depot replica (git-bug 12e2f6b, ADR-0077 grill
//! amendment item 5).
//!
//! A Cache is warm-only (ADR-0065), and warm is per replica. Saves and
//! restores sent through the Depot's ClusterIP Service scatter across
//! replicas, so at N > 1 a restore almost always missed. Instead the executor
//! picks one replica per cache key by rendezvous (highest random weight)
//! hashing over the StatefulSet ordinals `0..N`, and gives the Pod that
//! replica's stable address for cache content only. Everything else keeps the
//! ClusterIP.
//!
//! Rendezvous rather than `hash % N`: when N changes, only the keys whose
//! winner was a removed ordinal (or that a new ordinal now wins) move, about
//! `1/N` of them, and each moved key is one miss. Membership is the ordinals
//! from config; there is no discovery. A home that is down is a miss, never
//! a failed Step.

/// Where the Pod finds a Cache's home: the Depot's replica count and the URL
/// of one replica, with `{ordinal}` standing for its StatefulSet ordinal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheHome {
    /// The Depot StatefulSet's replica count. At 1 there is no home to
    /// choose: [`CacheHome::url_for`] answers `None` and the Pod keeps the
    /// service URL for caches, exactly as before homes existed.
    pub replicas: u32,
    /// The URL of one replica, e.g.
    /// `http://scarab-workspace-{ordinal}.scarab.svc`. Must contain
    /// [`ORDINAL_PLACEHOLDER`].
    pub url_template: String,
}

/// The placeholder in [`CacheHome::url_template`].
pub const ORDINAL_PLACEHOLDER: &str = "{ordinal}";

impl CacheHome {
    /// `None` when there is nothing to route: one replica (or zero), or a
    /// template without the placeholder (config validation refuses that at
    /// boot; this keeps a hand-built value from routing every key to one
    /// literal URL).
    pub fn new(replicas: u32, url_template: impl Into<String>) -> Option<Self> {
        let url_template = url_template.into();
        if replicas <= 1 || !url_template.contains(ORDINAL_PLACEHOLDER) {
            return None;
        }
        Some(Self {
            replicas,
            url_template,
        })
    }

    /// The home replica's URL for `cache_key`.
    pub fn url_for(&self, cache_key: &str) -> Option<String> {
        if self.replicas <= 1 {
            return None;
        }
        let ordinal = home_ordinal(cache_key, self.replicas);
        Some(
            self.url_template
                .replace(ORDINAL_PLACEHOLDER, &ordinal.to_string()),
        )
    }
}

/// The ordinal in `0..replicas` that wins `cache_key` under rendezvous
/// hashing: the highest SHA-256 score of `key \0 ordinal`. SHA-256 because
/// the choice must be the same in every server process and every release —
/// a std hasher promises neither.
pub fn home_ordinal(cache_key: &str, replicas: u32) -> u32 {
    (0..replicas.max(1))
        .max_by_key(|ordinal| score(cache_key, *ordinal))
        .unwrap_or(0)
}

fn score(cache_key: &str, ordinal: u32) -> u64 {
    let mut input = Vec::with_capacity(cache_key.len() + 5);
    input.extend_from_slice(cache_key.as_bytes());
    input.push(0);
    input.extend_from_slice(&ordinal.to_be_bytes());
    let hex = scarab_storage::sha256_hex(&input);
    u64::from_str_radix(&hex[..16], 16).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stable: the choice is a pure function of key and count, pinned to
    /// known values so a change to the scoring (which would turn every
    /// existing Cache into a miss on upgrade) fails here, not in production.
    #[test]
    fn the_home_is_stable_for_a_key_and_count() {
        let pinned: Vec<u32> = ["a", "b", "c", "d", "lockfile-hash"]
            .iter()
            .map(|k| home_ordinal(k, 3))
            .collect();
        assert_eq!(pinned, PINNED_AT_3);
        for key in ["a", "b", "lockfile-hash"] {
            assert_eq!(home_ordinal(key, 5), home_ordinal(key, 5));
        }
        assert_eq!(home_ordinal("anything", 1), 0);
    }

    /// Cross-checked outside Rust: `max(range(3), key=lambda o:
    /// int(sha256(k + b"\\0" + o.to_bytes(4, "big")).hexdigest()[:16], 16))`.
    const PINNED_AT_3: [u32; 5] = [2, 2, 0, 1, 1];

    /// Spread: over many keys every ordinal is home to roughly its share.
    #[test]
    fn homes_spread_across_replicas() {
        let n = 3u32;
        let mut counts = [0u32; 3];
        for i in 0..3_000 {
            counts[home_ordinal(&format!("key-{i}"), n) as usize] += 1;
        }
        for c in counts {
            assert!((800..=1_200).contains(&c), "uneven spread: {counts:?}");
        }
    }

    /// The rendezvous property: growing from 3 to 4 replicas moves only the
    /// keys the new ordinal wins; no key moves between two old ordinals.
    #[test]
    fn adding_a_replica_moves_only_the_keys_it_wins() {
        let mut moved = 0;
        for i in 0..2_000 {
            let key = format!("key-{i}");
            let (before, after) = (home_ordinal(&key, 3), home_ordinal(&key, 4));
            if before != after {
                assert_eq!(after, 3, "{key} moved between two old replicas");
                moved += 1;
            }
        }
        assert!((350..=650).contains(&moved), "about a quarter should move: {moved}");
    }

    #[test]
    fn one_replica_or_no_placeholder_routes_nothing() {
        assert!(CacheHome::new(1, "http://d-{ordinal}.ns.svc").is_none());
        assert!(CacheHome::new(3, "http://d.ns.svc").is_none());
        let home = CacheHome::new(3, "http://d-{ordinal}.ns.svc").unwrap();
        let url = home.url_for("k").unwrap();
        assert_eq!(url, format!("http://d-{}.ns.svc", home_ordinal("k", 3)));
    }
}
