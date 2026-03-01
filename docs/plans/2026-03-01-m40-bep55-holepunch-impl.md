# M40: BEP 55 — Holepunch Extension — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable NAT traversal between two NATed peers by relaying holepunch messages through a shared third-party peer, per BEP 55. Both TCP and uTP simultaneous-open are supported.

**Architecture:** The holepunch protocol operates as a BEP 10 extension (`ut_holepunch`, built-in ID 4). It introduces three binary message types (Rendezvous, Connect, Error) with a compact wire format. The `PeerTask` handles wire-level parsing/serialization and forwards events to the `TorrentActor`, which contains the relay logic (forwarding Connect to both parties) and the initiator logic (detecting NATed targets, selecting relay peers, and initiating simultaneous connect). A new `PeerState` field tracks whether each peer supports holepunching and whether they appear to be behind a NAT.

**Tech Stack:** Rust edition 2024, bytes, tokio, ferrite-wire, ferrite-session

---

## Task 1: Wire-Level Holepunch Message Types (`ferrite-wire`)

**Files:**
- Create: `crates/ferrite-wire/src/holepunch.rs`
- Modify: `crates/ferrite-wire/src/lib.rs:1-17`
- Modify: `crates/ferrite-wire/src/extended.rs:33-49` (ExtHandshake::new)

**Step 1: Write tests for HolepunchMessage encoding/decoding**

Create `crates/ferrite-wire/src/holepunch.rs` with the following complete content:

```rust
//! BEP 55: Holepunch Extension message types.
//!
//! Binary message format (all big-endian):
//! - msg_type:   u8  (0x00=Rendezvous, 0x01=Connect, 0x02=Error)
//! - addr_type:  u8  (0x00=IPv4, 0x01=IPv6)
//! - addr:       4 bytes (IPv4) or 16 bytes (IPv6)
//! - port:       u16
//! - err_code:   u32 (0 for non-error messages)

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::{Error, Result};

/// Holepunch message type (BEP 55).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HolepunchMsgType {
    /// Initiator → Relay: "please connect me to this peer."
    Rendezvous = 0x00,
    /// Relay → Both parties: "initiate simultaneous connect to this peer."
    Connect = 0x01,
    /// Relay → Initiator: "cannot complete the rendezvous."
    Error = 0x02,
}

/// Holepunch error codes (BEP 55).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum HolepunchError {
    /// The target endpoint is invalid.
    NoSuchPeer = 1,
    /// The relay is not connected to the target peer.
    NotConnected = 2,
    /// The target peer does not support the holepunch extension.
    NoSupport = 3,
    /// The target is the relay peer itself.
    NoSelf = 4,
}

impl HolepunchError {
    /// Parse an error code from its wire representation.
    pub fn from_u32(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::NoSuchPeer),
            2 => Some(Self::NotConnected),
            3 => Some(Self::NoSupport),
            4 => Some(Self::NoSelf),
            _ => None,
        }
    }
}

impl std::fmt::Display for HolepunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchPeer => write!(f, "no such peer"),
            Self::NotConnected => write!(f, "not connected to target"),
            Self::NoSupport => write!(f, "target does not support holepunch"),
            Self::NoSelf => write!(f, "cannot holepunch to self"),
        }
    }
}

/// A parsed BEP 55 holepunch extension message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolepunchMessage {
    /// Message type.
    pub msg_type: HolepunchMsgType,
    /// Target/subject peer address.
    pub addr: SocketAddr,
    /// Error code (meaningful only when `msg_type == Error`).
    pub error_code: u32,
}

impl HolepunchMessage {
    /// Create a Rendezvous message requesting the relay connect us to `target`.
    pub fn rendezvous(target: SocketAddr) -> Self {
        Self {
            msg_type: HolepunchMsgType::Rendezvous,
            addr: target,
            error_code: 0,
        }
    }

    /// Create a Connect message telling a peer to initiate a connection to `addr`.
    pub fn connect(addr: SocketAddr) -> Self {
        Self {
            msg_type: HolepunchMsgType::Connect,
            addr,
            error_code: 0,
        }
    }

    /// Create an Error message for a failed rendezvous.
    pub fn error(addr: SocketAddr, error: HolepunchError) -> Self {
        Self {
            msg_type: HolepunchMsgType::Error,
            addr,
            error_code: error as u32,
        }
    }

    /// Wire size of this message in bytes.
    fn wire_size(&self) -> usize {
        // msg_type(1) + addr_type(1) + addr(4|16) + port(2) + err_code(4)
        let addr_len = match self.addr.ip() {
            IpAddr::V4(_) => 4,
            IpAddr::V6(_) => 16,
        };
        1 + 1 + addr_len + 2 + 4
    }

    /// Serialize to binary payload (without the BEP 10 extension header).
    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.wire_size());
        buf.put_u8(self.msg_type as u8);
        match self.addr.ip() {
            IpAddr::V4(ip) => {
                buf.put_u8(0x00);
                buf.put_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                buf.put_u8(0x01);
                buf.put_slice(&ip.octets());
            }
        }
        buf.put_u16(self.addr.port());
        buf.put_u32(self.error_code);
        buf.freeze()
    }

    /// Parse from binary payload (after BEP 10 extension header is stripped).
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 2 {
            return Err(Error::InvalidExtended(
                "holepunch message too short".into(),
            ));
        }

        let msg_type = match data[0] {
            0x00 => HolepunchMsgType::Rendezvous,
            0x01 => HolepunchMsgType::Connect,
            0x02 => HolepunchMsgType::Error,
            n => {
                return Err(Error::InvalidExtended(format!(
                    "unknown holepunch msg_type {n:#04x}"
                )));
            }
        };

        let addr_type = data[1];
        let (addr_len, expected_total) = match addr_type {
            0x00 => (4usize, 12usize),  // 1+1+4+2+4
            0x01 => (16usize, 24usize), // 1+1+16+2+4
            n => {
                return Err(Error::InvalidExtended(format!(
                    "unknown holepunch addr_type {n:#04x}"
                )));
            }
        };

        if data.len() < expected_total {
            return Err(Error::InvalidExtended(format!(
                "holepunch message too short: need {expected_total} bytes, got {}",
                data.len()
            )));
        }

        let addr_start = 2;
        let ip: IpAddr = if addr_type == 0x00 {
            let o = &data[addr_start..addr_start + 4];
            IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], o[3]))
        } else {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&data[addr_start..addr_start + 16]);
            IpAddr::V6(Ipv6Addr::from(octets))
        };

        let port_start = addr_start + addr_len;
        let port = u16::from_be_bytes([data[port_start], data[port_start + 1]]);

        let err_start = port_start + 2;
        let error_code = u32::from_be_bytes([
            data[err_start],
            data[err_start + 1],
            data[err_start + 2],
            data[err_start + 3],
        ]);

        Ok(HolepunchMessage {
            msg_type,
            addr: SocketAddr::new(ip, port),
            error_code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendezvous_ipv4_round_trip() {
        let addr: SocketAddr = "192.168.1.100:6881".parse().unwrap();
        let msg = HolepunchMessage::rendezvous(addr);
        assert_eq!(msg.msg_type, HolepunchMsgType::Rendezvous);
        assert_eq!(msg.addr, addr);
        assert_eq!(msg.error_code, 0);

        let bytes = msg.to_bytes();
        assert_eq!(bytes.len(), 12); // 1+1+4+2+4

        let parsed = HolepunchMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn connect_ipv4_round_trip() {
        let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let msg = HolepunchMessage::connect(addr);
        assert_eq!(msg.msg_type, HolepunchMsgType::Connect);

        let bytes = msg.to_bytes();
        let parsed = HolepunchMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn error_ipv4_round_trip() {
        let addr: SocketAddr = "172.16.0.5:51413".parse().unwrap();
        let msg = HolepunchMessage::error(addr, HolepunchError::NotConnected);
        assert_eq!(msg.msg_type, HolepunchMsgType::Error);
        assert_eq!(msg.error_code, 2);

        let bytes = msg.to_bytes();
        let parsed = HolepunchMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn rendezvous_ipv6_round_trip() {
        let addr: SocketAddr = "[2001:db8::1]:6881".parse().unwrap();
        let msg = HolepunchMessage::rendezvous(addr);

        let bytes = msg.to_bytes();
        assert_eq!(bytes.len(), 24); // 1+1+16+2+4

        let parsed = HolepunchMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn connect_ipv6_round_trip() {
        let addr: SocketAddr = "[::1]:8080".parse().unwrap();
        let msg = HolepunchMessage::connect(addr);

        let bytes = msg.to_bytes();
        let parsed = HolepunchMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn error_ipv6_all_error_codes() {
        let addr: SocketAddr = "[fe80::1]:9999".parse().unwrap();

        for (code, variant) in [
            (1, HolepunchError::NoSuchPeer),
            (2, HolepunchError::NotConnected),
            (3, HolepunchError::NoSupport),
            (4, HolepunchError::NoSelf),
        ] {
            let msg = HolepunchMessage::error(addr, variant);
            assert_eq!(msg.error_code, code);

            let bytes = msg.to_bytes();
            let parsed = HolepunchMessage::from_bytes(&bytes).unwrap();
            assert_eq!(parsed.error_code, code);
            assert_eq!(HolepunchError::from_u32(code), Some(variant));
        }
    }

    #[test]
    fn unknown_msg_type_rejected() {
        let mut data = HolepunchMessage::rendezvous("1.2.3.4:80".parse().unwrap())
            .to_bytes()
            .to_vec();
        data[0] = 0x03; // invalid msg_type
        assert!(HolepunchMessage::from_bytes(&data).is_err());
    }

    #[test]
    fn unknown_addr_type_rejected() {
        let mut data = HolepunchMessage::rendezvous("1.2.3.4:80".parse().unwrap())
            .to_bytes()
            .to_vec();
        data[1] = 0x02; // invalid addr_type
        assert!(HolepunchMessage::from_bytes(&data).is_err());
    }

    #[test]
    fn too_short_rejected() {
        assert!(HolepunchMessage::from_bytes(&[]).is_err());
        assert!(HolepunchMessage::from_bytes(&[0x00]).is_err());
        // IPv4 needs 12 bytes, provide only 8
        assert!(HolepunchMessage::from_bytes(&[0x00, 0x00, 1, 2, 3, 4, 0, 80]).is_err());
    }

    #[test]
    fn ipv6_too_short_rejected() {
        // addr_type=0x01 (IPv6) but only 12 bytes total (need 24)
        let data = [0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(HolepunchMessage::from_bytes(&data).is_err());
    }

    #[test]
    fn error_code_unknown_parses_as_none() {
        assert!(HolepunchError::from_u32(0).is_none());
        assert!(HolepunchError::from_u32(5).is_none());
        assert!(HolepunchError::from_u32(u32::MAX).is_none());
    }

    #[test]
    fn error_display() {
        assert_eq!(HolepunchError::NoSuchPeer.to_string(), "no such peer");
        assert_eq!(HolepunchError::NotConnected.to_string(), "not connected to target");
        assert_eq!(HolepunchError::NoSupport.to_string(), "target does not support holepunch");
        assert_eq!(HolepunchError::NoSelf.to_string(), "cannot holepunch to self");
    }

    #[test]
    fn wire_size_ipv4() {
        let msg = HolepunchMessage::rendezvous("1.2.3.4:80".parse().unwrap());
        assert_eq!(msg.wire_size(), 12);
    }

    #[test]
    fn wire_size_ipv6() {
        let msg = HolepunchMessage::rendezvous("[::1]:80".parse().unwrap());
        assert_eq!(msg.wire_size(), 24);
    }

    #[test]
    fn exact_wire_bytes_ipv4_rendezvous() {
        let addr: SocketAddr = "192.168.1.100:6881".parse().unwrap();
        let msg = HolepunchMessage::rendezvous(addr);
        let bytes = msg.to_bytes();

        assert_eq!(bytes[0], 0x00); // msg_type = Rendezvous
        assert_eq!(bytes[1], 0x00); // addr_type = IPv4
        assert_eq!(&bytes[2..6], &[192, 168, 1, 100]); // addr
        assert_eq!(u16::from_be_bytes([bytes[6], bytes[7]]), 6881); // port
        assert_eq!(u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]), 0); // err_code
    }
}
```

