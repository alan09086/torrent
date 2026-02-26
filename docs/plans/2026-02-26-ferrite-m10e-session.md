# M10e: Prelude + Unified Error — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add unified `ferrite::Error`, `ferrite::prelude`, and integration tests to complete M10.

**Architecture:** New error.rs and prelude.rs modules, top-level re-exports, 3 tests.

**Tech Stack:** Rust 2024, thiserror, workspace resolver 2

---

### Task 1: Add thiserror dependency + module declarations

**Files:**
- Modify: `crates/ferrite/Cargo.toml`
- Modify: `crates/ferrite/src/lib.rs`

**Step 1: Add thiserror to Cargo.toml**

Add to `[dependencies]`:

```toml
thiserror = { workspace = true }
```

**Step 2: Add module declarations to lib.rs**

Add to the doc comment list:
```rust
//! - [`prelude`] — Convenience re-exports for `use ferrite::prelude::*`
```

Add after `pub mod client;`:
```rust
pub mod error;
pub mod prelude;
```

Also add top-level re-exports. Update the existing re-export section:
```rust
// Top-level convenience re-exports
pub use client::{ClientBuilder, AddTorrentParams};
pub use error::{Error, Result};
```

**Step 3: Commit**

```bash
git add crates/ferrite/Cargo.toml crates/ferrite/src/lib.rs
git commit -m "feat(ferrite): add error + prelude module declarations"
```

---

### Task 2: Create error.rs — unified error enum

**Files:**
- Create: `crates/ferrite/src/error.rs`

```rust
//! Unified error type wrapping all per-crate errors.
//!
//! Each facade module also re-exports its crate's `Error` type for
//! fine-grained matching (e.g., `ferrite::wire::Error`). This unified
//! type allows catching any ferrite error with a single type.

/// Unified error type for the ferrite crate family.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bencode serialization/deserialization error.
    #[error("bencode: {0}")]
    Bencode(#[from] ferrite_bencode::Error),

    /// Core type error (hash parsing, torrent validation, magnet links).
    #[error("core: {0}")]
    Core(#[from] ferrite_core::Error),

    /// Wire protocol error (handshake, message encoding).
    #[error("wire: {0}")]
    Wire(#[from] ferrite_wire::Error),

    /// Tracker communication error.
    #[error("tracker: {0}")]
    Tracker(#[from] ferrite_tracker::Error),

    /// DHT operation error.
    #[error("dht: {0}")]
    Dht(#[from] ferrite_dht::Error),

    /// Storage I/O error (pieces, chunks, files).
    #[error("storage: {0}")]
    Storage(#[from] ferrite_storage::Error),

    /// Session management error (peers, torrents).
    #[error("session: {0}")]
    Session(#[from] ferrite_session::Error),

    /// Raw I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Unified result type for the ferrite crate family.
pub type Result<T> = std::result::Result<T, Error>;
```

---

### Task 3: Create prelude.rs

**Files:**
- Create: `crates/ferrite/src/prelude.rs`

```rust
//! Convenience re-exports for `use ferrite::prelude::*`.
//!
//! Imports the most commonly needed types for building BitTorrent
//! applications with ferrite.

// Builder types
pub use crate::client::{ClientBuilder, AddTorrentParams};

// Session management
pub use crate::session::{
    SessionHandle, TorrentHandle,
    TorrentState, TorrentStats, TorrentInfo,
};

// Core types
pub use crate::core::{Id20, Magnet, TorrentMetaV1};

// Storage
pub use crate::storage::{TorrentStorage, FilesystemStorage};

// Unified error
pub use crate::error::{Error, Result};
```

**After creating both files:**

Run: `cargo check -p ferrite`
Expected: PASS

**Commit:**

```bash
git add crates/ferrite/src/error.rs crates/ferrite/src/prelude.rs
git commit -m "feat(ferrite): add unified error enum and prelude module"
```

---

### Task 4: Add tests

**Files:**
- Modify: `crates/ferrite/src/lib.rs` (add tests to existing `mod tests`)

Add these three tests:

```rust
    #[test]
    fn unified_error_from_conversions() {
        // Bencode error → unified Error
        let bencode_err = bencode::Error::Custom("test".into());
        let unified: crate::Error = bencode_err.into();
        assert!(matches!(unified, crate::Error::Bencode(_)));
        assert!(unified.to_string().contains("bencode:"));

        // Core error → unified Error
        let core_err = core::Error::InvalidHex("bad".into());
        let unified: crate::Error = core_err.into();
        assert!(matches!(unified, crate::Error::Core(_)));

        // IO error → unified Error
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let unified: crate::Error = io_err.into();
        assert!(matches!(unified, crate::Error::Io(_)));
    }

    #[test]
    fn prelude_types_accessible() {
        use crate::prelude::*;

        // Verify key types are in scope from prelude
        let _builder = ClientBuilder::new();
        let _magnet = Magnet::parse(
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        ).unwrap();
        let _hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();

        // Verify TorrentState variants accessible
        let _state = TorrentState::Downloading;
        let _state2 = TorrentState::Seeding;
    }

    #[test]
    fn full_type_chain_through_facade() {
        use crate::prelude::*;

        // Parse magnet → create AddTorrentParams → verify types compose
        let magnet = Magnet::parse(
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test%20file"
        ).unwrap();

        assert_eq!(magnet.display_name.as_deref(), Some("test file"));

        let _params = AddTorrentParams::from_magnet(magnet)
            .download_dir("/tmp/test");

        // Verify unified Result type works
        let ok_result: Result<i32> = Ok(42);
        assert_eq!(ok_result.unwrap(), 42);

        // Verify error conversion chain: bencode → unified
        let err_result: Result<()> = Err(
            Error::Bencode(crate::bencode::Error::Custom("test".into()))
        );
        assert!(err_result.is_err());
    }
```

**Run tests:**

Run: `cargo test -p ferrite`
Expected: 12+ tests PASS (9 existing unit + 3 new + doctests)

Run: `cargo test --workspace`
Expected: All pass

**Commit:**

```bash
git add crates/ferrite/src/lib.rs
git commit -m "test(ferrite): add unified error, prelude, and type chain tests"
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
## 0.11.0 — 2026-02-26

Prelude, unified error, and integration tests complete the M10 facade. Eight crates, ~355 tests.

### M10e: ferrite (prelude + unified error + integration tests)

### Added
- `ferrite::Error` — unified error enum wrapping all 7 per-crate error types + `std::io::Error`
- `ferrite::Result<T>` — unified result type alias
- `ferrite::prelude` — convenience re-exports (`ClientBuilder`, `AddTorrentParams`, `SessionHandle`, `TorrentHandle`, `TorrentState`, `Id20`, `Magnet`, `TorrentMetaV1`, `TorrentStorage`, `FilesystemStorage`, `Error`, `Result`)
- Top-level `ferrite::Error` and `ferrite::Result` re-exports
- 3 facade tests: unified error conversions, prelude imports, full type chain

### Milestone Complete
- **M10 is now complete.** The `ferrite` crate provides a full public facade over all internal crates.
```

**Step 2: Update README.md**

- Update `ferrite` crate description to: `Public facade: full API + ClientBuilder + prelude + unified error`
- Update ferrite test count to reflect actual count
- Update total test count
- Mark M10e as `Done`

**Step 3: Bump workspace version to 0.11.0**

**Step 4: Commit and push**

```bash
git add CHANGELOG.md README.md Cargo.toml
git commit -m "release: v0.11.0 — M10e prelude + unified error (M10 complete)"
git push origin main && git push github main
```
