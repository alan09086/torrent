# M10b: Wire + Tracker Re-exports — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `ferrite::wire` and `ferrite::tracker` modules to the facade crate, re-exporting all public types from `ferrite-wire` and `ferrite-tracker`.

**Architecture:** Same `pub use` re-export pattern as M10a. Two new modules in `crates/ferrite/src/`, two new dependencies in `Cargo.toml`, two tests, then docs/version bump.

**Tech Stack:** Rust 2024, workspace resolver 2, ferrite-wire, ferrite-tracker

---

### Task 1: Add wire and tracker dependencies + module declarations

**Files:**
- Modify: `crates/ferrite/Cargo.toml`
- Modify: `crates/ferrite/src/lib.rs`

**Step 1: Add dependencies to Cargo.toml**

Add `ferrite-wire` and `ferrite-tracker` to `[dependencies]`:

```toml
[dependencies]
ferrite-bencode = { path = "../ferrite-bencode" }
ferrite-core = { path = "../ferrite-core" }
ferrite-wire = { path = "../ferrite-wire" }
ferrite-tracker = { path = "../ferrite-tracker" }
```

**Step 2: Add module declarations and docs to lib.rs**

Update the crate-level doc comment and add `pub mod wire;` and `pub mod tracker;` after the existing `pub mod core;`:

```rust
//! A Rust BitTorrent library.
//!
//! `ferrite` is the public facade for the ferrite crate family. It re-exports
//! types from internal crates through a clean, ergonomic API.
//!
//! # Modules
//!
//! - [`bencode`] — Serde-based bencode serialization
//! - [`core`] — Hashes, metainfo, magnets, piece arithmetic
//! - [`wire`] — Peer wire protocol, handshake, extensions
//! - [`tracker`] — HTTP + UDP tracker announce

pub mod bencode;

// Note: "core" shadows std::core, but since this is a library crate and users
// access it as `ferrite::core`, there's no ambiguity. Internal code that needs
// std::core can use `::core::` path prefix.
pub mod core;

pub mod wire;
pub mod tracker;
```

**Step 3: Verify it compiles**

Run: `cargo check -p ferrite`
Expected: Fails because `wire.rs` and `tracker.rs` don't exist yet — that's fine, move to Task 2.

**Step 4: Commit**

```bash
git add crates/ferrite/Cargo.toml crates/ferrite/src/lib.rs
git commit -m "feat(ferrite): add wire + tracker deps and module declarations"
```

---

### Task 2: Create wire.rs re-exports

**Files:**
- Create: `crates/ferrite/src/wire.rs`

**Step 1: Create the wire re-export module**

Create `crates/ferrite/src/wire.rs`:

```rust
//! BitTorrent peer wire protocol (BEP 3, 10, 9, 11).
//!
//! Re-exports from [`ferrite_wire`].

pub use ferrite_wire::{
    // Handshake
    Handshake,
    // Wire messages (BEP 3 + BEP 6)
    Message,
    // Tokio codec for framed I/O
    MessageCodec,
    // Extension protocol (BEP 10)
    ExtHandshake,
    ExtMessage,
    // Metadata exchange (BEP 9)
    MetadataMessage,
    MetadataMessageType,
    // BEP 6 Fast Extension
    allowed_fast_set,
    // Error types
    Error,
    Result,
};
```

**Step 2: Verify it compiles**

Run: `cargo check -p ferrite`
Expected: Fails because `tracker.rs` doesn't exist yet. That's fine, move to Task 3.

---

### Task 3: Create tracker.rs re-exports

**Files:**
- Create: `crates/ferrite/src/tracker.rs`

**Step 1: Create the tracker re-export module**

Create `crates/ferrite/src/tracker.rs`:

```rust
//! BitTorrent tracker client (BEP 3, 15, 48).
//!
//! Re-exports from [`ferrite_tracker`].

pub use ferrite_tracker::{
    // HTTP tracker
    HttpTracker,
    HttpAnnounceResponse,
    // UDP tracker
    UdpTracker,
    UdpAnnounceResponse,
    // Common request/response types
    AnnounceRequest,
    AnnounceResponse,
    AnnounceEvent,
    // Compact peer parsing
    parse_compact_peers,
    // Error types
    Error,
    Result,
};
```

