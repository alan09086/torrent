//! PCP (RFC 6887) — Port Control Protocol.
//!
//! Successor to NAT-PMP, supports IPv6 and more features. Uses the same
//! port 5351 but sends 60-byte MAP opcode packets with a 12-byte nonce.

use std::net::{Ipv4Addr, IpAddr};

use crate::error::{Error, Result};

/// PCP server port (same as NAT-PMP).
pub const PCP_PORT: u16 = 5351;

/// PCP version number.
pub const PCP_VERSION: u8 = 2;

/// MAP opcode.
pub const OP_MAP: u8 = 1;

/// Response bit for PCP.
const RESPONSE_BIT: u8 = 128;

/// Protocol number for TCP.
pub const PROTO_TCP: u8 = 6;
/// Protocol number for UDP.
pub const PROTO_UDP: u8 = 17;

/// Initial retransmission timeout (250ms).
const INITIAL_TIMEOUT_MS: u64 = 250;
/// Maximum retransmission attempts.
const MAX_ATTEMPTS: u32 = 4;

// ── IPv4-mapped IPv6 helpers ──

/// Convert an IPv4 address to an IPv4-mapped IPv6 representation (16 bytes).
///
/// Format: `::ffff:a.b.c.d` stored as `[0,0,0,0, 0,0,0,0, 0,0,0xff,0xff, a,b,c,d]`.
pub fn ipv4_to_mapped_v6(ip: Ipv4Addr) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[10] = 0xff;
    buf[11] = 0xff;
    buf[12..16].copy_from_slice(&ip.octets());
    buf
}

/// Extract an IP address from an IPv4-mapped IPv6 representation.
///
/// Returns `IpAddr::V4` if the prefix is `::ffff:`, otherwise `IpAddr::V6`.
pub fn mapped_v6_to_ip(buf: &[u8; 16]) -> IpAddr {
    if buf[..10] == [0; 10] && buf[10] == 0xff && buf[11] == 0xff {
        IpAddr::V4(Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]))
    } else {
        IpAddr::V6(std::net::Ipv6Addr::from(*buf))
    }
}

// ── Encoding ──

/// Encode a PCP MAP request (60 bytes).
///
/// Layout (RFC 6887 §7.1 + §11.1):
/// - Bytes 0-1:   version, opcode
/// - Bytes 2-3:   reserved
/// - Bytes 4-7:   requested lifetime
/// - Bytes 8-23:  client IP (IPv4-mapped IPv6)
/// - Bytes 24-35: mapping nonce (12 bytes)
/// - Byte  36:    protocol
/// - Bytes 37-39: reserved
/// - Bytes 40-41: internal port
/// - Bytes 42-43: suggested external port
/// - Bytes 44-59: suggested external IP (IPv4-mapped IPv6)
pub fn encode_map_request(
    internal_ip: Ipv4Addr,
    nonce: &[u8; 12],
    protocol: u8,
    internal_port: u16,
    external_port: u16,
    external_ip: Ipv4Addr,
    lifetime: u32,
) -> [u8; 60] {
    let mut buf = [0u8; 60];
    buf[0] = PCP_VERSION;
    buf[1] = OP_MAP;
    // bytes 2-3: reserved
    buf[4..8].copy_from_slice(&lifetime.to_be_bytes());
    buf[8..24].copy_from_slice(&ipv4_to_mapped_v6(internal_ip));
    buf[24..36].copy_from_slice(nonce);
    buf[36] = protocol;
    // bytes 37-39: reserved
    buf[40..42].copy_from_slice(&internal_port.to_be_bytes());
    buf[42..44].copy_from_slice(&external_port.to_be_bytes());
    buf[44..60].copy_from_slice(&ipv4_to_mapped_v6(external_ip));
    buf
}

// ── Decoding ──

/// Decoded PCP MAP response.
#[derive(Debug, Clone)]
pub struct MapResponse {
    /// Result code (0 = success).
    pub result_code: u8,
    /// Granted lifetime in seconds.
    pub lifetime: u32,
    /// Epoch time.
    pub epoch: u32,
    /// Mapping nonce echoed back.
    pub nonce: [u8; 12],
    /// Protocol (6 = TCP, 17 = UDP).
    pub protocol: u8,
    /// Internal port.
    pub internal_port: u16,
    /// Assigned external port.
    pub external_port: u16,
    /// Assigned external IP.
    pub external_ip: IpAddr,
}

