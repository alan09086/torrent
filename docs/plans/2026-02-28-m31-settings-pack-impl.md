# M31: Settings Pack + Runtime Config — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace `SessionConfig` with a unified `Settings` struct, add runtime config mutation via `apply_settings()`, presets, validation, and serialization.

**Architecture:** New `settings.rs` in ferrite-session contains the `Settings` struct with all ~55 fields. `SessionConfig` is removed and all references updated. `SessionHandle` gets `settings()`/`apply_settings()` methods. Sub-configs constructed via `From<&Settings>` impls. `ClientBuilder` targets `Settings` directly.

**Tech Stack:** Rust, serde (Serialize/Deserialize), ferrite-bencode (serialization), existing actor command pattern.

---

### Step 1: Create Settings struct with defaults, presets, and validation

**Files:**
- Create: `crates/ferrite-session/src/settings.rs`
- Modify: `crates/ferrite-session/src/lib.rs` — add `mod settings;` + pub use

This step creates the new Settings struct as a standalone addition — no existing code is changed yet (except lib.rs wiring). All 55 fields, Default impl, 3 presets, validation, Serialize/Deserialize, and 8 tests.

**The struct** has all fields from the design doc. Every field gets `#[serde(default)]` for forward compatibility (bencode has no null, but JSON does — and missing keys during deserialization need defaults). `ProxyConfig` needs `Serialize + Deserialize` — check if it already has them; if not, add derives.

**Important serde notes:**
- `AlertCategory` is bitflags — verify it has Serialize/Deserialize (it does, from M15)
- `EncryptionMode` — verify serde derives (it does, from M17)
- `StorageMode` — verify serde derives (it does, from M26)
- `ProxyConfig` — check if it has serde derives. If not, add `Serialize, Deserialize` to `ProxyConfig` and `ProxyType` in `proxy.rs`
- `PathBuf` — serde handles this natively

**Settings struct fields (55 total):**

