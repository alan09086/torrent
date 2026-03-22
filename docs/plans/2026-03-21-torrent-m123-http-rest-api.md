# M123: HTTP REST API

## Context

M121 shipped the `SessionHandle` API surface with `list_torrent_summaries()`,
`add_magnet_uri()`, `add_torrent_bytes()`, and full `TorrentStats` with `Serialize`.
M122 completed the io_uring backend. The next step is exposing session management
over HTTP so GUI/TUI clients can control the engine remotely.

The `SessionHandle` already has 40+ async methods covering torrent CRUD, stats, queue
management, peer banning, IP filtering, and settings mutation. All response types
(`TorrentStats`, `TorrentSummary`, `SessionStats`, `Settings`) derive `Serialize`.
This milestone is a thin HTTP layer — no business logic, just routing.

### Framework Choice: axum

- **axum** — built on tokio + hyper + tower, zero-cost extractors, native async
- Alternatives considered: `warp` (macro-heavy), `actix-web` (separate runtime)
- axum shares the tokio runtime with the session — no additional threads needed
- ~15K downloads/day, maintained by the tokio team

## Design

### New Crate: `torrent-api`

A new crate in the workspace, depending on `torrent` (facade) and `axum`.
Keeps HTTP concerns out of the core library. The CLI enables it via a feature flag.

```
crates/torrent-api/
  Cargo.toml
  src/
    lib.rs          — ApiServer, start/shutdown
    routes.rs       — Router construction
    handlers/
      torrents.rs   — CRUD + stats + actions
      session.rs    — session stats, settings, metrics
      peers.rs      — ban/unban, IP filter
      files.rs      — file priority, streaming
```

### API Surface

All endpoints under `/api/v1/`. JSON request/response bodies.

#### Torrent Management

| Method | Path | Handler | SessionHandle Method |
|--------|------|---------|---------------------|
| GET | `/torrents` | `list_torrents` | `list_torrent_summaries()` → `Vec<TorrentSummary>` |
| GET | `/torrents/:hash` | `get_torrent` | `torrent_stats(hash)` → `TorrentStats` |
| GET | `/torrents/:hash/info` | `get_torrent_info` | `torrent_info(hash)` → `TorrentInfo` |
| POST | `/torrents` | `add_torrent` | `add_magnet_uri(uri)` or `add_torrent_bytes(bytes)` |
| DELETE | `/torrents/:hash` | `remove_torrent` | `remove_torrent(hash)` |
| POST | `/torrents/:hash/pause` | `pause_torrent` | `pause_torrent(hash)` |
| POST | `/torrents/:hash/resume` | `resume_torrent` | `resume_torrent(hash)` |
| POST | `/torrents/:hash/reannounce` | `force_reannounce` | `force_reannounce(hash)` |
| GET | `/torrents/:hash/trackers` | `list_trackers` | `tracker_list(hash)` |
| GET | `/torrents/:hash/peers` | `list_peers` | via `torrent_stats()` peer fields |

#### File Operations

| Method | Path | Handler | SessionHandle Method |
|--------|------|---------|---------------------|
| GET | `/torrents/:hash/files` | `list_files` | `torrent_info(hash).files` |
| PATCH | `/torrents/:hash/files/:idx/priority` | `set_priority` | `set_file_priority(hash, idx, pri)` |
| GET | `/torrents/:hash/files/:idx/stream` | `stream_file` | `open_file(hash, idx)` → AsyncRead |

#### Session

| Method | Path | Handler | SessionHandle Method |
|--------|------|---------|---------------------|
| GET | `/session/stats` | `session_stats` | `session_stats()` |
| GET | `/session/counters` | `session_counters` | `counters().snapshot()` |
| GET | `/session/settings` | `get_settings` | `settings()` |
| PATCH | `/session/settings` | `update_settings` | `apply_settings(partial)` |
| POST | `/session/shutdown` | `shutdown` | `shutdown()` |

#### Peer Management

