# M10d: Session + ClientBuilder — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `ferrite::session` re-exports and `ferrite::client` module with `ClientBuilder` + `AddTorrentParams` new types.

**Architecture:** Session module follows re-export pattern. Client module introduces two new wrapper types providing ergonomic API over `SessionConfig` and torrent addition.

**Tech Stack:** Rust 2024, workspace resolver 2, ferrite-session, tokio (dev-dep for async test)

---

### Task 1: Add session dependency + module declarations

**Files:**
- Modify: `crates/ferrite/Cargo.toml`
- Modify: `crates/ferrite/src/lib.rs`

**Step 1: Add dependency to Cargo.toml**

Add after `ferrite-storage`:

```toml
ferrite-session = { path = "../ferrite-session" }
```

Also add `tokio` as a dev-dependency (needed for async test of ClientBuilder::start):

```toml
[dev-dependencies]
pretty_assertions = { workspace = true }
serde = { workspace = true, features = ["derive"] }
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

**Step 2: Add module declarations to lib.rs**

Add to the doc comment list:
```rust
//! - [`session`] — Session management, torrent orchestration
//! - [`client`] — Ergonomic `ClientBuilder` and `AddTorrentParams`
```

Add after `pub mod storage;`:
```rust
pub mod session;
pub mod client;
```

**Step 3: Commit**

```bash
git add crates/ferrite/Cargo.toml crates/ferrite/src/lib.rs
git commit -m "feat(ferrite): add session dep and module declarations"
```

---

### Task 2: Create session.rs re-exports

**Files:**
- Create: `crates/ferrite/src/session.rs`

```rust
//! BitTorrent session management: peers, torrents, orchestration.
//!
//! Re-exports from [`ferrite_session`].

pub use ferrite_session::{
    // Session manager
    SessionHandle,
    SessionConfig,
    SessionStats,
    // Torrent handle and state
    TorrentHandle,
    TorrentConfig,
    TorrentState,
    TorrentStats,
    TorrentInfo,
    // File info
    FileInfo,
    // Storage factory type alias
    StorageFactory,
    // Error types
    Error,
    Result,
};
```

---

### Task 3: Create client.rs with ClientBuilder + AddTorrentParams

**Files:**
- Create: `crates/ferrite/src/client.rs`

```rust
//! Ergonomic builder types for creating sessions and adding torrents.

use std::path::PathBuf;
use std::sync::Arc;

use ferrite_core::{Magnet, TorrentMetaV1};
use ferrite_session::SessionConfig;
use ferrite_storage::TorrentStorage;

/// Ergonomic builder for creating a ferrite session.
///
/// Wraps [`SessionConfig`] with a fluent API. Call [`start()`](Self::start)
/// to spawn the session actor and get a [`SessionHandle`](ferrite_session::SessionHandle).
///
/// # Example
///
/// ```no_run
/// # async fn example() -> ferrite::session::Result<()> {
/// let session = ferrite::ClientBuilder::new()
///     .download_dir("/tmp/downloads")
///     .listen_port(6881)
///     .enable_dht(true)
///     .start()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct ClientBuilder {
    config: SessionConfig,
}

impl ClientBuilder {
    /// Create a new builder with default configuration.
    pub fn new() -> Self {
        Self {
            config: SessionConfig::default(),
        }
    }

    /// Set the TCP listen port for incoming peer connections.
    pub fn listen_port(mut self, port: u16) -> Self {
        self.config.listen_port = port;
        self
    }

