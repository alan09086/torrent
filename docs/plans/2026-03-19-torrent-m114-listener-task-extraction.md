# M114: Listener Task Extraction + Handshake Batching

## Context

After M110 (v0.110.0, 1647 tests), benchmarks show 148K heap allocations per download.
The #1 allocator is `TokioListener::accept` at 31,453 calls. The root cause is
architectural: the TCP listener is polled inside the SessionActor's `select!` loop.
Every time ANY select arm fires (every block received, every timer tick, every peer
event), tokio polls ALL arms including the listener. Each poll of `accept()` allocates
internally. With ~90K block receives per 1.4 GB download, the listener is polled
90K+ times but only accepts a handful of connections.

**M110 benchmark results:**

| Metric       | M110    | rqbit  | Target  |
|------------- |---------|--------|---------|
| Speed        | 58.8 MB/s | 74.6 MB/s | >= 65 |
| RSS          | 31 MiB  | --     | < 35    |
| Page faults  | 7,990   | 7K     | < 10K   |
| Heap allocs  | 148K    | ~4K    | < 50K   |

**Allocation breakdown (heaptrack):**

| Source                       | Calls  |
|------------------------------|--------|
| TokioListener::accept poll   | 31,453 |
| run_peer spawning            | 26,000 |
| Vec::finish_grow             | 18,000 |
| InfoDict::files per-piece    | 10,000 |
| String::clone                | 4,000  |
| bencode parsing              | 4,000  |
| misc                         | 55,000 |

rqbit solves this by running the listener as a separate spawned task that only wakes
on actual connections, never on unrelated events.

## Architecture

### Current: Listener Inside SessionActor select!

```
SessionActor::run() {
    loop { tokio::select! {
        result = tcp_listener.accept()       => handle_tcp_inbound(stream, addr)
        result = utp_listener.accept()       => handle_utp_inbound(stream, addr)
        result = utp_listener_v6.accept()    => handle_utp_inbound(stream, addr)
        result = ssl_listener.accept()       => handle_ssl_incoming(stream, addr)
        event  = torrent_event_rx.recv()     => ...
        cmd    = session_cmd_rx.recv()       => ...
        _      = refill_interval.tick()      => ...  // 100ms
        _      = auto_manage_interval.tick() => ...
        event  = nat_events_rx.recv()        => ...
        result = lsd_peers_rx.recv()         => ...
    }}
}
```

Every poll cycle re-polls ALL arms. The listener arms allocate on each poll even when
no connection is pending. With 10+ select arms, every event (block completions,
timer ticks) triggers unnecessary listener allocations.

### Proposed: Separate ListenerTask with FuturesUnordered

```
ListenerTask::run() {                          // Separate spawned task
    let mut futs = FuturesUnordered::new();
    loop { tokio::select! {
        Ok((stream, addr)) = tcp_listener.accept() => {
            futs.push(validate_handshake(stream, addr, info_hash_registry))
        }
        Ok((stream, addr)) = utp_listener.accept() => {
            futs.push(validate_handshake(stream, addr, info_hash_registry))
        }
        Some(Ok(validated)) = futs.next(), if !futs.is_empty() => {
            validated_conn_tx.send(validated)    // Send to SessionActor
        }
    }}
}

SessionActor::run() {
    loop { tokio::select! {
        validated = validated_conn_rx.recv()  => handle_validated_inbound(validated)
        // ... other arms unchanged, but NO listener arms
    }}
}
```

Only 2-3 select arms in ListenerTask (accept + futs + shutdown). The SessionActor
loses 4 arms and gains 1 channel receive arm. Listener polling is isolated from
torrent events.

## rqbit Reference

### `crates/librqbit/src/session.rs` lines 892-931: `task_listener`

```rust
async fn task_listener<A: Accept>(self: Arc<Self>, l: A) -> anyhow::Result<()> {
    let mut futs = FuturesUnordered::new();
    let session = Arc::downgrade(&self);
    drop(self);

    loop {
        tokio::select! {
            r = l.accept() => {
                match r {
                    Ok((addr, (read, write))) => {
                        let session = session.upgrade().context("session is dead")?;
                        futs.push(
                            session.check_incoming_connection(addr, A::KIND, Box::new(read), Box::new(write))
                        );
                    }
                    Err(e) => {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        continue
                    }
                }
            },
            Some(Ok((live, checked))) = futs.next(), if !futs.is_empty() => {
                if let Err(e) = live.add_incoming_peer(checked) {
                    warn!("error handing over incoming connection");
                }
            },
        }
    }
}
```

Key patterns:
- Separate spawned task per listener type (TCP, uTP)
- `FuturesUnordered` for concurrent handshake validation
- Weak reference to session (graceful shutdown)
- Only hands over fully validated connections
- Accept errors trigger 10s backoff (not crash)

### `crates/librqbit/src/listen.rs`: `Accept` trait

Abstraction over TCP and uTP listeners with `const KIND: ConnectionKind` for
connection type tracking.

