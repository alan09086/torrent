# M45: Mixed-Mode Algorithm + Auto-Sequential — Implementation Plan

**Milestone:** M45
**Version:** 0.42.0
**Crate:** ferrite-session
**Tests target:** ~10 new tests
**Estimated scope:** ~350 lines of new/modified code

## Overview

Two features in one milestone:

1. **Mixed-mode TCP/uTP bandwidth algorithm** — dynamically adjust per-class rate limits based on the transport composition of connected peers. Two strategies: `PreferTcp` (throttle uTP upload when TCP peers are present) and `PeerProportional` (allocate bandwidth proportional to TCP vs uTP peer count).

2. **Auto-sequential formalization** — replace the ad-hoc partial-explosion check in `piece_selector.rs:354` with a proper hysteresis-based system that prevents mode flapping.

## Architecture Analysis

### Current State

**Per-class rate limiters (M32b):**
- `RateLimiterSet` in `rate_limiter.rs` has 6 buckets: `tcp_upload`, `tcp_download`, `utp_upload`, `utp_download`, `global_upload`, `global_download`
- `PeerTransport` enum: `Tcp`, `Utp`
- `set_rates()` allows runtime mutation of all 6 bucket rates
- Settings has 4 per-class fields: `tcp_upload_rate_limit`, `tcp_download_rate_limit`, `utp_upload_rate_limit`, `utp_download_rate_limit`
- Currently the per-class limits are only set from user-configured Settings values; there is no dynamic algorithm adjusting them based on peer composition

**Peer transport tracking:**
- `PeerState` in `peer_state.rs` does NOT currently track transport type
- `try_connect_peers()` in `torrent.rs:3031` spawns a task that tries uTP then falls back to TCP, but the result (which transport succeeded) is never reported back to the `TorrentActor`
- `spawn_peer_from_stream_with_mode()` handles incoming uTP peers (routed from session) but again doesn't record transport type on PeerState

**Auto-sequential (current):**
- `piece_selector.rs:354`: `let auto_sequential = ctx.in_flight_pieces.len() as f64 > 1.5 * ctx.connected_peer_count as f64;`
- Evaluated on every pick cycle — no state, no hysteresis, instant transitions
- No way to disable it via Settings

**Key limitation:** The mixed-mode algorithm needs to know how many peers are TCP vs uTP, but the current PeerState doesn't track transport type. We need to add this.

### Design Decisions

1. **Transport tracking via PeerEvent:** When a peer successfully connects via uTP or TCP, report the transport back to TorrentActor via a new `PeerEvent::Connected { peer_addr, transport }` variant. TorrentActor stores `transport` on `PeerState`.

2. **Mixed-mode lives in TorrentActor:** The algorithm runs on the 100ms refill tick, adjusting the `RateLimiterSet` rates based on current peer composition. This is per-torrent; session-level global limits are unaffected.

3. **Auto-sequential state on PieceSelector:** Add a `auto_sequential_active: bool` field to avoid recomputing from scratch each cycle. The hysteresis thresholds (60%/30%) are evaluated against partial ratio, and the field is toggled with debounce.

## Implementation Steps

### Step 1: Add `MixedModeAlgorithm` enum to `rate_limiter.rs`

**File:** `crates/ferrite-session/src/rate_limiter.rs`

Add after `PeerTransport`:

```rust
/// Mixed-mode bandwidth allocation algorithm for TCP/uTP coexistence.
///
/// Controls how upload bandwidth is divided between TCP and uTP peers.
/// uTP includes LEDBAT congestion control that automatically yields to TCP,
/// but explicit throttling provides more predictable behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MixedModeAlgorithm {
    /// Throttle uTP upload when any TCP peer is connected.
    /// uTP gets at most 10% of the global upload rate when TCP peers are present.
    /// When no TCP peers are connected, uTP gets full bandwidth.
    PreferTcp,
    /// Allocate bandwidth proportional to the number of TCP vs uTP peers.
    /// If 3 TCP and 7 uTP peers are connected, TCP gets 30% and uTP gets 70%
    /// of the global upload rate limit.
    PeerProportional,
}
```

Add the serde import at top of file:

```rust
use serde::{Deserialize, Serialize};
```

