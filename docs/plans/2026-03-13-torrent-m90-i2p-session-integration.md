# M90: I2P Session Integration — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the existing I2P SAM protocol client into the session's peer connection lifecycle: outbound I2P connects, tracker announces with I2P destination, and mixed-mode peer segregation.

**Architecture:** The I2P stack is more complete than initially assessed. The SAM protocol client (`SamSession`), destination type (`I2pDestination`), settings (8 fields with validation), session startup (creates SAM session), and TorrentActor inbound path (accept loop + synthetic address assignment + peer spawning) are ALL already implemented. What remains is: (1) outbound connects via SAM when peers have I2P destinations, (2) including our I2P destination in tracker announces, and (3) mixed-mode enforcement in PEX messages.

**Tech Stack:** Rust, torrent-session crate, I2P SAM v3.1

**Existing code reference:** `crates/torrent-session/src/i2p/` (sam.rs, destination.rs, mod.rs)

---

### Task 1: Add I2P destination to peer address representation

**Files:**
- Modify: `crates/torrent-session/src/types.rs` (or wherever peer address types are defined)

- [ ] **Step 1: Identify how peers are addressed**

Search for how `SocketAddr` is used to identify peers in `TorrentActor`. Currently all peers are keyed by `SocketAddr` in `self.peers: HashMap<SocketAddr, PeerState>`. I2P peers already use synthetic 240.0.0.0/4 addresses (see `next_i2p_synthetic_addr()` at torrent.rs:6358).

For outbound connects, we need to associate the synthetic address with the actual I2P destination so `try_connect_peers()` can route via SAM.

- [ ] **Step 2: Add I2P destination tracking to PeerState or available_peers**

Add a field to track which peers are I2P peers and store their destination:

```rust
/// Optional I2P destination for peers reachable via SAM.
i2p_destination: Option<crate::i2p::I2pDestination>,
```

