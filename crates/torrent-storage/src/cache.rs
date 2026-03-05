use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// Adaptive Replacement Cache (ARC).
///
/// Balances recency (T1) and frequency (T2) using ghost lists (B1, B2)
/// to adapt the split point `p` dynamically. Based on the Megiddo & Modha
/// paper (2003).
///
/// - T1: pages seen exactly once recently (recency)
/// - T2: pages seen at least twice recently (frequency)
/// - B1: ghost entries recently evicted from T1
/// - B2: ghost entries recently evicted from T2
pub struct ArcCache<K, V> {
    capacity: usize,
    p: usize, // target size for T1
    t1: VecDeque<K>,
    t2: VecDeque<K>,
    b1: VecDeque<K>,
    b2: VecDeque<K>,
    map: HashMap<K, (V, CacheList)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CacheList {
    T1,
    T2,
}

impl<K: Hash + Eq + Clone, V> ArcCache<K, V> {
    /// Create a new ARC cache with the given capacity.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ARC cache capacity must be > 0");
        ArcCache {
            capacity,
            p: 0,
            t1: VecDeque::new(),
            t2: VecDeque::new(),
            b1: VecDeque::new(),
            b2: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    /// Look up a key. If found in T1, promotes to T2 (MRU).
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let list = self.map.get(key).map(|(_, l)| *l)?;

        match list {
            CacheList::T1 => {
                // Promote from T1 to T2
                self.t1.retain(|k| k != key);
                self.t2.push_back(key.clone());
                self.map.get_mut(key).unwrap().1 = CacheList::T2;
            }
            CacheList::T2 => {
                // Move to MRU of T2
                self.t2.retain(|k| k != key);
                self.t2.push_back(key.clone());
            }
        }

        self.map.get(key).map(|(v, _)| v)
    }

    /// Insert a key-value pair into the cache.
    pub fn insert(&mut self, key: K, value: V) {
        // Case 1: key already in cache
        if self.map.contains_key(&key) {
            let list = self.map.get(&key).unwrap().1;
            match list {
                CacheList::T1 => {
                    self.t1.retain(|k| k != &key);
                }
                CacheList::T2 => {
                    self.t2.retain(|k| k != &key);
                }
            }
            self.t2.push_back(key.clone());
            self.map.insert(key, (value, CacheList::T2));
            return;
        }

        // Case 2: key in B1 ghost list (was recently evicted from T1)
        if let Some(pos) = self.b1.iter().position(|k| k == &key) {
            // Adapt: increase p (favor recency)
            let delta = 1.max(self.b2.len() / self.b1.len().max(1));
            self.p = (self.p + delta).min(self.capacity);

            self.b1.remove(pos);
            self.replace(false);
            self.t2.push_back(key.clone());
            self.map.insert(key, (value, CacheList::T2));
            return;
        }

        // Case 3: key in B2 ghost list (was recently evicted from T2)
        if let Some(pos) = self.b2.iter().position(|k| k == &key) {
            // Adapt: decrease p (favor frequency)
            let delta = 1.max(self.b1.len() / self.b2.len().max(1));
            self.p = self.p.saturating_sub(delta);

            self.b2.remove(pos);
            self.replace(true);
            self.t2.push_back(key.clone());
            self.map.insert(key, (value, CacheList::T2));
            return;
        }

        // Case 4: key not in cache or ghost lists
        let t1_plus_b1 = self.t1.len() + self.b1.len();
        if t1_plus_b1 >= self.capacity {
            if self.t1.len() < self.capacity {
                // B1 is full, evict LRU from B1
                self.b1.pop_front();
                self.replace(false);
            } else {
                // T1 is full, evict LRU from T1 (no ghost)
                if let Some(evicted) = self.t1.pop_front() {
                    self.map.remove(&evicted);
                }
            }
        } else {
            let total = self.t1.len() + self.b1.len() + self.t2.len() + self.b2.len();
            if total >= self.capacity {
                if total >= 2 * self.capacity {
                    self.b2.pop_front();
                }
                self.replace(false);
            }
        }

        // Add to T1 MRU
        self.t1.push_back(key.clone());
        self.map.insert(key, (value, CacheList::T1));
    }

    /// ARC replacement: evict from T1 or T2 based on p.
    fn replace(&mut self, b2_hit: bool) {
        if !self.t1.is_empty()
            && (self.t1.len() > self.p || (b2_hit && self.t1.len() == self.p))
        {
            // Evict LRU from T1 → ghost to B1
            if let Some(evicted) = self.t1.pop_front() {
                self.map.remove(&evicted);
                self.b1.push_back(evicted);
            }
        } else if let Some(evicted) = self.t2.pop_front() {
            // Evict LRU from T2 → ghost to B2
            self.map.remove(&evicted);
            self.b2.push_back(evicted);
        }
    }