**Step 2: Register the holepunch module in `ferrite-wire/src/lib.rs`**

Add the module declaration and re-export. In `crates/ferrite-wire/src/lib.rs`, add:

```rust
mod holepunch;
```

after the `mod message;` line, and add to the re-exports:

```rust
pub use holepunch::{HolepunchError, HolepunchMessage, HolepunchMsgType};
```

after the `pub use message::...` line.

**Step 3: Add `ut_holepunch` as built-in extension ID 4 in `ExtHandshake::new()`**

In `crates/ferrite-wire/src/extended.rs`, modify `ExtHandshake::new()` to include `ut_holepunch`:

```rust
pub fn new() -> Self {
    let mut m = BTreeMap::new();
    m.insert("ut_metadata".into(), 1);
    m.insert("ut_pex".into(), 2);
    m.insert("lt_trackers".into(), 3);
    m.insert("ut_holepunch".into(), 4);

    ExtHandshake {
        m,
        v: Some("Ferrite 0.1.0".into()),
        p: None,
        reqq: Some(250),
        metadata_size: None,
        upload_only: None,
    }
}
```

**Step 4: Update the `new_with_plugins` doc comment and existing tests**

The test `ext_handshake_no_plugins` asserts `hs.m.len() == 3`. Update it to `4` since we added `ut_holepunch`.

In `crates/ferrite-wire/src/extended.rs`, update:
- Test `ext_handshake_no_plugins`: change `assert_eq!(hs.m.len(), 3)` to `assert_eq!(hs.m.len(), 4)`
- Add a test for `ut_holepunch` in `ext_handshake_ext_id_lookup`:

```rust
assert_eq!(hs.ext_id("ut_holepunch"), Some(4));
```

**Step 5: Run tests**

```bash
cargo test -p ferrite-wire
```

All holepunch wire-level encoding/decoding tests should pass.

---

## Task 2: Settings and Alerts (`ferrite-session`)

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs:117-283` (Settings struct)
- Modify: `crates/ferrite-session/src/alert.rs:52-137` (AlertKind)
- Modify: `crates/ferrite-session/src/types.rs:14-66` (TorrentConfig)

**Step 1: Add `enable_holepunch` to Settings**

In `crates/ferrite-session/src/settings.rs`, add a new field in the "Protocol features" section, after the `enable_web_seed` field:

```rust
    #[serde(default = "default_true")]
    pub enable_holepunch: bool,
```

Add it to `Default for Settings`:

```rust
    enable_holepunch: true,
```

Add it to the `PartialEq` implementation (after the `enable_web_seed` comparison):

```rust
    && self.enable_holepunch == other.enable_holepunch
```

**Step 2: Add `enable_holepunch` to TorrentConfig**

In `crates/ferrite-session/src/types.rs`, add to the `TorrentConfig` struct:

```rust
    /// BEP 55: enable holepunch extension for NAT traversal.
    pub enable_holepunch: bool,
```

Add to `Default for TorrentConfig`:

```rust
    enable_holepunch: true,
```

Add to `From<&Settings> for TorrentConfig`:

```rust
    enable_holepunch: s.enable_holepunch,
```

**Step 3: Add holepunch alerts**

In `crates/ferrite-session/src/alert.rs`, add to `AlertKind` enum (after the `InconsistentHashes` variant):

```rust
    // ── Holepunch (BEP 55, M40) ──
    /// A holepunch connection attempt succeeded.
    HolepunchSucceeded {
        info_hash: Id20,
        /// The peer we connected to via holepunch.
        addr: SocketAddr,
    },
    /// A holepunch connection attempt failed.
    HolepunchFailed {
        info_hash: Id20,
        /// The peer we tried to holepunch to.
        addr: SocketAddr,
        /// Error code from the relay, if any.
        error_code: Option<u32>,
        /// Human-readable reason.
        message: String,
    },