## Changes

### 1. ValidatedConnection type (`torrent-session/src/listener.rs`, ~20 lines)

```rust
/// A connection that has completed handshake validation.
pub(crate) struct ValidatedConnection {
    pub stream: Box<dyn AsyncRead + AsyncWrite + Unpin + Send>,
    pub addr: SocketAddr,
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub handshake: Handshake,
    pub transport: TransportKind,  // Tcp, Utp, Ssl
}

pub(crate) enum TransportKind {
    Tcp,
    Utp,
    Ssl,
}
```

### 2. ListenerTask struct (`torrent-session/src/listener.rs`, ~180 lines)

```rust
pub(crate) struct ListenerTask {
    /// All listeners (TCP, uTP IPv4, uTP IPv6, SSL).
    tcp_listener: Option<Box<dyn TransportListener>>,
    utp_listener: Option<torrent_utp::UtpListener>,
    utp_listener_v6: Option<torrent_utp::UtpListener>,
    ssl_listener: Option<Box<dyn TransportListener>>,

    /// Registry of active info hashes for handshake validation.
    info_hash_registry: Arc<DashMap<Id20, ()>>,

    /// Sends validated connections to the SessionActor.
    validated_tx: mpsc::Sender<ValidatedConnection>,

    /// Shutdown signal.
    shutdown: CancellationToken,
}

impl ListenerTask {
    pub fn new(
        tcp_listener: Option<Box<dyn TransportListener>>,
        utp_listener: Option<torrent_utp::UtpListener>,
        utp_listener_v6: Option<torrent_utp::UtpListener>,
        ssl_listener: Option<Box<dyn TransportListener>>,
        info_hash_registry: Arc<DashMap<Id20, ()>>,
        validated_tx: mpsc::Sender<ValidatedConnection>,
        shutdown: CancellationToken,
    ) -> Self { ... }

    pub async fn run(mut self) {
        let mut futs = FuturesUnordered::new();
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,

                result = accept_tcp(&mut self.tcp_listener) => {
                    if let Ok((stream, addr)) = result {
                        let registry = self.info_hash_registry.clone();
                        futs.push(Self::validate_handshake(stream, addr, registry, TransportKind::Tcp));
                    }
                }
                result = accept_utp(&mut self.utp_listener) => {
                    if let Ok((stream, addr)) = result {
                        let registry = self.info_hash_registry.clone();
                        futs.push(Self::validate_handshake(stream, addr, registry, TransportKind::Utp));
                    }
                }
                result = accept_utp(&mut self.utp_listener_v6) => {
                    if let Ok((stream, addr)) = result {
                        let registry = self.info_hash_registry.clone();
                        futs.push(Self::validate_handshake(stream, addr, registry, TransportKind::Utp));
                    }
                }
                result = accept_ssl(&mut self.ssl_listener) => {
                    if let Ok((stream, addr)) = result {
                        let registry = self.info_hash_registry.clone();
                        futs.push(Self::validate_handshake(stream, addr, registry, TransportKind::Ssl));
                    }
                }
                Some(Ok(validated)) = futs.next(), if !futs.is_empty() => {
                    if self.validated_tx.send(validated).await.is_err() {
                        break; // SessionActor dropped
                    }
                }
            }
        }
    }
}
```

### 3. Handshake validation in ListenerTask (~80 lines)

```rust
impl ListenerTask {
    /// Validate the BEP 3 handshake within a 5s timeout.
    /// Returns None (logged) on failure, Some(ValidatedConnection) on success.
    async fn validate_handshake(
        mut stream: Box<dyn AsyncRead + AsyncWrite + Unpin + Send>,
        addr: SocketAddr,
        registry: Arc<DashMap<Id20, ()>>,
        transport: TransportKind,
    ) -> Result<ValidatedConnection, ()> {
        // 5s timeout for the entire handshake exchange
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            // Read 68-byte handshake
            let mut hs_buf = [0u8; 68];
            stream.read_exact(&mut hs_buf).await?;
            let handshake = Handshake::from_bytes(&hs_buf)?;

            // Validate info hash against registry
            if !registry.contains_key(&handshake.info_hash) {
                return Err("unknown info hash");
            }

            Ok((handshake, hs_buf))
        }).await;

        match result {
            Ok(Ok((handshake, _))) => Ok(ValidatedConnection {
                stream,
                addr,
                info_hash: handshake.info_hash,
                peer_id: handshake.peer_id,
                handshake,
                transport,
            }),
            Ok(Err(e)) => {
                debug!(%addr, "handshake validation failed: {e}");
                Err(())
            }
            Err(_) => {
                debug!(%addr, "handshake timed out");
                Err(())
            }
        }
    }
}
```

### 4. Info hash registry (`torrent-session/src/session.rs`, ~30 lines)