/// Decode a PCP MAP response from raw bytes (60 bytes minimum).
pub fn decode_map_response(buf: &[u8]) -> Result<MapResponse> {
    if buf.len() < 60 {
        return Err(Error::Pcp(format!(
            "MAP response too short: {} bytes",
            buf.len()
        )));
    }
    if buf[0] != PCP_VERSION {
        return Err(Error::UnsupportedVersion);
    }
    if buf[1] != RESPONSE_BIT | OP_MAP {
        return Err(Error::Pcp(format!("unexpected opcode: {}", buf[1])));
    }

    let result_code = buf[3]; // byte 3 in the response header
    let lifetime = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let epoch = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    // bytes 12-23: reserved in response header

    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&buf[24..36]);
    let protocol = buf[36];
    let internal_port = u16::from_be_bytes([buf[40], buf[41]]);
    let external_port = u16::from_be_bytes([buf[42], buf[43]]);

    let mut ext_ip_bytes = [0u8; 16];
    ext_ip_bytes.copy_from_slice(&buf[44..60]);
    let external_ip = mapped_v6_to_ip(&ext_ip_bytes);

    Ok(MapResponse {
        result_code,
        lifetime,
        epoch,
        nonce,
        protocol,
        internal_port,
        external_port,
        external_ip,
    })
}

// ── Network I/O ──

/// Send a PCP request to the gateway and wait for a response.
///
/// Uses the same exponential backoff as NAT-PMP.
pub async fn send_pcp_request(gateway: Ipv4Addr, request: &[u8]) -> Result<Vec<u8>> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    socket
        .connect(std::net::SocketAddr::from((gateway, PCP_PORT)))
        .await?;

    let mut timeout_ms = INITIAL_TIMEOUT_MS;
    let mut buf = vec![0u8; 128];

    for attempt in 0..MAX_ATTEMPTS {
        socket.send(request).await?;

        match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            socket.recv(&mut buf),
        )
        .await
        {
            Ok(Ok(n)) => return Ok(buf[..n].to_vec()),
            Ok(Err(e)) => return Err(Error::Io(e)),
            Err(_) => {
                tracing::debug!(attempt, timeout_ms, "PCP request timed out, retrying");
                timeout_ms *= 2;
            }
        }
    }
    Err(Error::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn encode_map_request_size() {
        let nonce = [0xAA; 12];
        let buf = encode_map_request(
            Ipv4Addr::new(192, 168, 1, 100),
            &nonce,
            PROTO_TCP,
            6881,
            6881,
            Ipv4Addr::UNSPECIFIED,
            7200,
        );
        assert_eq!(buf.len(), 60);
        assert_eq!(buf[0], PCP_VERSION);
        assert_eq!(buf[1], OP_MAP);
    }

    #[test]
    fn encode_map_request_lifetime() {
        let nonce = [0xBB; 12];
        let buf = encode_map_request(
            Ipv4Addr::new(10, 0, 0, 1),
            &nonce,
            PROTO_UDP,
            51413,
            51413,
            Ipv4Addr::UNSPECIFIED,
            3600,
        );
        let lifetime = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(lifetime, 3600);
    }

    #[test]
    fn decode_map_response_valid() {
        let nonce = [0xCC; 12];
        // Build a fake response
        let mut buf = [0u8; 60];
        buf[0] = PCP_VERSION;
        buf[1] = RESPONSE_BIT | OP_MAP; // 129
        // buf[2] = reserved
        buf[3] = 0; // result_code = success
        buf[4..8].copy_from_slice(&7200u32.to_be_bytes()); // lifetime
        buf[8..12].copy_from_slice(&5000u32.to_be_bytes()); // epoch
        // bytes 12-23: reserved
        buf[24..36].copy_from_slice(&nonce); // nonce
        buf[36] = PROTO_TCP; // protocol
        buf[40..42].copy_from_slice(&6881u16.to_be_bytes()); // internal port
        buf[42..44].copy_from_slice(&6881u16.to_be_bytes()); // external port
        // external IP: 203.0.113.5 as IPv4-mapped
        buf[44..60].copy_from_slice(&ipv4_to_mapped_v6(Ipv4Addr::new(203, 0, 113, 5)));

        let resp = decode_map_response(&buf).unwrap();
        assert_eq!(resp.result_code, 0);
        assert_eq!(resp.lifetime, 7200);
        assert_eq!(resp.epoch, 5000);
        assert_eq!(resp.nonce, nonce);
        assert_eq!(resp.protocol, PROTO_TCP);
        assert_eq!(resp.internal_port, 6881);
        assert_eq!(resp.external_port, 6881);
        assert_eq!(resp.external_ip, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)));
    }

    #[test]
    fn decode_map_response_too_short() {
        let buf = [0u8; 30]; // too short
        let err = decode_map_response(&buf).unwrap_err();
        assert!(err.to_string().contains("too short"));
    }
}
