//! Pluggable DHT item storage (BEP 44).
//!
//! The `DhtStorage` trait abstracts over the storage backend used for
//! BEP 44 put/get items. The default `InMemoryDhtStorage` uses LRU
//! eviction to stay within configured limits.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use torrent_core::Id20;

use crate::bep44::{compute_mutable_target, ImmutableItem, MutableItem};

/// Pluggable storage backend for BEP 44 DHT items.
pub trait DhtStorage: Send + Sync + 'static {
    /// Retrieve an immutable item by its SHA-1 target hash.
    ///
    /// Touches the entry (updates LRU timestamp) on access.
    fn get_immutable(&mut self, target: &Id20) -> Option<ImmutableItem>;

    /// Store an immutable item. Returns `true` if newly inserted, `false` if updated.
    fn put_immutable(&mut self, item: ImmutableItem) -> bool;

    /// Retrieve a mutable item by public key and salt.
    ///
    /// Touches the entry (updates LRU timestamp) on access.
    fn get_mutable(&mut self, public_key: &[u8; 32], salt: &[u8]) -> Option<MutableItem>;

    /// Retrieve a mutable item by its SHA-1 target (i.e. `SHA-1(public_key + salt)`).
    ///
    /// This performs a linear scan over stored mutable items, checking whether
    /// `compute_mutable_target(&item.public_key, &item.salt) == *target`.
    /// Touches the entry on access.
    fn get_mutable_by_target(&mut self, target: &Id20) -> Option<MutableItem>;

    /// Store a mutable item. Returns `true` if stored (new or higher seq), `false` if rejected.
    ///
    /// Implementations MUST enforce sequence number monotonicity:
    /// reject if `item.seq <= existing.seq` for the same key+salt.
    fn put_mutable(&mut self, item: MutableItem) -> bool;

    /// Number of stored items as `(immutable_count, mutable_count)`.
    fn count(&self) -> (usize, usize);

    /// Remove expired items older than `lifetime`.
    fn expire(&mut self, lifetime: Duration);
}

/// Stored entry metadata for LRU tracking.
struct StoredEntry<T> {
    item: T,
    last_touched: Instant,
}

impl<T> StoredEntry<T> {
    fn new(item: T) -> Self {
        StoredEntry {
            item,
            last_touched: Instant::now(),
        }
    }

    fn touch(&mut self) {
        self.last_touched = Instant::now();
    }
}

/// Key for mutable items: (public_key, salt).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MutableKey {
    public_key: [u8; 32],
    salt: Vec<u8>,
}

/// In-memory DHT storage with LRU eviction.
///
/// When the number of items exceeds `max_items`, the least recently
/// accessed item is evicted on the next `put`.
pub struct InMemoryDhtStorage {
    immutable: HashMap<Id20, StoredEntry<ImmutableItem>>,
    mutable: HashMap<MutableKey, StoredEntry<MutableItem>>,
    max_items: usize,
}

impl InMemoryDhtStorage {
    /// Create a new in-memory storage with the given capacity limit.
    pub fn new(max_items: usize) -> Self {
        InMemoryDhtStorage {
            immutable: HashMap::new(),
            mutable: HashMap::new(),
            max_items,
        }
    }

    /// Total number of stored items across both maps.
    fn total_count(&self) -> usize {
        self.immutable.len() + self.mutable.len()
    }

    /// Evict the single least recently touched item across both maps.
    fn evict_lru(&mut self) {
        let oldest_immutable = self
            .immutable
            .iter()
            .min_by_key(|(_, e)| e.last_touched)
            .map(|(k, e)| (*k, e.last_touched));

        let oldest_mutable = self
            .mutable
            .iter()
            .min_by_key(|(_, e)| e.last_touched)
            .map(|(k, e)| (k.clone(), e.last_touched));

        match (oldest_immutable, oldest_mutable) {
            (Some((ik, it)), Some((mk, mt))) => {
                if it <= mt {
                    self.immutable.remove(&ik);
                } else {
                    self.mutable.remove(&mk);
                }
            }
            (Some((ik, _)), None) => {
                self.immutable.remove(&ik);
            }
            (None, Some((mk, _))) => {
                self.mutable.remove(&mk);
            }
            (None, None) => {}
        }
    }
}