```rust
// In SessionActor fields:
info_hash_registry: Arc<DashMap<Id20, ()>>,

// In SessionActor::start():
let info_hash_registry = Arc::new(DashMap::new());

// Register on torrent add:
self.info_hash_registry.insert(info_hash, ());

// Remove on torrent remove:
self.info_hash_registry.remove(&info_hash);
```

### 5. SessionActor changes (`torrent-session/src/session.rs`, ~-60/+30 lines)

Remove from SessionActor fields:
- `tcp_listener: Option<Box<dyn TransportListener>>`
- `ssl_listener: Option<Box<dyn TransportListener>>`
- `utp_listener: Option<torrent_utp::UtpListener>`
- `utp_listener_v6: Option<torrent_utp::UtpListener>`

Add:
- `validated_conn_rx: mpsc::Receiver<ValidatedConnection>`
- `listener_task: JoinHandle<()>`
- `info_hash_registry: Arc<DashMap<Id20, ()>>`

Remove from select! loop:
- All 4 listener accept arms (TCP, uTP v4, uTP v6, SSL)
- `handle_tcp_inbound()`, `handle_utp_inbound()`, `handle_ssl_incoming()` methods
- `accept_utp()` helper function

Add to select! loop:
```rust
validated = self.validated_conn_rx.recv() => {
    if let Some(conn) = validated {
        self.handle_validated_inbound(conn);
    }
}
```

New method:
```rust
fn handle_validated_inbound(&mut self, conn: ValidatedConnection) {
    // Look up the torrent by info_hash
    if let Some(entry) = self.torrents.get(&conn.info_hash) {
        // Forward the validated connection to the TorrentActor
        let _ = entry.handle.add_incoming_peer(conn);
    }
}
```

### 6. Spawn ListenerTask at session start (~15 lines)

In `SessionActor::start()`:
```rust
let (validated_tx, validated_rx) = mpsc::channel(64);
let listener_task = ListenerTask::new(
    tcp_listener, utp_listener, utp_listener_v6, ssl_listener,
    info_hash_registry.clone(),
    validated_tx,
    shutdown_token.clone(),
);
tokio::spawn(listener_task.run());
```

## Tests

### New Tests (8)

1. **`listener_accepts_valid_handshake`** — Connect to ListenerTask with valid
   handshake, verify ValidatedConnection is received on channel.

2. **`listener_rejects_invalid_protocol`** — Connect with wrong protocol string
   (not "\x13BitTorrent protocol"), verify connection is dropped, no
   ValidatedConnection sent.

3. **`listener_rejects_unknown_info_hash`** — Connect with valid protocol but
   info hash not in registry. Verify connection dropped.

4. **`listener_timeout_on_slow_handshake`** — Connect but don't send handshake
   bytes. Verify 5s timeout fires, connection dropped.

5. **`listener_concurrent_handshakes`** — Start 10 connections simultaneously,
   some valid, some invalid. Verify FuturesUnordered processes them concurrently
   and only valid ones arrive on channel.

6. **`listener_futures_unordered_ordering`** — Connect 3 peers: first sends
   handshake slowly, second immediately, third immediately. Verify second and
   third arrive before first (FuturesUnordered is not FIFO).

7. **`listener_channel_backpressure`** — Fill the validated_tx channel (capacity 64),
   verify ListenerTask blocks on send but doesn't drop connections.

8. **`listener_shutdown_graceful`** — Cancel shutdown token. Verify ListenerTask
   exits, pending handshakes are dropped, channel is closed.

### Existing Tests

All 1647+ existing tests must pass. The SessionActor's handling of inbound
connections is unchanged from the TorrentActor's perspective — it still receives
them via the same internal pathways.

## Success Criteria

- TokioListener::accept heap allocations < 1K (was 31,453)
- Total heap allocations < 120K (was 148K)
- All 1647+ existing tests pass
- 8 new listener tests pass
- No regression in download speed (>= 55 MB/s)
- No regression in connection success rate

## Verification

```bash
# Run all tests
cargo test --workspace

# Run new listener tests
cargo test -p torrent-session listener_

# Benchmark (10 trials)
./benchmarks/analyze.sh '<arch-iso-magnet>' -n 10

# Verify alloc reduction with heaptrack
heaptrack --record-only ./target/release/torrent-cli download '<arch-iso-magnet>'
heaptrack_print heaptrack.torrent-cli.*.zst | grep -i "accept\|listener"
```

## NOT in Scope

| Item | Why | Revisit |
|------|-----|---------|
| MSE/PE validation in ListenerTask | Encryption negotiation is torrent-specific (info-hash dependent), handled after handshake in `run_peer()` | If handshake latency is a bottleneck |
| Connection rate limiting | Existing `peer_semaphore` already limits concurrent peers | If DDoS becomes a concern |
| Per-torrent listener tasks | One global ListenerTask handles all torrents; rqbit does the same | Never (unnecessary complexity) |
| uTP listener extraction to separate crate | Already abstracted via `UtpListener` | If uTP needs independent lifecycle |