```rust
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use ferrite_core::StorageMode;
use ferrite_wire::mse::EncryptionMode;
use crate::alert::AlertCategory;
use crate::proxy::ProxyConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    // General
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,                          // 6881
    #[serde(default)]
    pub download_dir: PathBuf,                     // "."
    #[serde(default = "default_max_torrents")]
    pub max_torrents: usize,                       // 100
    #[serde(default)]
    pub resume_data_dir: Option<PathBuf>,           // None

    // Protocol features
    #[serde(default = "default_true")]
    pub enable_dht: bool,
    #[serde(default = "default_true")]
    pub enable_pex: bool,
    #[serde(default = "default_true")]
    pub enable_lsd: bool,
    #[serde(default = "default_true")]
    pub enable_fast_extension: bool,
    #[serde(default = "default_true")]
    pub enable_utp: bool,
    #[serde(default = "default_true")]
    pub enable_upnp: bool,
    #[serde(default = "default_true")]
    pub enable_natpmp: bool,
    #[serde(default = "default_true")]
    pub enable_ipv6: bool,
    #[serde(default = "default_true")]
    pub enable_web_seed: bool,
    #[serde(default = "default_encryption")]
    pub encryption_mode: EncryptionMode,           // Enabled
    #[serde(default)]
    pub anonymous_mode: bool,                      // false

    // Seeding
    #[serde(default)]
    pub seed_ratio_limit: Option<f64>,             // None
    #[serde(default)]
    pub default_super_seeding: bool,               // false
    #[serde(default = "default_true")]
    pub upload_only_announce: bool,                // true
    #[serde(default)]
    pub have_send_delay_ms: u64,                   // 0

    // Rate limiting
    #[serde(default)]
    pub upload_rate_limit: u64,                    // 0 = unlimited
    #[serde(default)]
    pub download_rate_limit: u64,                  // 0 = unlimited
    #[serde(default = "default_true")]
    pub auto_upload_slots: bool,                   // true
    #[serde(default = "default_auto_upload_slots_min")]
    pub auto_upload_slots_min: usize,              // 2
    #[serde(default = "default_auto_upload_slots_max")]
    pub auto_upload_slots_max: usize,              // 20

    // Queue management
    #[serde(default = "default_active_downloads")]
    pub active_downloads: i32,                     // 3
    #[serde(default = "default_active_seeds")]
    pub active_seeds: i32,                         // 5
    #[serde(default = "default_active_limit")]
    pub active_limit: i32,                         // 500
    #[serde(default = "default_active_checking")]
    pub active_checking: i32,                      // 1
    #[serde(default = "default_true")]
    pub dont_count_slow_torrents: bool,            // true
    #[serde(default = "default_inactive_rate")]
    pub inactive_down_rate: u64,                   // 2048
    #[serde(default = "default_inactive_rate")]
    pub inactive_up_rate: u64,                     // 2048
    #[serde(default = "default_auto_manage_interval")]
    pub auto_manage_interval: u64,                 // 30
    #[serde(default = "default_auto_manage_startup")]
    pub auto_manage_startup: u64,                  // 60
    #[serde(default)]
    pub auto_manage_prefer_seeds: bool,            // false

    // Alerts
    #[serde(default = "default_alert_mask")]
    pub alert_mask: AlertCategory,                 // ALL
    #[serde(default = "default_alert_channel_size")]
    pub alert_channel_size: usize,                 // 1024

    // Smart banning
    #[serde(default = "default_smart_ban_max_failures")]
    pub smart_ban_max_failures: u32,               // 3
    #[serde(default = "default_true")]
    pub smart_ban_parole: bool,                    // true

    // Disk I/O
    #[serde(default = "default_disk_io_threads")]
    pub disk_io_threads: usize,                    // 4
    #[serde(default = "default_storage_mode")]
    pub storage_mode: StorageMode,                 // Auto
    #[serde(default = "default_disk_cache_size")]
    pub disk_cache_size: usize,                    // 64 MiB
    #[serde(default = "default_disk_write_cache_ratio")]
    pub disk_write_cache_ratio: f32,               // 0.25
    #[serde(default = "default_disk_channel_capacity")]
    pub disk_channel_capacity: usize,              // 512

    // Hashing & piece picking
    #[serde(default = "default_hashing_threads")]
    pub hashing_threads: usize,                    // 2
    #[serde(default = "default_max_request_queue_depth")]
    pub max_request_queue_depth: usize,            // 250
    #[serde(default = "default_request_queue_time")]
    pub request_queue_time: f64,                   // 3.0
    #[serde(default = "default_block_request_timeout")]
    pub block_request_timeout_secs: u32,           // 60
    #[serde(default = "default_max_concurrent_streams")]
    pub max_concurrent_stream_reads: usize,        // 8

    // Proxy
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub force_proxy: bool,                         // false
    #[serde(default = "default_true")]
    pub apply_ip_filter_to_trackers: bool,         // true

    // DHT tuning
    #[serde(default = "default_dht_qps")]
    pub dht_queries_per_second: usize,             // 50
    #[serde(default = "default_dht_timeout")]
    pub dht_query_timeout_secs: u64,               // 10

    // NAT tuning
    #[serde(default = "default_upnp_lease")]
    pub upnp_lease_duration: u32,                  // 3600
    #[serde(default = "default_natpmp_lifetime")]
    pub natpmp_lifetime: u32,                      // 7200

    // uTP tuning
    #[serde(default = "default_utp_max_conns")]
    pub utp_max_connections: usize,                // 256
}
```

Then implement:
- `Default for Settings` — all default values matching current SessionConfig defaults
- `Settings::min_memory()` — constrained preset
- `Settings::high_performance()` — server preset
- `Settings::validate() -> Result<(), crate::Error>` — returns `Error::InvalidSettings(String)` with a message describing the first validation failure found
- Helper `fn default_true() -> bool { true }` and similar for serde defaults

Add `InvalidSettings(String)` variant to `crates/ferrite-session/src/error.rs`.

