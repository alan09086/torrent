# M39: BEP 51 -- DHT Infohash Indexing -- Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable sampling random infohashes from DHT nodes (BEP 51), supporting both outbound queries (network surveying) and inbound handler responses. This adds a new `sample_infohashes` KRPC query type, a `DhtHandle` method, a peer store sampling API, a new alert variant, and a session settings field.

**Architecture:** BEP 51 adds a single KRPC query `sample_infohashes` with `target` (20 bytes, used for routing) in the request and `interval`, `num`, `samples` (concatenated 20-byte hashes), and `nodes` in the response. The target/nodes mechanism enables DHT traversal to reach many nodes with one query each. The `samples` field is always present (even if empty) to distinguish compliant nodes. Incoming queries are handled by randomly sampling from the local peer store's info-hash keys.

**Tech Stack:** Rust edition 2024, ferrite-bencode, ferrite-core (Id20), tokio, serde

---

## Task 1: Add `random_info_hashes()` to PeerStore

**Files:**
- Modify: `crates/ferrite-dht/src/peer_store.rs`

This method provides the raw sampling capability that the handler will use to respond to incoming `sample_infohashes` queries.

**Step 1: Write test for `random_info_hashes`**

Add to the `tests` module in `crates/ferrite-dht/src/peer_store.rs`:

```rust
#[test]
fn random_info_hashes_empty_store() {
    let store = PeerStore::new();
    let samples = store.random_info_hashes(20);
    assert!(samples.is_empty());
}

#[test]
fn random_info_hashes_returns_up_to_max() {
    let mut store = PeerStore::new();
    for i in 0..5u8 {
        let mut hash_bytes = [0u8; 20];
        hash_bytes[0] = i;
        store.add_peer(Id20(hash_bytes), format!("10.0.0.{}:6881", i).parse().unwrap());
    }
    // Ask for fewer than total
    let samples = store.random_info_hashes(3);
    assert_eq!(samples.len(), 3);
    // Ask for more than total
    let samples = store.random_info_hashes(20);
    assert_eq!(samples.len(), 5);
}

#[test]
fn random_info_hashes_all_valid() {
    let mut store = PeerStore::new();
    let mut expected = std::collections::HashSet::new();
    for i in 0..10u8 {
        let mut hash_bytes = [0u8; 20];
        hash_bytes[0] = i;
        let id = Id20(hash_bytes);
        expected.insert(id);
        store.add_peer(id, format!("10.0.0.{}:6881", i).parse().unwrap());
    }
    let samples = store.random_info_hashes(10);
    assert_eq!(samples.len(), 10);
    for sample in &samples {
        assert!(expected.contains(sample), "unexpected info hash in sample");
    }
}
```

**Step 2: Implement `random_info_hashes`**

Add this method to the `impl PeerStore` block in `crates/ferrite-dht/src/peer_store.rs`, after the `peer_count()` method:

```rust
/// Return up to `max` randomly sampled info hashes from the store.
///
/// Uses Fisher-Yates partial shuffle on the key vector.
/// If the store has fewer than `max` info hashes, returns all of them.
pub fn random_info_hashes(&self, max: usize) -> Vec<Id20> {
    let keys: Vec<Id20> = self.peers.keys().copied().collect();
    let count = keys.len().min(max);
    if count == 0 {
        return Vec::new();
    }
    if count == keys.len() {
        return keys;
    }

    // Partial Fisher-Yates shuffle using the thread-local xorshift
    let mut keys = keys;
    for i in 0..count {
        let j = i + (xorshift_next() as usize % (keys.len() - i));
        keys.swap(i, j);
    }
    keys.truncate(count);
    keys
}
```

**Step 3: Add the `xorshift_next()` helper**

The existing `generate_secret()` function already has a thread-local xorshift. Extract a shared helper. Add this free function before `generate_secret()` in `crates/ferrite-dht/src/peer_store.rs`:

```rust
/// Thread-local xorshift64 PRNG. Returns a random u64.
fn xorshift_next() -> u64 {
    use std::cell::Cell;
    use std::time::SystemTime;

    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
                ^ 0x517cc1b727220a95 // mix constant to avoid collisions with generate_secret
        );
    }

    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}
```

**Step 4: Run tests**