impl Default for InMemoryDhtStorage {
    fn default() -> Self {
        Self::new(700)
    }
}

impl DhtStorage for InMemoryDhtStorage {
    fn get_immutable(&mut self, target: &Id20) -> Option<ImmutableItem> {
        self.immutable.get_mut(target).map(|e| {
            e.touch();
            e.item.clone()
        })
    }

    fn put_immutable(&mut self, item: ImmutableItem) -> bool {
        let target = item.target;

        if self.immutable.contains_key(&target) {
            // Already have it, just touch
            if let Some(entry) = self.immutable.get_mut(&target) {
                entry.touch();
            }
            return false;
        }

        // Evict if at capacity
        while self.total_count() >= self.max_items {
            self.evict_lru();
        }

        self.immutable.insert(target, StoredEntry::new(item));
        true
    }

    fn get_mutable(&mut self, public_key: &[u8; 32], salt: &[u8]) -> Option<MutableItem> {
        let key = MutableKey {
            public_key: *public_key,
            salt: salt.to_vec(),
        };
        self.mutable.get_mut(&key).map(|e| {
            e.touch();
            e.item.clone()
        })
    }

    fn get_mutable_by_target(&mut self, target: &Id20) -> Option<MutableItem> {
        // Linear scan: check each stored mutable item's computed target
        for entry in self.mutable.values_mut() {
            if compute_mutable_target(&entry.item.public_key, &entry.item.salt) == *target {
                entry.touch();
                return Some(entry.item.clone());
            }
        }
        None
    }

    fn put_mutable(&mut self, item: MutableItem) -> bool {
        let key = MutableKey {
            public_key: item.public_key,
            salt: item.salt.clone(),
        };

        // Enforce sequence number monotonicity
        if let Some(existing) = self.mutable.get(&key)
            && item.seq <= existing.item.seq
        {
            return false;
        }

        let is_new = !self.mutable.contains_key(&key);

        if is_new {
            // Evict if at capacity
            while self.total_count() >= self.max_items {
                self.evict_lru();
            }
        }

        self.mutable.insert(key, StoredEntry::new(item));
        true
    }

    fn count(&self) -> (usize, usize) {
        (self.immutable.len(), self.mutable.len())
    }