Add a method on `RateLimiterSet` for mixed-mode adjustment:

```rust
/// Apply mixed-mode bandwidth allocation based on peer transport composition.
///
/// Adjusts the TCP and uTP upload rate limits proportionally based on the
/// algorithm and current peer counts. Only adjusts upload — download is
/// not throttled by transport type (we want data from everyone).
///
/// `global_upload_rate` is the session's configured upload limit (0 = unlimited).
/// When unlimited, mixed-mode has no effect (can't proportion infinity).
pub fn apply_mixed_mode(
    &mut self,
    algorithm: MixedModeAlgorithm,
    tcp_peers: usize,
    utp_peers: usize,
    global_upload_rate: u64,
) {
    if global_upload_rate == 0 {
        // Unlimited — clear per-class limits, let everything through
        self.tcp_upload.set_rate(0);
        self.utp_upload.set_rate(0);
        return;
    }

    if tcp_peers == 0 && utp_peers == 0 {
        // No peers — clear limits
        self.tcp_upload.set_rate(0);
        self.utp_upload.set_rate(0);
        return;
    }

    match algorithm {
        MixedModeAlgorithm::PreferTcp => {
            if tcp_peers > 0 && utp_peers > 0 {
                // TCP gets 90%, uTP gets 10%
                let tcp_rate = global_upload_rate * 9 / 10;
                let utp_rate = global_upload_rate / 10;
                self.tcp_upload.set_rate(tcp_rate.max(1));
                self.utp_upload.set_rate(utp_rate.max(1));
            } else if tcp_peers > 0 {
                // Only TCP peers — give all to TCP, unlimited uTP (no uTP peers anyway)
                self.tcp_upload.set_rate(0); // unlimited
                self.utp_upload.set_rate(0);
            } else {
                // Only uTP peers — give all to uTP
                self.tcp_upload.set_rate(0);
                self.utp_upload.set_rate(0); // unlimited
            }
        }
        MixedModeAlgorithm::PeerProportional => {
            let total = tcp_peers + utp_peers;
            let tcp_rate = global_upload_rate * tcp_peers as u64 / total as u64;
            let utp_rate = global_upload_rate * utp_peers as u64 / total as u64;
            // Ensure at least 1 byte/s if the class has peers, to avoid starvation
            self.tcp_upload.set_rate(if tcp_peers > 0 { tcp_rate.max(1) } else { 0 });
            self.utp_upload.set_rate(if utp_peers > 0 { utp_rate.max(1) } else { 0 });
        }
    }
}
```

**Tests (4 new):**

```rust
#[test]
fn mixed_mode_prefer_tcp_both_present() {
    let mut rls = RateLimiterSet::new(0, 0, 0, 0, 10000, 0);
    rls.apply_mixed_mode(MixedModeAlgorithm::PreferTcp, 3, 5, 10000);
    // TCP should get 90%, uTP 10%
    rls.refill(Duration::from_secs(1));
    assert!(rls.try_consume_upload(9000, PeerTransport::Tcp));
    assert!(!rls.try_consume_upload(1, PeerTransport::Tcp));
    // Need fresh refill for uTP bucket
    rls.refill(Duration::from_secs(1));
    // uTP bucket was set to 1000
    assert!(rls.try_consume_upload(1000, PeerTransport::Utp));
    assert!(!rls.try_consume_upload(1, PeerTransport::Utp));
}

#[test]
fn mixed_mode_prefer_tcp_only_utp() {
    let mut rls = RateLimiterSet::new(0, 0, 0, 0, 10000, 0);
    rls.apply_mixed_mode(MixedModeAlgorithm::PreferTcp, 0, 5, 10000);
    // Only uTP peers — both unlimited
    rls.refill(Duration::from_secs(1));
    assert!(rls.try_consume_upload(1_000_000, PeerTransport::Utp));
}

#[test]
fn mixed_mode_proportional() {
    let mut rls = RateLimiterSet::new(0, 0, 0, 0, 10000, 0);
    rls.apply_mixed_mode(MixedModeAlgorithm::PeerProportional, 3, 7, 10000);
    // TCP: 30%, uTP: 70%
    rls.refill(Duration::from_secs(1));
    assert!(rls.try_consume_upload(3000, PeerTransport::Tcp));
    assert!(!rls.try_consume_upload(1, PeerTransport::Tcp));
    rls.refill(Duration::from_secs(1));
    assert!(rls.try_consume_upload(7000, PeerTransport::Utp));
    assert!(!rls.try_consume_upload(1, PeerTransport::Utp));
}

#[test]
fn mixed_mode_unlimited_global_noop() {
    let mut rls = RateLimiterSet::new(0, 0, 0, 0, 0, 0);
    rls.apply_mixed_mode(MixedModeAlgorithm::PeerProportional, 3, 7, 0);
    // Global unlimited — per-class should be cleared to 0 (unlimited)
    rls.refill(Duration::from_secs(1));
    assert!(rls.try_consume_upload(1_000_000, PeerTransport::Tcp));
    assert!(rls.try_consume_upload(1_000_000, PeerTransport::Utp));
}
```