Add to `crates/ferrite-session/src/lib.rs`:
```rust
mod settings;
pub use settings::Settings;
```

Before writing Settings, check if `ProxyConfig` and `ProxyType` have `Serialize, Deserialize` derives. If not, add them in `crates/ferrite-session/src/proxy.rs`.

**Tests (in settings.rs #[cfg(test)] module):**

1. `default_settings_values` — spot-check key defaults (listen_port=6881, max_torrents=100, disk_cache_size=64MiB, etc.)
2. `min_memory_preset` — verify divergent fields (disk_cache_size=8MiB, max_torrents=20, etc.)
3. `high_performance_preset` — verify divergent fields (disk_cache_size=256MiB, active_limit=2000, etc.)
4. `serialization_round_trip` — `ferrite_bencode::to_bytes(&settings)` → `from_bytes` → assert_eq
5. `json_round_trip` — `serde_json::to_string(&settings)` → `from_str` → assert_eq
6. `validation_force_proxy_no_proxy` — force_proxy=true, proxy_type=None → error
7. `validation_valid_defaults` — `Settings::default().validate()` → Ok
8. `validation_zero_threads` — hashing_threads=0 → error

**Verify:** `cargo test -p ferrite-session settings` — 8 new tests pass. `cargo clippy --workspace -- -D warnings` clean.

**Commit:** `feat: add Settings struct with presets, validation, serialization (M31)`

---

### Step 2: Add From<&Settings> impls for sub-configs + TorrentConfig

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs` — add From impls at bottom
- Modify: `crates/ferrite-session/src/types.rs` — add `From<&Settings>` for TorrentConfig

Add these From impls in `settings.rs` (after the main struct):

```rust
impl From<&Settings> for crate::disk::DiskConfig {
    fn from(s: &Settings) -> Self {
        Self {
            io_threads: s.disk_io_threads,
            storage_mode: s.storage_mode,
            cache_size: s.disk_cache_size,
            write_cache_ratio: s.disk_write_cache_ratio,
            channel_capacity: s.disk_channel_capacity,
        }
    }
}

impl From<&Settings> for crate::ban::BanConfig {
    fn from(s: &Settings) -> Self {
        Self {
            max_failures: s.smart_ban_max_failures,
            use_parole: s.smart_ban_parole,
        }
    }
}
```

For DhtConfig, NatConfig, UtpConfig — these live in other crates, so we can't impl From on foreign types. Instead, add helper methods on Settings:

```rust
impl Settings {
    pub(crate) fn to_dht_config(&self) -> ferrite_dht::DhtConfig {
        let mut config = ferrite_dht::DhtConfig::default();
        config.queries_per_second = self.dht_queries_per_second;
        config.query_timeout = std::time::Duration::from_secs(self.dht_query_timeout_secs);
        config
    }

    pub(crate) fn to_dht_config_v6(&self) -> ferrite_dht::DhtConfig {
        let mut config = ferrite_dht::DhtConfig::default_v6();
        config.queries_per_second = self.dht_queries_per_second;
        config.query_timeout = std::time::Duration::from_secs(self.dht_query_timeout_secs);
        config
    }

    pub(crate) fn to_nat_config(&self) -> ferrite_nat::NatConfig {
        ferrite_nat::NatConfig {
            enable_upnp: self.enable_upnp,
            enable_natpmp: self.enable_natpmp,
            upnp_lease_duration: self.upnp_lease_duration,
            natpmp_lifetime: self.natpmp_lifetime,
        }
    }

    pub(crate) fn to_utp_config(&self, port: u16) -> ferrite_utp::UtpConfig {
        ferrite_utp::UtpConfig {
            bind_addr: std::net::SocketAddr::from(([0, 0, 0, 0], port)),
            max_connections: self.utp_max_connections,
        }
    }

    pub(crate) fn to_utp_config_v6(&self, port: u16) -> ferrite_utp::UtpConfig {
        ferrite_utp::UtpConfig {
            bind_addr: std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)),
            max_connections: self.utp_max_connections,
        }
    }
}
```

In `types.rs`, add `From<&Settings>` for TorrentConfig (at the end of the TorrentConfig impl block):

```rust
impl From<&crate::settings::Settings> for TorrentConfig {
    fn from(s: &crate::settings::Settings) -> Self {
        Self {
            listen_port: s.listen_port,
            max_peers: 50, // TorrentConfig default, not in Settings
            target_request_queue: 5,
            download_dir: s.download_dir.clone(),
            enable_dht: s.enable_dht,
            enable_pex: s.enable_pex,
            enable_fast: s.enable_fast_extension,
            seed_ratio_limit: s.seed_ratio_limit,
            strict_end_game: true,
            upload_rate_limit: 0,  // per-torrent defaults to unlimited
            download_rate_limit: 0,
            encryption_mode: s.encryption_mode,
            enable_utp: s.enable_utp,
            enable_web_seed: s.enable_web_seed,
            max_web_seeds: 4,
            super_seeding: s.default_super_seeding,
            upload_only_announce: s.upload_only_announce,
            have_send_delay_ms: s.have_send_delay_ms,
            hashing_threads: s.hashing_threads,
            sequential_download: false,
            initial_picker_threshold: 4,
            whole_pieces_threshold: 20,
            snub_timeout_secs: 60,
            readahead_pieces: 8,
            streaming_timeout_escalation: true,
            max_concurrent_stream_reads: s.max_concurrent_stream_reads,
            proxy: s.proxy.clone(),
            anonymous_mode: s.anonymous_mode,
        }
    }
}
```

**Tests:** Add 2 tests in settings.rs:
9. `disk_config_from_settings` — verify DiskConfig::from(&Settings::default()) matches expected values
10. `torrent_config_from_settings` — verify TorrentConfig::from(&Settings::default()) picks up session defaults

**Verify:** `cargo test -p ferrite-session` — all pass. Clippy clean.

**Commit:** `feat: add From<&Settings> impls for sub-configs and TorrentConfig (M31)`

---

### Step 3: Replace SessionConfig with Settings in SessionHandle and SessionActor

**Files:**
- Modify: `crates/ferrite-session/src/session.rs` — the big migration

This is the core change. Replace every `SessionConfig` reference in session.rs with `Settings`.

**Changes in session.rs:**

1. **Imports (line 23):** Change `SessionConfig` → `Settings` in the use statement. Add `use crate::settings::Settings;` if not already imported from types.

2. **SessionHandle::start() (line 154):** Change signature from `config: SessionConfig` to `settings: Settings`.

3. **Inside start() body:** Replace all `config.` references with `settings.` and use the new helper methods:
   - `config.to_disk_config()` → `DiskConfig::from(&settings)`
   - `ferrite_utp::UtpConfig { bind_addr: ..., max_connections: 256 }` → `settings.to_utp_config(settings.listen_port)`
   - IPv6 uTP: → `settings.to_utp_config_v6(settings.listen_port)`
   - `ferrite_nat::NatConfig { enable_upnp: config.enable_upnp, ... }` → `settings.to_nat_config()`
   - `DhtConfig::default()` → `settings.to_dht_config()`
   - `DhtConfig::default_v6()` → `settings.to_dht_config_v6()`
   - `crate::ban::BanConfig { max_failures: ..., use_parole: ... }` → `BanConfig::from(&settings)`
   - `config` variable name → `settings` throughout

4. **SessionActor struct (line 634):** Change `config: SessionConfig` → `settings: Settings`

5. **All `self.config.` references in SessionActor methods:** Change to `self.settings.`
   - This is ~30+ occurrences throughout the file. Mechanical find-replace.
   - In the TorrentConfig construction block (~line 870-895): Replace the manual field-by-field construction with `TorrentConfig::from(&self.settings)` — then apply any per-torrent overrides after.

6. **SessionActor construction in start() (~line 303):** Change `config: config` → `settings: settings`

**Changes in types.rs:**
- Remove the `SessionConfig` struct (lines 287-382), its `Default` impl (lines 384-439), and the `to_disk_config()` method (lines 441-452)
- Remove `SessionConfig` from the test module (lines 470-588) — these tests move to settings.rs equivalents
- Keep all other types (TorrentConfig, SessionStats, etc.)

**Changes in lib.rs (line 39):**
- Remove `SessionConfig` from the pub use statement
- `Settings` is already exported from the `mod settings;` line added in Step 1

**Update tests in session.rs:**
- `test_session_config()` helper (~line 1539-1592): Rename to `test_settings()`, return `Settings` instead of `SessionConfig`. Update the struct literal to use Settings field names.
- All test call sites using `SessionHandle::start(test_session_config())` → `SessionHandle::start(test_settings())`
- `session_config_nat_defaults()` test (line 2186): Update to use `Settings::default()`
- Any struct literal tests: Update field names

**Verify:** `cargo test -p ferrite-session` — all existing tests pass with Settings. `cargo test --workspace` — everything compiles. Clippy clean.

**Commit:** `feat: replace SessionConfig with Settings in session layer (M31)`

---

### Step 4: Add runtime apply_settings() + SettingsChanged alert

**Files:**
- Modify: `crates/ferrite-session/src/session.rs` — new command variant, SessionHandle method, SessionActor handler
- Modify: `crates/ferrite-session/src/alert.rs` — new AlertKind variant + category mapping

**In alert.rs:**
- Add `SettingsChanged` variant to `AlertKind` enum (after the last variant, before closing brace)
- Add category mapping in `category()` method: `AlertKind::SettingsChanged => AlertCategory::STATUS`

**In session.rs:**

Add two new variants to `SessionCommand` enum:
```rust
GetSettings {
    reply: oneshot::Sender<Settings>,
},
ApplySettings {
    settings: Settings,
    reply: oneshot::Sender<crate::Result<()>>,
},
```

Add methods to `SessionHandle`:
```rust
pub async fn settings(&self) -> Settings {
    let (tx, rx) = oneshot::channel();
    let _ = self.cmd_tx.send(SessionCommand::GetSettings { reply: tx }).await;
    rx.await.unwrap()
}