    /// Set the default download directory.
    pub fn download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.download_dir = path.into();
        self
    }

    /// Set the maximum number of concurrent torrents.
    pub fn max_torrents(mut self, n: usize) -> Self {
        self.config.max_torrents = n;
        self
    }

    /// Enable or disable DHT peer discovery.
    pub fn enable_dht(mut self, v: bool) -> Self {
        self.config.enable_dht = v;
        self
    }

    /// Enable or disable Local Service Discovery.
    pub fn enable_lsd(mut self, v: bool) -> Self {
        self.config.enable_lsd = v;
        self
    }

    /// Enable or disable Peer Exchange.
    pub fn enable_pex(mut self, v: bool) -> Self {
        self.config.enable_pex = v;
        self
    }

    /// Enable or disable BEP 6 Fast Extension.
    pub fn enable_fast_extension(mut self, v: bool) -> Self {
        self.config.enable_fast_extension = v;
        self
    }

    /// Set the seed ratio limit. Torrents stop seeding when this ratio is reached.
    pub fn seed_ratio_limit(mut self, ratio: f64) -> Self {
        self.config.seed_ratio_limit = Some(ratio);
        self
    }

    /// Consume the builder and return the underlying `SessionConfig`.
    pub fn into_config(self) -> SessionConfig {
        self.config
    }

    /// Start the session, spawning the background actor.
    pub async fn start(self) -> ferrite_session::Result<ferrite_session::SessionHandle> {
        ferrite_session::SessionHandle::start(self.config).await
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Source for adding a torrent to a session.
enum TorrentSource {
    /// Parsed torrent metainfo.
    Meta(TorrentMetaV1),
    /// Magnet link (metadata fetched via BEP 9).
    Magnet(Magnet),
    /// Path to a .torrent file on disk.
    File(PathBuf),
    /// Raw .torrent file bytes.
    Bytes(Vec<u8>),
}

/// Unified parameters for adding a torrent to a session.
///
/// Construct via [`from_torrent()`](Self::from_torrent),
/// [`from_magnet()`](Self::from_magnet), [`from_file()`](Self::from_file),
/// or [`from_bytes()`](Self::from_bytes).
pub struct AddTorrentParams {
    source: TorrentSource,
    download_dir: Option<PathBuf>,
    storage: Option<Arc<dyn TorrentStorage>>,
}

impl AddTorrentParams {
    /// Create params from parsed torrent metainfo.
    pub fn from_torrent(meta: TorrentMetaV1) -> Self {
        Self {
            source: TorrentSource::Meta(meta),
            download_dir: None,
            storage: None,
        }
    }

    /// Create params from a magnet link.
    pub fn from_magnet(magnet: Magnet) -> Self {
        Self {
            source: TorrentSource::Magnet(magnet),
            download_dir: None,
            storage: None,
        }
    }

    /// Create params from a .torrent file path.
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self {
            source: TorrentSource::File(path.into()),
            download_dir: None,
            storage: None,
        }
    }

    /// Create params from raw .torrent file bytes.
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self {
            source: TorrentSource::Bytes(data),
            download_dir: None,
            storage: None,
        }
    }

    /// Override the download directory for this torrent.
    pub fn download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.download_dir = Some(path.into());
        self
    }

    /// Provide custom storage for this torrent.
    pub fn storage(mut self, s: Arc<dyn TorrentStorage>) -> Self {
        self.storage = Some(s);
        self
    }
}
```

**After creating both files:**

Run: `cargo check -p ferrite`
Expected: PASS

**Commit:**

```bash
git add crates/ferrite/src/session.rs crates/ferrite/src/client.rs
git commit -m "feat(ferrite): add session re-exports and client builder types"
```

---

### Task 4: Add lib.rs re-exports + facade tests

**Files:**
- Modify: `crates/ferrite/src/lib.rs`

**Step 1: Add top-level re-exports for ClientBuilder and AddTorrentParams**

After the module declarations in lib.rs, add:

```rust
// Top-level convenience re-exports
pub use client::{ClientBuilder, AddTorrentParams};
```

**Step 2: Add tests to existing `mod tests` block**

```rust
    #[test]
    fn client_builder_defaults_and_chaining() {
        let builder = crate::ClientBuilder::new()
            .listen_port(6882)
            .download_dir("/tmp/test")
            .max_torrents(50)
            .enable_dht(false)
            .enable_lsd(false)
            .enable_pex(true)
            .enable_fast_extension(true)
            .seed_ratio_limit(2.0);

        let config = builder.into_config();
        assert_eq!(config.listen_port, 6882);
        assert_eq!(config.download_dir, std::path::PathBuf::from("/tmp/test"));
        assert_eq!(config.max_torrents, 50);
        assert!(!config.enable_dht);
        assert!(!config.enable_lsd);
        assert!(config.enable_pex);
        assert!(config.enable_fast_extension);
        assert_eq!(config.seed_ratio_limit, Some(2.0));
    }

    #[test]
    fn add_torrent_params_from_magnet() {
        let magnet = core::Magnet::parse(
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test"
        ).unwrap();

        let params = crate::AddTorrentParams::from_magnet(magnet)
            .download_dir("/tmp/downloads");

        // Verify the builder pattern works (we can't inspect private fields,
        // but we can verify it compiles and chains correctly)
        let _ = params;
    }
```

**Step 3: Run tests**

Run: `cargo test -p ferrite`
Expected: 9 tests PASS (7 existing + 2 new)

Run: `cargo test --workspace`
Expected: All tests pass (351 total)

**Step 4: Commit**

```bash
git add crates/ferrite/src/lib.rs
git commit -m "feat(ferrite): add top-level re-exports and client builder tests"
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
## 0.10.0 — 2026-02-25

Session re-exports and ergonomic client builder in facade crate. Eight crates, 351 tests.

### M10d: ferrite (session + ClientBuilder)

### Added
- `ferrite::session` — re-exports `SessionHandle`, `SessionConfig`, `SessionStats`, `TorrentHandle`, `TorrentConfig`, `TorrentState`, `TorrentStats`, `TorrentInfo`, `FileInfo`, `StorageFactory`, `Error`
- `ferrite::client` — new `ClientBuilder` (fluent builder for `SessionConfig` → `SessionHandle`) and `AddTorrentParams` (unified torrent source with `from_torrent`/`from_magnet`/`from_file`/`from_bytes`)
- Top-level `ferrite::ClientBuilder` and `ferrite::AddTorrentParams` re-exports
- 2 facade tests: client builder defaults + chaining, AddTorrentParams from magnet
```

**Step 2: Update README.md**

- Update `ferrite` crate row: description to `Public facade: re-exports all crate APIs + ClientBuilder`, test count to `9`
- Update total from `349` to `351`
- Mark M10d as `Done`

**Step 3: Bump workspace version to 0.10.0**

**Step 4: Commit and push**

```bash
git add CHANGELOG.md README.md Cargo.toml
git commit -m "release: v0.10.0 — M10d session re-exports + ClientBuilder"
git push origin main && git push github main
```
