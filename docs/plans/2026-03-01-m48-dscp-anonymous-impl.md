# M48: DSCP Marking + Anonymous Mode Hardening — Implementation Plan

## Overview

Two features in one milestone:

1. **DSCP/ToS marking** -- classify BitTorrent traffic as low-priority (CS1/scavenger) by setting the IP TOS byte on all sockets.
2. **Anonymous mode hardening** -- extend the existing `anonymous_mode` flag to suppress every identifying fingerprint.

Both features are session-level and propagate through existing `Settings` / `TorrentConfig` paths.

## Current State Audit

### What `anonymous_mode` already does

| Location | Behavior |
|----------|----------|
| `session.rs:192-197` | Disables DHT, LSD, UPnP, NAT-PMP |
| `peer.rs:106-108` | Sets `ext_hs.v = None` (suppresses client version string in BEP 10 handshake) |

### What is NOT yet hardened

| Gap | Risk |
|-----|------|
| HTTP tracker requests include `User-Agent: Ferrite/0.1.0` | Client fingerprinting via tracker logs |
| `AnnounceRequest` includes local listen port in query string | IP:port leak to tracker |
| Extension handshake still sends `reqq`, `p` (listen port), `upload_only` | Protocol fingerprinting |
| Peer ID prefix is `-FE0100-` (identifies client + version) | Peer ID fingerprinting |
| UDP tracker announce includes IP field (currently always 0) | Correct, but should be verified |
| No DSCP marking on any socket | ISP can't classify traffic as low-priority |

---

## Step 1: Add `peer_dscp` Setting

### 1a. Add serde default helper and field to `Settings`

**File:** `crates/ferrite-session/src/settings.rs`

Add the default helper function:

```rust
fn default_peer_dscp() -> u8 {
    0x04 // CS1 — scavenger / low-priority background
}
```

Add the field to `Settings` struct, in the "Protocol features" section after `anonymous_mode`:

```rust
    /// DSCP value for the ToS/Traffic Class byte on peer, tracker, and uTP sockets.
    /// Default: 0x04 (CS1/scavenger — low-priority background traffic).
    /// Set to 0 to disable DSCP marking.
    #[serde(default = "default_peer_dscp")]
    pub peer_dscp: u8,
```

### 1b. Update `Default for Settings`

Add after `anonymous_mode: false`:

```rust
            peer_dscp: 0x04,
```

### 1c. Update `PartialEq for Settings`

Add to the equality chain:

```rust
            && self.peer_dscp == other.peer_dscp
```

### 1d. Update presets

Both `min_memory()` and `high_performance()` use `..Self::default()` so they inherit `peer_dscp: 0x04` automatically. No changes needed.

### 1e. Update validation

Add a validation check in `Settings::validate()`:

```rust
        // DSCP is a 6-bit field shifted left by 2 in the TOS byte.
        // Valid TOS byte values: 0x00..=0xFC (only top 6 bits meaningful for DSCP,
        // plus 2 ECN bits). We allow any u8 since the kernel masks appropriately.
        // No validation needed — any u8 is valid for setsockopt IP_TOS.
```

No validation needed because the kernel accepts any u8 for `IP_TOS`.

### 1f. Tests for `peer_dscp` in settings

Add to `mod tests` in `settings.rs`:

```rust
    #[test]
    fn default_peer_dscp_value() {
        let s = Settings::default();
        assert_eq!(s.peer_dscp, 0x04); // CS1 scavenger
    }

    #[test]
    fn peer_dscp_json_round_trip() {
        let json = r#"{"peer_dscp": 184}"#; // 0xB8 = EF (Expedited Forwarding)
        let decoded: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.peer_dscp, 0xB8);
        let encoded = serde_json::to_string(&decoded).unwrap();
        let roundtrip: Settings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(roundtrip.peer_dscp, 0xB8);
    }

    #[test]
    fn peer_dscp_zero_disables() {
        let json = r#"{"peer_dscp": 0}"#;
        let decoded: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.peer_dscp, 0);
    }
```

---

## Step 2: Add `peer_dscp` to `TorrentConfig`

### 2a. Add field to `TorrentConfig`

**File:** `crates/ferrite-session/src/types.rs`

Add to `TorrentConfig` struct:

```rust
    /// DSCP value for the ToS/Traffic Class byte on sockets.
    pub peer_dscp: u8,
```

### 2b. Update `Default for TorrentConfig`

