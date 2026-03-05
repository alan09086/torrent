# M10a: Crate Scaffold + Core Re-exports — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create the `ferrite` facade crate with workspace wiring and re-export the foundation types from `ferrite-bencode` and `ferrite-core`.

**Architecture:** New `crates/ferrite/` crate with `pub use` re-exports organized into `bencode` and `core` sub-modules. No new functionality — pure re-export layer. Tests verify types are accessible through the facade namespace.

**Tech Stack:** Rust 2024 edition, workspace inheritance, `ferrite-bencode`, `ferrite-core`

**Baseline:** 342 tests passing, zero clippy warnings (`cargo test --workspace && cargo clippy --workspace -- -D warnings`)

---

## Task 1: Create Crate Scaffold

Create the `ferrite` crate directory, Cargo.toml, and empty lib.rs. Wire it into the workspace.

**Files:**
- Create: `crates/ferrite/Cargo.toml`
- Create: `crates/ferrite/src/lib.rs`

**Step 1: Create the crate directory**

```bash
mkdir -p /mnt/TempNVME/projects/ferrite/crates/ferrite/src
```

**Step 2: Write Cargo.toml**

Create `crates/ferrite/Cargo.toml`:

```toml
[package]
name = "ferrite"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "A Rust BitTorrent library — ergonomic facade for the ferrite crate family"

[dependencies]
ferrite-bencode = { path = "../ferrite-bencode" }
ferrite-core = { path = "../ferrite-core" }

[dev-dependencies]
pretty_assertions = { workspace = true }
serde = { workspace = true, features = ["derive"] }
```

**Step 3: Write initial lib.rs**

Create `crates/ferrite/src/lib.rs`:

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
```

**Step 4: Verify it compiles**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo check -p ferrite`
Expected: compiles with no errors

**Step 5: Verify full workspace still passes**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace 2>&1 | grep "^test result"`
Expected: 342 tests passing (same as baseline), ferrite crate appears with 0 tests

**Step 6: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite/Cargo.toml crates/ferrite/src/lib.rs
git commit -m "feat(ferrite): scaffold facade crate with bencode + core deps"
```

---

## Task 2: Bencode Re-exports Module

Re-export all public types from `ferrite-bencode` through `ferrite::bencode`.

**Files:**
- Create: `crates/ferrite/src/bencode.rs`
- Modify: `crates/ferrite/src/lib.rs`

**Step 1: Write the failing test**