Add this to the peer info struct used in `available_peers` or wherever queued peers are stored before connection.

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-session/src/types.rs
git commit -m "feat: add I2P destination tracking to peer address types (M90)"
```

---

### Task 2: Write failing test for outbound I2P connect routing

**Files:**
- Modify: test module in `crates/torrent-session/src/torrent.rs`

- [ ] **Step 1: Write test**

```rust
#[tokio::test]
async fn test_outbound_i2p_connect_uses_sam() {
    // Test that when try_connect_peers encounters a peer with an I2P
    // destination, it calls sam_session.connect() instead of TCP connect.
    // This may require mocking SamSession or testing the routing decision
    // logic in isolation.
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p torrent-session -- i2p_connect
```

Expected: FAIL — routing logic doesn't exist yet.

---

### Task 3: Implement outbound I2P connect in `try_connect_peers`

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs`

- [ ] **Step 1: Add I2P routing in the connection task**

In `try_connect_peers()` (lines ~5889-6322), after the peer is dequeued and before the TCP connection is attempted (line ~6137), add a check for I2P peers:

```rust
// If this peer has an I2P destination, connect via SAM instead of TCP
if let Some(ref i2p_dest) = peer_info.i2p_destination {
    if let Some(ref sam) = self.sam_session {
        let sam_clone = Arc::clone(sam);
        let dest = i2p_dest.clone();
        // Connect via SAM in the spawned task
        let stream_result = sam_clone.connect(&dest.to_base64()).await;
        match stream_result {
            Ok(sam_stream) => {
                let tcp_stream = sam_stream.into_inner();
                // Continue with peer handshake using tcp_stream
                // (SamStream wraps a TcpStream internally)
            }
            Err(e) => {
                debug!(%addr, "I2P SAM connect failed: {e}");
                // Mark peer as failed, continue
            }
        }
    }
    // Skip normal TCP connect path
}
```

The exact integration point depends on how the connection task closure is structured. The implementer should:
1. Read the full `try_connect_peers()` function
2. Find where `factory.connect_tcp(addr)` is called (line ~6137-6159)
3. Add an `if peer_is_i2p` branch before that call
4. Use `sam_session.connect()` to get a `SamStream`, extract the inner `TcpStream`, and feed it into the same peer handshake path

- [ ] **Step 2: Run tests**

```bash
cargo test -p torrent-session -- i2p
```

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-session/src/torrent.rs
git commit -m "feat: route outbound peer connects via SAM for I2P peers (M90)"
```

---

### Task 4: Include I2P destination in tracker announces

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs` (tracker announce logic)
- Modify: `crates/torrent-tracker/src/http.rs` (if announce params need extension)

- [ ] **Step 1: Identify tracker announce code**

Search for `tracker` and `announce` in torrent.rs to find where HTTP/UDP tracker announces are constructed. The announce typically includes our listening port and IP.

- [ ] **Step 2: Add I2P destination to announce parameters**

When I2P is enabled and we have a SAM session, include the I2P destination as an additional parameter. BEP 7 specifies:
- For HTTP trackers: add `&i2p=1` flag and optionally the destination in the request
- For I2P-specific trackers (`.i2p` hostnames): use the I2P destination as our address

```rust
// When building announce URL/request:
if let Some(ref sam) = self.sam_session {
    let dest = sam.destination().to_base64();
    // Include i2p destination in announce
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --workspace
```

- [ ] **Step 4: Commit**

```bash
git add crates/torrent-session/src/torrent.rs crates/torrent-tracker/src/http.rs
git commit -m "feat: include I2P destination in tracker announces (M90)"
```

---

### Task 5: Implement mixed-mode PEX filtering

**Files:**
- Modify: `crates/torrent-session/src/pex.rs` (or wherever PEX messages are constructed)

- [ ] **Step 1: Identify PEX message construction**

PEX messages are constructed in `pex.rs`. The `PexMessage` struct has `added` (compact IPv4 peers) and `added6` (compact IPv6 peers). I2P peers use synthetic 240.0.0.0/4 addresses which would be included in `added`.

- [ ] **Step 2: Filter I2P synthetic addresses from PEX when mixed mode is disabled**

When `allow_i2p_mixed = false`:
- Don't include I2P synthetic addresses (240.0.0.0/4) in PEX messages sent to clearnet peers
- Don't include clearnet addresses in PEX messages sent to I2P peers

```rust
fn is_i2p_synthetic(addr: &SocketAddr) -> bool {
    match addr {
        SocketAddr::V4(v4) => v4.ip().octets()[0] & 0xF0 == 240,
        _ => false,
    }
}

// In PEX message construction:
if !self.config.allow_i2p_mixed {
    let peer_is_i2p = is_i2p_synthetic(&peer_addr);
    // Filter: only include peers of the same transport type
    added_peers.retain(|p| is_i2p_synthetic(p) == peer_is_i2p);
}
```

- [ ] **Step 3: Write test for mixed-mode filtering**

```rust
#[test]
fn test_pex_mixed_mode_filtering() {
    let clearnet = "1.2.3.4:6881".parse().unwrap();
    let i2p_synthetic = "240.0.0.1:1".parse().unwrap();

    // When mixed mode disabled, clearnet peer should not see I2P peers
    assert!(!is_i2p_synthetic(&clearnet));
    assert!(is_i2p_synthetic(&i2p_synthetic));

    // Filter logic test...
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --workspace && cargo clippy --workspace -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add crates/torrent-session/src/pex.rs
git commit -m "feat: enforce mixed-mode I2P/clearnet peer segregation in PEX (M90)"
```

---

### Task 6: Final verification and commit

- [ ] **Step 1: Run full test suite**

```bash
cargo test --workspace && cargo clippy --workspace -- -D warnings
```

Expected: All tests pass.

- [ ] **Step 2: Write integration test (ignored, requires I2P router)**

```rust
#[tokio::test]
#[ignore = "requires local I2P router running SAM on port 7656"]
async fn test_i2p_full_session() {
    // Create session with enable_i2p = true
    // Verify SAM session creation
    // Verify I2P destination is valid
    // Verify accept loop is running
}
```

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: add ignored I2P integration test (M90)"
```
