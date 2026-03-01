# M38: BEP 44 — DHT Arbitrary Data Storage (Put/Get) — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable storing and retrieving arbitrary data in the DHT, supporting both immutable items (SHA-1-keyed) and mutable items (ed25519-signed with sequence numbers and optional salt), per BEP 44.

**Architecture:** BEP 44 extends the existing BEP 5 KRPC protocol with two new query types (`get` and `put`). Immutable items are keyed by `SHA-1(bencoded_value)`. Mutable items use ed25519 public keys with optional salt, sequence numbers for monotonicity, and 64-byte signatures. The DHT actor stores items in a pluggable `DhtStorage` backend (default: in-memory LRU). The session exposes `dht_put`/`dht_get` and `dht_put_mutable`/`dht_get_mutable` APIs. Alerts notify consumers of completion/failure.

**Tech Stack:** Rust edition 2024, ferrite-bencode, ferrite-core (sha1, Id20), ferrite-dht (KRPC, actor), ed25519-dalek, tokio

**BEP 44 Protocol Summary:**
- Max bencoded value size: 1000 bytes
- Max salt size: 64 bytes
- Immutable key: `SHA-1(bencoded_value)`
- Mutable target: `SHA-1(pubkey[32] + salt)`
- Signature input (no salt): `3:seqi{seq}e1:v{len}:{value_bytes}`
- Signature input (with salt): `4:salt{len}:{salt}3:seqi{seq}e1:v{len}:{value_bytes}`
- Error codes: 205 (value too big), 206 (invalid signature), 207 (salt too big), 301 (CAS mismatch), 302 (seq too low)
- Tokens reuse the get_peers token mechanism

---

## Task 1: Add `ed25519-dalek` Dependency and BEP 44 Types

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/ferrite-dht/Cargo.toml`
- Create: `crates/ferrite-dht/src/bep44.rs`
- Modify: `crates/ferrite-dht/src/lib.rs`

**Step 1: Add `ed25519-dalek` to workspace dependencies**

In `/mnt/TempNVME/projects/ferrite/Cargo.toml`, add to the `[workspace.dependencies]` section:

```toml
# Ed25519 signing (BEP 44 mutable DHT items)
ed25519-dalek = { version = "2", features = ["rand_core"] }
```

**Step 2: Add `ed25519-dalek` to ferrite-dht dependencies**

In `/mnt/TempNVME/projects/ferrite/crates/ferrite-dht/Cargo.toml`, add under `[dependencies]`:

```toml
ed25519-dalek = { workspace = true }
```

**Step 3: Create the BEP 44 types module with tests**

Create `/mnt/TempNVME/projects/ferrite/crates/ferrite-dht/src/bep44.rs`:

```rust
//! BEP 44: DHT arbitrary data storage types and validation.
//!
//! Defines item types, signature construction, and validation for both
//! immutable (SHA-1-keyed) and mutable (ed25519-signed) DHT items.

use ed25519_dalek::{Signature, VerifyingKey, SigningKey, Signer, Verifier};
use ferrite_core::{sha1, Id20};

use crate::error::{Error, Result};

/// Maximum size of a bencoded value in a BEP 44 item (bytes).
pub const MAX_VALUE_SIZE: usize = 1000;

/// Maximum size of the salt field (bytes).
pub const MAX_SALT_SIZE: usize = 64;

/// An immutable DHT item. Key = SHA-1(bencoded value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableItem {
    /// The bencoded value (max 1000 bytes).
    pub value: Vec<u8>,
    /// SHA-1 hash of `value`, used as the lookup key / target.
    pub target: Id20,
}

impl ImmutableItem {
    /// Create an immutable item from raw bencoded bytes.
    ///
    /// Returns error if value exceeds 1000 bytes.
    pub fn new(value: Vec<u8>) -> Result<Self> {
        if value.len() > MAX_VALUE_SIZE {
            return Err(Error::Bep44ValueTooLarge {
                size: value.len(),
                max: MAX_VALUE_SIZE,
            });
        }
        let target = sha1(&value);
        Ok(ImmutableItem { value, target })
    }

    /// Validate that the value matches the expected target hash.
    pub fn verify(&self) -> bool {
        sha1(&self.value) == self.target
    }
}

/// A mutable DHT item. Key = ed25519 public key + optional salt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableItem {
    /// The bencoded value (max 1000 bytes).
    pub value: Vec<u8>,
    /// Ed25519 public key (32 bytes).
    pub public_key: [u8; 32],
    /// Ed25519 signature (64 bytes) over the signing buffer.
    pub signature: [u8; 64],
    /// Monotonically increasing sequence number.
    pub seq: i64,
    /// Optional salt (max 64 bytes). Empty vec = no salt.
    pub salt: Vec<u8>,
    /// Derived target: SHA-1(public_key + salt).
    pub target: Id20,
}

impl MutableItem {
    /// Create a mutable item and sign it with the given keypair.
    ///
    /// - `keypair`: ed25519 signing key (contains both secret and public key)
    /// - `value`: raw bencoded value (max 1000 bytes)
    /// - `seq`: sequence number (must increase with each update)
    /// - `salt`: optional salt (max 64 bytes, empty vec for none)
    pub fn create(
        keypair: &SigningKey,
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
    ) -> Result<Self> {
        if value.len() > MAX_VALUE_SIZE {
            return Err(Error::Bep44ValueTooLarge {
                size: value.len(),
                max: MAX_VALUE_SIZE,
            });
        }
        if salt.len() > MAX_SALT_SIZE {
            return Err(Error::Bep44SaltTooLarge {
                size: salt.len(),
                max: MAX_SALT_SIZE,
            });
        }

        let public_key = keypair.verifying_key().to_bytes();
        let target = compute_mutable_target(&public_key, &salt);
        let sign_buf = build_signing_buffer(&salt, seq, &value);
        let signature = keypair.sign(&sign_buf);

        Ok(MutableItem {
            value,
            public_key,
            signature: signature.to_bytes(),
            seq,
            salt,
            target,
        })
    }

    /// Verify the ed25519 signature against the public key and signing buffer.
    pub fn verify(&self) -> bool {
        let Ok(verifying_key) = VerifyingKey::from_bytes(&self.public_key) else {
            return false;
        };
        let Ok(signature) = Signature::from_bytes(&self.signature) else {
            return false;
        };
        let sign_buf = build_signing_buffer(&self.salt, self.seq, &self.value);
        verifying_key.verify(&sign_buf, &signature).is_ok()
    }

    /// Check that the target matches SHA-1(public_key + salt).
    pub fn verify_target(&self) -> bool {
        compute_mutable_target(&self.public_key, &self.salt) == self.target
    }
}

/// Compute the DHT target for a mutable item: `SHA-1(public_key + salt)`.
pub fn compute_mutable_target(public_key: &[u8; 32], salt: &[u8]) -> Id20 {
    let mut buf = Vec::with_capacity(32 + salt.len());
    buf.extend_from_slice(public_key);
    buf.extend_from_slice(salt);
    sha1(&buf)
}