```

In the `category()` match, add:

```rust
    // HOLEPUNCH (PEER)
    HolepunchSucceeded { .. } => AlertCategory::PEER,
    HolepunchFailed { .. } => AlertCategory::PEER | AlertCategory::ERROR,
```

**Step 4: Add settings and alert tests**

Add to `crates/ferrite-session/src/settings.rs` tests:

```rust
    #[test]
    fn enable_holepunch_default_true() {
        let s = Settings::default();
        assert!(s.enable_holepunch);
    }

    #[test]
    fn enable_holepunch_json_round_trip() {
        let json = r#"{"enable_holepunch": false}"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert!(!s.enable_holepunch);
        let encoded = serde_json::to_string(&s).unwrap();
        let roundtrip: Settings = serde_json::from_str(&encoded).unwrap();
        assert!(!roundtrip.enable_holepunch);
    }
```

Add to `crates/ferrite-session/src/alert.rs` tests:

```rust
    #[test]
    fn holepunch_succeeded_has_peer_category() {
        let alert = Alert::new(AlertKind::HolepunchSucceeded {
            info_hash: Id20::from([0u8; 20]),
            addr: "1.2.3.4:6881".parse().unwrap(),
        });
        assert!(alert.category().contains(AlertCategory::PEER));
    }

    #[test]
    fn holepunch_failed_has_peer_and_error_category() {
        let alert = Alert::new(AlertKind::HolepunchFailed {
            info_hash: Id20::from([0u8; 20]),
            addr: "1.2.3.4:6881".parse().unwrap(),
            error_code: Some(2),
            message: "not connected".into(),
        });
        assert!(alert.category().contains(AlertCategory::PEER));
        assert!(alert.category().contains(AlertCategory::ERROR));
    }
```

Add to `crates/ferrite-session/src/types.rs` tests:

```rust
    #[test]
    fn torrent_config_holepunch_default() {
        let cfg = TorrentConfig::default();
        assert!(cfg.enable_holepunch);
    }
```

**Step 5: Run tests**

```bash
cargo test -p ferrite-session
```

---

## Task 3: PeerEvent/PeerCommand Holepunch Variants (`ferrite-session`)

**Files:**
- Modify: `crates/ferrite-session/src/types.rs:171-295` (PeerEvent, PeerCommand)

**Step 1: Add holepunch PeerEvent variants**

In `crates/ferrite-session/src/types.rs`, add to the `PeerEvent` enum:

```rust
    /// BEP 55: Received a Rendezvous request (we are the relay).
    HolepunchRendezvous {
        /// The peer that sent us the Rendezvous message.
        peer_addr: SocketAddr,
        /// The target peer address they want to connect to.
        target: SocketAddr,
    },
    /// BEP 55: Received a Connect message (we should initiate simultaneous connect).
    HolepunchConnect {
        /// The relay peer that sent us the Connect message.
        peer_addr: SocketAddr,
        /// The peer we should simultaneously connect to.
        target: SocketAddr,
    },
    /// BEP 55: Received an Error message from the relay.
    HolepunchError {
        /// The relay peer that sent the error.
        peer_addr: SocketAddr,
        /// The target peer we requested the rendezvous for.
        target: SocketAddr,
        /// The BEP 55 error code.
        error_code: u32,
    },
```

**Step 2: Add holepunch PeerCommand variants**

In `crates/ferrite-session/src/types.rs`, add to the `PeerCommand` enum:

```rust
    /// BEP 55: Send a holepunch message to this peer.
    SendHolepunch(ferrite_wire::HolepunchMessage),
```

**Step 3: Run tests**

```bash
cargo test -p ferrite-session
```

---

## Task 4: PeerTask Holepunch Wire Integration (`ferrite-session/src/peer.rs`)

**Files:**
- Modify: `crates/ferrite-session/src/peer.rs`

This task wires up the holepunch messages in the peer's message loop. Changes are spread across several functions in `peer.rs`.

**Step 1: Add `peer_ut_holepunch` and `our_ut_holepunch` tracking variables**

In the `run_peer()` function, after the existing `peer_lt_trackers` / `our_lt_trackers` variables (around line 164-172), add:

```rust
    let mut peer_ut_holepunch: Option<u8> = None;
    // ...
    let our_ut_holepunch: Option<u8> = our_ext.ext_id("ut_holepunch");
```

**Step 2: Update `handle_message` signature**

Add `peer_ut_holepunch: &mut Option<u8>` and `our_ut_holepunch: Option<u8>` parameters to `handle_message()`. Update the call site in the `run_peer()` main loop and all the parameter forwarding.

**Step 3: Capture peer's `ut_holepunch` ID in ext handshake handling**

In the `Message::Extended { ext_id: 0, payload }` arm of `handle_message()`, after `*peer_lt_trackers = ext_hs.ext_id("lt_trackers");`, add:

```rust
    *peer_ut_holepunch = ext_hs.ext_id("ut_holepunch");
```

**Step 4: Handle incoming holepunch messages**

In the `Message::Extended { ext_id, payload }` arm (the non-handshake branch), after the `lt_trackers` check and before the plugin dispatch, add:

```rust
    } else if Some(ext_id) == our_ut_holepunch {
        handle_holepunch_message(addr, &payload, event_tx).await?;
    }