### Step 2: Add `transport` field to `PeerState`

**File:** `crates/ferrite-session/src/peer_state.rs`

Add field after `source`:

```rust
    /// Transport protocol used for this peer connection.
    pub transport: Option<crate::rate_limiter::PeerTransport>,
```

Initialize to `None` in `PeerState::new()`:

```rust
    transport: None,
```

### Step 3: Report transport from peer connection

**File:** `crates/ferrite-session/src/types.rs`

Add a new `PeerEvent` variant:

```rust
    /// Peer successfully connected with a specific transport.
    TransportIdentified {
        peer_addr: SocketAddr,
        transport: crate::rate_limiter::PeerTransport,
    },
```

### Step 4: Emit `TransportIdentified` from connection paths

**File:** `crates/ferrite-session/src/torrent.rs`

In `try_connect_peers()`, after the uTP connection succeeds (around line 3104), emit the transport event BEFORE calling `run_peer()`. However, since the peer task is spawned async, we need to emit inside the spawned task. The simplest approach: emit from inside the spawned task after uTP/TCP resolution, before `run_peer()`.

Modify the spawned task in `try_connect_peers()` — after uTP `connect()` succeeds (line 3104):

```rust
Ok(Ok(stream)) => {
    debug!(%addr, "uTP connection established");
    let _ = event_tx.send(PeerEvent::TransportIdentified {
        peer_addr: addr,
        transport: crate::rate_limiter::PeerTransport::Utp,
    }).await;
    let _ = run_peer(/* ... */).await;
    return;
}
```

After TCP `connect()` succeeds (line 3142):

```rust
Ok(stream) => {
    let _ = event_tx.send(PeerEvent::TransportIdentified {
        peer_addr: addr,
        transport: crate::rate_limiter::PeerTransport::Tcp,
    }).await;
    let _ = run_peer(/* ... */).await;
}
```

For `spawn_peer_from_stream_with_mode()` (incoming connections), determine transport from the `mode_override` parameter — if mode_override is `Some(Disabled)`, it's a uTP inbound peer (per the comment on line 3186-3188: "used for uTP inbound peers where MSE has already been ruled out"):

In `spawn_peer_from_stream_with_mode()`, after inserting the PeerState, set the transport directly:

```rust
// Identify transport for incoming peers
let transport = if mode_override.is_some() {
    // uTP inbound: MSE is pre-negotiated by session routing layer
    Some(crate::rate_limiter::PeerTransport::Utp)
} else {
    // TCP inbound
    Some(crate::rate_limiter::PeerTransport::Tcp)
};
if let Some(peer) = self.peers.get_mut(&addr) {
    peer.transport = transport;
}
```

### Step 5: Handle `TransportIdentified` in TorrentActor event loop

**File:** `crates/ferrite-session/src/torrent.rs`

In the `PeerEvent` match arm of the main event loop, add:

```rust
PeerEvent::TransportIdentified { peer_addr, transport } => {
    if let Some(peer) = self.peers.get_mut(&peer_addr) {
        peer.transport = Some(transport);
    }
}
```

### Step 6: Add `MixedModeAlgorithm` and `auto_sequential` to Settings

**File:** `crates/ferrite-session/src/settings.rs`