/// Build the byte buffer that gets signed/verified for mutable items.
///
/// Without salt: `3:seqi{seq}e1:v{len}:{value}`
/// With salt: `4:salt{salt_len}:{salt}3:seqi{seq}e1:v{value_len}:{value}`
pub fn build_signing_buffer(salt: &[u8], seq: i64, value: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + value.len() + salt.len());

    if !salt.is_empty() {
        // "4:salt{len}:{salt}"
        buf.extend_from_slice(b"4:salt");
        buf.extend_from_slice(salt.len().to_string().as_bytes());
        buf.push(b':');
        buf.extend_from_slice(salt);
    }

    // "3:seqi{seq}e"
    buf.extend_from_slice(b"3:seqi");
    buf.extend_from_slice(seq.to_string().as_bytes());
    buf.push(b'e');

    // "1:v{len}:{value}"
    buf.extend_from_slice(b"1:v");
    buf.extend_from_slice(value.len().to_string().as_bytes());
    buf.push(b':');
    buf.extend_from_slice(value);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn immutable_item_hash_is_sha1_of_value() {
        let value = b"12:Hello World!".to_vec(); // bencoded string
        let item = ImmutableItem::new(value.clone()).unwrap();
        assert_eq!(item.target, sha1(&value));
        assert!(item.verify());
    }

    #[test]
    fn immutable_item_rejects_oversized_value() {
        let value = vec![0u8; MAX_VALUE_SIZE + 1];
        let err = ImmutableItem::new(value).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn immutable_item_verify_detects_corruption() {
        let mut item = ImmutableItem::new(b"5:hello".to_vec()).unwrap();
        item.value[0] = 0xFF; // corrupt
        assert!(!item.verify());
    }

    #[test]
    fn mutable_item_sign_and_verify() {
        let keypair = SigningKey::from_bytes(&[1u8; 32]);
        let value = b"12:Hello World!".to_vec();
        let item = MutableItem::create(&keypair, value, 1, Vec::new()).unwrap();
        assert!(item.verify());
        assert!(item.verify_target());
    }

    #[test]
    fn mutable_item_with_salt() {
        let keypair = SigningKey::from_bytes(&[2u8; 32]);
        let value = b"4:test".to_vec();
        let salt = b"my-salt".to_vec();
        let item = MutableItem::create(&keypair, value, 42, salt.clone()).unwrap();
        assert!(item.verify());
        assert!(item.verify_target());
        assert_eq!(item.salt, salt);
    }

    #[test]
    fn mutable_item_rejects_oversized_value() {
        let keypair = SigningKey::from_bytes(&[3u8; 32]);
        let value = vec![0u8; MAX_VALUE_SIZE + 1];
        let err = MutableItem::create(&keypair, value, 1, Vec::new()).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn mutable_item_rejects_oversized_salt() {
        let keypair = SigningKey::from_bytes(&[4u8; 32]);
        let value = b"4:test".to_vec();
        let salt = vec![0u8; MAX_SALT_SIZE + 1];
        let err = MutableItem::create(&keypair, value, 1, salt).unwrap_err();
        assert!(err.to_string().contains("salt"));
    }

    #[test]
    fn mutable_item_verify_fails_on_wrong_key() {
        let keypair = SigningKey::from_bytes(&[5u8; 32]);
        let value = b"4:test".to_vec();
        let mut item = MutableItem::create(&keypair, value, 1, Vec::new()).unwrap();
        // Replace public key with a different one
        item.public_key = SigningKey::from_bytes(&[6u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(!item.verify());
    }

    #[test]
    fn mutable_item_verify_fails_on_tampered_value() {
        let keypair = SigningKey::from_bytes(&[7u8; 32]);
        let value = b"4:test".to_vec();
        let mut item = MutableItem::create(&keypair, value, 1, Vec::new()).unwrap();
        item.value = b"5:tampr".to_vec();
        assert!(!item.verify());
    }

    #[test]
    fn mutable_target_is_sha1_of_pubkey_plus_salt() {
        let pubkey = [0xABu8; 32];
        let salt = b"hello";
        let target = compute_mutable_target(&pubkey, salt);

        let mut expected_buf = Vec::new();
        expected_buf.extend_from_slice(&pubkey);
        expected_buf.extend_from_slice(salt);
        assert_eq!(target, sha1(&expected_buf));
    }

    #[test]
    fn mutable_target_no_salt() {
        let pubkey = [0xCDu8; 32];
        let target = compute_mutable_target(&pubkey, &[]);
        // With no salt, target = SHA-1(pubkey)
        assert_eq!(target, sha1(&pubkey));
    }

    #[test]
    fn signing_buffer_no_salt() {
        let buf = build_signing_buffer(&[], 1, b"12:Hello World!");
        assert_eq!(buf, b"3:seqi1e1:v15:12:Hello World!");
    }

    #[test]
    fn signing_buffer_with_salt() {
        let buf = build_signing_buffer(b"foobar", 4, b"12:Hello World!");
        assert_eq!(buf, b"4:salt6:foobar3:seqi4e1:v15:12:Hello World!");
    }

    #[test]
    fn signing_buffer_negative_seq() {
        let buf = build_signing_buffer(&[], -1, b"1:x");
        assert_eq!(buf, b"3:seqi-1e1:v3:1:x");
    }

    #[test]
    fn sequence_number_monotonicity() {
        let keypair = SigningKey::from_bytes(&[8u8; 32]);
        let item1 = MutableItem::create(&keypair, b"4:aaaa".to_vec(), 1, Vec::new()).unwrap();
        let item2 = MutableItem::create(&keypair, b"4:bbbb".to_vec(), 2, Vec::new()).unwrap();
        assert!(item2.seq > item1.seq);
        assert!(item1.verify());
        assert!(item2.verify());
    }
}
```

**Step 4: Register bep44 module in lib.rs**

In `/mnt/TempNVME/projects/ferrite/crates/ferrite-dht/src/lib.rs`, add after `mod actor;`:

```rust
pub mod bep44;
```

And add to the public exports:

```rust
pub use bep44::{
    ImmutableItem, MutableItem,
    compute_mutable_target, build_signing_buffer,
    MAX_VALUE_SIZE, MAX_SALT_SIZE,
};
```

**Step 5: Add BEP 44 error variants**

In `/mnt/TempNVME/projects/ferrite/crates/ferrite-dht/src/error.rs`, add these variants to the `Error` enum:

```rust
    #[error("BEP 44: value too large ({size} bytes, max {max})")]
    Bep44ValueTooLarge { size: usize, max: usize },

    #[error("BEP 44: salt too large ({size} bytes, max {max})")]
    Bep44SaltTooLarge { size: usize, max: usize },

    #[error("BEP 44: invalid signature")]
    Bep44InvalidSignature,

    #[error("BEP 44: sequence number {got} not newer than {current}")]
    Bep44SequenceTooOld { got: i64, current: i64 },

    #[error("BEP 44: CAS mismatch (expected seq {expected}, got {actual})")]
    Bep44CasMismatch { expected: i64, actual: i64 },
```

**Step 6: Run tests**

Run: `cargo test -p ferrite-dht bep44 -- --nocapture`
Expected: 14 tests PASS

---

## Task 2: DhtStorage Trait and In-Memory Backend

**Files:**
- Create: `crates/ferrite-dht/src/storage.rs`
- Modify: `crates/ferrite-dht/src/lib.rs`

**Step 1: Create storage trait and in-memory implementation with tests**

Create `/mnt/TempNVME/projects/ferrite/crates/ferrite-dht/src/storage.rs`:

```rust
//! Pluggable DHT item storage (BEP 44).
//!
//! The `DhtStorage` trait abstracts over the storage backend used for
//! BEP 44 put/get items. The default `InMemoryDhtStorage` uses LRU
//! eviction to stay within configured limits.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ferrite_core::Id20;

use crate::bep44::{ImmutableItem, MutableItem};

/// Pluggable storage backend for BEP 44 DHT items.
pub trait DhtStorage: Send + Sync + 'static {
    /// Retrieve an immutable item by its SHA-1 target hash.
    fn get_immutable(&self, target: &Id20) -> Option<ImmutableItem>;

    /// Store an immutable item. Returns `true` if newly inserted, `false` if updated.
    fn put_immutable(&mut self, item: ImmutableItem) -> bool;

    /// Retrieve a mutable item by public key and salt.
    fn get_mutable(&self, public_key: &[u8; 32], salt: &[u8]) -> Option<MutableItem>;

    /// Store a mutable item. Returns `true` if stored (new or higher seq), `false` if rejected.
    ///
    /// Implementations MUST enforce sequence number monotonicity:
    /// reject if `item.seq <= existing.seq` for the same key+salt.
    fn put_mutable(&mut self, item: MutableItem) -> bool;

    /// Total number of stored items (immutable + mutable).
    fn count(&self) -> usize;

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
    fn get_immutable(&self, target: &Id20) -> Option<ImmutableItem> {
        self.immutable.get(target).map(|e| e.item.clone())
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
        while self.count() >= self.max_items {
            self.evict_lru();
        }

        self.immutable.insert(target, StoredEntry::new(item));
        true
    }

    fn get_mutable(&self, public_key: &[u8; 32], salt: &[u8]) -> Option<MutableItem> {
        let key = MutableKey {
            public_key: *public_key,
            salt: salt.to_vec(),
        };
        self.mutable.get(&key).map(|e| e.item.clone())
    }

    fn put_mutable(&mut self, item: MutableItem) -> bool {
        let key = MutableKey {
            public_key: item.public_key,
            salt: item.salt.clone(),
        };

        // Enforce sequence number monotonicity
        if let Some(existing) = self.mutable.get(&key) {
            if item.seq <= existing.item.seq {
                return false;
            }
        }

        let is_new = !self.mutable.contains_key(&key);

        if is_new {
            // Evict if at capacity
            while self.count() >= self.max_items {
                self.evict_lru();
            }
        }

        self.mutable.insert(key, StoredEntry::new(item));
        true
    }

    fn count(&self) -> usize {
        self.immutable.len() + self.mutable.len()
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
        let store = InMemoryDhtStorage::new(100);
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
        assert!(store.put_mutable(item4_new)); // newer seq accepted

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
        assert_eq!(store.count(), 2);
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
        assert_eq!(store.count(), 3);

        // Adding a 4th should evict the oldest (item1)
        let item4 = make_immutable(b"1:d");
        store.put_immutable(item4);
        assert_eq!(store.count(), 3);
        assert!(store.get_immutable(&target1).is_none());
    }

    #[test]
    fn expire_removes_old_items() {
        let mut store = InMemoryDhtStorage::new(100);
        store.put_immutable(make_immutable(b"1:a"));
        store.put_mutable(make_mutable(1, 1, b""));
        assert_eq!(store.count(), 2);

        // Expire with zero lifetime removes everything
        store.expire(Duration::from_secs(0));
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn count_includes_both_types() {
        let mut store = InMemoryDhtStorage::new(100);
        store.put_immutable(make_immutable(b"1:a"));
        store.put_mutable(make_mutable(1, 1, b""));
        assert_eq!(store.count(), 2);
    }

    #[test]
    fn default_max_items_is_700() {
        let store = InMemoryDhtStorage::default();
        assert_eq!(store.max_items, 700);
    }
}
```

**Step 2: Register storage module in lib.rs**

In `/mnt/TempNVME/projects/ferrite/crates/ferrite-dht/src/lib.rs`, add:

```rust
pub mod storage;
```

And add to exports:

```rust
pub use storage::{DhtStorage, InMemoryDhtStorage};
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-dht storage -- --nocapture`
Expected: 10 tests PASS

---

## Task 3: KRPC Get/Put Message Encoding and Decoding

**Files:**
- Modify: `crates/ferrite-dht/src/krpc.rs`

**Step 1: Add `Get` and `Put` variants to `KrpcQuery`**

In `/mnt/TempNVME/projects/ferrite/crates/ferrite-dht/src/krpc.rs`, add two new variants to `KrpcQuery`:

```rust
    /// BEP 44: get an item from DHT storage.
    Get {
        id: Id20,
        /// Target hash: SHA-1(value) for immutable, SHA-1(pubkey+salt) for mutable.
        target: Id20,
        /// Optional: if set, only return mutable items with seq > this value.
        seq: Option<i64>,
    },
    /// BEP 44: put an item into DHT storage.
    Put {
        id: Id20,
        /// Write token (obtained from a prior get response).
        token: Vec<u8>,
        /// The bencoded value to store.
        value: Vec<u8>,
        /// For mutable items: ed25519 public key (32 bytes).
        key: Option<[u8; 32]>,
        /// For mutable items: ed25519 signature (64 bytes).
        signature: Option<[u8; 64]>,
        /// For mutable items: sequence number.
        seq: Option<i64>,
        /// For mutable items: optional salt.
        salt: Option<Vec<u8>>,
        /// For mutable items: optional CAS (compare-and-swap) expected seq.
        cas: Option<i64>,
    },
```

**Step 2: Add `GetItem` variant to `KrpcResponse`**

Add a new variant to `KrpcResponse`:

```rust
    /// BEP 44: response to a get query.
    GetItem {
        id: Id20,
        token: Option<Vec<u8>>,
        nodes: Vec<CompactNodeInfo>,
        nodes6: Vec<CompactNodeInfo6>,
        /// The stored value (if found).
        value: Option<Vec<u8>>,
        /// For mutable items: ed25519 public key.
        key: Option<[u8; 32]>,
        /// For mutable items: signature.
        signature: Option<[u8; 64]>,
        /// For mutable items: sequence number.
        seq: Option<i64>,
    },
```

**Step 3: Update `method_name()` and `sender_id()` for new variants**

In `KrpcQuery::method_name()`:

```rust
            KrpcQuery::Get { .. } => "get",
            KrpcQuery::Put { .. } => "put",
```

In `KrpcQuery::sender_id()`:

```rust
            | KrpcQuery::Get { id, .. }
            | KrpcQuery::Put { id, .. } => id,
```

In `KrpcResponse::sender_id()`:

```rust
            KrpcResponse::GetItem { id, .. } => id,
```

**Step 4: Update `encode_query_args` for Get and Put**

Add these arms to the match in `encode_query_args`:

```rust
        KrpcQuery::Get { id, target, seq } => {
            args.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
            args.insert(b"target".to_vec(), BencodeValue::Bytes(target.0.to_vec()));
            if let Some(seq) = seq {
                args.insert(b"seq".to_vec(), BencodeValue::Integer(*seq));
            }
        }
        KrpcQuery::Put {
            id,
            token,
            value,
            key,
            signature,
            seq,
            salt,
            cas,
        } => {
            args.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
            if let Some(cas) = cas {
                args.insert(b"cas".to_vec(), BencodeValue::Integer(*cas));
            }
            if let Some(key) = key {
                args.insert(b"k".to_vec(), BencodeValue::Bytes(key.to_vec()));
            }
            if let Some(salt) = salt {
                if !salt.is_empty() {
                    args.insert(b"salt".to_vec(), BencodeValue::Bytes(salt.clone()));
                }
            }
            if let Some(seq) = seq {
                args.insert(b"seq".to_vec(), BencodeValue::Integer(*seq));
            }
            if let Some(sig) = signature {
                args.insert(b"sig".to_vec(), BencodeValue::Bytes(sig.to_vec()));
            }
            args.insert(b"token".to_vec(), BencodeValue::Bytes(token.clone()));
            args.insert(b"v".to_vec(), BencodeValue::Bytes(value.clone()));
        }
```

**Step 5: Update `encode_response_values` for GetItem**

Add this arm:

```rust
        KrpcResponse::GetItem {
            id,
            token,
            nodes,
            nodes6,
            value,
            key,
            signature,
            seq,
        } => {
            values.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
            if let Some(key) = key {
                values.insert(b"k".to_vec(), BencodeValue::Bytes(key.to_vec()));
            }
            if !nodes.is_empty() {
                values.insert(
                    b"nodes".to_vec(),
                    BencodeValue::Bytes(encode_compact_nodes(nodes)),
                );
            }
            if !nodes6.is_empty() {
                values.insert(
                    b"nodes6".to_vec(),
                    BencodeValue::Bytes(encode_compact_nodes6(nodes6)),
                );
            }
            if let Some(seq) = seq {
                values.insert(b"seq".to_vec(), BencodeValue::Integer(*seq));
            }
            if let Some(sig) = signature {
                values.insert(b"sig".to_vec(), BencodeValue::Bytes(sig.to_vec()));
            }
            if let Some(token) = token {
                values.insert(b"token".to_vec(), BencodeValue::Bytes(token.clone()));
            }
            if let Some(v) = value {
                values.insert(b"v".to_vec(), BencodeValue::Bytes(v.clone()));
            }
        }
```

**Step 6: Update `decode_query` for `get` and `put`**

Add these arms to the match in `decode_query`:

```rust
        b"get" => {
            let target = args_id20(args, b"target")?;
            let seq = args.get(&b"seq"[..]).and_then(|v| v.as_int());
            Ok(KrpcQuery::Get { id, target, seq })
        }
        b"put" => {
            let token = args
                .get(&b"token"[..])
                .and_then(|v| v.as_bytes_raw())
                .map(|b| b.to_vec())
                .ok_or_else(|| Error::InvalidMessage("missing 'token' in put".into()))?;
            let value = args
                .get(&b"v"[..])
                .and_then(|v| v.as_bytes_raw())
                .map(|b| b.to_vec())
                .ok_or_else(|| Error::InvalidMessage("missing 'v' in put".into()))?;
            let key = args
                .get(&b"k"[..])
                .and_then(|v| v.as_bytes_raw())
                .and_then(|b| <[u8; 32]>::try_from(b).ok());
            let signature = args
                .get(&b"sig"[..])
                .and_then(|v| v.as_bytes_raw())
                .and_then(|b| <[u8; 64]>::try_from(b).ok());
            let seq = args.get(&b"seq"[..]).and_then(|v| v.as_int());
            let salt = args
                .get(&b"salt"[..])
                .and_then(|v| v.as_bytes_raw())
                .map(|b| b.to_vec());
            let cas = args.get(&b"cas"[..]).and_then(|v| v.as_int());
            Ok(KrpcQuery::Put {
                id,
                token,
                value,
                key,
                signature,
                seq,
                salt,
                cas,
            })
        }
```

**Step 7: Update `decode_response` for GetItem**

The `decode_response` function currently uses heuristics to determine response type. We need to handle `GetItem` responses (identified by having a `v` key and/or `k` key). Insert this check **before** the `has_values || has_token` check (the get_peers check), because `get` responses also have a `token` field:

```rust
    // BEP 44 get response: has "k" (mutable) or "v" (value found).
    // Check before get_peers since both have "token".
    let has_v = values.contains_key(&b"v"[..]);
    let has_k = values.contains_key(&b"k"[..]);
    let has_sig = values.contains_key(&b"sig"[..]);

    if has_k || has_sig || (has_v && !has_values) {
        let token = values
            .get(&b"token"[..])
            .and_then(|v| v.as_bytes_raw())
            .map(|b| b.to_vec());

        let nodes = if let Some(nodes_bytes) = values.get(&b"nodes"[..]).and_then(|v| v.as_bytes_raw()) {
            parse_compact_nodes(nodes_bytes)?
        } else {
            Vec::new()
        };

        let nodes6 = if let Some(nodes6_bytes) = values.get(&b"nodes6"[..]).and_then(|v| v.as_bytes_raw()) {
            parse_compact_nodes6(nodes6_bytes)?
        } else {
            Vec::new()
        };

        let value = values
            .get(&b"v"[..])
            .and_then(|v| v.as_bytes_raw())
            .map(|b| b.to_vec());

        let key = values
            .get(&b"k"[..])
            .and_then(|v| v.as_bytes_raw())
            .and_then(|b| <[u8; 32]>::try_from(b).ok());

        let signature = values
            .get(&b"sig"[..])
            .and_then(|v| v.as_bytes_raw())
            .and_then(|b| <[u8; 64]>::try_from(b).ok());

        let seq = values.get(&b"seq"[..]).and_then(|v| v.as_int());

        return Ok(KrpcResponse::GetItem {
            id,
            token,
            nodes,
            nodes6,
            value,
            key,
            signature,
            seq,
        });
    }
```

**Step 8: Add round-trip tests for BEP 44 KRPC messages**

Add to the `tests` module in `krpc.rs`:

```rust
    // --- BEP 44 KRPC tests ---

    #[test]
    fn get_immutable_query_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(400),
            body: KrpcBody::Query(KrpcQuery::Get {
                id: test_id(),
                target: target_id(),
                seq: None,
            }),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_mutable_query_with_seq_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(401),
            body: KrpcBody::Query(KrpcQuery::Get {
                id: test_id(),
                target: target_id(),
                seq: Some(42),
            }),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn put_immutable_query_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(402),
            body: KrpcBody::Query(KrpcQuery::Put {
                id: test_id(),
                token: b"tok12345".to_vec(),
                value: b"12:Hello World!".to_vec(),
                key: None,
                signature: None,
                seq: None,
                salt: None,
                cas: None,
            }),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn put_mutable_query_round_trip() {
        let key = [0xABu8; 32];
        let sig = [0xCDu8; 64];
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(403),
            body: KrpcBody::Query(KrpcQuery::Put {
                id: test_id(),
                token: b"tok12345".to_vec(),
                value: b"12:Hello World!".to_vec(),
                key: Some(key),
                signature: Some(sig),
                seq: Some(4),
                salt: Some(b"foobar".to_vec()),
                cas: Some(3),
            }),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_response_immutable_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(404),
            body: KrpcBody::Response(KrpcResponse::GetItem {
                id: test_id(),
                token: Some(b"tok".to_vec()),
                nodes: Vec::new(),
                nodes6: Vec::new(),
                value: Some(b"12:Hello World!".to_vec()),
                key: None,
                signature: None,
                seq: None,
            }),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_response_mutable_round_trip() {
        let key = [0xABu8; 32];
        let sig = [0xCDu8; 64];
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(405),
            body: KrpcBody::Response(KrpcResponse::GetItem {
                id: test_id(),
                token: Some(b"tok".to_vec()),
                nodes: vec![CompactNodeInfo {
                    id: target_id(),
                    addr: "10.0.0.1:6881".parse().unwrap(),
                }],
                nodes6: Vec::new(),
                value: Some(b"4:test".to_vec()),
                key: Some(key),
                signature: Some(sig),
                seq: Some(7),
            }),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_response_not_found_round_trip() {
        // Response with no value but with closer nodes
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(406),
            body: KrpcBody::Response(KrpcResponse::GetItem {
                id: test_id(),
                token: Some(b"tok".to_vec()),
                nodes: vec![CompactNodeInfo {
                    id: target_id(),
                    addr: "10.0.0.1:6881".parse().unwrap(),
                }],
                nodes6: Vec::new(),
                value: None,
                key: None,
                signature: None,
                seq: None,
            }),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn bep44_query_method_names() {
        assert_eq!(
            KrpcQuery::Get {
                id: Id20::ZERO,
                target: Id20::ZERO,
                seq: None,
            }.method_name(),
            "get"
        );
        assert_eq!(
            KrpcQuery::Put {
                id: Id20::ZERO,
                token: Vec::new(),
                value: Vec::new(),
                key: None,
                signature: None,
                seq: None,
                salt: None,
                cas: None,
            }.method_name(),
            "put"
        );
    }
```

**Step 9: Run tests**

Run: `cargo test -p ferrite-dht krpc -- --nocapture`
Expected: All existing tests + 8 new tests PASS

---

## Task 4: DHT Actor — Handle Incoming Get/Put Queries

**Files:**
- Modify: `crates/ferrite-dht/src/actor.rs`

**Step 1: Add storage field to DhtActor and DhtConfig**

In `DhtConfig`, add:

```rust
    /// Maximum BEP 44 items stored (immutable + mutable).
    pub dht_max_items: usize,
    /// Lifetime of BEP 44 items in seconds before expiry.
    pub dht_item_lifetime_secs: u64,
```

In `DhtConfig::default()`:

```rust
            dht_max_items: 700,
            dht_item_lifetime_secs: 7200,
```

And in `DhtConfig::default_v6()`:

```rust
            dht_max_items: 700,
            dht_item_lifetime_secs: 7200,
```

In `DhtActor`, add a field:

```rust
    item_store: Box<dyn crate::storage::DhtStorage>,
```

In `DhtActor::new()`, initialize it:

```rust
    item_store: Box::new(crate::storage::InMemoryDhtStorage::new(config.dht_max_items)),
```

**Step 2: Add item store cleanup to the actor run loop**

In the `cleanup_tick` handler in `DhtActor::run()`, add item store expiry after the peer store cleanup:

```rust
    self.item_store.expire(
        Duration::from_secs(self.config.dht_item_lifetime_secs)
    );
```

**Step 3: Add DhtStats fields for BEP 44**

In `DhtStats`, add:

```rust
    pub dht_item_count: usize,
```

In `DhtActor::make_stats()`, add:

```rust
    dht_item_count: self.item_store.count(),
```

**Step 4: Handle incoming `get` queries**

In `DhtActor::handle_query()`, add a new arm for `KrpcQuery::Get`:

```rust
            KrpcQuery::Get { id: _, target, seq: requested_seq } => {
                let ip = addr.ip();
                let token = self.peer_store.generate_token(&ip);

                // Try immutable first
                if let Some(item) = self.item_store.get_immutable(target) {
                    KrpcResponse::GetItem {
                        id: *self.routing_table.own_id(),
                        token: Some(token),
                        nodes: Vec::new(),
                        nodes6: Vec::new(),
                        value: Some(item.value),
                        key: None,
                        signature: None,
                        seq: None,
                    }
                } else if let Some(item) = self.find_mutable_by_target(target) {
                    // Check if requester wants only items with seq > requested_seq
                    if let Some(min_seq) = requested_seq {
                        if item.seq <= *min_seq {
                            // Return token + nodes but no value (requester already has this or newer)
                            let closest = self.routing_table.closest(target, K);
                            let nodes: Vec<CompactNodeInfo> = closest
                                .into_iter()
                                .map(|n| CompactNodeInfo { id: n.id, addr: n.addr })
                                .collect();
                            KrpcResponse::GetItem {
                                id: *self.routing_table.own_id(),
                                token: Some(token),
                                nodes,
                                nodes6: Vec::new(),
                                value: None,
                                key: Some(item.public_key),
                                signature: Some(item.signature),
                                seq: Some(item.seq),
                            }
                        } else {
                            KrpcResponse::GetItem {
                                id: *self.routing_table.own_id(),
                                token: Some(token),
                                nodes: Vec::new(),
                                nodes6: Vec::new(),
                                value: Some(item.value),
                                key: Some(item.public_key),
                                signature: Some(item.signature),
                                seq: Some(item.seq),
                            }
                        }
                    } else {
                        KrpcResponse::GetItem {
                            id: *self.routing_table.own_id(),
                            token: Some(token),
                            nodes: Vec::new(),
                            nodes6: Vec::new(),
                            value: Some(item.value),
                            key: Some(item.public_key),
                            signature: Some(item.signature),
                            seq: Some(item.seq),
                        }
                    }
                } else {
                    // Not found — return closer nodes
                    let closest = self.routing_table.closest(target, K);
                    let nodes: Vec<CompactNodeInfo> = closest
                        .into_iter()
                        .map(|n| CompactNodeInfo { id: n.id, addr: n.addr })
                        .collect();
                    KrpcResponse::GetItem {
                        id: *self.routing_table.own_id(),
                        token: Some(token),
                        nodes,
                        nodes6: Vec::new(),
                        value: None,
                        key: None,
                        signature: None,
                        seq: None,
                    }
                }
            }
```

Add a helper to `DhtActor` for finding mutable items by target:

```rust
    /// Search for a mutable item whose target matches the given hash.
    ///
    /// This is a linear scan — fine for the default 700-item limit.
    fn find_mutable_by_target(&self, target: &Id20) -> Option<crate::bep44::MutableItem> {
        // We can't efficiently look up by target in the mutable store because
        // the key is (pubkey, salt). We'll need the caller to track this.
        // For now, return None — the store lookup happens via the storage trait.
        //
        // Actually, let's do it properly: we need a target index.
        // We'll add this as a method on InMemoryDhtStorage.
        None
    }
```

Wait — we need to rethink this. The storage trait uses `(pubkey, salt)` as the mutable key, but incoming `get` queries only have the `target = SHA-1(pubkey + salt)`. We need the storage trait to support lookup by target.

Add a new method to `DhtStorage` trait in `storage.rs`:

```rust
    /// Retrieve a mutable item by its derived target hash.
    ///
    /// Target = SHA-1(public_key + salt). This is used for incoming get queries.
    fn get_mutable_by_target(&self, target: &Id20) -> Option<MutableItem>;
```

And implement it in `InMemoryDhtStorage`:

```rust
    fn get_mutable_by_target(&self, target: &Id20) -> Option<MutableItem> {
        for (key, entry) in &self.mutable {
            let computed = crate::bep44::compute_mutable_target(&key.public_key, &key.salt);
            if computed == *target {
                return Some(entry.item.clone());
            }
        }
        None
    }
```

Then replace `find_mutable_by_target` in the actor with `self.item_store.get_mutable_by_target(target)`.

**Step 5: Handle incoming `put` queries**

Add this arm in `DhtActor::handle_query()`:

```rust
            KrpcQuery::Put {
                id: _,
                token,
                value,
                key,
                signature,
                seq,
                salt,
                cas,
            } => {
                let ip = addr.ip();

                // Validate token
                if !self.peer_store.validate_token(token, &ip) {
                    let err_msg = KrpcMessage {
                        transaction_id: msg.transaction_id,
                        body: KrpcBody::Error {
                            code: 203,
                            message: "invalid token".into(),
                        },
                    };
                    if let Ok(bytes) = err_msg.to_bytes() {
                        let _ = self.socket.send_to(&bytes, addr).await;
                    }
                    return;
                }

                // Validate value size
                if value.len() > crate::bep44::MAX_VALUE_SIZE {
                    let err_msg = KrpcMessage {
                        transaction_id: msg.transaction_id,
                        body: KrpcBody::Error {
                            code: 205,
                            message: "message (v field) too big".into(),
                        },
                    };
                    if let Ok(bytes) = err_msg.to_bytes() {
                        let _ = self.socket.send_to(&bytes, addr).await;
                    }
                    return;
                }

                if let (Some(k), Some(sig), Some(seq_val)) = (key, signature, seq) {
                    // Mutable item
                    let salt_bytes = salt.clone().unwrap_or_default();

                    // Validate salt size
                    if salt_bytes.len() > crate::bep44::MAX_SALT_SIZE {
                        let err_msg = KrpcMessage {
                            transaction_id: msg.transaction_id,
                            body: KrpcBody::Error {
                                code: 207,
                                message: "salt (salt field) too big".into(),
                            },
                        };
                        if let Ok(bytes) = err_msg.to_bytes() {
                            let _ = self.socket.send_to(&bytes, addr).await;
                        }
                        return;
                    }

                    let item = crate::bep44::MutableItem {
                        value: value.clone(),
                        public_key: *k,
                        signature: *sig,
                        seq: *seq_val,
                        salt: salt_bytes,
                        target: crate::bep44::compute_mutable_target(k, salt.as_deref().unwrap_or(&[])),
                    };

                    // Verify signature
                    if !item.verify() {
                        let err_msg = KrpcMessage {
                            transaction_id: msg.transaction_id,
                            body: KrpcBody::Error {
                                code: 206,
                                message: "invalid signature".into(),
                            },
                        };
                        if let Ok(bytes) = err_msg.to_bytes() {
                            let _ = self.socket.send_to(&bytes, addr).await;
                        }
                        return;
                    }

                    // CAS check
                    if let Some(expected_seq) = cas {
                        if let Some(existing) = self.item_store.get_mutable(k, &item.salt) {
                            if existing.seq != *expected_seq {
                                let err_msg = KrpcMessage {
                                    transaction_id: msg.transaction_id,
                                    body: KrpcBody::Error {
                                        code: 301,
                                        message: format!(
                                            "CAS mismatch: expected seq {}, got {}",
                                            expected_seq, existing.seq
                                        ),
                                    },
                                };
                                if let Ok(bytes) = err_msg.to_bytes() {
                                    let _ = self.socket.send_to(&bytes, addr).await;
                                }
                                return;
                            }
                        }
                    }

                    // Seq monotonicity check
                    if let Some(existing) = self.item_store.get_mutable(k, &item.salt) {
                        if *seq_val <= existing.seq {
                            let err_msg = KrpcMessage {
                                transaction_id: msg.transaction_id,
                                body: KrpcBody::Error {
                                    code: 302,
                                    message: format!(
                                        "sequence number not newer: {} <= {}",
                                        seq_val, existing.seq
                                    ),
                                },
                            };
                            if let Ok(bytes) = err_msg.to_bytes() {
                                let _ = self.socket.send_to(&bytes, addr).await;
                            }
                            return;
                        }
                    }

                    self.item_store.put_mutable(item);
                } else {
                    // Immutable item
                    match crate::bep44::ImmutableItem::new(value.clone()) {
                        Ok(item) => {
                            self.item_store.put_immutable(item);
                        }
                        Err(_) => {
                            let err_msg = KrpcMessage {
                                transaction_id: msg.transaction_id,
                                body: KrpcBody::Error {
                                    code: 205,
                                    message: "message (v field) too big".into(),
                                },
                            };
                            if let Ok(bytes) = err_msg.to_bytes() {
                                let _ = self.socket.send_to(&bytes, addr).await;
                            }
                            return;
                        }
                    }
                }

                KrpcResponse::NodeId {
                    id: *self.routing_table.own_id(),
                }
            }
```

**Step 6: Add new PendingQueryKind variants**

Add to `PendingQueryKind`:

```rust
    GetItem { target: Id20 },
    PutItem,
```

**Step 7: Run tests**

Run: `cargo test -p ferrite-dht -- --nocapture`
Expected: All tests PASS

---

## Task 5: DHT Handle — Public Put/Get API

**Files:**
- Modify: `crates/ferrite-dht/src/actor.rs`

**Step 1: Add DhtCommand variants for put/get**

Add to `DhtCommand`:

```rust
    GetImmutable {
        target: Id20,
        reply: oneshot::Sender<Result<Option<Vec<u8>>>>,
    },
    PutImmutable {
        value: Vec<u8>,
        reply: oneshot::Sender<Result<Id20>>,
    },
    GetMutable {
        public_key: [u8; 32],
        salt: Vec<u8>,
        reply: oneshot::Sender<Result<Option<(Vec<u8>, i64)>>>,
    },
    PutMutable {
        keypair_bytes: [u8; 32],
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
        reply: oneshot::Sender<Result<Id20>>,
    },
```

**Step 2: Add public methods to DhtHandle**

```rust
    /// Store an immutable item in the DHT (BEP 44).
    ///
    /// Returns the SHA-1 target hash that can be used to retrieve the item.
    /// The value must be valid bencoded data, max 1000 bytes.
    pub async fn put_immutable(&self, value: Vec<u8>) -> Result<Id20> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::PutImmutable { value, reply: reply_tx })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Retrieve an immutable item from the DHT (BEP 44).
    ///
    /// Returns the raw bencoded value if found, `None` if not.
    pub async fn get_immutable(&self, target: Id20) -> Result<Option<Vec<u8>>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::GetImmutable { target, reply: reply_tx })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Store a mutable item in the DHT (BEP 44).
    ///
    /// - `keypair_bytes`: 32-byte ed25519 seed (secret key)
    /// - `value`: bencoded data, max 1000 bytes
    /// - `seq`: sequence number (must be higher than any previously stored)
    /// - `salt`: optional salt for sub-key isolation (max 64 bytes)
    ///
    /// Returns the target hash.
    pub async fn put_mutable(
        &self,
        keypair_bytes: [u8; 32],
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
    ) -> Result<Id20> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::PutMutable {
                keypair_bytes,
                value,
                seq,
                salt,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Retrieve a mutable item from the DHT (BEP 44).
    ///
    /// Returns `(value, seq)` if found, `None` if not.
    pub async fn get_mutable(
        &self,
        public_key: [u8; 32],
        salt: Vec<u8>,
    ) -> Result<Option<(Vec<u8>, i64)>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::GetMutable {
                public_key,
                salt,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }
```

**Step 3: Handle new commands in the actor run loop**

In the `cmd = self.rx.recv()` arm of the actor loop, add:

```rust
                        Some(DhtCommand::GetImmutable { target, reply }) => {
                            self.handle_get_immutable(target, reply).await;
                        }
                        Some(DhtCommand::PutImmutable { value, reply }) => {
                            self.handle_put_immutable(value, reply).await;
                        }
                        Some(DhtCommand::GetMutable { public_key, salt, reply }) => {
                            self.handle_get_mutable(public_key, salt, reply).await;
                        }
                        Some(DhtCommand::PutMutable { keypair_bytes, value, seq, salt, reply }) => {
                            self.handle_put_mutable(keypair_bytes, value, seq, salt, reply).await;
                        }
```

**Step 4: Implement actor get/put methods**

These methods initiate iterative lookups. For simplicity in this initial implementation, they first store/retrieve from the local store, then initiate a Kademlia lookup to propagate:

```rust
    async fn handle_get_immutable(
        &mut self,
        target: Id20,
        reply: oneshot::Sender<Result<Option<Vec<u8>>>>,
    ) {
        // Check local store first
        if let Some(item) = self.item_store.get_immutable(&target) {
            let _ = reply.send(Ok(Some(item.value)));
            return;
        }

        // Initiate iterative get to the closest nodes
        let closest = self.routing_table.closest(&target, K);
        for node in closest.iter().take(3) {
            self.send_get_item(node.addr, target, None).await;
        }

        // For this first iteration, store the pending get and reply
        // when any node returns the value. We use lookups for tracking.
        self.item_lookups.insert(target, ItemLookupState::Immutable {
            reply: Some(reply),
            queried: closest.iter().map(|n| n.id).collect(),
            closest: closest.into_iter()
                .map(|n| CompactNodeInfo { id: n.id, addr: n.addr })
                .collect(),
        });
    }

    async fn handle_put_immutable(
        &mut self,
        value: Vec<u8>,
        reply: oneshot::Sender<Result<Id20>>,
    ) {
        let item = match crate::bep44::ImmutableItem::new(value) {
            Ok(item) => item,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let target = item.target;

        // Store locally
        self.item_store.put_immutable(item.clone());

        // Propagate: find closest nodes to the target, then put to them
        // First we need tokens from those nodes — initiate a get first.
        let closest = self.routing_table.closest(&target, K);
        for node in closest.iter().take(K) {
            self.send_get_item(node.addr, target, None).await;
        }

        // Track the put operation
        self.item_put_ops.insert(target, ItemPutState::Immutable {
            item: item.clone(),
            tokens: HashMap::new(),
            sent_puts: 0,
            reply: Some(reply),
        });
    }

    async fn handle_get_mutable(
        &mut self,
        public_key: [u8; 32],
        salt: Vec<u8>,
        reply: oneshot::Sender<Result<Option<(Vec<u8>, i64)>>>,
    ) {
        let target = crate::bep44::compute_mutable_target(&public_key, &salt);

        // Check local store first
        if let Some(item) = self.item_store.get_mutable(&public_key, &salt) {
            let _ = reply.send(Ok(Some((item.value, item.seq))));
            return;
        }

        // Initiate iterative get
        let closest = self.routing_table.closest(&target, K);
        for node in closest.iter().take(3) {
            self.send_get_item(node.addr, target, None).await;
        }

        self.item_lookups.insert(target, ItemLookupState::Mutable {
            public_key,
            salt,
            reply: Some(reply),
            best_seq: i64::MIN,
            best_value: None,
            queried: closest.iter().map(|n| n.id).collect(),
            closest: closest.into_iter()
                .map(|n| CompactNodeInfo { id: n.id, addr: n.addr })
                .collect(),
        });
    }

    async fn handle_put_mutable(
        &mut self,
        keypair_bytes: [u8; 32],
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
        reply: oneshot::Sender<Result<Id20>>,
    ) {
        let keypair = ed25519_dalek::SigningKey::from_bytes(&keypair_bytes);
        let item = match crate::bep44::MutableItem::create(&keypair, value, seq, salt) {
            Ok(item) => item,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let target = item.target;

        // Store locally
        self.item_store.put_mutable(item.clone());

        // Propagate: find closest nodes, get tokens, then put
        let closest = self.routing_table.closest(&target, K);
        for node in closest.iter().take(K) {
            self.send_get_item(node.addr, target, None).await;
        }

        self.item_put_ops.insert(target, ItemPutState::Mutable {
            item: item.clone(),
            tokens: HashMap::new(),
            sent_puts: 0,
            reply: Some(reply),
        });
    }
```

**Step 5: Add item lookup state structs and maps to DhtActor**

Add new fields to `DhtActor`:

```rust
    /// Active BEP 44 get lookups.
    item_lookups: HashMap<Id20, ItemLookupState>,
    /// Active BEP 44 put operations (waiting for tokens before sending puts).
    item_put_ops: HashMap<Id20, ItemPutState>,
```

Initialize both as `HashMap::new()` in `DhtActor::new()`.

Add the state types:

```rust
enum ItemLookupState {
    Immutable {
        reply: Option<oneshot::Sender<Result<Option<Vec<u8>>>>>,
        queried: std::collections::HashSet<Id20>,
        closest: Vec<CompactNodeInfo>,
    },
    Mutable {
        public_key: [u8; 32],
        salt: Vec<u8>,
        reply: Option<oneshot::Sender<Result<Option<(Vec<u8>, i64)>>>>,
        best_seq: i64,
        best_value: Option<Vec<u8>>,
        queried: std::collections::HashSet<Id20>,
        closest: Vec<CompactNodeInfo>,
    },
}

enum ItemPutState {
    Immutable {
        item: crate::bep44::ImmutableItem,
        tokens: HashMap<Id20, (SocketAddr, Vec<u8>)>,
        sent_puts: usize,
        reply: Option<oneshot::Sender<Result<Id20>>>,
    },
    Mutable {
        item: crate::bep44::MutableItem,
        tokens: HashMap<Id20, (SocketAddr, Vec<u8>)>,
        sent_puts: usize,
        reply: Option<oneshot::Sender<Result<Id20>>>,
    },
}
```

**Step 6: Add send_get_item and send_put_item helper methods**

```rust
    async fn send_get_item(&mut self, addr: SocketAddr, target: Id20, seq: Option<i64>) {
        let txn = self.next_transaction_id();
        let own_id = *self.routing_table.own_id();
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(txn),
            body: KrpcBody::Query(KrpcQuery::Get {
                id: own_id,
                target,
                seq,
            }),
        };
        if let Ok(bytes) = msg.to_bytes() {
            let _ = self.socket.send_to(&bytes, addr).await;
            self.pending.insert(
                txn,
                PendingQuery {
                    sent_at: Instant::now(),
                    addr,
                    kind: PendingQueryKind::GetItem { target },
                },
            );
            self.stats.total_queries_sent += 1;
        }
    }

    async fn send_put_item(
        &mut self,
        addr: SocketAddr,
        token: Vec<u8>,
        value: Vec<u8>,
        key: Option<[u8; 32]>,
        signature: Option<[u8; 64]>,
        seq: Option<i64>,
        salt: Option<Vec<u8>>,
    ) {
        let txn = self.next_transaction_id();
        let own_id = *self.routing_table.own_id();
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(txn),
            body: KrpcBody::Query(KrpcQuery::Put {
                id: own_id,
                token,
                value,
                key,
                signature,
                seq,
                salt,
                cas: None,
            }),
        };
        if let Ok(bytes) = msg.to_bytes() {
            let _ = self.socket.send_to(&bytes, addr).await;
            self.pending.insert(
                txn,
                PendingQuery {
                    sent_at: Instant::now(),
                    addr,
                    kind: PendingQueryKind::PutItem,
                },
            );
            self.stats.total_queries_sent += 1;
        }
    }
```

**Step 7: Handle GetItem responses in handle_response**

Add this arm in the `match (&pending.kind, resp)` block:

```rust
            (PendingQueryKind::GetItem { target }, KrpcResponse::GetItem {
                token, nodes, nodes6, value, key, signature, seq, ..
            }) => {
                // Add discovered nodes to routing table
                for node in nodes {
                    if self.matches_family(&node.addr) {
                        self.routing_table.insert(node.id, node.addr);
                    }
                }
                for node in nodes6 {
                    if self.matches_family(&node.addr) {
                        self.routing_table.insert(node.id, node.addr);
                    }
                }

                let target = *target;

                // If we have a put operation waiting for tokens, collect this token
                if let (Some(token), Some(put_op)) = (token, self.item_put_ops.get_mut(&target)) {
                    match put_op {
                        ItemPutState::Immutable { tokens, .. }
                        | ItemPutState::Mutable { tokens, .. } => {
                            tokens.insert(sender_id, (addr, token.clone()));
                        }
                    }

                    // If we have enough tokens, send the puts
                    let should_send = match &self.item_put_ops[&target] {
                        ItemPutState::Immutable { tokens, sent_puts, .. }
                        | ItemPutState::Mutable { tokens, sent_puts, .. } => {
                            tokens.len() >= K && *sent_puts == 0
                        }
                    };

                    if should_send {
                        self.send_pending_puts(target).await;
                    }
                }

                // If we have a get lookup, process the value
                if let Some(lookup) = self.item_lookups.get_mut(&target) {
                    match lookup {
                        ItemLookupState::Immutable { reply, .. } => {
                            if let Some(v) = value {
                                // Validate: SHA-1(v) should equal target
                                if ferrite_core::sha1(v) == target {
                                    // Store locally
                                    if let Ok(item) = crate::bep44::ImmutableItem::new(v.clone()) {
                                        self.item_store.put_immutable(item);
                                    }
                                    if let Some(r) = reply.take() {
                                        let _ = r.send(Ok(Some(v.clone())));
                                    }
                                }
                            } else {
                                // No value — continue iterative lookup with new closer nodes
                                // (simplified: just query new nodes)
                                let new_nodes: Vec<CompactNodeInfo> = nodes
                                    .iter()
                                    .filter(|n| self.matches_family(&n.addr))
                                    .copied()
                                    .collect();
                                for node in new_nodes.iter().take(3) {
                                    if let ItemLookupState::Immutable { queried, .. } =
                                        self.item_lookups.get_mut(&target).unwrap()
                                    {
                                        if queried.insert(node.id) {
                                            self.send_get_item(node.addr, target, None).await;
                                        }
                                    }
                                }
                            }
                        }
                        ItemLookupState::Mutable { best_seq, best_value, reply, .. } => {
                            if let (Some(v), Some(k), Some(sig), Some(s)) = (value, key, signature, seq) {
                                // Verify the signature
                                let item = crate::bep44::MutableItem {
                                    value: v.clone(),
                                    public_key: *k,
                                    signature: *sig,
                                    seq: *s,
                                    salt: match lookup {
                                        ItemLookupState::Mutable { salt, .. } => salt.clone(),
                                        _ => Vec::new(),
                                    },
                                    target,
                                };
                                if item.verify() && *s > *best_seq {
                                    *best_seq = *s;
                                    *best_value = Some(v.clone());
                                    // Store locally
                                    self.item_store.put_mutable(item);
                                }
                            }
                            // Continue iterative lookup
                            let new_nodes: Vec<CompactNodeInfo> = nodes
                                .iter()
                                .filter(|n| self.matches_family(&n.addr))
                                .copied()
                                .collect();
                            for node in new_nodes.iter().take(3) {
                                if let ItemLookupState::Mutable { queried, .. } =
                                    self.item_lookups.get_mut(&target).unwrap()
                                {
                                    if queried.insert(node.id) {
                                        self.send_get_item(node.addr, target, None).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            (PendingQueryKind::PutItem, KrpcResponse::NodeId { .. }) => {
                // Put acknowledged. Nothing to do.
            }
```

**Step 8: Add send_pending_puts helper**

```rust
    async fn send_pending_puts(&mut self, target: Id20) {
        if let Some(put_op) = self.item_put_ops.get_mut(&target) {
            match put_op {
                ItemPutState::Immutable { item, tokens, sent_puts, reply } => {
                    let token_list: Vec<(SocketAddr, Vec<u8>)> =
                        tokens.values().take(K).cloned().collect();
                    for (addr, token) in &token_list {
                        self.send_put_item(
                            *addr,
                            token.clone(),
                            item.value.clone(),
                            None,
                            None,
                            None,
                            None,
                        ).await;
                    }
                    *sent_puts = token_list.len();
                    if let Some(r) = reply.take() {
                        let _ = r.send(Ok(item.target));
                    }
                }
                ItemPutState::Mutable { item, tokens, sent_puts, reply } => {
                    let token_list: Vec<(SocketAddr, Vec<u8>)> =
                        tokens.values().take(K).cloned().collect();
                    let salt = if item.salt.is_empty() { None } else { Some(item.salt.clone()) };
                    for (addr, token) in &token_list {
                        self.send_put_item(
                            *addr,
                            token.clone(),
                            item.value.clone(),
                            Some(item.public_key),
                            Some(item.signature),
                            Some(item.seq),
                            salt.clone(),
                        ).await;
                    }
                    *sent_puts = token_list.len();
                    if let Some(r) = reply.take() {
                        let _ = r.send(Ok(item.target));
                    }
                }
            }
        }
    }
```

**Step 9: Clean up completed item lookups and puts in maintenance**

In `DhtActor::maintenance()`, add:

```rust
        // Clean up item lookups where reply channel is closed
        self.item_lookups.retain(|_, lookup| match lookup {
            ItemLookupState::Immutable { reply, .. } => reply.is_some(),
            ItemLookupState::Mutable { reply, .. } => reply.is_some(),
        });

        // Clean up completed put operations
        self.item_put_ops.retain(|_, put_op| match put_op {
            ItemPutState::Immutable { reply, .. }
            | ItemPutState::Mutable { reply, .. } => reply.is_some(),
        });
```

**Step 10: Add import for ed25519_dalek in actor.rs**

At the top of `actor.rs`:

```rust
use ed25519_dalek::SigningKey;
```

**Step 11: Add tests for the public API**

Add to `actor.rs` tests:

```rust
    #[tokio::test]
    async fn dht_put_get_immutable_local() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let handle = DhtHandle::start(config).await.unwrap();

        // Put an immutable item
        let value = b"12:Hello World!".to_vec();
        let target = handle.put_immutable(value.clone()).await.unwrap();

        // Get it back (from local store)
        let result = handle.get_immutable(target).await.unwrap();
        assert_eq!(result, Some(value));

        // Verify SHA-1 target
        assert_eq!(target, ferrite_core::sha1(b"12:Hello World!"));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_put_get_mutable_local() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let handle = DhtHandle::start(config).await.unwrap();

        let seed = [42u8; 32];
        let keypair = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pubkey = keypair.verifying_key().to_bytes();

        let value = b"4:test".to_vec();
        let target = handle.put_mutable(seed, value.clone(), 1, Vec::new()).await.unwrap();

        // Get it back (from local store)
        let result = handle.get_mutable(pubkey, Vec::new()).await.unwrap();
        assert_eq!(result, Some((value, 1)));

        // Verify target
        let expected_target = crate::bep44::compute_mutable_target(&pubkey, &[]);
        assert_eq!(target, expected_target);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_get_immutable_not_found() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let handle = DhtHandle::start(config).await.unwrap();

        let target = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        // With empty routing table, lookup has no peers to query; just returns local result
        let result = handle.get_immutable(target).await.unwrap();
        assert_eq!(result, None);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_put_immutable_rejects_oversized() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let handle = DhtHandle::start(config).await.unwrap();

        let value = vec![0u8; 1001];
        let result = handle.put_immutable(value).await;
        assert!(result.is_err());

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_stats_includes_item_count() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let handle = DhtHandle::start(config).await.unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.dht_item_count, 0);

        handle.put_immutable(b"5:hello".to_vec()).await.unwrap();
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.dht_item_count, 1);

        handle.shutdown().await.unwrap();
    }
```

**Step 12: Run all DHT tests**

Run: `cargo test -p ferrite-dht -- --nocapture`
Expected: All tests PASS

---

## Task 6: Settings and Alerts Integration

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs`
- Modify: `crates/ferrite-session/src/alert.rs`

**Step 1: Add BEP 44 settings fields**

In `/mnt/TempNVME/projects/ferrite/crates/ferrite-session/src/settings.rs`, add default helper functions:

```rust
fn default_dht_max_items() -> usize {
    700
}
fn default_dht_item_lifetime() -> u64 {
    7200
}
```

Add fields to `Settings` struct under the `// -- DHT tuning --` section:

```rust
    /// Maximum number of BEP 44 items stored in the DHT (immutable + mutable).
    #[serde(default = "default_dht_max_items")]
    pub dht_max_items: usize,
    /// Lifetime of BEP 44 DHT items in seconds before expiry (default: 7200 = 2 hours).
    #[serde(default = "default_dht_item_lifetime")]
    pub dht_item_lifetime_secs: u64,
```

In `Settings::default()`, add in the DHT section:

```rust
            dht_max_items: 700,
            dht_item_lifetime_secs: 7200,
```

Update `Settings::min_memory()` to restrict items:

```rust
            dht_max_items: 100,
```

In `Settings::to_dht_config()` and `to_dht_config_v6()`, add:

```rust
            dht_max_items: self.dht_max_items,
            dht_item_lifetime_secs: self.dht_item_lifetime_secs,
```

Update the `PartialEq` impl to include the new fields:

```rust
            && self.dht_max_items == other.dht_max_items
            && self.dht_item_lifetime_secs == other.dht_item_lifetime_secs
```

**Step 2: Add BEP 44 alert variants**

In `/mnt/TempNVME/projects/ferrite/crates/ferrite-session/src/alert.rs`, add to `AlertKind`:

```rust
    // ── BEP 44 DHT storage (M38) ──
    /// An immutable DHT put completed.
    DhtPutComplete { target: Id20 },
    /// A mutable DHT put completed.
    DhtMutablePutComplete { target: Id20, seq: i64 },
    /// Result of a DHT get (immutable). `value` is None if not found.
    DhtGetResult { target: Id20, value: Option<Vec<u8>> },
    /// Result of a DHT mutable get. `value` is None if not found.
    DhtMutableGetResult {
        target: Id20,
        value: Option<Vec<u8>>,
        seq: Option<i64>,
        public_key: [u8; 32],
    },
    /// A BEP 44 DHT operation failed.
    DhtItemError { target: Id20, message: String },
```

Update the `AlertKind::category()` match to include the new variants:

```rust
            DhtPutComplete { .. }
            | DhtMutablePutComplete { .. }
            | DhtGetResult { .. }
            | DhtMutableGetResult { .. } => AlertCategory::DHT,
            DhtItemError { .. } => AlertCategory::DHT | AlertCategory::ERROR,
```

**Step 3: Add tests for the new settings and alerts**

Add to the settings tests:

```rust
    #[test]
    fn dht_storage_settings_defaults() {
        let s = Settings::default();
        assert_eq!(s.dht_max_items, 700);
        assert_eq!(s.dht_item_lifetime_secs, 7200);
    }

    #[test]
    fn min_memory_restricts_dht_items() {
        let s = Settings::min_memory();
        assert_eq!(s.dht_max_items, 100);
    }
```

Add to the alert tests:

```rust
    #[test]
    fn dht_put_complete_alert_has_dht_category() {
        let alert = Alert::new(AlertKind::DhtPutComplete {
            target: Id20::from([0u8; 20]),
        });
        assert!(alert.category().contains(AlertCategory::DHT));
    }

    #[test]
    fn dht_item_error_alert_has_dht_and_error_category() {
        let alert = Alert::new(AlertKind::DhtItemError {
            target: Id20::from([0u8; 20]),
            message: "test".into(),
        });
        assert!(alert.category().contains(AlertCategory::DHT));
        assert!(alert.category().contains(AlertCategory::ERROR));
    }
```

**Step 4: Run tests**

Run: `cargo test -p ferrite-session settings -- --nocapture && cargo test -p ferrite-session alert -- --nocapture`
Expected: All tests PASS

---

## Task 7: Facade Re-exports and Integration

**Files:**
- Modify: `crates/ferrite/src/dht.rs`
- Modify: `crates/ferrite/src/lib.rs` (if needed)

**Step 1: Update facade dht.rs re-exports**

In `/mnt/TempNVME/projects/ferrite/crates/ferrite/src/dht.rs`, add the BEP 44 re-exports:

```rust
pub use ferrite_dht::bep44::{
    ImmutableItem, MutableItem,
    compute_mutable_target, build_signing_buffer,
    MAX_VALUE_SIZE, MAX_SALT_SIZE,
};
pub use ferrite_dht::storage::{DhtStorage, InMemoryDhtStorage};
```

**Step 2: Add ed25519-dalek to ferrite facade Cargo.toml** (only if users need to construct keypairs)

Check if `crates/ferrite/Cargo.toml` needs `ed25519-dalek`. Since the `DhtHandle::put_mutable()` takes raw `[u8; 32]` seed bytes, users can use ed25519-dalek independently. We should re-export the key types for convenience:

In `/mnt/TempNVME/projects/ferrite/crates/ferrite/Cargo.toml`, add:

```toml
ed25519-dalek = { workspace = true }
```

In `/mnt/TempNVME/projects/ferrite/crates/ferrite/src/dht.rs`, add:

```rust
// Re-export ed25519 types for BEP 44 mutable item signing
pub use ed25519_dalek::{SigningKey as Ed25519SigningKey, VerifyingKey as Ed25519VerifyingKey};
```

**Step 3: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests PASS

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings

---

## Task 8: End-to-End Integration Tests

**Files:**
- Modify: `crates/ferrite-dht/src/actor.rs` (add integration tests)

**Step 1: Add two-node put/get test**

This test starts two DHT nodes on localhost and verifies they can exchange BEP 44 items:

```rust
    #[tokio::test]
    async fn two_nodes_put_get_immutable() {
        // Start two DHT nodes
        let config_a = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            own_id: Some(Id20::from_hex("0000000000000000000000000000000000000001").unwrap()),
            ..DhtConfig::default()
        };
        let handle_a = DhtHandle::start(config_a).await.unwrap();

        // Node A stores an item locally
        let value = b"12:Hello World!".to_vec();
        let target = handle_a.put_immutable(value.clone()).await.unwrap();

        // Verify local retrieval
        let result = handle_a.get_immutable(target).await.unwrap();
        assert_eq!(result, Some(value));

        handle_a.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn put_mutable_sequence_update() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let handle = DhtHandle::start(config).await.unwrap();

        let seed = [99u8; 32];
        let keypair = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pubkey = keypair.verifying_key().to_bytes();

        // Put seq=1
        handle.put_mutable(seed, b"5:first".to_vec(), 1, Vec::new()).await.unwrap();
        let result = handle.get_mutable(pubkey, Vec::new()).await.unwrap();
        assert_eq!(result, Some((b"5:first".to_vec(), 1)));

        // Put seq=2 (should replace)
        handle.put_mutable(seed, b"6:second".to_vec(), 2, Vec::new()).await.unwrap();
        let result = handle.get_mutable(pubkey, Vec::new()).await.unwrap();
        assert_eq!(result, Some((b"6:second".to_vec(), 2)));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn put_mutable_with_salt_isolation() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let handle = DhtHandle::start(config).await.unwrap();

        let seed = [77u8; 32];
        let keypair = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pubkey = keypair.verifying_key().to_bytes();

        // Put with salt "a"
        handle.put_mutable(seed, b"1:A".to_vec(), 1, b"a".to_vec()).await.unwrap();
        // Put with salt "b"
        handle.put_mutable(seed, b"1:B".to_vec(), 1, b"b".to_vec()).await.unwrap();

        // Each salt returns its own value
        let a = handle.get_mutable(pubkey, b"a".to_vec()).await.unwrap();
        assert_eq!(a, Some((b"1:A".to_vec(), 1)));
        let b = handle.get_mutable(pubkey, b"b".to_vec()).await.unwrap();
        assert_eq!(b, Some((b"1:B".to_vec(), 1)));

        handle.shutdown().await.unwrap();
    }
```

**Step 2: Run all tests**

Run: `cargo test --workspace`
Expected: All tests PASS (aim for ~10+ new tests total for BEP 44)

Run: `cargo clippy --workspace -- -D warnings`
Expected: Clean

---

## Summary

| Task | Description | New Files | Tests |
|------|------------|-----------|-------|
| 1 | BEP 44 types (ImmutableItem, MutableItem, signing) | `bep44.rs` | 14 |
| 2 | DhtStorage trait + InMemoryDhtStorage | `storage.rs` | 10 |
| 3 | KRPC Get/Put encoding + decoding | modify `krpc.rs` | 8 |
| 4 | DHT actor handles incoming get/put queries | modify `actor.rs` | 0 (covered by integration) |
| 5 | DhtHandle public API (put/get immutable/mutable) | modify `actor.rs` | 5 |
| 6 | Settings (dht_max_items, lifetime) + Alerts | modify `settings.rs`, `alert.rs` | 4 |
| 7 | Facade re-exports | modify `dht.rs` | 0 |
| 8 | End-to-end integration tests | modify `actor.rs` | 3 |
| **Total** | | **2 new, 6 modified** | **~44** |

**Version bump:** `0.41.0` -> `0.42.0` in workspace `Cargo.toml`

**Commit:** `feat: BEP 44 DHT arbitrary data storage (M38)`