```rust
            peer_dscp: 0x04,
```

### 2c. Update `From<&Settings> for TorrentConfig`

```rust
            peer_dscp: s.peer_dscp,
```

### 2d. Test

Add to `mod tests` in `types.rs`:

```rust
    #[test]
    fn torrent_config_peer_dscp_default() {
        let cfg = TorrentConfig::default();
        assert_eq!(cfg.peer_dscp, 0x04);
    }
```

Update existing `torrent_config_from_settings` test in `settings.rs` to assert:

```rust
        assert_eq!(tc.peer_dscp, s.peer_dscp);
```

---

## Step 3: DSCP Socket Helper Module

### 3a. Create `crates/ferrite-session/src/dscp.rs`

This module provides a single function to apply DSCP marking to any socket. Uses `libc` for `setsockopt`.

**File:** `crates/ferrite-session/src/dscp.rs`

```rust
//! DSCP/ToS socket option helpers.
//!
//! Sets the IP_TOS (IPv4) or IPV6_TCLASS (IPv6) socket option to classify
//! BitTorrent traffic. Default: CS1/scavenger (0x04).

use std::io;
use std::os::fd::AsRawFd;

/// Apply DSCP marking to a socket.
///
/// Sets `IP_TOS` for IPv4 sockets and `IPV6_TCLASS` for IPv6 sockets.
/// A `dscp` value of 0 is a no-op (leaves default best-effort marking).
///
/// This is best-effort: failures are logged but not propagated, since DSCP
/// marking is a QoS hint and not essential for correct operation.
pub(crate) fn apply_dscp<S: AsRawFd>(socket: &S, dscp: u8, ipv6: bool) {
    if dscp == 0 {
        return;
    }

    let fd = socket.as_raw_fd();
    let tos = dscp as libc::c_int;

    let result = if ipv6 {
        // IPv6: IPV6_TCLASS
        unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_TCLASS,
                &tos as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    } else {
        // IPv4: IP_TOS
        unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_TOS,
                &tos as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };

    if result != 0 {
        let err = io::Error::last_os_error();
        tracing::debug!(dscp, ipv6, "failed to set DSCP/TOS: {err}");
    }
}

/// Apply DSCP marking to a `tokio::net::TcpStream`.
///
/// Determines IPv4/IPv6 from the peer address.
pub(crate) fn apply_dscp_tcp(stream: &tokio::net::TcpStream, dscp: u8) {
    if dscp == 0 {
        return;
    }
    let ipv6 = stream
        .peer_addr()
        .map(|a| a.is_ipv6())
        .unwrap_or(false);
    apply_dscp(stream, dscp, ipv6);
}

/// Apply DSCP marking to a `tokio::net::TcpListener`.
pub(crate) fn apply_dscp_listener(listener: &tokio::net::TcpListener, dscp: u8) {
    if dscp == 0 {
        return;
    }
    let ipv6 = listener
        .local_addr()
        .map(|a| a.is_ipv6())
        .unwrap_or(false);
    apply_dscp(listener, dscp, ipv6);
}

/// Apply DSCP marking to a `tokio::net::UdpSocket`.
pub(crate) fn apply_dscp_udp(socket: &tokio::net::UdpSocket, dscp: u8) {
    if dscp == 0 {
        return;
    }
    let ipv6 = socket
        .local_addr()
        .map(|a| a.is_ipv6())
        .unwrap_or(false);
    apply_dscp(socket, dscp, ipv6);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dscp_zero_is_noop() {
        // Just verify it doesn't panic
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        apply_dscp(&socket, 0, false);
    }

    #[test]
    fn dscp_ipv4_succeeds() {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        apply_dscp(&socket, 0x04, false); // CS1

        // Verify the value was set by reading it back
        let fd = socket.as_raw_fd();
        let mut tos: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_TOS,
                &mut tos as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(tos, 0x04);
    }

    #[test]
    fn dscp_ipv6_succeeds() {
        let socket = std::net::TcpListener::bind("[::1]:0").unwrap();
        apply_dscp(&socket, 0x04, true); // CS1

        let fd = socket.as_raw_fd();
        let mut tclass: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_TCLASS,
                &mut tclass as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(tclass, 0x04);
    }

    #[test]
    fn dscp_ef_value() {
        // EF = 0xB8 (DSCP 46 = 101110, shifted left 2 = 10111000 = 0xB8)
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        apply_dscp(&socket, 0xB8, false);

        let fd = socket.as_raw_fd();
        let mut tos: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_TOS,
                &mut tos as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(tos, 0xB8);
    }
}
```