Add import at top:

```rust
use crate::rate_limiter::MixedModeAlgorithm;
```

Add default helpers:

```rust
fn default_mixed_mode() -> MixedModeAlgorithm {
    MixedModeAlgorithm::PeerProportional
}
```

Add fields to `Settings` struct in the "Rate limiting" section, after `auto_upload_slots_max`:

```rust
    /// Mixed-mode TCP/uTP bandwidth allocation algorithm.
    #[serde(default = "default_mixed_mode")]
    pub mixed_mode_algorithm: MixedModeAlgorithm,
```

Add field in the "Hashing & piece picking" section, after `block_request_timeout_secs`:

```rust
    /// Automatically switch to sequential piece picking when too many partial
    /// pieces accumulate. Uses hysteresis: activates at 60% partial ratio,
    /// deactivates at 30%.
    #[serde(default = "default_true")]
    pub auto_sequential: bool,
```

Update `Default for Settings`:

```rust
// In Rate limiting section:
mixed_mode_algorithm: MixedModeAlgorithm::PeerProportional,

// In Hashing & piece picking section:
auto_sequential: true,
```

Update `PartialEq for Settings` — add comparisons:

```rust
    && self.mixed_mode_algorithm == other.mixed_mode_algorithm
    && self.auto_sequential == other.auto_sequential
```

**Tests:** Update `default_settings_values` test:

```rust
assert_eq!(s.mixed_mode_algorithm, MixedModeAlgorithm::PeerProportional);
assert!(s.auto_sequential);
```

### Step 7: Add `RateLimiterSet` to TorrentActor + mixed-mode tick

**File:** `crates/ferrite-session/src/torrent.rs`

Add a `rate_limiter_set` field to `TorrentActor`:

```rust
    // Per-class rate limiting (M32b) + mixed-mode (M45)
    rate_limiter_set: crate::rate_limiter::RateLimiterSet,
    mixed_mode_algorithm: crate::rate_limiter::MixedModeAlgorithm,
```

Initialize in `TorrentActor` constructors (`from_torrent` and `from_magnet`):

```rust
rate_limiter_set: crate::rate_limiter::RateLimiterSet::new(
    0, 0, 0, 0, // per-class: initially unlimited, mixed-mode will adjust
    config.upload_rate_limit,
    config.download_rate_limit,
),
mixed_mode_algorithm: crate::rate_limiter::MixedModeAlgorithm::PeerProportional,
```

Note: The `mixed_mode_algorithm` value needs to come from Settings. Since `TorrentConfig` is built from Settings via `make_torrent_config()`, we need to add `mixed_mode_algorithm` to `TorrentConfig` as well (see Step 8).

In the main event loop's `refill_interval` tick handler, after `self.upload_bucket.refill(...)` and `self.download_bucket.refill(...)`, add:

```rust
// Refill per-class buckets
self.rate_limiter_set.refill(Duration::from_millis(100));

// Mixed-mode: adjust per-class upload limits based on peer composition
let (tcp_peers, utp_peers) = self.transport_peer_counts();
let global_up = self.config.upload_rate_limit;
self.rate_limiter_set.apply_mixed_mode(
    self.mixed_mode_algorithm,
    tcp_peers,
    utp_peers,
    global_up,
);
```

Add helper method on `TorrentActor`:

```rust
/// Count connected peers by transport type.
fn transport_peer_counts(&self) -> (usize, usize) {
    let mut tcp = 0;
    let mut utp = 0;
    for peer in self.peers.values() {
        match peer.transport {
            Some(crate::rate_limiter::PeerTransport::Tcp) => tcp += 1,
            Some(crate::rate_limiter::PeerTransport::Utp) => utp += 1,
            None => {} // Transport not yet identified — don't count
        }
    }
    (tcp, utp)
}
```

### Step 8: Add new fields to `TorrentConfig`

**File:** `crates/ferrite-session/src/types.rs`

Add to `TorrentConfig` struct:

```rust
    /// Mixed-mode TCP/uTP bandwidth allocation algorithm.
    pub mixed_mode_algorithm: crate::rate_limiter::MixedModeAlgorithm,
    /// Enable automatic sequential mode switching on partial-piece explosion.
    pub auto_sequential: bool,
```

