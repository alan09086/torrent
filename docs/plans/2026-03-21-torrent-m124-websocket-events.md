# M124: WebSocket Event Stream

## Context

M123 ships the HTTP REST API via axum. Clients can poll `GET /torrents/:hash` for
stats, but real-time updates require push. The session already has a rich alert system
(`AlertStream` with 60+ `AlertKind` variants and category filtering via `AlertCategory`
bitflags). This milestone bridges alerts to WebSocket consumers.

### Existing Alert Infrastructure

- `SessionHandle::subscribe()` → `broadcast::Receiver<Alert>` (all alerts)
- `SessionHandle::subscribe_filtered(mask)` → `AlertStream` (category-filtered)
- `AlertCategory` bitflags: STATUS, ERROR, PEER, TRACKER, STORAGE, DHT, STATS, PIECE, BLOCK, PERFORMANCE, PORT_MAPPING, I2P
- `Alert { timestamp: SystemTime, kind: AlertKind }` — both derive `Serialize`
- `AlertKind` has 60+ variants covering every session event

## Design

### WebSocket Endpoint

```
WS /api/v1/events?mask=<hex>&interval=<ms>
```

**Parameters:**
- `mask` — `AlertCategory` hex bitmask (default: `0xFFF` = ALL). Example: `0x0C1` = STATUS | STATS | PIECE
- `interval` — optional stats heartbeat interval in ms (default: 1000, 0 = disabled)

### Message Format

Server → Client (JSON):
```json
{
  "type": "alert",
  "timestamp": "2026-03-21T19:30:00Z",
  "category": "STATUS",
  "kind": "TorrentFinished",
  "data": { "info_hash": "abcd1234..." }
}
```

Stats heartbeat (when `interval > 0`):
```json
{
  "type": "stats",
  "timestamp": "2026-03-21T19:30:01Z",
  "counters": { "bytes_recv": 1234567, "download_rate": 65000000, ... }
}
```

Client → Server:
```json
// Update mask at runtime
{ "type": "set_mask", "mask": "0x041" }

// Request immediate stats snapshot
{ "type": "get_stats" }
```

### Implementation

```rust
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(session): State<SessionHandle>,
    Query(params): Query<WsParams>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, session, params))
}

async fn handle_ws(mut socket: WebSocket, session: SessionHandle, params: WsParams) {
    let mask = AtomicU32::new(params.mask);
    let mut alerts = session.subscribe();
    let mut heartbeat = tokio::time::interval(Duration::from_millis(params.interval));

    loop {
        tokio::select! {
            // Alert from session
            Ok(alert) = alerts.recv() => {
                if alert.kind.category().intersects(AlertCategory::from_bits_truncate(mask.load(Relaxed))) {
                    let msg = serde_json::to_string(&WsMessage::Alert(alert)).unwrap();
                    if socket.send(Message::Text(msg)).await.is_err() { break; }
                }
            }
            // Stats heartbeat
            _ = heartbeat.tick(), if params.interval > 0 => {
                let counters = session.counters().snapshot();
                let msg = serde_json::to_string(&WsMessage::Stats(counters)).unwrap();
                if socket.send(Message::Text(msg)).await.is_err() { break; }
            }
            // Client message
            Some(Ok(msg)) = socket.recv() => {
                if let Message::Text(text) = msg {
                    if let Ok(cmd) = serde_json::from_str::<WsCommand>(&text) {
                        match cmd {
                            WsCommand::SetMask { mask: m } => mask.store(m, Relaxed),
                            WsCommand::GetStats => {
                                let counters = session.counters().snapshot();
                                let msg = serde_json::to_string(&WsMessage::Stats(counters)).unwrap();
                                let _ = socket.send(Message::Text(msg)).await;
                            }
                        }
                    }
                }
            }
            else => break,
        }
    }
}
```

### Alert Serialization

`AlertKind` already derives `Serialize`. The 60+ variants serialize as:
```json
{ "TorrentFinished": { "info_hash": "abcd..." } }
// or for unit variants:
{ "DhtBootstrapComplete": null }
```

We wrap this in the `WsMessage` envelope for consistency.

## Files Modified

| File | Change |
|------|--------|
| `crates/torrent-api/src/handlers/events.rs` | **NEW** — WebSocket handler |
| `crates/torrent-api/src/routes.rs` | Add WS route |
| `crates/torrent-api/Cargo.toml` | Add `tokio-tungstenite` or use axum's built-in WS |

## Task Breakdown

### Task 1: WsMessage + WsCommand types
Serializable message envelope, command parsing.

### Task 2: WebSocket handler
Alert subscription, heartbeat timer, mask filtering, select! loop.

### Task 3: Route integration + client commands
Wire into axum Router. Handle set_mask and get_stats commands.

### Task 4: Tests (6-8 new)
Connect via WS, verify alert delivery, mask filtering, heartbeat interval, runtime mask update.

## Success Criteria

- Alerts stream in real-time over WebSocket
- Category mask filtering works (connect-time and runtime)
- Stats heartbeat fires at configured interval
- Client can update mask without reconnecting
- 6-8 new tests pass