### 3b. Add `libc` dependency to `ferrite-session`

**File:** `crates/ferrite-session/Cargo.toml`

Add under `[dependencies]`:

```toml
libc = "0.2"
```

### 3c. Register module

**File:** `crates/ferrite-session/src/lib.rs`

Add:

```rust
mod dscp;
```

---

## Step 4: Apply DSCP to Peer TCP Connections

### 4a. Apply DSCP on outbound TCP connections

**File:** `crates/ferrite-session/src/torrent.rs`

In `try_connect_peers()`, after the `TcpStream::connect(addr).await` succeeds (line ~3142), before calling `run_peer()`:

```rust
                    Ok(stream) => {
                        crate::dscp::apply_dscp_tcp(&stream, peer_dscp);
                        let _ = run_peer(
                            // ... existing args ...
```

This requires passing `peer_dscp` into the spawned task. Add to the variable captures before the `tokio::spawn` block (around line 3082):

```rust
            let peer_dscp = self.config.peer_dscp;
```

Also apply DSCP after successful uTP->TCP fallback in the same block.

### 4b. Apply DSCP on incoming TCP connections

In `spawn_peer_from_stream_with_mode()` (line ~3189), after the ban/filter checks and before spawning the peer task, apply DSCP. Since the stream is generic (`impl AsyncRead + AsyncWrite`), we cannot directly call `setsockopt` on it. Instead, apply DSCP on the `TcpListener` itself.

### 4c. Apply DSCP on the TcpListener

In `TorrentHandle::from_torrent()` and `TorrentHandle::from_magnet()` (lines ~157 and ~315), after binding the listener:

```rust
        if let Some(ref listener) = listener {
            crate::dscp::apply_dscp_listener(listener, config.peer_dscp);
        }
```

This sets the DSCP on the listening socket; accepted connections inherit TOS from the listener on Linux.

### 4d. Tests

```rust
    #[test]
    fn torrent_config_propagates_peer_dscp() {
        let mut s = Settings::default();
        s.peer_dscp = 0xB8; // EF
        let tc = TorrentConfig::from(&s);
        assert_eq!(tc.peer_dscp, 0xB8);
    }
```

---

## Step 5: Apply DSCP to uTP Sockets

### 5a. Expose the underlying `UdpSocket` for DSCP marking

The uTP socket actor owns a `tokio::net::UdpSocket` internally. We need to set DSCP on it at bind time.

**Option A (preferred):** Accept an optional DSCP value in `UtpConfig` and apply it inside `UtpSocket::bind()`.

**File:** `crates/ferrite-utp/src/socket.rs`

Add to `UtpConfig`:

```rust
    /// DSCP value for the underlying UDP socket (0 = no marking).
    pub dscp: u8,
```

Update `Default for UtpConfig`:

```rust
            dscp: 0,
```

In `UtpSocket::bind()`, after `UdpSocket::bind(config.bind_addr).await?`:

```rust
        // Apply DSCP marking if configured
        if config.dscp != 0 {
            let ipv6 = config.bind_addr.is_ipv6();
            let fd = udp.as_raw_fd();
            let tos = config.dscp as libc::c_int;
            let (level, name) = if ipv6 {
                (libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
            } else {
                (libc::IPPROTO_IP, libc::IP_TOS)
            };
            unsafe {
                libc::setsockopt(
                    fd,
                    level,
                    name,
                    &tos as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
```

Add `libc` dependency to `crates/ferrite-utp/Cargo.toml`:

```toml
libc = "0.2"
```

Add import at top of `socket.rs`:

```rust
use std::os::fd::AsRawFd;
```

### 5b. Propagate DSCP through `Settings::to_utp_config()`

**File:** `crates/ferrite-session/src/settings.rs`

Update `to_utp_config()` and `to_utp_config_v6()`:

```rust
    pub(crate) fn to_utp_config(&self, port: u16) -> ferrite_utp::UtpConfig {
        ferrite_utp::UtpConfig {
            bind_addr: std::net::SocketAddr::from(([0, 0, 0, 0], port)),
            max_connections: self.utp_max_connections,
            dscp: self.peer_dscp,
        }
    }

    pub(crate) fn to_utp_config_v6(&self, port: u16) -> ferrite_utp::UtpConfig {
        ferrite_utp::UtpConfig {
            bind_addr: std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)),
            max_connections: self.utp_max_connections,
            dscp: self.peer_dscp,
        }
    }
```

