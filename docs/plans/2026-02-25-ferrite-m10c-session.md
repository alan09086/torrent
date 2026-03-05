# M10c: DHT + Storage Re-exports — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `ferrite::dht` and `ferrite::storage` modules to the facade crate, re-exporting all public types from `ferrite-dht` and `ferrite-storage`.

**Architecture:** Same `pub use` re-export pattern as M10a/M10b. Two new modules, two new dependencies, two tests, then docs/version bump.

**Tech Stack:** Rust 2024, workspace resolver 2, ferrite-dht, ferrite-storage

---

### Task 1: Add dht and storage dependencies + module declarations

**Files:**
- Modify: `crates/ferrite/Cargo.toml`
- Modify: `crates/ferrite/src/lib.rs`

**Step 1: Add dependencies to Cargo.toml**

Add after the existing `ferrite-tracker` line:

```toml
ferrite-dht = { path = "../ferrite-dht" }
ferrite-storage = { path = "../ferrite-storage" }
```

**Step 2: Add module declarations and docs to lib.rs**

Add two new lines to the module doc comment list:
```rust
//! - [`dht`] — Kademlia DHT peer discovery
//! - [`storage`] — Piece storage, verification, disk I/O
```

Add two new module declarations after `pub mod tracker;`:
```rust
pub mod dht;
pub mod storage;
```

**Step 3: Commit**

```bash
git add crates/ferrite/Cargo.toml crates/ferrite/src/lib.rs
git commit -m "feat(ferrite): add dht + storage deps and module declarations"
```

---

### Task 2: Create dht.rs re-exports

**Files:**
- Create: `crates/ferrite/src/dht.rs`

```rust
//! Kademlia DHT for BitTorrent peer discovery (BEP 5).
//!
//! Re-exports from [`ferrite_dht`].

pub use ferrite_dht::{
    // Actor handle and configuration
    DhtHandle,
    DhtConfig,
    DhtStats,
    // Compact node encoding (26-byte format)
    CompactNodeInfo,
    // Routing table
    RoutingTable,
    // KRPC protocol messages
    KrpcMessage,
    KrpcBody,
    KrpcQuery,
    KrpcResponse,
    GetPeersResponse,
    TransactionId,
    // Error types
    Error,
    Result,
};

// Re-export items from public sub-modules
pub use ferrite_dht::compact::{parse_compact_nodes, encode_compact_nodes};
pub use ferrite_dht::peer_store::PeerStore;
pub use ferrite_dht::routing_table::K;
```

---

### Task 3: Create storage.rs re-exports

**Files:**
- Create: `crates/ferrite/src/storage.rs`

```rust
//! Piece storage, verification, and disk I/O for BitTorrent.
//!
//! Re-exports from [`ferrite_storage`].

pub use ferrite_storage::{
    // Storage trait
    TorrentStorage,
    // Bit-vector for piece completion
    Bitfield,
    // Per-piece chunk tracking
    ChunkTracker,
    // Piece-to-file mapping
    FileMap,
    FileSegment,
    // Storage backends
    MemoryStorage,
    FilesystemStorage,
    // Error types
    Error,
    Result,
};
```

**After creating both files:**

Run: `cargo check -p ferrite`
Expected: PASS

**Commit:**

```bash
git add crates/ferrite/src/dht.rs crates/ferrite/src/storage.rs
git commit -m "feat(ferrite): add dht + storage re-export modules"
```

---

### Task 4: Add facade tests

**Files:**
- Modify: `crates/ferrite/src/lib.rs` (add tests to existing `#[cfg(test)] mod tests`)

Add these two tests to the existing `mod tests` block:

```rust
    #[test]
    fn dht_compact_node_round_trip_through_facade() {
        use std::net::{Ipv4Addr, SocketAddrV4};

        let node = dht::CompactNodeInfo {
            id: core::Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            addr: SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 6881),
        };

        let encoded = dht::encode_compact_nodes(&[node.clone()]);
        assert_eq!(encoded.len(), 26); // 20-byte ID + 4-byte IP + 2-byte port

        let decoded = dht::parse_compact_nodes(&encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, node.id);
        assert_eq!(decoded[0].addr, node.addr);
    }

    #[test]
    fn storage_bitfield_through_facade() {
        let mut bf = storage::Bitfield::new(16);
        assert_eq!(bf.len(), 16);
        assert!(!bf.get(0));
        assert_eq!(bf.count_ones(), 0);

        bf.set(0);
        bf.set(5);
        bf.set(15);
        assert!(bf.get(0));
        assert!(bf.get(5));
        assert!(bf.get(15));
        assert!(!bf.get(1));
        assert_eq!(bf.count_ones(), 3);
    }
```

**Run tests:**

Run: `cargo test -p ferrite`
Expected: 7 tests PASS (5 existing + 2 new)

Run: `cargo test --workspace`
Expected: All tests pass (349 total)

**Commit:**

```bash
git add crates/ferrite/src/lib.rs
git commit -m "test(ferrite): add dht compact node + storage bitfield facade tests"
```

---

### Task 5: Workspace verification

Run: `cargo clippy --workspace -- -D warnings`
Expected: Zero warnings

Run: `cargo doc -p ferrite --no-deps 2>&1 | tail -5`
Expected: Clean build

---

### Task 6: Docs, version bump, push

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `README.md`
- Modify: `Cargo.toml` (root, workspace version)

**Step 1: Update CHANGELOG.md**

Add after `## [Unreleased]`:

```markdown
## 0.9.0 — 2026-02-25

DHT and storage re-exports in facade crate. Eight crates, 349 tests.

### M10c: ferrite (dht + storage re-exports)

### Added
- `ferrite::dht` — re-exports `DhtHandle`, `DhtConfig`, `DhtStats`, `CompactNodeInfo`, `RoutingTable`, `KrpcMessage`, `KrpcBody`, `KrpcQuery`, `KrpcResponse`, `GetPeersResponse`, `TransactionId`, `parse_compact_nodes`, `encode_compact_nodes`, `PeerStore`, `K`, `Error`
- `ferrite::storage` — re-exports `TorrentStorage`, `Bitfield`, `ChunkTracker`, `FileMap`, `FileSegment`, `MemoryStorage`, `FilesystemStorage`, `Error`
- 2 facade tests: DHT compact node round-trip, storage bitfield operations
```

**Step 2: Update README.md**

- Update `ferrite` crate row description to: `Public facade: re-exports bencode, core, wire, tracker, dht, storage APIs` and test count to `7`
- Update total from `347` to `349`
- Mark M10c as `Done`

**Step 3: Bump workspace version to 0.9.0**

**Step 4: Commit and push**

```bash
git add CHANGELOG.md README.md Cargo.toml
git commit -m "release: v0.9.0 — M10c dht + storage facade re-exports"
git push origin main && git push github main
```
