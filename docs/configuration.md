# Configuration

All session configuration is managed through the `Settings` struct (106 fields). Settings can be provided at session creation via `ClientBuilder` or modified at runtime via `SessionHandle::apply_settings()`.

## Quick Start

```rust
use torrent::prelude::*;

let session = ClientBuilder::new()
    .download_dir("/tmp/downloads")
    .listen_port(6881)
    .enable_dht(true)
    .max_peers_per_torrent(128)
    .start()
    .await?;
```

## Presets

Two built-in presets are available:

- **`Settings::default()`** — Balanced defaults suitable for most desktop use
- **`Settings::min_memory()`** — Constrained environments (8 MiB cache, 20 torrents, 30 peers)
- **`Settings::high_performance()`** — Desktop/server (256 MiB cache, 2000 torrents, 500 peers)

## Key Settings Groups

### Resource Limits (M60)

| Setting | Default | Description |
|---------|---------|-------------|
| `max_metadata_size` | 4 MiB | Maximum BEP 9 metadata accepted from peers |
| `max_message_size` | 16 MiB | Maximum wire protocol message size |
| `max_piece_length` | 32 MiB | Maximum piece size when adding a torrent |
| `max_outstanding_requests` | 500 | Maximum incoming request queue per peer |

### Transfer

| Setting | Default | Description |
|---------|---------|-------------|
| `max_request_queue_depth` | 250 | Maximum outbound request pipeline per peer |
| `initial_queue_depth` | 128 | Starting pipeline depth (skip slow-start) |
| `request_queue_time` | 3.0s | Seconds of requests to keep in flight |
| `block_request_timeout_secs` | 60 | Timeout before re-issuing a block request |

### Disk I/O

| Setting | Default | Description |
|---------|---------|-------------|
| `disk_io_threads` | 4 | Concurrent disk I/O threads |
| `disk_cache_size` | 64 MiB | ARC cache size (min 1 MiB) |
| `disk_write_cache_ratio` | 0.25 | Fraction of cache for write buffering |
| `hashing_threads` | 2 | Concurrent piece hash verifications |
| `storage_mode` | Auto | Auto, FullPreallocate, or SparseFile |

### Networking

| Setting | Default | Description |
|---------|---------|-------------|
| `listen_port` | 6881 | TCP listen port |
| `enable_dht` | true | Kademlia DHT peer discovery |
| `enable_utp` | true | uTP (BEP 29) transport |
| `enable_upnp` | true | UPnP IGD port mapping |
| `enable_natpmp` | true | NAT-PMP / PCP port mapping |
| `encryption_mode` | Enabled | MSE/PE (Disabled/Enabled/Forced) |
| `peer_dscp` | 0x08 | DSCP value for peer traffic (CS1) |

### Security

| Setting | Default | Description |
|---------|---------|-------------|
| `ssrf_mitigation` | true | Block SSRF in tracker/web seed URLs |
| `allow_idna` | false | Allow non-ASCII domain names |
| `validate_https_trackers` | true | Enforce TLS certificate validation |
| `anonymous_mode` | false | Suppress client identity |

## JSON Configuration

Settings serialize to JSON for file-based configuration:

```rust
let settings = Settings::default();
let json = serde_json::to_string_pretty(&settings)?;

// Load from file
let settings: Settings = serde_json::from_str(&json)?;
settings.validate()?;
```

## Runtime Updates

```rust
let mut settings = session.settings().await?;
settings.download_rate_limit = 1_000_000; // 1 MB/s
session.apply_settings(settings).await?;
```

Runtime updates immediately affect rate limiters and alert mask. Sub-actor configurations (DHT, NAT) take effect on next restart.