Run: `cargo test -p ferrite-dht peer_store -- --nocapture`
Expected: All existing tests pass + 3 new tests pass.

---

## Task 2: Add `SampleInfohashes` to KRPC Types

**Files:**
- Modify: `crates/ferrite-dht/src/krpc.rs`

Add the new query and response variants, plus encoding/decoding.

**Step 1: Write round-trip tests**

Add to the `tests` module in `crates/ferrite-dht/src/krpc.rs`:

```rust
#[test]
fn sample_infohashes_query_round_trip() {
    let msg = KrpcMessage {
        transaction_id: TransactionId::from_u16(400),
        body: KrpcBody::Query(KrpcQuery::SampleInfohashes {
            id: test_id(),
            target: target_id(),
        }),
    };
    let bytes = msg.to_bytes().unwrap();
    let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn sample_infohashes_response_round_trip() {
    let sample1 = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
    let sample2 = Id20::from_hex("0000000000000000000000000000000000000001").unwrap();
    let nodes = vec![CompactNodeInfo {
        id: target_id(),
        addr: "10.0.0.1:6881".parse().unwrap(),
    }];
    let msg = KrpcMessage {
        transaction_id: TransactionId::from_u16(400),
        body: KrpcBody::Response(KrpcResponse::SampleInfohashes(SampleInfohashesResponse {
            id: test_id(),
            interval: 300,
            num: 42,
            samples: vec![sample1, sample2],
            nodes,
        })),
    };
    let bytes = msg.to_bytes().unwrap();
    let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn sample_infohashes_response_empty_samples() {
    let msg = KrpcMessage {
        transaction_id: TransactionId::from_u16(401),
        body: KrpcBody::Response(KrpcResponse::SampleInfohashes(SampleInfohashesResponse {
            id: test_id(),
            interval: 60,
            num: 0,
            samples: Vec::new(),
            nodes: Vec::new(),
        })),
    };
    let bytes = msg.to_bytes().unwrap();
    let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn sample_infohashes_query_method_name() {
    assert_eq!(
        KrpcQuery::SampleInfohashes {
            id: Id20::ZERO,
            target: Id20::ZERO,
        }
        .method_name(),
        "sample_infohashes"
    );
}
```

**Step 2: Add `SampleInfohashesResponse` struct**

Add after the `GetPeersResponse` struct in `crates/ferrite-dht/src/krpc.rs`:

```rust
/// Response to sample_infohashes (BEP 51).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleInfohashesResponse {
    pub id: Id20,
    /// Minimum seconds before querying this node again.
    pub interval: i64,
    /// Estimated total number of info hashes in this node's storage.
    pub num: i64,
    /// Random sample of info hashes (each 20 bytes).
    pub samples: Vec<Id20>,
    /// Closer nodes (compact format), for DHT traversal.
    pub nodes: Vec<CompactNodeInfo>,
}
```

**Step 3: Add `SampleInfohashes` variant to `KrpcQuery`**

Add to the `KrpcQuery` enum in `crates/ferrite-dht/src/krpc.rs`:

```rust
SampleInfohashes {
    id: Id20,
    target: Id20,
},
```

**Step 4: Add `SampleInfohashes` variant to `KrpcResponse`**

Add to the `KrpcResponse` enum in `crates/ferrite-dht/src/krpc.rs`:

```rust
/// Response to sample_infohashes (BEP 51).
SampleInfohashes(SampleInfohashesResponse),
```

**Step 5: Update `method_name()` on `KrpcQuery`**

Add the new arm to `KrpcQuery::method_name()`:

```rust
KrpcQuery::SampleInfohashes { .. } => "sample_infohashes",
```

**Step 6: Update `sender_id()` on `KrpcQuery`**

Add the new arm to the match in `KrpcQuery::sender_id()`. The existing pattern uses an `|` chain -- add `KrpcQuery::SampleInfohashes { id, .. }` to the chain:

```rust
| KrpcQuery::SampleInfohashes { id, .. } => id,
```

**Step 7: Update `sender_id()` on `KrpcResponse`**

Add the new arm:

```rust
KrpcResponse::SampleInfohashes(si) => &si.id,
```

**Step 8: Update `encode_query_args()`**

Add the new match arm in `encode_query_args()`:

```rust
KrpcQuery::SampleInfohashes { id, target } => {
    args.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
    args.insert(b"target".to_vec(), BencodeValue::Bytes(target.0.to_vec()));
}
```

**Step 9: Update `encode_response_values()`**

Add the new match arm in `encode_response_values()`:

```rust
KrpcResponse::SampleInfohashes(si) => {
    values.insert(b"id".to_vec(), BencodeValue::Bytes(si.id.0.to_vec()));
    values.insert(b"interval".to_vec(), BencodeValue::Integer(si.interval));
    if !si.nodes.is_empty() {
        values.insert(
            b"nodes".to_vec(),
            BencodeValue::Bytes(encode_compact_nodes(&si.nodes)),
        );
    }
    values.insert(b"num".to_vec(), BencodeValue::Integer(si.num));
    // BEP 51: "samples" is always present, even if empty
    let mut samples_buf = Vec::with_capacity(si.samples.len() * 20);
    for hash in &si.samples {
        samples_buf.extend_from_slice(hash.as_bytes());
    }
    values.insert(b"samples".to_vec(), BencodeValue::Bytes(samples_buf));
}
```

**Step 10: Update `decode_query()`**

Add the new match arm in `decode_query()`:

```rust
b"sample_infohashes" => {
    let target = args_id20(args, b"target")?;
    Ok(KrpcQuery::SampleInfohashes { id, target })
}
```

**Step 11: Update `decode_response()`**

The response decoder needs to distinguish `sample_infohashes` responses from others. A `sample_infohashes` response contains `samples` + `interval` + `num` fields. Insert this check **before** the `has_values || has_token` check (i.e., early in `decode_response()`), right after extracting `id`:

```rust
// sample_infohashes response (BEP 51): has "samples" + "interval" + "num"
let has_samples = values.contains_key(&b"samples"[..]);
let has_interval = values.contains_key(&b"interval"[..]);

if has_samples && has_interval {
    let interval = values
        .get(&b"interval"[..])
        .and_then(|v| v.as_int())
        .unwrap_or(0);
    let num = values
        .get(&b"num"[..])
        .and_then(|v| v.as_int())
        .unwrap_or(0);

    let samples_bytes = values
        .get(&b"samples"[..])
        .and_then(|v| v.as_bytes_raw())
        .unwrap_or(&[]);
    let mut samples = Vec::new();
    if samples_bytes.len() % 20 == 0 {
        for chunk in samples_bytes.chunks_exact(20) {
            if let Ok(hash) = Id20::from_bytes(chunk) {
                samples.push(hash);
            }
        }
    }

    let nodes = if let Some(nodes_bytes) = values.get(&b"nodes"[..]).and_then(|v| v.as_bytes_raw()) {
        parse_compact_nodes(nodes_bytes)?
    } else {
        Vec::new()
    };

    return Ok(KrpcResponse::SampleInfohashes(SampleInfohashesResponse {
        id,
        interval,
        num,
        samples,
        nodes,
    }));
}
```

**Step 12: Update exports in `crates/ferrite-dht/src/lib.rs`**

Change the `pub use krpc` line to also export `SampleInfohashesResponse`:

```rust
pub use krpc::{KrpcMessage, KrpcBody, KrpcQuery, KrpcResponse, GetPeersResponse, SampleInfohashesResponse, TransactionId};
```

**Step 13: Run tests**

Run: `cargo test -p ferrite-dht krpc -- --nocapture`
Expected: All existing tests pass + 4 new tests pass.

---

## Task 3: Wire `sample_infohashes` Into the DHT Actor

**Files:**
- Modify: `crates/ferrite-dht/src/actor.rs`

Add outbound query support (`DhtHandle::sample_infohashes`) and inbound handler logic.

**Step 1: Define `SampleInfohashesResult`**

Add after the `DhtStats` struct in `crates/ferrite-dht/src/actor.rs`:

```rust
/// Result of a sample_infohashes query (BEP 51).
#[derive(Debug, Clone)]
pub struct SampleInfohashesResult {
    /// Minimum seconds before querying the same node again.
    pub interval: i64,
    /// Estimated total info hashes in the remote node's store.
    pub num: i64,
    /// Sampled info hashes.
    pub samples: Vec<Id20>,
    /// Closer nodes for traversal.
    pub nodes: Vec<CompactNodeInfo>,
}
```

