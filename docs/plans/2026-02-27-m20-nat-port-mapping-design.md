# M20: UPnP / NAT-PMP / PCP Port Mapping — Design

**Status:** Implemented
**Version:** 0.21.0
**Crate:** `ferrite-nat`

## Goal

Automatic port mapping so peers behind NAT can receive inbound connections.

## Architecture

```
SessionActor
├── NatHandle (Option) ──→ NatActor (background task)
│   ├── PCP client    ──→ UDP to gateway:5351 (RFC 6887, try first)
│   ├── NAT-PMP client ──→ UDP to gateway:5351 (RFC 6886, fallback)
│   └── UPnP client   ──→ SSDP multicast → SOAP HTTP to IGD (last resort)
```

## Protocol Priority

1. **PCP** (RFC 6887) — newest, IPv6-ready, 60-byte MAP packets
2. **NAT-PMP** (RFC 6886) — simpler predecessor, same port 5351
3. **UPnP IGD** — SSDP discovery + SOAP XML, most widely deployed

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Separate crate | `ferrite-nat` | Self-contained, follows workspace pattern |
| No external deps | From scratch | Consistent with other ferrite crates |
| Gateway discovery | `/proc/net/route` | Simple, no deps, Linux target |
| Local IP | UDP socket trick | Reliable, cross-platform |
| UPnP XML parsing | String search | No XML parser dep needed |
| SOAP HTTP | Raw `TcpStream` | Trivial POST, avoids reqwest |
| TCP bind | `0.0.0.0` | Required for external reachability |

## Files

| File | Role |
|------|------|
| `crates/ferrite-nat/src/error.rs` | Error / Result types |
| `crates/ferrite-nat/src/gateway.rs` | Default gateway + local IP discovery |
| `crates/ferrite-nat/src/natpmp.rs` | NAT-PMP encode/decode + client |
| `crates/ferrite-nat/src/pcp.rs` | PCP encode/decode + client |
| `crates/ferrite-nat/src/upnp/ssdp.rs` | SSDP M-SEARCH discovery |
| `crates/ferrite-nat/src/upnp/soap.rs` | SOAP XML + HTTP |
| `crates/ferrite-nat/src/upnp/mod.rs` | UPnP orchestration |
| `crates/ferrite-nat/src/actor.rs` | NatActor / NatHandle lifecycle |
| `crates/ferrite-session/src/session.rs` | Integration: select! arm, init, shutdown |
| `crates/ferrite-session/src/types.rs` | `enable_upnp`, `enable_natpmp` config |
| `crates/ferrite-session/src/torrent.rs` | TCP bind 0.0.0.0 |

## Deferred

- IPv6 NAT traversal (M21)
- Namespace-aware UPnP XML parsing
- Per-torrent port mapping