pub async fn apply_settings(&self, settings: Settings) -> crate::Result<()> {
    let (tx, rx) = oneshot::channel();
    let _ = self.cmd_tx.send(SessionCommand::ApplySettings { settings, reply: tx }).await;
    rx.await.unwrap()
}
```

Add handler in SessionActor's command dispatch match:
```rust
SessionCommand::GetSettings { reply } => {
    let _ = reply.send(self.settings.clone());
}
SessionCommand::ApplySettings { settings, reply } => {
    let result = self.handle_apply_settings(settings);
    let _ = reply.send(result);
}
```

Add `handle_apply_settings()` method to SessionActor:
```rust
fn handle_apply_settings(&mut self, new: Settings) -> crate::Result<()> {
    new.validate()?;

    // Update rate limiters if changed
    if new.upload_rate_limit != self.settings.upload_rate_limit {
        if new.upload_rate_limit > 0 {
            self.upload_limiter.lock().unwrap().set_rate(new.upload_rate_limit);
        }
        // If 0 (unlimited), the limiter's allow() always returns true when rate=0
    }
    if new.download_rate_limit != self.settings.download_rate_limit {
        if new.download_rate_limit > 0 {
            self.download_limiter.lock().unwrap().set_rate(new.download_rate_limit);
        }
    }

    // Store new settings
    self.settings = new;

    // Fire alert
    crate::alert::post_alert(
        &self.alert_tx,
        &self.alert_mask,
        AlertKind::SettingsChanged,
    );

    Ok(())
}
```

Note: For this milestone, we push the most important runtime changes (rate limits, alert mask). Full sub-actor propagation (disk, DHT, NAT reconfiguration) is left for future enhancement — the settings are stored and will take effect on next session restart. Document this in a comment.

Check if `TokenBucket` has a `set_rate()` method. If not, add one (simple: `self.rate = rate; self.tokens = self.tokens.min(rate);`).

**Tests:** Add 2 tests in session.rs:
11. `apply_settings_runtime` — start session → apply_settings with changed max_torrents → query settings() → verify changed
12. `apply_settings_validation_error` — apply invalid settings (force_proxy=true, no proxy) → error returned, settings unchanged

**Verify:** `cargo test -p ferrite-session` — all pass including new tests. Clippy clean.

**Commit:** `feat: add apply_settings() runtime config + SettingsChanged alert (M31)`

---

### Step 5: Update ClientBuilder facade + re-exports + version bump

**Files:**
- Modify: `crates/ferrite/src/client.rs` — ClientBuilder targets Settings
- Modify: `crates/ferrite/src/session.rs` — re-export Settings instead of SessionConfig
- Modify: `crates/ferrite/src/prelude.rs` — add Settings
- Modify: `Cargo.toml` — version 0.31.0 → 0.32.0
- Modify: `CHANGELOG.md` — M31 entry

**In client.rs:**
- Change `use ferrite_session::SessionConfig;` → `use ferrite_session::Settings;`
- Change `ClientBuilder { config: SessionConfig }` field → `ClientBuilder { settings: Settings }`
- `ClientBuilder::new()` → `Self { settings: Settings::default() }`
- All builder methods: `self.config.field_name` → `self.settings.field_name`
- `into_config()` → `into_settings()` (rename method, return type Settings)
- `start()` → passes `self.settings` to `SessionHandle::start()`
- Add 3 new builder methods for the newly surfaced fields:
  - `dht_queries_per_second(n: usize)`
  - `upnp_lease_duration(secs: u32)`
  - `utp_max_connections(n: usize)`
- Update all tests in client.rs: `.into_config()` → `.into_settings()`, `SessionConfig` → `Settings`

**In session.rs (facade re-exports):**
- Replace `SessionConfig,` with `Settings,` in the re-export block
- Add `Settings` if not already there

**In prelude.rs:**
- Add `pub use crate::core::Settings;` — wait, Settings is in session, not core
- Add `pub use crate::session::Settings;` under a new M31 comment

**Version bump:** Root `Cargo.toml` workspace version 0.31.0 → 0.32.0

**CHANGELOG.md:** Add 0.32.0 section with M31 entry listing all additions/changes.

**Verify:**
```bash
cargo test --workspace                          # all tests pass (748 + ~12 new = ~760)
cargo clippy --workspace -- -D warnings         # zero warnings
cargo test -p ferrite-session settings          # M31 settings tests specifically
```

**Commit:** `feat: update ClientBuilder facade, re-exports, bump to 0.32.0 (M31)`

---

## Verification Checklist

```bash
cd /mnt/TempNVME/projects/ferrite
cargo test --workspace                          # 748 + ~12 new tests pass
cargo clippy --workspace -- -D warnings         # zero warnings
cargo test -p ferrite-session settings          # M31 tests specifically
```

## Test Summary (12 tests)

| # | Test | Validates |
|---|------|-----------|
| 1 | `default_settings_values` | Key defaults match current SessionConfig |
| 2 | `min_memory_preset` | Constrained preset diverges correctly |
| 3 | `high_performance_preset` | Server preset diverges correctly |
| 4 | `serialization_round_trip` | Bencode serialize/deserialize |
| 5 | `json_round_trip` | JSON serialize/deserialize |
| 6 | `validation_force_proxy_no_proxy` | Invalid combo caught |
| 7 | `validation_valid_defaults` | Defaults pass validation |
| 8 | `validation_zero_threads` | Zero threads caught |
| 9 | `disk_config_from_settings` | DiskConfig conversion correct |
| 10 | `torrent_config_from_settings` | TorrentConfig inherits session defaults |
| 11 | `apply_settings_runtime` | Runtime settings change works |
| 12 | `apply_settings_validation_error` | Invalid settings rejected |