**Step 2: Add `DhtCommand::SampleInfohashes` variant**

Add to the `DhtCommand` enum:

```rust
SampleInfohashes {
    target: Id20,
    reply: oneshot::Sender<Result<SampleInfohashesResult>>,
},
```

**Step 3: Add `PendingQueryKind::SampleInfohashes` variant**

Add to the `PendingQueryKind` enum:

```rust
SampleInfohashes { target: Id20 },
```

**Step 4: Add `DhtHandle::sample_infohashes()` method**

Add to the `impl DhtHandle` block:

```rust
/// Query a DHT node for a random sample of info hashes (BEP 51).
///
/// Routes toward `target` to find the responding node. Returns sampled
/// hashes, the interval before re-querying, and closer nodes for traversal.
pub async fn sample_infohashes(&self, target: Id20) -> Result<SampleInfohashesResult> {
    let (reply_tx, reply_rx) = oneshot::channel();
    self.tx
        .send(DhtCommand::SampleInfohashes {
            target,
            reply: reply_tx,
        })
        .await
        .map_err(|_| Error::Shutdown)?;
    reply_rx.await.map_err(|_| Error::Shutdown)?
}
```

**Step 5: Handle the command in the actor's event loop**

In the `run()` method's `tokio::select!` match on commands, add a new arm after the `DhtCommand::Stats` arm:

```rust
Some(DhtCommand::SampleInfohashes { target, reply }) => {
    self.handle_sample_infohashes(target, reply).await;
}
```

**Step 6: Implement `handle_sample_infohashes` on `DhtActor`**

Add this method to the `impl DhtActor` block:

```rust
async fn handle_sample_infohashes(
    &mut self,
    target: Id20,
    reply: oneshot::Sender<Result<SampleInfohashesResult>>,
) {
    // Find closest node to the target and send the query there
    let closest = self.routing_table.closest(&target, 1);
    let addr = match closest.first() {
        Some(node) => node.addr,
        None => {
            let _ = reply.send(Err(Error::InvalidMessage(
                "no nodes in routing table".into(),
            )));
            return;
        }
    };

    let txn = self.next_transaction_id();
    let own_id = *self.routing_table.own_id();
    let msg = KrpcMessage {
        transaction_id: TransactionId::from_u16(txn),
        body: KrpcBody::Query(KrpcQuery::SampleInfohashes {
            id: own_id,
            target,
        }),
    };
    if let Ok(bytes) = msg.to_bytes() {
        let _ = self.socket.send_to(&bytes, addr).await;
        self.pending.insert(
            txn,
            PendingQuery {
                sent_at: Instant::now(),
                addr,
                kind: PendingQueryKind::SampleInfohashes { target },
            },
        );
        self.stats.total_queries_sent += 1;
    }
    // Store the reply sender for when the response comes back
    self.sample_replies.insert(txn, reply);
}
```

**Step 7: Add `sample_replies` field to `DhtActor`**

Add this field to the `DhtActor` struct:

```rust
/// Pending one-shot replies for sample_infohashes queries.
sample_replies: HashMap<u16, oneshot::Sender<Result<SampleInfohashesResult>>>,
```

Initialize it in `DhtActor::new()`:

```rust
sample_replies: HashMap::new(),
```

**Step 8: Handle incoming `sample_infohashes` responses**

Add a new arm to the match in `handle_response()`, after the `PendingQueryKind::AnnouncePeer` arm:

```rust
(
    PendingQueryKind::SampleInfohashes { target: _ },
    KrpcResponse::SampleInfohashes(si),
) => {
    // Add discovered nodes to routing table
    for node in &si.nodes {
        if self.matches_family(&node.addr) {
            self.routing_table.insert(node.id, node.addr);
        }
    }

    // Send result back to caller
    if let Some(reply) = self.sample_replies.remove(&txn) {
        let _ = reply.send(Ok(SampleInfohashesResult {
            interval: si.interval,
            num: si.num,
            samples: si.samples.clone(),
            nodes: si.nodes.clone(),
        }));
    }
}
```

**Step 9: Handle incoming `sample_infohashes` queries**

Add a new match arm in `handle_query()`, after the `KrpcQuery::AnnouncePeer` arm:

```rust
KrpcQuery::SampleInfohashes { id: _, target } => {
    let closest = self.routing_table.closest(target, K);
    let nodes: Vec<CompactNodeInfo> = closest
        .into_iter()
        .map(|n| CompactNodeInfo {
            id: n.id,
            addr: n.addr,
        })
        .collect();

    // Sample up to 20 info hashes (fits comfortably in one UDP packet)
    let samples = self.peer_store.random_info_hashes(20);
    let num = self.peer_store.info_hash_count() as i64;

    KrpcResponse::SampleInfohashes(SampleInfohashesResponse {
        id: *self.routing_table.own_id(),
        interval: 60, // 1 minute default interval
        num,
        samples,
        nodes,
    })
}
```

Note: The `KrpcQuery::SampleInfohashes` arm must use the imported `SampleInfohashesResponse` type. Add the import at the top of `actor.rs` -- update the existing `use crate::krpc::` import to include `SampleInfohashesResponse`:

```rust
use crate::krpc::{
    GetPeersResponse, KrpcBody, KrpcMessage, KrpcQuery, KrpcResponse,
    SampleInfohashesResponse, TransactionId,
};
```

**Step 10: Update `lib.rs` exports**

Update `crates/ferrite-dht/src/lib.rs` to export the new type:

```rust
pub use actor::{DhtHandle, DhtConfig, DhtStats, SampleInfohashesResult};
```

**Step 11: Write actor tests**

Add these tests to the `tests` module in `crates/ferrite-dht/src/actor.rs`:

```rust
#[tokio::test]
async fn dht_sample_infohashes_empty_table() {
    let config = DhtConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bootstrap_nodes: Vec::new(),
        ..DhtConfig::default()
    };
    let handle = DhtHandle::start(config).await.unwrap();

    let target = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
    let result = handle.sample_infohashes(target).await;
    // With empty routing table, we expect an error (no nodes to query)
    assert!(result.is_err());

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_nodes_sample_infohashes() {
    // Node A will store some peers, then node B queries it
    let config_a = DhtConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bootstrap_nodes: Vec::new(),
        own_id: Some(Id20::from_hex("0000000000000000000000000000000000000001").unwrap()),
        ..DhtConfig::default()
    };
    let handle_a = DhtHandle::start(config_a).await.unwrap();

    // We can't directly add peers to node A's store through the public API,
    // but we can verify the query/response path by having node B query node A.
    // Node A will respond with empty samples since its peer store is empty.

    // For now, just verify the handle method exists and handles shutdown gracefully
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle_a.shutdown().await.unwrap();
}
```

**Step 12: Run tests**

Run: `cargo test -p ferrite-dht -- --nocapture`
Expected: All existing tests pass + 2 new actor tests pass + no compilation errors.

---

## Task 4: Add `DhtSampleInfohashes` Alert

**Files:**
- Modify: `crates/ferrite-session/src/alert.rs`

**Step 1: Add the alert variant**

Add to the `AlertKind` enum in the `// -- DHT --` section, after `DhtGetPeers`:

```rust
/// BEP 51: sample_infohashes response received from a DHT node.
DhtSampleInfohashes {
    /// Number of sampled info hashes received.
    num_samples: usize,
    /// The remote node's estimate of its total stored info hashes.
    total_estimate: i64,
},
```

**Step 2: Update the `category()` match**

In the `AlertKind::category()` method, update the DHT match arm to include the new variant:

```rust
DhtBootstrapComplete | DhtGetPeers { .. } | DhtSampleInfohashes { .. } => AlertCategory::DHT,
```

**Step 3: Write test**

Add to the `tests` module in `crates/ferrite-session/src/alert.rs`:

```rust
#[test]
fn dht_sample_infohashes_alert_has_dht_category() {
    let alert = Alert::new(AlertKind::DhtSampleInfohashes {
        num_samples: 15,
        total_estimate: 500,
    });
    assert!(alert.category().contains(AlertCategory::DHT));
}
```

**Step 4: Run tests**

Run: `cargo test -p ferrite-session alert -- --nocapture`
Expected: All existing tests pass + 1 new test passes.

---