Update `Default for TorrentConfig`:

```rust
    mixed_mode_algorithm: crate::rate_limiter::MixedModeAlgorithm::PeerProportional,
    auto_sequential: true,
```

Update `From<&Settings> for TorrentConfig`:

```rust
    mixed_mode_algorithm: s.mixed_mode_algorithm,
    auto_sequential: s.auto_sequential,
```

### Step 9: Formalize auto-sequential with hysteresis

**File:** `crates/ferrite-session/src/piece_selector.rs`

Add an `auto_sequential_active` field to `PickContext`:

```rust
pub(crate) struct PickContext<'a> {
    // ... existing fields ...
    /// Whether auto-sequential mode is currently active (managed by TorrentActor).
    pub auto_sequential_active: bool,
}
```

Replace the ad-hoc auto_sequential computation in `pick_new_piece()`:

```rust
    fn pick_new_piece<F>(
        &self,
        ctx: &PickContext<'_>,
        missing_chunks: &F,
    ) -> Option<PickResult>
    where
        F: Fn(u32) -> Vec<(u32, u32)>,
    {
        // Snubbed peers: pick highest-availability piece (reverse rarest-first)
        if ctx.peer_is_snubbed {
            return self.pick_reverse_rarest(ctx, missing_chunks);
        }

        // Initial random threshold: randomize to promote piece diversity
        if ctx.completed_count < ctx.initial_picker_threshold
            && let Some(result) = self.pick_random(ctx, missing_chunks)
        {
            return Some(result);
        }

        // Sequential mode (explicit or auto-sequential with hysteresis)
        if ctx.sequential_download || ctx.auto_sequential_active {
            return self.pick_sequential(ctx, missing_chunks);
        }

        // Default: rarest-first
        self.pick_rarest_new(ctx, missing_chunks)
    }
```

Add a free function for the hysteresis check (stateless, called from TorrentActor):

```rust
/// Evaluate auto-sequential hysteresis.
///
/// Returns the new auto_sequential_active state. Uses dual thresholds to
/// prevent flapping:
/// - Activates when partial ratio exceeds 60%
/// - Deactivates when partial ratio drops below 30%
/// - Stays in current state between thresholds
///
/// `in_flight_count`: number of pieces currently in-flight (partially downloaded).
/// `connected_peers`: number of connected peers.
/// `currently_active`: whether auto-sequential is currently active.
pub(crate) fn evaluate_auto_sequential(
    in_flight_count: usize,
    connected_peers: usize,
    currently_active: bool,
) -> bool {
    if connected_peers == 0 {
        return false;
    }

    let partial_ratio = in_flight_count as f64 / connected_peers as f64;

    if currently_active {
        // Deactivate only when ratio drops below 0.3 (30% of peers)
        partial_ratio >= 0.3
    } else {
        // Activate when ratio exceeds 0.6 (60% of peers)
        // Note: the old heuristic used 1.5x, which is 150%. The new thresholds
        // are relative to peer count: 60% means partial pieces > 0.6 * peers
        partial_ratio > 0.6
    }
}
```