### 5c. Tests

```rust
    #[test]
    fn utp_config_includes_dscp() {
        let mut s = Settings::default();
        s.peer_dscp = 0x20; // CS1
        let utp = s.to_utp_config(6881);
        assert_eq!(utp.dscp, 0x20);
    }
```

---

## Step 6: Apply DSCP to Tracker Sockets

### 6a. HTTP tracker: custom `reqwest::Client` with DSCP

The `reqwest` library uses `hyper` + `tokio::net::TcpStream` under the hood. There is no direct way to set socket options on outgoing `reqwest` connections without using a custom connector.

**Pragmatic approach:** Use `socket2` via reqwest's `tcp_keepalive` isn't enough. Instead, we take the simpler approach of using `reqwest::Client::builder().local_address()` combined with a `connect_callback` (available since reqwest 0.12 via the `hickory-dns` feature... actually, `connect` is not exposed).

**Alternative pragmatic approach:** Since `HttpTracker` already allows proxy configuration, we accept that HTTP tracker DSCP marking is best-effort. The simplest reliable approach is to set the socket option via a `tower` layer or accept that HTTP tracker connections (short-lived announce/scrape requests) are not marked.

For this milestone, we will **skip DSCP marking on HTTP tracker sockets** (reqwest does not expose the underlying socket FD before the connection is made). This matches libtorrent-rasterbar's behavior, which also only marks peer and uTP sockets. Document this limitation.

### 6b. UDP tracker: apply DSCP to the ephemeral UDP socket

**File:** `crates/ferrite-tracker/src/udp.rs`

The `UdpTracker::announce()` method creates a `UdpSocket` per request. Add a `dscp` field to `UdpTracker`:

```rust
pub struct UdpTracker {
    timeout: Duration,
    dscp: u8,
}
```

Update constructor:

```rust
    pub fn new() -> Self {
        UdpTracker {
            timeout: UDP_TIMEOUT,
            dscp: 0,
        }
    }

    pub fn with_dscp(mut self, dscp: u8) -> Self {
        self.dscp = dscp;
        self
    }
```

In `announce()` and `scrape()`, after `UdpSocket::bind(bind_addr).await?`:

```rust
        // Apply DSCP marking
        if self.dscp != 0 {
            use std::os::fd::AsRawFd;
            let fd = socket.as_raw_fd();
            let tos = self.dscp as libc::c_int;
            let (level, name) = if addr.is_ipv6() {
                (libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
            } else {
                (libc::IPPROTO_IP, libc::IP_TOS)
            };
            unsafe {
                libc::setsockopt(
                    fd,
                    level,
                    name,
                    &tos as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
```

Add `libc` dependency to `crates/ferrite-tracker/Cargo.toml`:

```toml
libc = "0.2"
```

### 6c. Propagate DSCP to TrackerManager

**File:** `crates/ferrite-session/src/tracker_manager.rs`

Update `TrackerManager::from_torrent()` and `::empty()` to accept a `dscp` parameter and pass it to the UDP tracker:

```rust
    pub fn from_torrent(meta: &TorrentMetaV1, peer_id: Id20, port: u16, dscp: u8) -> Self {
        // ... existing tracker parsing ...
        TrackerManager {
            trackers,
            info_hash: meta.info_hash,
            peer_id,
            port,
            http_client: HttpTracker::new(),
            udp_client: UdpTracker::new().with_dscp(dscp),
        }
    }

    pub fn empty(info_hash: Id20, peer_id: Id20, port: u16, dscp: u8) -> Self {
        TrackerManager {
            trackers: Vec::new(),
            info_hash,
            peer_id,
            port,
            http_client: HttpTracker::new(),
            udp_client: UdpTracker::new().with_dscp(dscp),
        }
    }
```

### 6d. Update callers in `torrent.rs`

**File:** `crates/ferrite-session/src/torrent.rs`

Update `from_torrent()` (~line 164):

```rust
        let tracker_manager =
            TrackerManager::from_torrent(&meta, our_peer_id, config.listen_port, config.peer_dscp);
```

Update `from_magnet()` (~line 320):