**Step 2: Verify the full crate compiles**

Run: `cargo check -p ferrite`
Expected: PASS — all modules now exist.

**Step 3: Commit wire.rs and tracker.rs together**

```bash
git add crates/ferrite/src/wire.rs crates/ferrite/src/tracker.rs
git commit -m "feat(ferrite): add wire + tracker re-export modules"
```

---

### Task 4: Add facade tests

**Files:**
- Modify: `crates/ferrite/src/lib.rs` (add tests to existing `#[cfg(test)] mod tests`)

**Step 1: Write the handshake round-trip test**

Add to the existing `mod tests` block in `lib.rs`:

```rust
    #[test]
    fn handshake_round_trip_through_facade() {
        let info_hash = core::Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let peer_id = core::Id20::from_hex("0102030405060708091011121314151617181920").unwrap();

        let hs = wire::Handshake::new(info_hash, peer_id);
        assert!(hs.supports_extensions());

        // Encode to bytes and decode back
        let bytes = hs.to_bytes();
        assert_eq!(bytes.len(), 68);

        let parsed = wire::Handshake::from_bytes(&bytes).unwrap();
        assert_eq!(hs, parsed);
        assert_eq!(parsed.info_hash, info_hash);
    }

    #[test]
    fn announce_request_construction_through_facade() {
        let info_hash = core::Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let peer_id = core::Id20::from_hex("0102030405060708091011121314151617181920").unwrap();

        let req = tracker::AnnounceRequest {
            info_hash,
            peer_id,
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1048576,
            event: tracker::AnnounceEvent::Started,
            num_want: Some(50),
            compact: true,
        };

        assert_eq!(req.port, 6881);
        assert_eq!(req.left, 1048576);
        assert!(req.compact);
        assert_eq!(req.event, tracker::AnnounceEvent::Started);
    }
```

**Step 2: Run the tests**

Run: `cargo test -p ferrite`
Expected: 5 tests PASS (3 existing + 2 new)

**Step 3: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass (347 expected: 345 + 2 new)

**Step 4: Commit**

```bash
git add crates/ferrite/src/lib.rs
git commit -m "test(ferrite): add wire handshake + tracker announce facade tests"
```

---

### Task 5: Workspace verification

**Step 1: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Zero warnings

**Step 2: Check docs build**

Run: `cargo doc -p ferrite --no-deps 2>&1 | tail -5`
Expected: Clean build, no warnings

---

### Task 6: Docs, version bump, push

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `README.md`
- Modify: `Cargo.toml` (root, workspace version)

**Step 1: Update CHANGELOG.md**

Add a new version entry after `## [Unreleased]`:

```markdown
## 0.8.0 — 2026-02-25

Wire and tracker re-exports in facade crate. Eight crates, 347 tests.

### M10b: ferrite (wire + tracker re-exports)

### Added
- `ferrite::wire` — re-exports `Handshake`, `Message`, `MessageCodec`, `ExtHandshake`, `ExtMessage`, `MetadataMessage`, `MetadataMessageType`, `allowed_fast_set`, `Error`
- `ferrite::tracker` — re-exports `HttpTracker`, `HttpAnnounceResponse`, `UdpTracker`, `UdpAnnounceResponse`, `AnnounceRequest`, `AnnounceResponse`, `AnnounceEvent`, `parse_compact_peers`, `Error`
- 2 facade tests: handshake round-trip, announce request construction
```

**Step 2: Update README.md**

- In the Crates table, update `ferrite` row description to: `Public facade: re-exports bencode, core, wire, tracker APIs` and test count to `5`
- Update total test count to 347
- Mark M10b as Done in the Roadmap table

**Step 3: Bump workspace version**

In root `Cargo.toml`, change `version = "0.7.0"` to `version = "0.8.0"`.

**Step 4: Commit and push**

```bash
git add CHANGELOG.md README.md Cargo.toml
git commit -m "release: v0.8.0 — M10b wire + tracker facade re-exports"
git push origin main && git push github main
```
