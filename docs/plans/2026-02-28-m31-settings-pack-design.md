# M31: Settings Pack + Runtime Config — Design

**Date:** 2026-02-28
**Crate:** ferrite-session (primary), ferrite (facade)
**Depends on:** M30 merged (v0.31.0, 748 tests)

## Goal

Replace `SessionConfig` with a single strongly-typed `Settings` struct that consolidates all configurable knobs. Add runtime config mutation via `SessionHandle::apply_settings()`, preset profiles, serialization, and validation.

## Reference: libtorrent `settings_pack`

libtorrent uses a loosely-typed enum-indexed bag (bool/int/string values identified by integer IDs). We diverge: ferrite uses a **strongly-typed struct with named fields** — idiomatic Rust, compile-time safety, IDE autocomplete. The behavioral model is the same: full replacement semantics, internal diff, preset factory functions.

## Design

### Settings Struct

Location: `crates/ferrite-session/src/settings.rs`

A single struct with all ~55 configurable fields as concrete Rust types. Derives `Debug, Clone, Serialize, Deserialize`. All `Option` and collection fields use `#[serde(default)]` for forward compatibility.

**Field groups** (organized by logical area, flat in the struct):

#### General
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `listen_port` | `u16` | `6881` | |
| `download_dir` | `PathBuf` | `"."` | |
| `max_torrents` | `usize` | `100` | |
| `resume_data_dir` | `Option<PathBuf>` | `None` | defaults to `<download_dir>/.ferrite/` |

#### Protocol Features
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `enable_dht` | `bool` | `true` | |
| `enable_pex` | `bool` | `true` | |
| `enable_lsd` | `bool` | `true` | |
| `enable_fast_extension` | `bool` | `true` | |
| `enable_utp` | `bool` | `true` | |
| `enable_upnp` | `bool` | `true` | |
| `enable_natpmp` | `bool` | `true` | |
| `enable_ipv6` | `bool` | `true` | |
| `enable_web_seed` | `bool` | `true` | |
| `encryption_mode` | `EncryptionMode` | `Enabled` | |
| `anonymous_mode` | `bool` | `false` | |

#### Seeding
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `seed_ratio_limit` | `Option<f64>` | `None` | |
| `default_super_seeding` | `bool` | `false` | |
| `upload_only_announce` | `bool` | `true` | |
| `have_send_delay_ms` | `u64` | `0` | |

#### Rate Limiting
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `upload_rate_limit` | `u64` | `0` | 0 = unlimited |
| `download_rate_limit` | `u64` | `0` | 0 = unlimited |
| `auto_upload_slots` | `bool` | `true` | |
| `auto_upload_slots_min` | `usize` | `2` | |
| `auto_upload_slots_max` | `usize` | `20` | |

#### Queue Management
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `active_downloads` | `i32` | `3` | |
| `active_seeds` | `i32` | `5` | |
| `active_limit` | `i32` | `500` | |
| `active_checking` | `i32` | `1` | |
| `dont_count_slow_torrents` | `bool` | `true` | |
| `inactive_down_rate` | `u64` | `2048` | bytes/sec |
| `inactive_up_rate` | `u64` | `2048` | bytes/sec |
| `auto_manage_interval` | `u64` | `30` | seconds |
| `auto_manage_startup` | `u64` | `60` | seconds |
| `auto_manage_prefer_seeds` | `bool` | `false` | |

#### Alerts
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `alert_mask` | `AlertCategory` | `ALL` | |
| `alert_channel_size` | `usize` | `1024` | |

#### Smart Banning
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `smart_ban_max_failures` | `u32` | `3` | |
| `smart_ban_parole` | `bool` | `true` | |

#### Disk I/O
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `disk_io_threads` | `usize` | `4` | |
| `storage_mode` | `StorageMode` | `Auto` | |
| `disk_cache_size` | `usize` | `67_108_864` | 64 MiB |
| `disk_write_cache_ratio` | `f32` | `0.25` | |
| `disk_channel_capacity` | `usize` | `512` | NEW: was buried in DiskConfig |