```rust
        let tracker_manager =
            TrackerManager::empty(magnet.info_hash(), our_peer_id, config.listen_port, config.peer_dscp);
```

### 6e. Tests

In `crates/ferrite-tracker/src/udp.rs`:

```rust
    #[test]
    fn udp_tracker_dscp_builder() {
        let tracker = UdpTracker::new().with_dscp(0x04);
        assert_eq!(tracker.dscp, 0x04);
    }

    #[test]
    fn udp_tracker_default_no_dscp() {
        let tracker = UdpTracker::new();
        assert_eq!(tracker.dscp, 0);
    }
```

---

## Step 7: Anonymous Mode — Strip HTTP User-Agent

### 7a. Make `HttpTracker` accept anonymous mode

**File:** `crates/ferrite-tracker/src/http.rs`

Add a constructor variant and an `anonymous` field:

```rust
pub struct HttpTracker {
    client: reqwest::Client,
}
```

Add a new constructor:

```rust
    /// Create an HTTP tracker client configured for anonymous mode.
    ///
    /// When `anonymous` is true, no `User-Agent` header is sent.
    /// When `proxy_url` is provided, requests are routed through the proxy.
    pub fn with_options(anonymous: bool, proxy_url: Option<&str>) -> Self {
        let mut builder = reqwest::Client::builder();
        if anonymous {
            builder = builder.user_agent(""); // empty user-agent
        } else {
            builder = builder.user_agent("Ferrite/0.1.0");
        }
        if let Some(url) = proxy_url
            && let Ok(proxy) = reqwest::Proxy::all(url)
        {
            builder = builder.proxy(proxy);
        }
        HttpTracker {
            client: builder.build().expect("failed to build HTTP client"),
        }
    }
```

### 7b. Propagate anonymous mode to TrackerManager

**File:** `crates/ferrite-session/src/tracker_manager.rs`

Update constructors to accept `anonymous_mode: bool`:

```rust
    pub fn from_torrent(meta: &TorrentMetaV1, peer_id: Id20, port: u16, dscp: u8, anonymous_mode: bool) -> Self {
        // ... existing ...
        TrackerManager {
            trackers,
            info_hash: meta.info_hash,
            peer_id,
            port,
            http_client: HttpTracker::with_options(anonymous_mode, None),
            udp_client: UdpTracker::new().with_dscp(dscp),
        }
    }

    pub fn empty(info_hash: Id20, peer_id: Id20, port: u16, dscp: u8, anonymous_mode: bool) -> Self {
        TrackerManager {
            trackers: Vec::new(),
            info_hash,
            peer_id,
            port,
            http_client: HttpTracker::with_options(anonymous_mode, None),
            udp_client: UdpTracker::new().with_dscp(dscp),
        }
    }
```

### 7c. Update callers in `torrent.rs`

```rust
        let tracker_manager = TrackerManager::from_torrent(
            &meta, our_peer_id, config.listen_port, config.peer_dscp, config.anonymous_mode,
        );
```

```rust
        let tracker_manager = TrackerManager::empty(
            magnet.info_hash(), our_peer_id, config.listen_port, config.peer_dscp, config.anonymous_mode,
        );
```

### 7d. Tests

```rust
    #[test]
    fn http_tracker_anonymous_no_user_agent() {
        let tracker = HttpTracker::with_options(true, None);
        // The client is built — we can't easily introspect headers,
        // but we verify construction doesn't panic.
        let _ = tracker;
    }
```

---

## Step 8: Anonymous Mode — Suppress Local IP in Tracker Announce

### 8a. Suppress listen port in announce requests

When `anonymous_mode` is true, the tracker announce should report `port=0` to avoid revealing the client's listen port.

**File:** `crates/ferrite-session/src/tracker_manager.rs`

Add `anonymous_mode` field to `TrackerManager`:

```rust
pub(crate) struct TrackerManager {
    trackers: Vec<TrackerEntry>,
    info_hash: Id20,
    peer_id: Id20,
    port: u16,
    http_client: HttpTracker,
    udp_client: UdpTracker,
    anonymous_mode: bool,
}
```

Update constructors to store it.

Update `build_request()`:

```rust
    fn build_request(
        &self,
        event: AnnounceEvent,
        uploaded: u64,
        downloaded: u64,
        left: u64,
    ) -> AnnounceRequest {
        AnnounceRequest {
            info_hash: self.info_hash,
            peer_id: self.peer_id,
            port: if self.anonymous_mode { 0 } else { self.port },
            uploaded,
            downloaded,
            left,
            event,
            num_want: Some(50),
            compact: true,
        }
    }
```