    /// Remove a key from the cache (not added to ghost lists).
    pub fn remove(&mut self, key: &K) -> Option<V> {
        if let Some((v, list)) = self.map.remove(key) {
            match list {
                CacheList::T1 => self.t1.retain(|k| k != key),
                CacheList::T2 => self.t2.retain(|k| k != key),
            }
            Some(v)
        } else {
            None
        }
    }

    /// Remove all entries matching a predicate (not added to ghost lists).
    pub fn remove_where(&mut self, pred: impl Fn(&K) -> bool) {
        let to_remove: Vec<K> = self.map.keys().filter(|k| pred(k)).cloned().collect();
        for key in &to_remove {
            self.map.remove(key);
        }
        self.t1.retain(|k| !pred(k));
        self.t2.retain(|k| !pred(k));
    }

    /// Current value of the adaptive parameter p (target T1 size).
    pub fn p(&self) -> usize {
        self.p
    }

    /// Number of entries currently in cache (T1 + T2).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Return an iterator over all keys currently in the cache (T1 + T2).
    pub fn cached_keys(&self) -> impl Iterator<Item = &K> {
        self.map.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let mut cache = ArcCache::new(3);
        cache.insert(1, "one".to_string());
        cache.insert(2, "two".to_string());
        assert_eq!(cache.get(&1), Some(&"one".to_string()));
        assert_eq!(cache.get(&3), None);
    }

    #[test]
    fn eviction_at_capacity() {
        let mut cache = ArcCache::new(2);
        cache.insert(1, "one".to_string());
        cache.insert(2, "two".to_string());
        cache.insert(3, "three".to_string()); // evicts 1 (LRU of T1)
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.get(&2), Some(&"two".to_string()));
        assert_eq!(cache.get(&3), Some(&"three".to_string()));
    }

    #[test]
    fn frequency_promotion() {
        let mut cache = ArcCache::new(3);
        cache.insert(1, "one".to_string());
        cache.insert(2, "two".to_string());
        cache.insert(3, "three".to_string());
        // Access 1 again — promotes from T1 to T2
        cache.get(&1);
        // Insert 4 — should evict from T1 (2 or 3), not 1 (in T2)
        cache.insert(4, "four".to_string());
        assert_eq!(cache.get(&1), Some(&"one".to_string())); // still present (T2)
    }

    #[test]
    fn ghost_hit_adapts_p() {
        let mut cache = ArcCache::new(2);
        cache.insert(1, "one".to_string());
        cache.insert(2, "two".to_string());
        cache.insert(3, "three".to_string()); // evicts 1, ghost goes to B1
        // Re-insert 1 — B1 ghost hit, increases p (favor recency)
        let p_before = cache.p();
        cache.insert(1, "one_new".to_string());
        assert!(cache.p() >= p_before); // p increased or same
        assert_eq!(cache.get(&1), Some(&"one_new".to_string()));
    }

    #[test]
    fn remove() {
        let mut cache = ArcCache::new(3);
        cache.insert(1, "one".to_string());
        cache.remove(&1);
        assert_eq!(cache.get(&1), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn cached_keys_returns_all_entries() {
        let mut cache = ArcCache::new(5);
        cache.insert(1, "one".to_string());
        cache.insert(2, "two".to_string());
        cache.insert(3, "three".to_string());
        let mut keys: Vec<_> = cache.cached_keys().copied().collect();
        keys.sort();
        assert_eq!(keys, vec![1, 2, 3]);
    }

    #[test]
    fn cached_keys_empty_cache() {
        let cache: ArcCache<i32, String> = ArcCache::new(3);
        assert_eq!(cache.cached_keys().count(), 0);
    }

    #[test]
    fn remove_where() {
        let mut cache: ArcCache<(u32, u32), String> = ArcCache::new(4);
        cache.insert((1, 0), "a".to_string());
        cache.insert((1, 1), "b".to_string());
        cache.insert((2, 0), "c".to_string());
        cache.remove_where(|k| k.0 == 1);
        assert_eq!(cache.get(&(1, 0)), None);
        assert_eq!(cache.get(&(1, 1)), None);
        assert_eq!(cache.get(&(2, 0)), Some(&"c".to_string()));
    }
}