#### Hashing & Piece Picking
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `hashing_threads` | `usize` | `2` | |
| `max_request_queue_depth` | `usize` | `250` | |
| `request_queue_time` | `f64` | `3.0` | seconds |
| `block_request_timeout_secs` | `u32` | `60` | |
| `max_concurrent_stream_reads` | `usize` | `8` | |

#### Proxy
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `proxy` | `ProxyConfig` | `ProxyConfig::default()` | composite type stays grouped |
| `force_proxy` | `bool` | `false` | |
| `apply_ip_filter_to_trackers` | `bool` | `true` | |

#### DHT Tuning
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `dht_queries_per_second` | `usize` | `50` | NEW: was buried in DhtConfig |
| `dht_query_timeout_secs` | `u64` | `10` | NEW: was buried in DhtConfig |

#### NAT Tuning
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `upnp_lease_duration` | `u32` | `3600` | NEW: was buried in NatConfig |
| `natpmp_lifetime` | `u32` | `7200` | NEW: was buried in NatConfig |

#### uTP Tuning
| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `utp_max_connections` | `usize` | `256` | NEW: was buried in UtpConfig |

**Total: 55 fields** (48 from SessionConfig + 7 newly surfaced from sub-configs)

### Presets

Three factory functions:

```rust
impl Settings {
    pub fn default() -> Self;            // balanced (today's defaults)
    pub fn min_memory() -> Self;         // constrained environments
    pub fn high_performance() -> Self;   // desktop/server
}
```

**`min_memory()` tweaks:** `disk_cache_size: 8 MiB`, `max_torrents: 20`, `active_downloads: 1`, `active_seeds: 2`, `active_limit: 10`, `alert_channel_size: 256`, `utp_max_connections: 64`, `max_request_queue_depth: 50`, `max_concurrent_stream_reads: 2`, `hashing_threads: 1`, `disk_io_threads: 1`

**`high_performance()` tweaks:** `disk_cache_size: 256 MiB`, `max_torrents: 2000`, `active_downloads: 30`, `active_seeds: 100`, `active_limit: 2000`, `alert_channel_size: 4096`, `utp_max_connections: 1024`, `max_request_queue_depth: 1000`, `max_concurrent_stream_reads: 32`, `hashing_threads: 4`, `disk_io_threads: 8`, `auto_upload_slots_max: 100`

### Runtime Changes

```rust
impl SessionHandle {
    /// Get a snapshot of current settings.
    pub fn settings(&self) -> Settings;

    /// Apply new settings. Returns error if validation fails.
    pub fn apply_settings(&self, settings: Settings) -> Result<()>;
}
```

`apply_settings()` internally:
1. Calls `settings.validate()`? — returns error if invalid
2. Diffs old vs new (field-by-field comparison)
3. Sends changed sub-configs to relevant actors:
   - Rate limit changes → each TorrentActor
   - Disk config changes → DiskActor (if disk fields changed)
   - DHT config changes → DhtActor(s)
   - NAT config changes → NatActor
   - Alert mask → atomic store (no actor message needed)
4. Stores new `Settings` as current
5. Fires `AlertKind::SettingsChanged` alert

### Validation

```rust
impl Settings {
    pub fn validate(&self) -> Result<(), SettingsError>;
}
```

Checks:
- `force_proxy && proxy.proxy_type == None` → error
- `active_downloads > active_limit` when both > 0 → error
- `active_seeds > active_limit` when both > 0 → error
- `disk_write_cache_ratio` not in 0.0..=1.0 → error
- `disk_cache_size < 1 MiB` → error (not useful below this)
- `hashing_threads == 0` → error
- `disk_io_threads == 0` → error

`SettingsError` is a new variant in `ferrite_session::Error` or a standalone type with a `Vec<String>` of issues.

### Serialization

`Settings` derives `Serialize + Deserialize`. All fields with non-trivial defaults use `#[serde(default)]` so that loading a config file with missing fields gets the default. Format is bencode (matches libtorrent) or JSON (for human editing) — the struct supports both via serde.