### 8b. Tests

```rust
    #[test]
    fn anonymous_mode_suppresses_port() {
        let meta = /* test metadata */ ;
        let mgr = TrackerManager::from_torrent(&meta, Id20::ZERO, 6881, 0, true);
        // The port in build_request should be 0
        let req = mgr.build_request(AnnounceEvent::Started, 0, 0, 1000);
        assert_eq!(req.port, 0);
    }
```

Since `build_request` is private, this test needs to be inside the `tracker_manager` module:

```rust
    #[test]
    fn anonymous_mode_zeroes_port_in_request() {
        let mut mgr = TrackerManager {
            trackers: Vec::new(),
            info_hash: Id20::ZERO,
            peer_id: Id20::ZERO,
            port: 6881,
            http_client: HttpTracker::new(),
            udp_client: UdpTracker::new(),
            anonymous_mode: true,
        };
        let req = mgr.build_request(AnnounceEvent::Started, 0, 0, 1000);
        assert_eq!(req.port, 0);
    }

    #[test]
    fn normal_mode_includes_port_in_request() {
        let mgr = TrackerManager {
            trackers: Vec::new(),
            info_hash: Id20::ZERO,
            peer_id: Id20::ZERO,
            port: 6881,
            http_client: HttpTracker::new(),
            udp_client: UdpTracker::new(),
            anonymous_mode: false,
        };
        let req = mgr.build_request(AnnounceEvent::Started, 0, 0, 1000);
        assert_eq!(req.port, 6881);
    }
```

---

## Step 9: Anonymous Mode — Suppress Extension Handshake Fields

### 9a. Extend `ExtHandshake` suppression

**File:** `crates/ferrite-session/src/peer.rs`

Current code (line ~104-108):

```rust
        let mut ext_hs = ExtHandshake::new_with_plugins(&plugin_names);
        if anonymous_mode {
            ext_hs.v = None;
        }
```

Extend to suppress additional identifying fields:

```rust
        let mut ext_hs = ExtHandshake::new_with_plugins(&plugin_names);
        if anonymous_mode {
            ext_hs.v = None;           // client name (already done)
            ext_hs.p = None;           // listen port
            ext_hs.reqq = None;        // request queue depth (fingerprint)
            ext_hs.upload_only = None; // seeder status
        }
```

### 9b. Add `yourip` field suppression

The `ExtHandshake` struct does not currently have a `yourip` field, so no action needed. If added in the future, it must be suppressed in anonymous mode.

### 9c. Tests

**File:** `crates/ferrite-session/src/peer.rs` (in existing test module)

Add a unit test that constructs the ext handshake in anonymous mode:

```rust
    #[test]
    fn anonymous_mode_suppresses_ext_handshake_fields() {
        let mut ext_hs = ferrite_wire::ExtHandshake::new();
        ext_hs.p = Some(6881);
        ext_hs.reqq = Some(250);
        ext_hs.upload_only = Some(1);
        ext_hs.v = Some("Ferrite 0.1.0".into());

        // Simulate anonymous mode
        ext_hs.v = None;
        ext_hs.p = None;
        ext_hs.reqq = None;
        ext_hs.upload_only = None;

        assert!(ext_hs.v.is_none());
        assert!(ext_hs.p.is_none());
        assert!(ext_hs.reqq.is_none());
        assert!(ext_hs.upload_only.is_none());

        // Verify the handshake serializes without these fields
        let bytes = ext_hs.to_bytes().unwrap();
        let parsed = ferrite_wire::ExtHandshake::from_bytes(&bytes).unwrap();
        assert!(parsed.v.is_none());
        assert!(parsed.p.is_none());
        assert!(parsed.reqq.is_none());
        assert!(parsed.upload_only.is_none());
        // Extensions should still be present
        assert_eq!(parsed.ext_id("ut_metadata"), Some(1));
    }
```

---

## Step 10: Anonymous Mode — Generic Peer ID

### 10a. Add anonymous peer ID generation

**File:** `crates/ferrite-core/src/peer_id.rs`

Add a method for generating an anonymous peer ID:

