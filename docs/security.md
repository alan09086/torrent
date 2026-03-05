# Security

Torrent implements multiple layers of defence against malicious peers, trackers, and crafted inputs.

## Resource Exhaustion Limits

Configurable limits prevent OOM and CPU exhaustion from malicious peers:

| Limit | Default | Boundary |
|-------|---------|----------|
| `max_metadata_size` | 4 MiB | BEP 9 extension handshake — rejects oversized metadata |
| `max_message_size` | 16 MiB | Wire codec — drops connections sending oversized messages |
| `max_piece_length` | 32 MiB | `add_torrent` — rejects torrents with absurd piece sizes |
| `max_outstanding_requests` | 500 | Incoming request handler — drops excess upload requests |

## URL Security (SSRF Mitigation)

The `url_guard` module validates all tracker and web seed URLs:

- **Localhost restriction**: Tracker URLs pointing to `127.0.0.1`/`[::1]` are restricted to `/announce` paths only
- **Private network blocking**: HTTP redirects from public to private IP ranges are blocked
- **IDNA rejection**: Non-ASCII domain names (punycode/IDNA) are rejected by default to prevent homograph attacks
- **HTTPS validation**: TLS certificates on HTTPS trackers are validated by default

## Encryption

- **MSE/PE** (Message Stream Encryption / Protocol Encryption): RC4 + DH key exchange for peer traffic obfuscation. Three modes: Disabled, Enabled (prefer encrypted), Forced (require encrypted).
- **SSL/TLS**: BEP 35 SSL torrent support with rustls. SNI-based routing for multiple torrents per port.

## Peer Security

- **Smart banning**: Tracks which peers contributed to hash-failed pieces. After `smart_ban_max_failures` (default: 3) involvements, the peer is banned.
- **Parole mode**: Before banning, re-downloads the piece from a single uninvolved peer to confirm attribution.
- **IP filtering**: `.dat` and `.p2p` filter file support for blocking IP ranges.
- **BEP 42 DHT security**: Node ID verification tied to IP address, one-node-per-IP routing table restriction.

## Anonymous Mode

When `anonymous_mode` is enabled:
- Client version string is suppressed in BEP 10 extension handshake
- PeerId uses randomised prefix (no client fingerprint)
- DHT, LSD, UPnP, and NAT-PMP are disabled
- Extension handshake omits port and request queue size

## Fuzzing

Network-facing parsers are covered by [cargo-fuzz targets](fuzzing.md):
- Bencode deserialization
- Wire protocol message parsing
- Tracker response parsing
- .torrent / magnet URI parsing

All parsers use `Result`-based error handling — no `unwrap()` on untrusted input.