| Method | Path | Handler | SessionHandle Method |
|--------|------|---------|---------------------|
| GET | `/peers/banned` | `list_banned` | `banned_peers()` |
| POST | `/peers/ban` | `ban_peer` | `ban_peer(ip)` |
| DELETE | `/peers/ban/:ip` | `unban_peer` | `unban_peer(ip)` |

### Request/Response Format

**Add torrent** — multipart or JSON:
```json
// Magnet
{ "magnet": "magnet:?xt=urn:btih:..." }

// .torrent file (base64-encoded)
{ "torrent": "base64..." }
```

**Error responses** — consistent structure:
```json
{ "error": "torrent not registered", "code": "NOT_FOUND" }
```

**HTTP status codes**: 200 OK, 201 Created (add), 204 No Content (delete/pause/resume),
400 Bad Request, 404 Not Found, 500 Internal Server Error.

### Info Hash Parsing

The `:hash` path parameter accepts:
- 40-char hex SHA-1 (v1 info hash)
- 64-char hex SHA-256 (v2 info hash)

Parsed via `Id20::from_hex()` / `Id32::from_hex()` in an axum extractor.

### ApiServer

```rust
pub struct ApiServer {
    session: SessionHandle,
    shutdown_tx: watch::Sender<()>,
}

impl ApiServer {
    pub fn new(session: SessionHandle) -> Self;

    /// Start the HTTP server on the given address.
    /// Returns a JoinHandle — the server runs until shutdown.
    pub async fn start(self, addr: SocketAddr) -> tokio::task::JoinHandle<()>;
}
```

The `SessionHandle` is passed as axum `State` to all handlers.

### CLI Integration

New flags on the Download command:
- `--api-port PORT` — enable HTTP API on this port (0 = disabled, default)
- `--api-bind ADDR` — bind address (default: 127.0.0.1)

When `api_port > 0`, the CLI spawns `ApiServer` alongside the download.

### Settings Integration

New fields in `Settings`:
- `api_port: u16` (default: 0 = disabled)
- `api_bind_address: String` (default: "127.0.0.1")

## Files Modified

| File | Change |
|------|--------|
| `crates/torrent-api/Cargo.toml` | **NEW** — axum, serde_json, tokio deps |
| `crates/torrent-api/src/lib.rs` | **NEW** — ApiServer |
| `crates/torrent-api/src/routes.rs` | **NEW** — Router construction |
| `crates/torrent-api/src/handlers/torrents.rs` | **NEW** — torrent CRUD handlers |
| `crates/torrent-api/src/handlers/session.rs` | **NEW** — session stats/settings |
| `crates/torrent-api/src/handlers/peers.rs` | **NEW** — peer ban management |
| `crates/torrent-api/src/handlers/files.rs` | **NEW** — file priority + streaming |
| `crates/torrent-cli/Cargo.toml` | Add torrent-api dependency |
| `crates/torrent-cli/src/main.rs` | Add --api-port, --api-bind flags |
| `crates/torrent-session/src/settings.rs` | Add api_port, api_bind_address |
| `Cargo.toml` | Add torrent-api to workspace members |

## Task Breakdown

### Task 1: Crate scaffold + Router
Create `torrent-api` crate, axum Router with all routes (handlers return `todo!()`).

### Task 2: Torrent CRUD handlers
list, get, add (magnet + .torrent bytes), delete, pause, resume, reannounce.

### Task 3: Session + file handlers
Session stats, counters, settings GET/PATCH, shutdown. File list, priority, stream.

### Task 4: Peer management + error handling
Ban/unban/list. Consistent error response type. Info hash extractor.

### Task 5: CLI + settings integration
`--api-port`, `--api-bind` flags. Spawn ApiServer in download flow.

### Task 6: Tests (10-12 new)
Integration tests using axum's `TestClient`. Test each endpoint group.

## Success Criteria

- All endpoints return correct JSON for happy path
- Error responses are consistent
- `--api-port 8080` starts the API alongside download
- 10-12 new tests pass
- Zero regression on existing tests