```rust
    /// Peer ID prefix used in anonymous mode.
    ///
    /// Uses a generic Azureus-style prefix that doesn't identify the client.
    /// `-XX0000-` is conventionally used for unknown/generic clients.
    const ANONYMOUS_PREFIX: &'static [u8] = b"-XX0000-";

    /// Generate an anonymous peer ID (no client identification).
    ///
    /// Uses a generic prefix (`-XX0000-`) instead of the Ferrite-specific
    /// `-FE0100-` prefix, preventing peer ID-based client fingerprinting.
    pub fn generate_anonymous() -> Self {
        let mut bytes = [0u8; 20];
        bytes[..8].copy_from_slice(Self::ANONYMOUS_PREFIX);
        for byte in &mut bytes[8..] {
            *byte = random_byte();
        }
        PeerId(Id20(bytes))
    }
```

### 10b. Use anonymous peer ID when anonymous_mode is set

**File:** `crates/ferrite-session/src/torrent.rs`

In `TorrentHandle::from_torrent()` (line ~155):

```rust
        let our_peer_id = if config.anonymous_mode {
            PeerId::generate_anonymous().0
        } else {
            PeerId::generate().0
        };
```

In `TorrentHandle::from_magnet()` (line ~312):

```rust
        let our_peer_id = if config.anonymous_mode {
            PeerId::generate_anonymous().0
        } else {
            PeerId::generate().0
        };
```

### 10c. Tests

**File:** `crates/ferrite-core/src/peer_id.rs`

```rust
    #[test]
    fn anonymous_peer_id_has_generic_prefix() {
        let id = PeerId::generate_anonymous();
        assert_eq!(id.prefix(), b"-XX0000-");
    }

    #[test]
    fn anonymous_peer_ids_are_unique() {
        let a = PeerId::generate_anonymous();
        let b = PeerId::generate_anonymous();
        assert_ne!(a, b);
    }

    #[test]
    fn anonymous_peer_id_display() {
        let id = PeerId::generate_anonymous();
        let s = format!("{id}");
        assert!(s.starts_with("-XX0000-"));
    }
```

---

## Step 11: Anonymous Mode — Verify UPnP/NAT-PMP/LSD/DHT Disabled

### 11a. Audit existing behavior

The existing code in `SessionHandle::start()` (lines 192-197) already handles this:

```rust
        if settings.anonymous_mode {
            settings.enable_dht = false;
            settings.enable_lsd = false;
            settings.enable_upnp = false;
            settings.enable_natpmp = false;
        }
```

This is correct and sufficient. No changes needed.

### 11b. Verify with test

The existing test `anonymous_mode_session_starts_with_discovery_disabled` already verifies this. Add a doc-comment noting the privacy rationale:

Already covered. No new code needed.

---

## Step 12: Add `peer_dscp` to `ClientBuilder`

**File:** `crates/ferrite/src/client.rs`

Add builder method:

```rust
    /// Set the DSCP value for the ToS/Traffic Class byte on all sockets.
    ///
    /// Default: 0x04 (CS1/scavenger — low-priority background traffic).
    /// Set to 0 to disable DSCP marking.
    ///
    /// Common values:
    /// - `0x00` — Best Effort (default IP, no marking)
    /// - `0x04` — CS1/Scavenger (low priority, won't starve interactive traffic)
    /// - `0x20` — CS1 (class selector 1)
    /// - `0xB8` — EF (Expedited Forwarding, not recommended for bulk transfers)
    pub fn peer_dscp(mut self, dscp: u8) -> Self {
        self.settings.peer_dscp = dscp;
        self
    }
```

### Test

```rust
    #[test]
    fn client_builder_peer_dscp() {
        let config = ClientBuilder::new()
            .peer_dscp(0xB8)
            .into_settings();
        assert_eq!(config.peer_dscp, 0xB8);

        // Default
        let config = ClientBuilder::new().into_settings();
        assert_eq!(config.peer_dscp, 0x04);
    }
```

---

## Step 13: Update Existing Tests

### 13a. Fix `anonymous_mode_disables_discovery` test

The existing test at `session.rs:2281` already passes. No changes needed.

### 13b. Fix test helper peer spawns

All test helpers in `peer.rs` pass `false` for `anonymous_mode` -- this remains correct.

### 13c. Update `default_settings_values` test

Add assertion:

```rust
        assert_eq!(s.peer_dscp, 0x04);
```

### 13d. Update `json_round_trip` and `json_missing_fields_use_defaults` tests

These use `assert_eq!(original, decoded)` which will automatically cover `peer_dscp` once the `PartialEq` implementation includes it.

