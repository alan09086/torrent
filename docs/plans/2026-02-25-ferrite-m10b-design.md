# M10b: Wire + Tracker Re-exports — Design

**Date:** 2026-02-25
**Status:** Approved
**Crate:** `crates/ferrite/`

## Goal

Add `ferrite::wire` and `ferrite::tracker` modules to the facade crate,
re-exporting all public types from `ferrite-wire` and `ferrite-tracker`.
Same `pub use` pattern as M10a.

## Changes

### 1. `Cargo.toml` — Add dependencies

```toml
ferrite-wire = { path = "../ferrite-wire" }
ferrite-tracker = { path = "../ferrite-tracker" }
```

### 2. `src/wire.rs` — Re-export 9 public items

```rust
pub use ferrite_wire::{
    Handshake, Message, MessageCodec,
    ExtHandshake, ExtMessage, MetadataMessage, MetadataMessageType,
    allowed_fast_set,
    Error, Result,
};
```

Note: `HANDSHAKE_SIZE` is not re-exported — it's `pub` in `handshake.rs`
but not in `ferrite-wire`'s `lib.rs`.

### 3. `src/tracker.rs` — Re-export 10 public items

```rust
pub use ferrite_tracker::{
    HttpTracker, HttpAnnounceResponse,
    UdpTracker, UdpAnnounceResponse,
    AnnounceRequest, AnnounceResponse, AnnounceEvent,
    parse_compact_peers,
    Error, Result,
};
```

### 4. `src/lib.rs` — Add module declarations

```rust
pub mod wire;
pub mod tracker;
```

Plus doc comments in the crate-level rustdoc.

### 5. Tests (~2)

- **Handshake round-trip through facade:** construct a `Handshake`, encode to
  bytes, decode back, assert equality
- **AnnounceRequest construction through facade:** build an `AnnounceRequest`
  with all fields, verify values accessible

### 6. Release

- CHANGELOG.md: add v0.8.0 entry
- README.md: mark M10b as Done
- Bump workspace version to 0.8.0
- Push to both remotes
