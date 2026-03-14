# M89: NAT Cleanup Logging — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace silent `let _ =` error suppression in NAT port mapping cleanup with `debug!`-level logging so mapping deletion failures leave a diagnostic trail.

**Architecture:** `do_unmap()` in the NAT actor runs best-effort cleanup on shutdown. Four `let _ =` patterns discard errors from NAT-PMP and UPnP deletion requests. Replace each with `if let Err(e)` + `debug!()`. Shutdown semantics unchanged — errors don't block or propagate.

**Tech Stack:** Rust, torrent-nat crate, tracing

---

### Task 1: Replace silent error suppression with debug logging

**Files:**
- Modify: `crates/torrent-nat/src/actor.rs`

- [ ] **Step 1: Replace NAT-PMP TCP deletion (line ~505)**

```rust
// BEFORE:
let _ = crate::natpmp::delete_tcp_mapping(gw, tcp_port).await;

// AFTER:
if let Err(e) = crate::natpmp::delete_tcp_mapping(gw, tcp_port).await {
    debug!("failed to delete NAT-PMP TCP mapping on port {tcp_port}: {e}");
}
```

- [ ] **Step 2: Replace NAT-PMP UDP deletion (line ~507)**

```rust
// BEFORE:
let _ = crate::natpmp::delete_udp_mapping(gw, udp_port).await;

// AFTER:
if let Err(e) = crate::natpmp::delete_udp_mapping(gw, udp_port).await {
    debug!("failed to delete NAT-PMP UDP mapping on port {udp_port}: {e}");
}
```

- [ ] **Step 3: Replace UPnP TCP deletion (line ~514-520)**

```rust
// BEFORE:
let _ = crate::upnp::soap::soap_request(
    control_url,
    service_type,
    "DeletePortMapping",
    &body,
)
.await;

// AFTER:
if let Err(e) = crate::upnp::soap::soap_request(
    control_url,
    service_type,
    "DeletePortMapping",
    &body,
)
.await
{
    debug!("failed to delete UPnP TCP mapping on port {tcp_port}: {e}");
}
```

- [ ] **Step 4: Replace UPnP UDP deletion (line ~524-530)**

Same pattern as step 3 but for UDP:

```rust
if let Err(e) = crate::upnp::soap::soap_request(
    control_url,
    service_type,
    "DeletePortMapping",
    &body,
)
.await
{
    debug!("failed to delete UPnP UDP mapping on port {udp_port}: {e}");
}
```

---

### Task 2: Verify and commit

- [ ] **Step 1: Run tests**

```bash
cargo test --workspace
```

Expected: All tests pass. Logging-only change.

- [ ] **Step 2: Run clippy**

```bash
cargo clippy --workspace -- -D warnings
```

Expected: Clean. The `let _ =` suppression removal should satisfy any future unused-result warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-nat/src/actor.rs
git commit -m "feat: log NAT mapping deletion failures instead of silently ignoring (M89)

Replace four let _ = patterns in do_unmap() with debug!-level logging.
Best-effort shutdown semantics unchanged — errors don't block or
propagate. Helps diagnose stale port mappings on gateway."
```