```

**Step 5: Implement `handle_holepunch_message`**

Add a new function after `handle_lt_trackers_message`:

```rust
/// Handle an incoming ut_holepunch extension message (BEP 55).
async fn handle_holepunch_message(
    addr: SocketAddr,
    payload: &[u8],
    event_tx: &mpsc::Sender<PeerEvent>,
) -> crate::Result<()> {
    use ferrite_wire::{HolepunchMessage, HolepunchMsgType};

    let msg = HolepunchMessage::from_bytes(payload)?;
    match msg.msg_type {
        HolepunchMsgType::Rendezvous => {
            event_tx
                .send(PeerEvent::HolepunchRendezvous {
                    peer_addr: addr,
                    target: msg.addr,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        HolepunchMsgType::Connect => {
            event_tx
                .send(PeerEvent::HolepunchConnect {
                    peer_addr: addr,
                    target: msg.addr,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        HolepunchMsgType::Error => {
            event_tx
                .send(PeerEvent::HolepunchError {
                    peer_addr: addr,
                    target: msg.addr,
                    error_code: msg.error_code,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
    }
    Ok(())
}
```

**Step 6: Handle `SendHolepunch` PeerCommand**

In `handle_command()`, add a new arm before `PeerCommand::Shutdown`:

```rust
    PeerCommand::SendHolepunch(hp_msg) => {
        let ext_id = peer_ut_holepunch.ok_or_else(|| {
            crate::Error::Connection("peer does not support ut_holepunch".into())
        })?;
        let payload = hp_msg.to_bytes();
        Message::Extended { ext_id, payload }
    }
```

This requires adding `peer_ut_holepunch: Option<u8>` as a parameter to `handle_command()`. Update the signature and call site.

**Step 7: Update `handle_message` and `handle_command` call sites in the main loop**

Pass the new parameters through from the `run_peer` function's local variables.

**Step 8: Add tests**

Add to the existing `peer.rs` test module:

```rust
    #[tokio::test]
    async fn holepunch_rendezvous_event() {
        use ferrite_wire::HolepunchMessage;

        let (event_tx, mut event_rx) = mpsc::channel(16);
        let addr = test_addr();
        let target: SocketAddr = "10.0.0.1:8080".parse().unwrap();

        let msg = HolepunchMessage::rendezvous(target);
        let payload = msg.to_bytes();

        handle_holepunch_message(addr, &payload, &event_tx)
            .await
            .unwrap();

        match event_rx.recv().await.unwrap() {
            PeerEvent::HolepunchRendezvous { peer_addr, target: t } => {
                assert_eq!(peer_addr, addr);
                assert_eq!(t, target);
            }
            other => panic!("expected HolepunchRendezvous, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn holepunch_connect_event() {
        use ferrite_wire::HolepunchMessage;

        let (event_tx, mut event_rx) = mpsc::channel(16);
        let addr = test_addr();
        let target: SocketAddr = "10.0.0.2:9000".parse().unwrap();

        let msg = HolepunchMessage::connect(target);
        let payload = msg.to_bytes();

        handle_holepunch_message(addr, &payload, &event_tx)
            .await
            .unwrap();

        match event_rx.recv().await.unwrap() {
            PeerEvent::HolepunchConnect { peer_addr, target: t } => {
                assert_eq!(peer_addr, addr);
                assert_eq!(t, target);
            }
            other => panic!("expected HolepunchConnect, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn holepunch_error_event() {
        use ferrite_wire::{HolepunchError, HolepunchMessage};

        let (event_tx, mut event_rx) = mpsc::channel(16);
        let addr = test_addr();
        let target: SocketAddr = "10.0.0.3:7000".parse().unwrap();

        let msg = HolepunchMessage::error(target, HolepunchError::NotConnected);
        let payload = msg.to_bytes();

        handle_holepunch_message(addr, &payload, &event_tx)
            .await
            .unwrap();

        match event_rx.recv().await.unwrap() {
            PeerEvent::HolepunchError { peer_addr, target: t, error_code } => {
                assert_eq!(peer_addr, addr);
                assert_eq!(t, target);
                assert_eq!(error_code, 2);
            }
            other => panic!("expected HolepunchError, got {other:?}"),
        }
    }
```

**Step 9: Run tests**

```bash
cargo test -p ferrite-session
```

---

## Task 5: PeerState Holepunch Tracking (`ferrite-session/src/peer_state.rs`)

**Files:**
- Modify: `crates/ferrite-session/src/peer_state.rs`

**Step 1: Add holepunch-related fields to PeerState**

Add two new fields to the `PeerState` struct:

```rust
    /// BEP 55: peer advertised `ut_holepunch` support in their extension handshake.
    pub supports_holepunch: bool,
    /// Whether this peer appears to be NATed (no incoming connections observed).
    /// Used as a heuristic for deciding when to attempt holepunch.
    pub appears_nated: bool,
```

**Step 2: Initialize in `PeerState::new()`**

Add to the constructor:

```rust
    supports_holepunch: false,
    appears_nated: false,
```

**Step 3: Run tests**

```bash
cargo test -p ferrite-session
```

---

## Task 6: TorrentActor Relay Logic (`ferrite-session/src/torrent.rs`)

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

This is the core of the holepunch protocol. The `TorrentActor` handles three roles:
1. **Relay:** when we receive a `HolepunchRendezvous`, forward `Connect` to both parties
2. **Target:** when we receive `HolepunchConnect`, attempt simultaneous connect
3. **Initiator:** when we want to connect to a NATed peer, select a relay and send `Rendezvous`

**Step 1: Handle `PeerEvent::HolepunchRendezvous` (relay logic)**

In `handle_peer_event()`, add a new arm:

```rust
    PeerEvent::HolepunchRendezvous { peer_addr, target } => {
        if self.config.enable_holepunch {
            self.handle_holepunch_rendezvous(peer_addr, target).await;
        }
    }
```

Implement the relay handler method on `TorrentActor`:

```rust
    /// BEP 55 relay logic: forward Connect messages to both parties.
    async fn handle_holepunch_rendezvous(
        &mut self,
        initiator_addr: SocketAddr,
        target_addr: SocketAddr,
    ) {
        use ferrite_wire::{HolepunchError, HolepunchMessage};

        // Validate: target is not the same as the initiator
        if initiator_addr == target_addr {
            if let Some(peer) = self.peers.get(&initiator_addr) {
                let _ = peer.cmd_tx.send(PeerCommand::SendHolepunch(
                    HolepunchMessage::error(target_addr, HolepunchError::NoSelf),
                )).await;
            }
            return;
        }

        // Check target is connected to us
        let target_peer = match self.peers.get(&target_addr) {
            Some(p) => p,
            None => {
                // Target not connected — send NotConnected error to initiator
                if let Some(peer) = self.peers.get(&initiator_addr) {
                    let _ = peer.cmd_tx.send(PeerCommand::SendHolepunch(
                        HolepunchMessage::error(target_addr, HolepunchError::NotConnected),
                    )).await;
                }
                return;
            }
        };

        // Check target supports holepunch
        if !target_peer.supports_holepunch {
            if let Some(peer) = self.peers.get(&initiator_addr) {
                let _ = peer.cmd_tx.send(PeerCommand::SendHolepunch(
                    HolepunchMessage::error(target_addr, HolepunchError::NoSupport),
                )).await;
            }
            return;
        }

        // Forward Connect to target: "connect to the initiator"
        let _ = target_peer.cmd_tx.send(PeerCommand::SendHolepunch(
            HolepunchMessage::connect(initiator_addr),
        )).await;

        // Forward Connect to initiator: "connect to the target"
        if let Some(peer) = self.peers.get(&initiator_addr) {
            let _ = peer.cmd_tx.send(PeerCommand::SendHolepunch(
                HolepunchMessage::connect(target_addr),
            )).await;
        }

        debug!(
            initiator = %initiator_addr,
            target = %target_addr,
            "relayed holepunch Connect to both parties"
        );
    }
```

**Step 2: Handle `PeerEvent::HolepunchConnect` (initiator/target receive Connect)**

In `handle_peer_event()`, add:

```rust
    PeerEvent::HolepunchConnect { peer_addr: _, target } => {
        if self.config.enable_holepunch {
            self.handle_holepunch_connect(target).await;
        }
    }
```

Implement the handler:

```rust
    /// BEP 55: received a Connect message — attempt simultaneous connect to target.
    async fn handle_holepunch_connect(&mut self, target: SocketAddr) {
        // If we're already connected to this peer, ignore
        if self.peers.contains_key(&target) {
            return;
        }

        // If at peer capacity, ignore
        if self.peers.len() >= self.config.max_peers {
            return;
        }

        // Skip banned or IP-filtered peers
        if self.ban_manager.read().unwrap().is_banned(&target.ip()) {
            return;
        }
        if self.ip_filter.read().unwrap().is_blocked(target.ip()) {
            post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerBlocked { addr: target });
            return;
        }

        debug!(%target, "holepunch: initiating simultaneous connect");

        // Spawn a connection attempt — try uTP first, then TCP
        let info_hash = self.info_hash;
        let peer_id = self.our_peer_id;
        let num_pieces = self.num_pieces;
        let event_tx = self.event_tx.clone();
        let alert_tx = self.alert_tx.clone();
        let alert_mask = Arc::clone(&self.alert_mask);
        let enable_dht = self.config.enable_dht;
        let enable_fast = self.config.enable_fast;
        let encryption_mode = self.config.encryption_mode;
        let enable_utp = self.config.enable_utp;
        let anonymous_mode = self.config.anonymous_mode;
        let info_bytes = self.meta.as_ref().and_then(|m| m.info_bytes.clone());
        let utp_socket = if target.is_ipv6() {
            self.utp_socket_v6.clone()
        } else {
            self.utp_socket.clone()
        };
        let plugins = Arc::clone(&self.plugins);

        let bitfield = if self.super_seed.is_some() {
            Bitfield::new(self.num_pieces)
        } else {
            self.chunk_tracker
                .as_ref()
                .map(|ct| ct.bitfield().clone())
                .unwrap_or_else(|| Bitfield::new(self.num_pieces))
        };

        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        self.peers.insert(target, PeerState::new(target, self.num_pieces, cmd_tx, PeerSource::Pex));
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerConnected {
            info_hash: self.info_hash,
            addr: target,
        });

        tokio::spawn(async move {
            let mut connected = false;

            // Try uTP first (simultaneous UDP holepunch)
            if enable_utp {
                if let Some(socket) = utp_socket {
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        socket.connect(target),
                    ).await {
                        Ok(Ok(stream)) => {
                            debug!(%target, "holepunch: uTP connection established");
                            post_alert(
                                &alert_tx,
                                &alert_mask,
                                AlertKind::HolepunchSucceeded { info_hash, addr: target },
                            );
                            let _ = run_peer(
                                target, stream, info_hash, peer_id, bitfield,
                                num_pieces, event_tx, cmd_rx, enable_dht, enable_fast,
                                encryption_mode, true, anonymous_mode, info_bytes, plugins,
                            ).await;
                            connected = true;
                        }
                        Ok(Err(e)) => {
                            debug!(%target, error = %e, "holepunch: uTP connect failed, trying TCP");
                        }
                        Err(_) => {
                            debug!(%target, "holepunch: uTP connect timed out, trying TCP");
                        }
                    }
                }
            }

            // Fall back to TCP simultaneous open
            if !connected {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect(target),
                ).await {
                    Ok(Ok(stream)) => {
                        debug!(%target, "holepunch: TCP connection established");
                        post_alert(
                            &alert_tx,
                            &alert_mask,
                            AlertKind::HolepunchSucceeded { info_hash, addr: target },
                        );
                        let _ = run_peer(
                            target, stream, info_hash, peer_id, bitfield,
                            num_pieces, event_tx, cmd_rx, enable_dht, enable_fast,
                            encryption_mode, true, anonymous_mode, info_bytes, plugins,
                        ).await;
                    }
                    Ok(Err(e)) => {
                        post_alert(
                            &alert_tx,
                            &alert_mask,
                            AlertKind::HolepunchFailed {
                                info_hash,
                                addr: target,
                                error_code: None,
                                message: e.to_string(),
                            },
                        );
                        let _ = event_tx.send(PeerEvent::Disconnected {
                            peer_addr: target,
                            reason: Some(format!("holepunch TCP failed: {e}")),
                        }).await;
                    }
                    Err(_) => {
                        post_alert(
                            &alert_tx,
                            &alert_mask,
                            AlertKind::HolepunchFailed {
                                info_hash,
                                addr: target,
                                error_code: None,
                                message: "holepunch TCP connect timed out".into(),
                            },
                        );
                        let _ = event_tx.send(PeerEvent::Disconnected {
                            peer_addr: target,
                            reason: Some("holepunch TCP connect timed out".into()),
                        }).await;
                    }
                }
            }
        });
    }
```

**Step 3: Handle `PeerEvent::HolepunchError`**

In `handle_peer_event()`, add:

```rust
    PeerEvent::HolepunchError { peer_addr: _, target, error_code } => {
        let message = ferrite_wire::HolepunchError::from_u32(error_code)
            .map(|e| e.to_string())
            .unwrap_or_else(|| format!("unknown error code {error_code}"));
        debug!(%target, %error_code, %message, "holepunch rendezvous failed");
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::HolepunchFailed {
            info_hash: self.info_hash,
            addr: target,
            error_code: Some(error_code),
            message,
        });
    }
```

**Step 4: Mark `supports_holepunch` when ext handshake arrives**

In the `PeerEvent::ExtHandshake` handler in `handle_peer_event()`, after the line `peer.upload_only = handshake.is_upload_only();`, add:

```rust
    peer.supports_holepunch = handshake.ext_id("ut_holepunch").is_some();
```

**Step 5: NAT detection heuristic**

Add a simple heuristic in `PeerState`: a peer is marked as `appears_nated` based on its source. Incoming connections are not NATed; outbound connections to peers discovered via tracker/DHT/PEX might be.

In the `PeerEvent::ExtHandshake` handler, after setting `supports_holepunch`, add:

```rust
    // Heuristic: peers that connected to us (incoming) are likely not NATed
    peer.appears_nated = peer.source != PeerSource::Incoming;
```

**Step 6: Holepunch initiation from TorrentActor**

Add a method that can be called when `try_connect_peers()` fails to connect to a peer (the peer appears NATed). This uses a relay peer to initiate holepunch.

```rust
    /// BEP 55: attempt holepunch to a NATed target via a connected relay peer.
    ///
    /// Selects the first connected peer that supports holepunch as a relay,
    /// then sends a Rendezvous message requesting the relay connect us to `target`.
    async fn try_holepunch(&mut self, target: SocketAddr) {
        use ferrite_wire::HolepunchMessage;

        if !self.config.enable_holepunch {
            return;
        }

        // Don't holepunch to peers we're already connected to
        if self.peers.contains_key(&target) {
            return;
        }

        // Find a relay: a connected peer that supports holepunch and is not the target
        let relay_addr = self.peers.iter()
            .find(|(addr, peer)| {
                **addr != target
                    && peer.supports_holepunch
                    && peer.ext_handshake.is_some()
            })
            .map(|(addr, _)| *addr);

        let Some(relay_addr) = relay_addr else {
            debug!(%target, "no relay peer available for holepunch");
            return;
        };

        // Send Rendezvous to the relay
        if let Some(relay) = self.peers.get(&relay_addr) {
            debug!(relay = %relay_addr, %target, "sending holepunch Rendezvous via relay");
            let _ = relay.cmd_tx.send(PeerCommand::SendHolepunch(
                HolepunchMessage::rendezvous(target),
            )).await;
        }
    }
```

**Step 7: Conditionally advertise `ut_holepunch` based on `enable_holepunch` setting**

In `ExtHandshake::new()`, we unconditionally add `ut_holepunch`. However, the `run_peer` function in `peer.rs` needs a way to suppress it when `enable_holepunch` is false in the config.

The cleanest approach: add `enable_holepunch: bool` as a parameter to `run_peer()`. When false, build the ext handshake without `ut_holepunch`. Modify the ext handshake construction in Phase 3 of `run_peer()`:

```rust
    let mut ext_hs = ExtHandshake::new_with_plugins(&plugin_names);
    if !enable_holepunch {
        ext_hs.m.remove("ut_holepunch");
    }
```

Update all call sites of `run_peer()` to pass `config.enable_holepunch` (from `TorrentConfig`).

**Step 8: Run tests**

```bash
cargo test -p ferrite-session
```

---

## Task 7: Integration Tests and Edge Cases

**Files:**
- Modify: `crates/ferrite-wire/src/holepunch.rs` (edge case tests)
- Modify: `crates/ferrite-session/src/peer.rs` (integration tests)

**Step 1: Add edge case tests to holepunch.rs**

Already included in Task 1. Verify they all pass.

**Step 2: Add relay logic unit tests**

These tests cannot easily be unit-tested without the full TorrentActor, but the individual `handle_holepunch_message` function in peer.rs was tested in Task 4. Add additional tests for the wire format edge cases:

In `crates/ferrite-wire/src/holepunch.rs`, add:

```rust
    #[test]
    fn extra_trailing_bytes_ignored() {
        // Per BEP 55, extra bytes after the message should be tolerated
        let mut data = HolepunchMessage::rendezvous("1.2.3.4:80".parse().unwrap())
            .to_bytes()
            .to_vec();
        data.push(0xFF); // extra trailing byte
        data.push(0xAA);
        let parsed = HolepunchMessage::from_bytes(&data).unwrap();
        assert_eq!(parsed.msg_type, HolepunchMsgType::Rendezvous);
        assert_eq!(parsed.addr, "1.2.3.4:80".parse().unwrap());
    }

    #[test]
    fn port_zero_accepted() {
        let msg = HolepunchMessage::rendezvous("1.2.3.4:0".parse().unwrap());
        let bytes = msg.to_bytes();
        let parsed = HolepunchMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.addr.port(), 0);
    }
```

**Step 3: Verify ext handshake interop**

In `crates/ferrite-wire/src/extended.rs`, add:

```rust
    #[test]
    fn ext_handshake_holepunch_id() {
        let hs = ExtHandshake::new();
        assert_eq!(hs.ext_id("ut_holepunch"), Some(4));
    }

    #[test]
    fn ext_handshake_holepunch_can_be_removed() {
        let mut hs = ExtHandshake::new();
        hs.m.remove("ut_holepunch");
        assert_eq!(hs.ext_id("ut_holepunch"), None);
        // Other extensions unaffected
        assert_eq!(hs.ext_id("ut_metadata"), Some(1));
        assert_eq!(hs.ext_id("ut_pex"), Some(2));
    }
```

**Step 4: Run full workspace tests**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

---

## Summary of Changes

### New Files
| File | Description |
|------|-------------|
| `crates/ferrite-wire/src/holepunch.rs` | BEP 55 message types: `HolepunchMessage`, `HolepunchMsgType`, `HolepunchError` with binary serialization |

### Modified Files
| File | Changes |
|------|---------|
| `crates/ferrite-wire/src/lib.rs` | Add `mod holepunch` + re-exports |
| `crates/ferrite-wire/src/extended.rs` | Add `ut_holepunch` as built-in ID 4 in `ExtHandshake::new()`, update test |
| `crates/ferrite-session/src/settings.rs` | Add `enable_holepunch: bool` field |
| `crates/ferrite-session/src/alert.rs` | Add `HolepunchSucceeded`, `HolepunchFailed` alert variants |
| `crates/ferrite-session/src/types.rs` | Add `enable_holepunch` to `TorrentConfig`, add `PeerEvent::HolepunchRendezvous/Connect/Error`, add `PeerCommand::SendHolepunch` |
| `crates/ferrite-session/src/peer_state.rs` | Add `supports_holepunch: bool`, `appears_nated: bool` fields |
| `crates/ferrite-session/src/peer.rs` | Wire holepunch message handling, `handle_holepunch_message()`, `SendHolepunch` command handling, `enable_holepunch` parameter |
| `crates/ferrite-session/src/torrent.rs` | Relay logic (`handle_holepunch_rendezvous`), connect logic (`handle_holepunch_connect`), error handling, NAT detection heuristic, `try_holepunch()` |

### Test Count
- `ferrite-wire/src/holepunch.rs`: 15 tests (encoding, decoding, round-trips, edge cases)
- `ferrite-wire/src/extended.rs`: 3 new tests (holepunch ext ID, removal)
- `ferrite-session/src/settings.rs`: 2 new tests
- `ferrite-session/src/alert.rs`: 2 new tests
- `ferrite-session/src/types.rs`: 1 new test
- `ferrite-session/src/peer.rs`: 3 new tests (holepunch event dispatch)
- **Total: ~26 new tests**

### Version Bump
After all tasks pass: bump workspace version in `Cargo.toml` from `0.41.0` to `0.42.0`.

### Commit
```
feat: BEP 55 holepunch extension for NAT traversal (M40)
```