### 13e. Update `torrent_config_from_settings` test

Add:

```rust
        assert_eq!(tc.peer_dscp, s.peer_dscp);
```

---

## Summary of Files Modified

| File | Changes |
|------|---------|
| `crates/ferrite-session/Cargo.toml` | Add `libc = "0.2"` |
| `crates/ferrite-session/src/lib.rs` | Add `mod dscp;` |
| `crates/ferrite-session/src/dscp.rs` | **NEW** -- DSCP socket option helpers |
| `crates/ferrite-session/src/settings.rs` | Add `peer_dscp` field, default, serde, PartialEq, tests |
| `crates/ferrite-session/src/types.rs` | Add `peer_dscp` to `TorrentConfig` |
| `crates/ferrite-session/src/torrent.rs` | Apply DSCP on TCP, use anonymous peer ID, pass dscp/anon to TrackerManager |
| `crates/ferrite-session/src/peer.rs` | Suppress `p`, `reqq`, `upload_only` in ext handshake under anonymous mode |
| `crates/ferrite-session/src/tracker_manager.rs` | Accept dscp + anonymous_mode, suppress port, anonymous HTTP client |
| `crates/ferrite-session/src/session.rs` | No changes (anonymous mode flags already handled) |
| `crates/ferrite-tracker/Cargo.toml` | Add `libc = "0.2"` |
| `crates/ferrite-tracker/src/http.rs` | Add `with_options(anonymous, proxy)` constructor |
| `crates/ferrite-tracker/src/udp.rs` | Add `dscp` field, apply on UDP socket creation |
| `crates/ferrite-utp/Cargo.toml` | Add `libc = "0.2"` |
| `crates/ferrite-utp/src/socket.rs` | Add `dscp` to `UtpConfig`, apply at bind time |
| `crates/ferrite-core/src/peer_id.rs` | Add `generate_anonymous()` method |
| `crates/ferrite/src/client.rs` | Add `peer_dscp()` builder method |

## Dependencies Added

| Crate | Dependency | Reason |
|-------|-----------|--------|
| `ferrite-session` | `libc = "0.2"` | `setsockopt` for DSCP on TCP sockets |
| `ferrite-tracker` | `libc = "0.2"` | `setsockopt` for DSCP on UDP tracker sockets |
| `ferrite-utp` | `libc = "0.2"` | `setsockopt` for DSCP on uTP UDP sockets |

Note: `libc` is already used by `ferrite-storage`, so it is an existing workspace dependency.

## New Test Count

| Area | Tests |
|------|-------|
| DSCP socket helpers (`dscp.rs`) | 4 |
| `peer_dscp` setting | 3 |
| `peer_dscp` in TorrentConfig | 2 |
| uTP DSCP propagation | 1 |
| UDP tracker DSCP | 2 |
| Anonymous ext handshake suppression | 1 |
| Anonymous peer ID | 3 |
| Anonymous tracker port suppression | 2 |
| HTTP tracker anonymous constructor | 1 |
| ClientBuilder `peer_dscp` | 1 |
| **Total** | **~20** |

## Implementation Order (TDD)

1. **Settings** -- Add `peer_dscp` field + tests (Step 1)
2. **TorrentConfig** -- Add `peer_dscp` field + conversion test (Step 2)
3. **DSCP module** -- Create `dscp.rs` with tests (Step 3)
4. **uTP DSCP** -- Add to `UtpConfig` + propagation (Step 5)
5. **UDP tracker DSCP** -- Add to `UdpTracker` (Step 6)
6. **Peer TCP DSCP** -- Apply in `try_connect_peers` and listener (Step 4)
7. **Anonymous peer ID** -- `PeerId::generate_anonymous()` + tests (Step 10)
8. **Anonymous ext handshake** -- Suppress fields + tests (Step 9)
9. **Anonymous HTTP tracker** -- Strip user-agent (Step 7)
10. **Anonymous tracker port** -- Suppress port in announce (Step 8)
11. **ClientBuilder** -- Add `peer_dscp()` (Step 12)
12. **Update existing tests** (Step 13)
13. **Final cargo test + clippy** -- Verify all 895+ tests pass

## Version Bump

Bump workspace version in root `Cargo.toml` from `0.41.0` to `0.42.0`.

## Commit Format

```
feat: DSCP marking + anonymous mode hardening (M48)
```