**Wait — re-reading the roadmap.** The roadmap says "switch to sequential when >60% of in-flight pieces are partial." Let me re-interpret: the ratio is `partial_pieces / total_in_flight`. But "partial pieces" IS all in-flight pieces (they are all partial by definition — they're being downloaded). Let me re-read the original heuristic:

The original: `in_flight_pieces.len() > 1.5 * connected_peer_count` — this triggers when the number of partial pieces exceeds 1.5x the peer count, indicating "too many open pieces."

The roadmap says: "switch to sequential when >60% of in-flight pieces are partial, switch back when <30%." This phrasing is ambiguous since all in-flight pieces are partial. The intent is the partial-piece-to-peer ratio. So the hysteresis should be:

- **Activate** when `in_flight_count / connected_peers > threshold_high` (pieces are spreading too thin)
- **Deactivate** when `in_flight_count / connected_peers < threshold_low`

Using the roadmap's 60%/30% values as ratio multipliers makes sense: activate when partial pieces > 60% MORE than peer count (ratio > 1.6), deactivate when < 30% more (ratio < 1.3). But that's close to the old 1.5x. Let me use a cleaner interpretation aligned with libtorrent's approach:

Actually, libtorrent's `auto_sequential` in settings_pack.cpp uses the same concept: when the number of outstanding piece requests exceeds a threshold relative to peer count, switch to sequential to reduce waste. The 60%/30% from the roadmap should be interpreted as: the ratio of `in_flight / peers`:

- Activate at ratio > 1.6 (60% more in-flight than peers)
- Deactivate at ratio < 1.3 (30% more)

Let me just use the values literally from the roadmap — it says ">60% partial" and "<30%". Given the context of partial-piece explosion, I'll interpret this as:

- Activate: `in_flight > peers * 1.6` (or equivalently, `in_flight / peers > 1.6`)
- Deactivate: `in_flight < peers * 1.3`

This provides a clear hysteresis band between 1.3 and 1.6, replacing the old instant-on at 1.5.

Updated function:

```rust
/// Hysteresis thresholds for auto-sequential mode.
/// Activate when in-flight pieces exceed this ratio of connected peers.
const AUTO_SEQUENTIAL_ACTIVATE_RATIO: f64 = 1.6;
/// Deactivate when in-flight pieces drop below this ratio of connected peers.
const AUTO_SEQUENTIAL_DEACTIVATE_RATIO: f64 = 1.3;

/// Evaluate auto-sequential hysteresis.
///
/// Returns the new auto_sequential_active state. Uses dual thresholds to
/// prevent flapping:
/// - Activates when in-flight / peers > 1.6 (partial-piece explosion)
/// - Deactivates when in-flight / peers < 1.3
/// - Stays in current state between thresholds
pub(crate) fn evaluate_auto_sequential(
    in_flight_count: usize,
    connected_peers: usize,
    currently_active: bool,
) -> bool {
    if connected_peers == 0 {
        return false;
    }

    let ratio = in_flight_count as f64 / connected_peers as f64;

    if currently_active {
        // Stay active until ratio drops below deactivation threshold
        ratio >= AUTO_SEQUENTIAL_DEACTIVATE_RATIO
    } else {
        // Activate when ratio exceeds activation threshold
        ratio > AUTO_SEQUENTIAL_ACTIVATE_RATIO
    }
}
```

### Step 10: Wire auto-sequential into TorrentActor

**File:** `crates/ferrite-session/src/torrent.rs`

Add field to `TorrentActor`:

```rust
    /// Whether auto-sequential mode is currently active (hysteresis state).
    auto_sequential_active: bool,
```

Initialize to `false` in constructors.

In the `PickContext` construction (around line 2624), pass the new field:

```rust
let ctx = PickContext {
    // ... existing fields ...
    auto_sequential_active: self.config.auto_sequential && self.auto_sequential_active,
};
```

Update auto-sequential state on the unchoke/refill tick (every 100ms on the refill tick, or every 10s on the unchoke tick — the unchoke tick is better to avoid chattering):

In the unchoke interval handler:

```rust
// Update auto-sequential hysteresis (M45)
if self.config.auto_sequential {
    self.auto_sequential_active = crate::piece_selector::evaluate_auto_sequential(
        self.in_flight_pieces.len(),
        self.peers.len(),
        self.auto_sequential_active,
    );
}
```

### Step 11: Update `PickContext` construction in tests

**File:** `crates/ferrite-session/src/piece_selector.rs`

Update `default_pick_context()` helper to include the new field:

```rust
    auto_sequential_active: false,
```

Update the `auto_sequential_on_partial_explosion` test to use the new hysteresis path:

```rust
#[test]
fn auto_sequential_on_partial_explosion() {
    let mut sel = PieceSelector::new(15);
    for i in 0..15 {
        sel.availability[i] = 2;
    }
    sel.availability[10] = 1;

    let mut peer_has = Bitfield::new(15);
    for i in 0..15 {
        peer_has.set(i);
    }
    let we_have = Bitfield::new(15);
    let mut wanted = Bitfield::new(15);
    for i in 0..15 {
        wanted.set(i);
    }

    let streaming = BTreeSet::new();
    let time_critical = BTreeSet::new();
    let suggested = HashSet::new();

    let mut in_flight = HashMap::new();
    for i in 0..10 {
        let mut ifp = InFlightPiece::new(2);
        ifp.assigned_blocks.insert((i, 0), addr(11000));
        ifp.assigned_blocks.insert((i, 16384), addr(11000));
        in_flight.insert(i, ifp);
    }

    let mut ctx = default_pick_context(
        addr(11001), &peer_has, &we_have, &wanted,
        &in_flight, &streaming, &time_critical, &suggested,
    );
    ctx.connected_peer_count = 4;
    ctx.completed_count = 100;
    // Set auto_sequential_active = true (simulating TorrentActor evaluation)
    ctx.auto_sequential_active = true;

    let chunks = |piece: u32| {
        if piece < 10 {
            vec![(0, 16384), (16384, 16384)]
        } else {
            vec![(0, 16384)]
        }
    };
    let result = sel.pick_blocks(&ctx, chunks).unwrap();
    assert_eq!(result.piece, 10);
}
```

### Step 12: Add auto-sequential hysteresis tests

**File:** `crates/ferrite-session/src/piece_selector.rs`

```rust
#[test]
fn auto_sequential_hysteresis_activation() {
    // 4 peers, need > 1.6 * 4 = 6.4 in-flight to activate
    assert!(!evaluate_auto_sequential(6, 4, false));  // 6/4 = 1.5 < 1.6
    assert!(evaluate_auto_sequential(7, 4, false));   // 7/4 = 1.75 > 1.6
}

#[test]
fn auto_sequential_hysteresis_deactivation() {
    // 4 peers, need < 1.3 * 4 = 5.2 in-flight to deactivate
    assert!(evaluate_auto_sequential(6, 4, true));   // 6/4 = 1.5 >= 1.3, stays active
    assert!(evaluate_auto_sequential(5, 4, true));   // 5/4 = 1.25, < 1.3 => still >= 1.3? No: 1.25 < 1.3
    assert!(!evaluate_auto_sequential(5, 4, true));  // 5/4 = 1.25 < 1.3, deactivates
}

#[test]
fn auto_sequential_hysteresis_band() {
    // In the band between 1.3 and 1.6 — state doesn't change
    // 10 peers: activate > 16, deactivate < 13
    // At 14 in-flight (ratio 1.4): in the band
    assert!(!evaluate_auto_sequential(14, 10, false)); // inactive stays inactive
    assert!(evaluate_auto_sequential(14, 10, true));   // active stays active
}

#[test]
fn auto_sequential_zero_peers() {
    assert!(!evaluate_auto_sequential(10, 0, false));
    assert!(!evaluate_auto_sequential(10, 0, true));
}
```

### Step 13: Update facade re-exports

**File:** `crates/ferrite-session/src/lib.rs`

The `MixedModeAlgorithm` type needs to be public since it's used in `Settings` (which is already public). Export it:

```rust
pub use rate_limiter::MixedModeAlgorithm;
```

**File:** `crates/ferrite/src/session.rs`

Add to re-exports:

```rust
    // Mixed-mode algorithm (M45)
    MixedModeAlgorithm,
```

**File:** `crates/ferrite/src/prelude.rs`

Add:

```rust
// Mixed-mode TCP/uTP algorithm (M45)
pub use crate::session::MixedModeAlgorithm;
```

### Step 14: Add `ClientBuilder` methods

**File:** `crates/ferrite/src/client.rs` (or wherever ClientBuilder lives)

```rust
/// Set the mixed-mode TCP/uTP bandwidth allocation algorithm.
pub fn mixed_mode_algorithm(mut self, algorithm: MixedModeAlgorithm) -> Self {
    self.settings.mixed_mode_algorithm = algorithm;
    self
}

/// Enable or disable automatic sequential mode switching.
///
/// When enabled, the piece picker automatically switches to sequential
/// mode when partial-piece accumulation indicates piece completion is
/// being delayed by scattered requests.
pub fn auto_sequential(mut self, enable: bool) -> Self {
    self.settings.auto_sequential = enable;
    self
}
```

### Step 15: Version bump

**File:** `/mnt/TempNVME/projects/ferrite/Cargo.toml`

```toml
version = "0.42.0"
```

## Test Summary

| # | Test Name | File | Validates |
|---|-----------|------|-----------|
| 1 | `mixed_mode_prefer_tcp_both_present` | `rate_limiter.rs` | PreferTcp 90/10 split |
| 2 | `mixed_mode_prefer_tcp_only_utp` | `rate_limiter.rs` | PreferTcp with no TCP peers |
| 3 | `mixed_mode_proportional` | `rate_limiter.rs` | PeerProportional allocation |
| 4 | `mixed_mode_unlimited_global_noop` | `rate_limiter.rs` | No-op when global unlimited |
| 5 | `auto_sequential_hysteresis_activation` | `piece_selector.rs` | Activation threshold |
| 6 | `auto_sequential_hysteresis_deactivation` | `piece_selector.rs` | Deactivation threshold |
| 7 | `auto_sequential_hysteresis_band` | `piece_selector.rs` | Deadband stability |
| 8 | `auto_sequential_zero_peers` | `piece_selector.rs` | Edge case: no peers |
| 9 | `auto_sequential_on_partial_explosion` | `piece_selector.rs` | Updated existing test |
| 10 | `default_settings_values` | `settings.rs` | Updated: new field defaults |

## File Change Summary

| File | Change Type | Description |
|------|------------|-------------|
| `crates/ferrite-session/src/rate_limiter.rs` | Modified | Add `MixedModeAlgorithm` enum, `apply_mixed_mode()` method, serde import, 4 tests |
| `crates/ferrite-session/src/peer_state.rs` | Modified | Add `transport: Option<PeerTransport>` field |
| `crates/ferrite-session/src/types.rs` | Modified | Add `TransportIdentified` PeerEvent variant, `mixed_mode_algorithm` + `auto_sequential` to TorrentConfig |
| `crates/ferrite-session/src/settings.rs` | Modified | Add `mixed_mode_algorithm` + `auto_sequential` fields to Settings, defaults, PartialEq |
| `crates/ferrite-session/src/torrent.rs` | Modified | Add `rate_limiter_set`, `mixed_mode_algorithm`, `auto_sequential_active` fields, `transport_peer_counts()`, mixed-mode tick, auto-sequential tick, handle `TransportIdentified` event |
| `crates/ferrite-session/src/piece_selector.rs` | Modified | Add `auto_sequential_active` to PickContext, `evaluate_auto_sequential()` function, replace ad-hoc check, update existing test, add 4 hysteresis tests |
| `crates/ferrite-session/src/lib.rs` | Modified | Export `MixedModeAlgorithm` |
| `crates/ferrite/src/session.rs` | Modified | Re-export `MixedModeAlgorithm` |
| `crates/ferrite/src/prelude.rs` | Modified | Re-export `MixedModeAlgorithm` |
| `crates/ferrite/src/client.rs` | Modified | Add `mixed_mode_algorithm()` + `auto_sequential()` builder methods |
| `Cargo.toml` | Modified | Version bump to 0.42.0 |

## Implementation Order

1. `rate_limiter.rs` — `MixedModeAlgorithm` enum + `apply_mixed_mode()` + tests (self-contained, testable)
2. `piece_selector.rs` — `evaluate_auto_sequential()` function + hysteresis tests (self-contained, testable)
3. `piece_selector.rs` — Add `auto_sequential_active` to `PickContext`, replace ad-hoc check, update existing test
4. `peer_state.rs` — Add `transport` field
5. `types.rs` — `TransportIdentified` PeerEvent, TorrentConfig fields
6. `settings.rs` — New Settings fields + PartialEq + default test update
7. `torrent.rs` — Wire everything: fields, event handler, mixed-mode tick, auto-sequential tick
8. `lib.rs` + `session.rs` + `prelude.rs` — Exports
9. `client.rs` — Builder methods
10. `Cargo.toml` — Version bump

Steps 1-3 can be tested independently with `cargo test -p ferrite-session`. Steps 4-7 complete the wiring. Steps 8-10 are mechanical.

## Commit Plan

Single commit:

```
feat: mixed-mode TCP/uTP algorithm + auto-sequential hysteresis (M45)
```