Add to `crates/ferrite/src/lib.rs` at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bencode_round_trip_through_facade() {
        use serde::{Serialize, Deserialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Demo {
            name: String,
            value: i64,
        }

        let original = Demo { name: "ferrite".into(), value: 42 };
        let encoded = bencode::to_bytes(&original).unwrap();
        let decoded: Demo = bencode::from_bytes(&encoded).unwrap();
        assert_eq!(original, decoded);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite -- bencode_round_trip_through_facade 2>&1`
Expected: FAIL — `bencode` module doesn't exist yet

**Step 3: Create bencode.rs**

Create `crates/ferrite/src/bencode.rs`:

```rust
//! Serde-based bencode codec for BitTorrent.
//!
//! Re-exports from [`ferrite_bencode`].

pub use ferrite_bencode::{
    // Codec functions
    to_bytes,
    from_bytes,
    // Span utility
    find_dict_key_span,
    // Dynamic value type
    BencodeValue,
    // Serde impls (for advanced use)
    Serializer,
    Deserializer,
    // Error types
    Error,
    Result,
};
```

**Step 4: Add module declaration to lib.rs**

Add after the doc comment in `crates/ferrite/src/lib.rs`:

```rust
pub mod bencode;
```

**Step 5: Run test to verify it passes**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite -- bencode_round_trip_through_facade -v`
Expected: PASS

**Step 6: Run clippy**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo clippy -p ferrite -- -D warnings`
Expected: no warnings

**Step 7: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite/src/bencode.rs crates/ferrite/src/lib.rs
git commit -m "feat(ferrite): add bencode re-exports module"
```

---

## Task 3: Core Re-exports Module

Re-export all public types from `ferrite-core` through `ferrite::core`.

**Files:**
- Create: `crates/ferrite/src/core.rs`
- Modify: `crates/ferrite/src/lib.rs`

**Step 1: Write the failing test — torrent construction**

Add to the `tests` module in `crates/ferrite/src/lib.rs`:

```rust
    #[test]
    fn core_types_accessible_through_facade() {
        // Verify hash types
        let data = b"hello";
        let hash = core::sha1(data);
        assert_eq!(hash.to_hex().len(), 40);

        // Verify Id20 hex round-trip
        let hex = hash.to_hex();
        let parsed = core::Id20::from_hex(&hex).unwrap();
        assert_eq!(hash, parsed);

        // Verify Lengths arithmetic
        let lengths = core::Lengths::new(1048576, 262144, core::DEFAULT_CHUNK_SIZE);
        assert_eq!(lengths.num_pieces(), 4);

        // Verify PeerId generation
        let peer_id = core::PeerId::generate();
        assert_eq!(peer_id.0 .0.len(), 20);
    }
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite -- core_types_accessible_through_facade 2>&1`
Expected: FAIL — `core` module doesn't exist yet

**Step 3: Create core.rs**

Create `crates/ferrite/src/core.rs`:

```rust
//! Core BitTorrent types: hashes, metainfo, magnets, piece arithmetic.
//!
//! Re-exports from [`ferrite_core`].

pub use ferrite_core::{
    // Hash types
    Id20,
    Id32,
    // Peer identity
    PeerId,
    // Magnet links (BEP 9)
    Magnet,
    // Torrent metainfo (BEP 3)
    TorrentMetaV1,
    torrent_from_bytes,
    // Piece/chunk arithmetic
    Lengths,
    DEFAULT_CHUNK_SIZE,
    // SHA1 utility
    sha1,
    // Error types
    Error,
    Result,
};

// Re-export info dict sub-types (needed to access TorrentMetaV1 fields)
pub use ferrite_core::{FileInfo, InfoDict, FileEntry};
```

Note: `InfoDict`, `FileEntry`, and `FileInfo` must be re-exported because they're used as field types on `TorrentMetaV1` — users need them to access `.info` and call `.files()`.

**Step 4: Add module declaration to lib.rs**

Add after the bencode module declaration in `crates/ferrite/src/lib.rs`:

```rust
// Note: "core" shadows std::core, but since this is a library crate and users
// access it as `ferrite::core`, there's no ambiguity. Internal code that needs
// std::core can use `::core::` path prefix.
pub mod core;
```

**Step 5: Run test to verify it passes**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite -- core_types_accessible_through_facade -v`
Expected: PASS

**Step 6: Run clippy**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo clippy -p ferrite -- -D warnings`
Expected: no warnings

**Step 7: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite/src/core.rs crates/ferrite/src/lib.rs
git commit -m "feat(ferrite): add core re-exports module"
```

---

## Task 4: Magnet Link Test Through Facade

Add a test that exercises the magnet parsing path through the facade, confirming the full `ferrite::core::Magnet` API works.

**Files:**
- Modify: `crates/ferrite/src/lib.rs` (tests module)

**Step 1: Write the test**

Add to the `tests` module in `crates/ferrite/src/lib.rs`:

```rust
    #[test]
    fn magnet_parse_through_facade() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d\
                   &dn=test%20file\
                   &tr=http%3A%2F%2Ftracker.example.com%2Fannounce";
        let magnet = core::Magnet::parse(uri).unwrap();

        assert_eq!(magnet.display_name.as_deref(), Some("test file"));
        assert_eq!(magnet.trackers.len(), 1);
        assert_eq!(magnet.info_hash.to_hex(), "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");

        // Round-trip back to URI
        let rebuilt = magnet.to_uri();
        let reparsed = core::Magnet::parse(&rebuilt).unwrap();
        assert_eq!(magnet.info_hash, reparsed.info_hash);
        assert_eq!(magnet.display_name, reparsed.display_name);
    }
```

**Step 2: Run test to verify it passes**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite -- magnet_parse_through_facade -v`
Expected: PASS (this uses already-working re-exports from Task 3)

**Step 3: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite/src/lib.rs
git commit -m "test(ferrite): add magnet parse round-trip through facade"
```

---

## Task 5: Full Workspace Verification + Docs

Run all tests, clippy, and verify the facade crate is properly documented.

**Files:**
- Modify: `crates/ferrite/src/lib.rs` (doc updates only)

**Step 1: Run full workspace tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace 2>&1 | grep "^test result"`
Expected: 345 tests passing (342 baseline + 3 new facade tests), all OK

**Step 2: Run clippy on entire workspace**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo clippy --workspace -- -D warnings`
Expected: zero warnings

**Step 3: Check `pub mod core` doesn't shadow std::core**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo doc -p ferrite --no-deps 2>&1`
Expected: docs build without errors. Verify `ferrite::bencode` and `ferrite::core` appear as modules.

**Step 4: Handle core shadowing if needed**

If `pub mod core` causes issues with `std::core`, rename to `pub mod types` and update all tests. Based on the Rust 2024 edition behavior, a `pub mod core` in a library crate is fine — users access it as `ferrite::core` and the compiler resolves `::core` for language primitives. But verify with the doc build.

If the doc build succeeds, no changes needed.

**Step 5: Commit any doc fixes**

Only if Step 3/4 required changes:

```bash
cd /mnt/TempNVME/projects/ferrite
git add -A
git commit -m "fix(ferrite): resolve core module naming"
```

---

## Task 6: Update README, CHANGELOG, Push

Update project documentation to reflect M10a completion and push to both remotes.

**Files:**
- Modify: `/mnt/TempNVME/projects/ferrite/README.md`
- Modify: `/mnt/TempNVME/projects/ferrite/CHANGELOG.md`

**Step 1: Update README milestone table**

Find the M10a row and mark it as done. Update the test count to 345.

**Step 2: Update CHANGELOG**

Add a new entry at the top:

```markdown
## v0.7.0 — M10a: Crate Scaffold + Core Re-exports

### Added
- `ferrite` facade crate with workspace wiring
- `ferrite::bencode` — re-exports `to_bytes`, `from_bytes`, `BencodeValue`, `find_dict_key_span`, `Serializer`, `Deserializer`, `Error`
- `ferrite::core` — re-exports `Id20`, `Id32`, `PeerId`, `Magnet`, `TorrentMetaV1`, `InfoDict`, `FileEntry`, `FileInfo`, `torrent_from_bytes`, `Lengths`, `DEFAULT_CHUNK_SIZE`, `sha1`, `Error`
- 3 facade tests: bencode round-trip, core types, magnet parse
```

**Step 3: Bump workspace version**

In root `Cargo.toml`, change `version = "0.6.0"` to `version = "0.7.0"`.

**Step 4: Verify version bump compiles**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo check --workspace`
Expected: compiles clean

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add README.md CHANGELOG.md Cargo.toml
git commit -m "docs: update README/changelog for v0.7.0 (M10a)"
```

**Step 6: Push to both remotes**

```bash
cd /mnt/TempNVME/projects/ferrite
git push origin main && git push github main
```

---

## Verification

After all tasks complete:

```bash
cd /mnt/TempNVME/projects/ferrite
cargo test --workspace                        # 345 tests pass
cargo clippy --workspace -- -D warnings       # Zero warnings
cargo doc -p ferrite --no-deps                # Docs build clean
```

## Key Files Summary

| File | Action |
|------|--------|
| `crates/ferrite/Cargo.toml` | Create — crate manifest with bencode + core deps |
| `crates/ferrite/src/lib.rs` | Create — crate root with module declarations + 3 tests |
| `crates/ferrite/src/bencode.rs` | Create — `pub use ferrite_bencode::*` re-exports |
| `crates/ferrite/src/core.rs` | Create — `pub use ferrite_core::*` re-exports |
| `README.md` | Modify — update M10a status + test count |
| `CHANGELOG.md` | Modify — add v0.7.0 entry |
| `Cargo.toml` (root) | Modify — bump version to 0.7.0 |
