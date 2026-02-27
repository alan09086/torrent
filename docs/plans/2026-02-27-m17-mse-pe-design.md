# M17: Connection Encryption (MSE/PE) — Design

## Overview

Message Stream Encryption / Protocol Encryption (MSE/PE) obfuscates BitTorrent traffic from ISP deep packet inspection. It wraps a raw TCP stream in an encrypted tunnel using Diffie-Hellman key exchange and RC4 stream cipher, before the standard BitTorrent handshake occurs.

Lives in `ferrite-wire/src/mse/` — wire-level protocol, same crate as handshake and codec.

## Encryption Modes

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionMode {
    /// No encryption. Plain BitTorrent handshake.
    Disabled,
    /// Prefer encrypted, fallback to plain on failure (default).
    Enabled,
    /// Encrypted only. Reject unencrypted peers.
    Forced,
}
```

Default: `Enabled`.

## Crypto Methods (Negotiated)

```rust
bitflags! {
    pub struct CryptoMethod: u32 {
        const PLAINTEXT = 0x01;  // Header obfuscation only
        const RC4       = 0x02;  // Full RC4 stream cipher
    }
}
```

- `crypto_provide`: bitmask of methods the initiator supports
- `crypto_select`: responder picks exactly one method from the offer
- In `Enabled` mode, offer both. In `Forced` mode, offer RC4 only.

## Protocol Constants

| Constant | Value |
|----------|-------|
| DH Prime P | 768-bit (96 bytes): `FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A63A36210000000000090563` |
| Generator G | 2 |
| HASH function | SHA-1 (20 bytes output) |
| VC | 8 zero bytes `[0u8; 8]` |
| RC4 discard | First 1024 bytes of each keystream |
| Padding | 0–512 random bytes |
| SKEY | info_hash (20 bytes) |

## Key Derivation

All hashes use SHA-1 with concatenated inputs (string literals as raw bytes + binary data):

| Key | Formula |
|-----|---------|
| Sync marker | `HASH("req1" + S)` |
| SKEY proof | `HASH("req2" + SKEY) XOR HASH("req3" + S)` |
| Initiator encrypt key | `HASH("keyA" + S + SKEY)` |
| Initiator decrypt key | `HASH("keyB" + S + SKEY)` |
| Responder encrypt key | `HASH("keyB" + S + SKEY)` |
| Responder decrypt key | `HASH("keyA" + S + SKEY)` |

Where `S` = DH shared secret (raw big-endian bytes, 96 bytes).

## Handshake Flow

### Phase 1: DH Key Exchange

Both sides generate random private key X (768-bit), compute public key Y = G^X mod P (96 bytes).

**Packet 1 (Initiator → Responder):** `Ya[96] + PadA[0-512]`
**Packet 2 (Responder → Initiator):** `Yb[96] + PadB[0-512]`

Both compute shared secret: `S = Y_peer^X_self mod P`

### Phase 2: Crypto Negotiation (Initiator → Responder)

**Packet 3:**
```
HASH("req1" + S)                           [20 bytes — sync marker]
HASH("req2" + SKEY) XOR HASH("req3" + S)  [20 bytes — SKEY proof]
ENCRYPT_A(VC[8] + crypto_provide[4] + len(PadC)[2] + PadC[0-512] + len(IA)[2] + IA[0-65535])
```

`ENCRYPT_A` uses the initiator's encrypt RC4 cipher (keyA-derived, 1024 bytes discarded).

### Phase 3: Crypto Selection (Responder → Initiator)

Responder:
1. Receives 96 bytes (Yb already sent, now reads initiator's phase 2)
2. Scans for `HASH("req1" + S)` in incoming stream (up to 628 bytes past DH key)
3. Extracts SKEY proof, verifies against known info_hash
4. Decrypts remainder with responder's decrypt cipher
5. Reads VC, crypto_provide, padding, and initial payload

**Packet 4:**
```
ENCRYPT_B(VC[8] + crypto_select[4] + len(PadD)[2] + PadD[0-512])
```

### Phase 4: Encrypted Stream Active

If RC4 selected: all subsequent data encrypted with the established RC4 ciphers.
If Plaintext selected: ciphers deactivated, data flows unencrypted (but DH exchange already happened for obfuscation).

## Module Structure

```
ferrite-wire/src/mse/
├── mod.rs       — re-exports, EncryptionMode, CryptoMethod, public negotiate functions
├── dh.rs        — DiffieHellman: keypair generation, shared secret computation (num-bigint)
├── cipher.rs    — Rc4Cipher: wrapper with 1024-byte discard, encrypt/decrypt in-place
├── handshake.rs — negotiate_outbound(), negotiate_inbound() async state machines
└── stream.rs    — MseStream<S>: AsyncRead + AsyncWrite wrapper with optional RC4
```

## Key Types

```rust
/// Encrypted stream wrapping any AsyncRead + AsyncWrite stream.
pub struct MseStream<S> {
    inner: S,
    read_cipher: Option<Rc4Cipher>,   // None = plaintext mode
    write_cipher: Option<Rc4Cipher>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for MseStream<S> { ... }
impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for MseStream<S> { ... }
```

```rust
/// Result of MSE/PE negotiation.
pub struct NegotiationResult<S> {
    pub stream: MseStream<S>,
    pub crypto_method: CryptoMethod,
    pub initial_payload: Vec<u8>,
}
```

## Public API

```rust
/// Initiator (outbound connection). Returns encrypted stream.
/// Falls back to plain stream if mode=Enabled and peer doesn't support MSE.
pub async fn negotiate_outbound<S>(
    stream: S,
    info_hash: Id20,
    mode: EncryptionMode,
) -> Result<NegotiationResult<S>>
where S: AsyncRead + AsyncWrite + Unpin;

/// Responder (inbound connection). Returns encrypted stream.
pub async fn negotiate_inbound<S>(
    stream: S,
    info_hash: Id20,
    mode: EncryptionMode,
) -> Result<NegotiationResult<S>>
where S: AsyncRead + AsyncWrite + Unpin;
```

## Integration into ferrite-session

In `run_peer()` (peer.rs), before the BitTorrent handshake:

```rust
let mse_result = if is_outbound {
    mse::negotiate_outbound(stream, info_hash, encryption_mode).await
} else {
    mse::negotiate_inbound(stream, info_hash, encryption_mode).await
};
// Use mse_result.stream for the rest of the peer lifecycle
```

For `Enabled` mode outbound: if MSE handshake fails, the TCP connection is dead (DH bytes already sent). The caller should catch the error and reconnect with a plain handshake. This retry logic lives in `try_connect_peers()` in torrent.rs.

## Configuration

New field on `SessionConfig` and `TorrentConfig`:

```rust
pub encryption_mode: EncryptionMode,  // default: Enabled
```

New `ClientBuilder` method:

```rust
pub fn encryption_mode(mut self, mode: EncryptionMode) -> Self
```

## Dependencies (ferrite-wire)

```toml
sha1 = "0.10"             # HASH function (workspace-shared with ferrite-core)
num-bigint = "0.4"        # 768-bit DH modular exponentiation
num-traits = "0.2"        # Zero, One traits for bigint
num-integer = "0.1"       # Integer trait (modpow)
```

Random bytes: use ferrite-core's existing xorshift64 RNG (no `rand` dependency).

## Error Handling

New variants in `ferrite-wire::Error`:

```rust
/// MSE/PE handshake failed (timeout, protocol mismatch, etc.)
EncryptionHandshakeFailed(String),
/// Peer offered no compatible crypto method.
UnsupportedCryptoMethod,
/// Encryption required (mode=Forced) but peer doesn't support MSE.
EncryptionRequired,
```

## Alerts

No new alert variants. Connection failures already produce `PeerDisconnected { reason }`.

## Testing Strategy (~12 tests)

**Unit tests (dh.rs):**
- DH keypair generation produces valid 96-byte public key
- Two parties compute identical shared secret

**Unit tests (cipher.rs):**
- RC4 encrypt then decrypt is identity
- First 1024 bytes are discarded (cipher state is offset)
- Known test vector validation

**Unit tests (handshake.rs):**
- HASH("req1"/req2/req3/keyA/keyB) formulas produce expected output for known inputs
- crypto_provide/crypto_select bitmask encoding

**Integration tests (handshake.rs):**
- Full initiator ↔ responder handshake over in-memory stream (tokio duplex)
- Plaintext method negotiation
- RC4 method negotiation
- Forced mode rejects plaintext-only peer
- Enabled mode with compatible peer succeeds

**Integration tests (stream.rs):**
- MseStream round-trip: write data through encrypted stream, read back correctly

## Interaction with Existing Systems

- **Handshake (ferrite-wire):** Unmodified. MSE wraps the stream before handshake runs.
- **MessageCodec:** Unmodified. Operates on the MseStream transparently.
- **BEP 6/10/5:** Unaffected. Extension negotiation happens after encryption is established.
- **Bandwidth limiter (M14):** Counts encrypted bytes (same as libtorrent behavior).
- **Alerts (M15):** PeerDisconnected reason can include encryption failure details.
- **Queue (M16):** Independent. Queue doesn't care about encryption.

## Non-Goals

- BEP 8 (tracker protocol encryption) — different protocol, out of scope
- AES encryption — RC4 is the standard, AES not specified in MSE/PE
- Per-torrent encryption mode override — session-level only for now
- Certificate-based authentication — MSE/PE is anonymous