### Sub-Config Construction

Internal conversion from Settings to per-actor configs:

```rust
impl From<&Settings> for DhtConfig { ... }
impl From<&Settings> for NatConfig { ... }
impl From<&Settings> for UtpConfig { ... }
impl From<&Settings> for DiskConfig { ... }
impl From<&Settings> for BanConfig { ... }
```

These From impls are internal to ferrite-session. The sub-config types (`DhtConfig`, `NatConfig`, etc.) are no longer part of the public API — they're implementation details.

### TorrentConfig

Stays as-is — it represents per-torrent overrides that can differ from session defaults. Gains `From<&Settings>` to populate defaults from session settings when a new torrent is added.

### ClientBuilder Changes

`ClientBuilder` internally builds `Settings` instead of `SessionConfig`:
- All 42 existing builder methods stay, just target `Settings` fields
- `into_config()` renamed to `into_settings() -> Settings`
- `start()` passes `Settings` to `SessionHandle::start()`

### Alert

New variant:
```rust
AlertKind::SettingsChanged
```
Category: `STATUS`. Fired after successful `apply_settings()`.

### What Gets Removed

- `SessionConfig` struct (replaced by `Settings`)
- Public re-exports of `DhtConfig`, `NatConfig`, `UtpConfig` from facade (if any)

### What Stays Unchanged

- `TorrentConfig` (per-torrent overrides)
- `ProxyConfig` (composite type, used as field in Settings)
- `BanConfig`, `DiskConfig`, `DhtConfig`, `NatConfig`, `UtpConfig` (internal, constructed from Settings)
- All alert types, session/torrent handle APIs (except new settings methods)

## Test Plan (~12 tests)

1. `default_settings` — verify `Settings::default()` matches current `SessionConfig::default()` field values
2. `min_memory_preset` — verify all min_memory fields differ from default
3. `high_performance_preset` — verify all high_performance fields differ from default
4. `settings_serialization_round_trip` — serialize to bencode → deserialize → assert equal
5. `settings_json_round_trip` — serialize to JSON → deserialize → assert equal (serde compat)
6. `validation_force_proxy_no_proxy` — force_proxy without proxy type → error
7. `validation_active_limit` — active_downloads > active_limit → error
8. `validation_valid_settings` — default settings pass validation
9. `apply_settings_runtime` — apply new settings → query → verify changed
10. `apply_settings_alert` — apply settings → receive SettingsChanged alert
11. `apply_settings_validation_error` — apply invalid settings → error, old settings unchanged
12. `torrent_config_from_settings` — TorrentConfig::from(&settings) picks up session defaults

## Files

### New
| File | Contents |
|------|----------|
| `crates/ferrite-session/src/settings.rs` | `Settings` struct, presets, validation, `SettingsError` |

### Modified
| File | Changes |
|------|---------|
| `crates/ferrite-session/src/types.rs` | Remove `SessionConfig`, add `From<&Settings>` to `TorrentConfig` |
| `crates/ferrite-session/src/session.rs` | `SessionActor` stores `Settings`, add `settings()`/`apply_settings()` commands |
| `crates/ferrite-session/src/lib.rs` | `mod settings`, pub use `Settings` |
| `crates/ferrite-session/src/disk.rs` | `DiskConfig: From<&Settings>` |
| `crates/ferrite-session/src/ban.rs` | `BanConfig: From<&Settings>` |
| `crates/ferrite-session/src/alert.rs` | `AlertKind::SettingsChanged` variant |
| `crates/ferrite/src/client.rs` | `ClientBuilder` builds `Settings`, rename `into_config()` → `into_settings()` |
| `crates/ferrite/src/session.rs` | Re-export `Settings` |
| `crates/ferrite/src/prelude.rs` | Add `Settings` |
| `Cargo.toml` | Version bump 0.31.0 → 0.32.0 |
| `CHANGELOG.md` | M31 entry |

### Tests to Update
All tests referencing `SessionConfig::default()` → `Settings::default()` (mechanical rename).