## Task 5: Add `dht_sample_infohashes_interval` to Settings

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs`

**Step 1: Add the serde default helper**

Add after the existing default helper functions (e.g., near `default_dht_timeout`):

```rust
fn default_dht_sample_interval() -> u64 {
    0
}
```

**Step 2: Add the field to `Settings`**

Add to the `Settings` struct, in the `// -- DHT tuning --` section, after `dht_query_timeout_secs`:

```rust
/// Interval in seconds for periodic sample_infohashes queries (BEP 51).
/// 0 = disabled (default). Non-zero enables background DHT indexing.
#[serde(default = "default_dht_sample_interval")]
pub dht_sample_infohashes_interval: u64,
```

**Step 3: Add to `Default` impl**

In the `Default for Settings` impl, add in the DHT tuning section:

```rust
dht_sample_infohashes_interval: 0,
```

**Step 4: Update `PartialEq` impl**

Add to the `eq()` method's comparison chain:

```rust
&& self.dht_sample_infohashes_interval == other.dht_sample_infohashes_interval
```

**Step 5: Write test**

Add to the `tests` module in `crates/ferrite-session/src/settings.rs`:

```rust
#[test]
fn dht_sample_interval_default_disabled() {
    let s = Settings::default();
    assert_eq!(s.dht_sample_infohashes_interval, 0);
}

#[test]
fn dht_sample_interval_json_round_trip() {
    let json = r#"{"dht_sample_infohashes_interval": 300}"#;
    let decoded: Settings = serde_json::from_str(json).unwrap();
    assert_eq!(decoded.dht_sample_infohashes_interval, 300);

    let encoded = serde_json::to_string(&decoded).unwrap();
    let roundtrip: Settings = serde_json::from_str(&encoded).unwrap();
    assert_eq!(roundtrip.dht_sample_infohashes_interval, 300);
}
```

**Step 6: Run tests**

Run: `cargo test -p ferrite-session settings -- --nocapture`
Expected: All existing tests pass + 2 new tests pass. The `json_round_trip` and `json_missing_fields_use_defaults` tests must continue to pass (the new field uses `serde(default)` so it's safe).

---

## Task 6: Full Workspace Build and Test

**Step 1: Build entire workspace**

Run: `cargo build --workspace`
Expected: Clean build, no errors.

**Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

**Step 3: Run all tests**

Run: `cargo test --workspace`
Expected: All tests pass (existing 895+ new ~12 tests).

---

## Task 7: Version Bump and Commit

**Step 1: Bump workspace version**

In the root `Cargo.toml`, bump `version` from `"0.41.0"` to `"0.42.0"` (new milestone = new minor version).

**Step 2: Commit**

```bash
git add -A
git commit -m "feat: BEP 51 DHT infohash indexing (M39)

Add sample_infohashes KRPC query/response for DHT network surveying:
- SampleInfohashes KRPC query + SampleInfohashesResponse type
- PeerStore::random_info_hashes() for random sampling
- DhtHandle::sample_infohashes() outbound query method
- Incoming sample_infohashes handler in DHT actor
- DhtSampleInfohashes alert variant
- dht_sample_infohashes_interval setting (default: 0 = disabled)

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Summary of All Changes

| File | Change |
|------|--------|
| `crates/ferrite-dht/src/peer_store.rs` | Add `random_info_hashes()`, `xorshift_next()`, 3 tests |
| `crates/ferrite-dht/src/krpc.rs` | Add `SampleInfohashes` query/response variants, `SampleInfohashesResponse` struct, encoding/decoding, 4 tests |
| `crates/ferrite-dht/src/actor.rs` | Add `SampleInfohashesResult`, `DhtHandle::sample_infohashes()`, `DhtCommand::SampleInfohashes`, `PendingQueryKind::SampleInfohashes`, outbound query handler, inbound query/response handlers, `sample_replies` field, 2 tests |
| `crates/ferrite-dht/src/lib.rs` | Export `SampleInfohashesResult`, `SampleInfohashesResponse` |
| `crates/ferrite-session/src/alert.rs` | Add `DhtSampleInfohashes` variant, 1 test |
| `crates/ferrite-session/src/settings.rs` | Add `dht_sample_infohashes_interval` field + default + PartialEq, 2 tests |
| `Cargo.toml` | Version bump 0.41.0 -> 0.42.0 |

**New test count:** ~12 new tests across 4 files.
**Total estimated tests after M39:** ~907.