    fn expire(&mut self, lifetime: Duration) {
        let cutoff = Instant::now() - lifetime;
        self.immutable.retain(|_, e| e.last_touched > cutoff);
        self.mutable.retain(|_, e| e.last_touched > cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bep44::ImmutableItem;
    use ed25519_dalek::SigningKey;

    fn make_immutable(data: &[u8]) -> ImmutableItem {
        ImmutableItem::new(data.to_vec()).unwrap()
    }

    fn make_mutable(seed: u8, seq: i64, salt: &[u8]) -> MutableItem {
        let keypair = SigningKey::from_bytes(&[seed; 32]);
        MutableItem::create(&keypair, b"4:test".to_vec(), seq, salt.to_vec()).unwrap()
    }

    #[test]
    fn immutable_put_get_round_trip() {
        let mut store = InMemoryDhtStorage::new(100);
        let item = make_immutable(b"5:hello");
        let target = item.target;
        assert!(store.put_immutable(item.clone()));
        assert_eq!(store.get_immutable(&target), Some(item));
    }

    #[test]
    fn immutable_duplicate_returns_false() {
        let mut store = InMemoryDhtStorage::new(100);
        let item = make_immutable(b"5:hello");
        assert!(store.put_immutable(item.clone()));
        assert!(!store.put_immutable(item));
    }

    #[test]
    fn immutable_get_missing_returns_none() {
        let mut store = InMemoryDhtStorage::new(100);
        assert_eq!(store.get_immutable(&Id20::ZERO), None);
    }

    #[test]
    fn mutable_put_get_round_trip() {
        let mut store = InMemoryDhtStorage::new(100);
        let item = make_mutable(1, 1, b"");
        let pk = item.public_key;
        assert!(store.put_mutable(item.clone()));
        assert_eq!(store.get_mutable(&pk, b""), Some(item));
    }

    #[test]
    fn mutable_sequence_monotonicity() {
        let mut store = InMemoryDhtStorage::new(100);
        let item1 = make_mutable(1, 5, b"");
        let item2_old = make_mutable(1, 3, b"");
        let item3_same = make_mutable(1, 5, b"");
        let item4_new = make_mutable(1, 6, b"");

        assert!(store.put_mutable(item1));
        assert!(!store.put_mutable(item2_old)); // older seq rejected
        assert!(!store.put_mutable(item3_same)); // same seq rejected
        assert!(store.put_mutable(item4_new.clone())); // newer seq accepted

        let pk = item4_new.public_key;
        let stored = store.get_mutable(&pk, b"").unwrap();
        assert_eq!(stored.seq, 6);
    }

    #[test]
    fn mutable_salt_separates_items() {
        let mut store = InMemoryDhtStorage::new(100);
        let item_a = make_mutable(1, 1, b"salt-a");
        let item_b = make_mutable(1, 1, b"salt-b");
        let pk = item_a.public_key;

        assert!(store.put_mutable(item_a.clone()));
        assert!(store.put_mutable(item_b.clone()));
        assert_eq!(store.count(), (0, 2));
        assert_eq!(store.get_mutable(&pk, b"salt-a"), Some(item_a));
        assert_eq!(store.get_mutable(&pk, b"salt-b"), Some(item_b));
    }

    #[test]
    fn lru_eviction_at_capacity() {
        let mut store = InMemoryDhtStorage::new(3);

        let item1 = make_immutable(b"1:a");
        let item2 = make_immutable(b"1:b");
        let item3 = make_immutable(b"1:c");
        let target1 = item1.target;

        store.put_immutable(item1);
        store.put_immutable(item2);
        store.put_immutable(item3);
        assert_eq!(store.total_count(), 3);

        // Adding a 4th should evict the oldest (item1)
        let item4 = make_immutable(b"1:d");
        store.put_immutable(item4);
        assert_eq!(store.total_count(), 3);
        assert!(store.get_immutable(&target1).is_none());
    }

    #[test]
    fn expire_removes_old_items() {
        let mut store = InMemoryDhtStorage::new(100);
        store.put_immutable(make_immutable(b"1:a"));
        store.put_mutable(make_mutable(1, 1, b""));
        assert_eq!(store.count(), (1, 1));

        // Expire with zero lifetime removes everything
        store.expire(Duration::from_secs(0));
        assert_eq!(store.count(), (0, 0));
    }

    #[test]
    fn count_includes_both_types() {
        let mut store = InMemoryDhtStorage::new(100);
        store.put_immutable(make_immutable(b"1:a"));
        store.put_mutable(make_mutable(1, 1, b""));
        assert_eq!(store.count(), (1, 1));
    }

    #[test]
    fn default_max_items_is_700() {
        let store = InMemoryDhtStorage::default();
        assert_eq!(store.max_items, 700);
    }

    #[test]
    fn get_mutable_by_target_finds_item() {
        let mut store = InMemoryDhtStorage::new(100);
        let item = make_mutable(1, 1, b"my-salt");
        let target = compute_mutable_target(&item.public_key, &item.salt);
        store.put_mutable(item.clone());

        let found = store.get_mutable_by_target(&target);
        assert_eq!(found, Some(item));
    }

    #[test]
    fn get_mutable_by_target_returns_none_for_missing() {
        let mut store = InMemoryDhtStorage::new(100);
        assert_eq!(store.get_mutable_by_target(&Id20::ZERO), None);
    }

    #[test]
    fn get_immutable_touches_entry() {
        let mut store = InMemoryDhtStorage::new(3);

        let item1 = make_immutable(b"1:a");
        let item2 = make_immutable(b"1:b");
        let item3 = make_immutable(b"1:c");
        let target1 = item1.target;
        let target2 = item2.target;

        store.put_immutable(item1);
        store.put_immutable(item2);
        store.put_immutable(item3);

        // Touch item1 to make it recent
        store.get_immutable(&target1);

        // Adding a 4th should evict item2 (oldest untouched), not item1
        let item4 = make_immutable(b"1:d");
        store.put_immutable(item4);
        assert_eq!(store.total_count(), 3);
        assert!(store.get_immutable(&target1).is_some());
        assert!(store.get_immutable(&target2).is_none());
    }
}
