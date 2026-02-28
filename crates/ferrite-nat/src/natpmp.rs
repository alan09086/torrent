//! NAT-PMP (RFC 6886) — NAT Port Mapping Protocol.
//!
//! UDP-based protocol for requesting port mappings from the gateway router.
//! Sends requests to `gateway:5351` and receives responses on the same socket.

use std::net::Ipv4Addr;

use crate::error::{Error, Result};

/// NAT-PMP server port.
pub const NATPMP_PORT: u16 = 5351;

/// Protocol version.
const VERSION: u8 = 0;

/// Opcode: request external address.
const OP_EXTERNAL_ADDR: u8 = 0;
/// Opcode: request UDP mapping.
pub const OP_MAP_UDP: u8 = 1;
/// Opcode: request TCP mapping.
pub const OP_MAP_TCP: u8 = 2;

/// Response bit OR'd with the opcode in replies.
const RESPONSE_BIT: u8 = 128;

/// Initial retransmission timeout (250ms per RFC 6886 §3.1).
const INITIAL_TIMEOUT_MS: u64 = 250;
/// Maximum number of retransmission attempts.
const MAX_ATTEMPTS: u32 = 4;

// ── Encoding ──

/// Encode a request for the external IP address (2 bytes).
pub fn encode_external_addr_request() -> [u8; 2] {
    [VERSION, OP_EXTERNAL_ADDR]
}

/// Encode a port mapping request (12 bytes).
///
/// `opcode` should be [`OP_MAP_TCP`] or [`OP_MAP_UDP`].
/// Set `lifetime` to 0 to delete an existing mapping.
pub fn encode_mapping_request(
    opcode: u8,
    internal_port: u16,
    external_port: u16,
    lifetime: u32,
) -> [u8; 12] {
    let mut buf = [0u8; 12];
    buf[0] = VERSION;
    buf[1] = opcode;
    // bytes 2-3: reserved (zero)
    buf[4..6].copy_from_slice(&internal_port.to_be_bytes());
    buf[6..8].copy_from_slice(&external_port.to_be_bytes());
    buf[8..12].copy_from_slice(&lifetime.to_be_bytes());
    buf
}

// ── Decoding ──

/// Decoded external address response (12 bytes).
#[derive(Debug, Clone)]
pub struct ExternalAddrResponse {
    /// Result code (0 = success).
    pub result_code: u16,
    /// Seconds since last epoch reset.
    pub epoch: u32,
    /// External IPv4 address.
    pub external_ip: Ipv4Addr,
}

/// Decode an external address response from raw bytes.
pub fn decode_external_addr_response(buf: &[u8]) -> Result<ExternalAddrResponse> {
    if buf.len() < 12 {
        return Err(Error::NatPmp(format!(
            "external addr response too short: {} bytes",
            buf.len()
        )));
    }
    if buf[0] != VERSION {
        return Err(Error::UnsupportedVersion);
    }
    if buf[1] != RESPONSE_BIT | OP_EXTERNAL_ADDR {
        return Err(Error::NatPmp(format!("unexpected opcode: {}", buf[1])));
    }
    let result_code = u16::from_be_bytes([buf[2], buf[3]]);
    let epoch = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let external_ip = Ipv4Addr::new(buf[8], buf[9], buf[10], buf[11]);
    Ok(ExternalAddrResponse {
        result_code,
        epoch,
        external_ip,
    })
}

/// Decoded port mapping response (16 bytes).
#[derive(Debug, Clone)]
pub struct MappingResponse {
    /// Opcode from response (128 + original opcode).
    pub opcode: u8,
    /// Result code (0 = success).
    pub result_code: u16,
    /// Seconds since last epoch reset.
    pub epoch: u32,
    /// Internal port that was mapped.
    pub internal_port: u16,
    /// Assigned external port.
    pub external_port: u16,
    /// Granted lifetime in seconds.
    pub lifetime: u32,
}

/// Decode a port mapping response from raw bytes.
pub fn decode_mapping_response(buf: &[u8]) -> Result<MappingResponse> {
    if buf.len() < 16 {
        return Err(Error::NatPmp(format!(
            "mapping response too short: {} bytes",
            buf.len()
        )));
    }
    if buf[0] != VERSION {
        return Err(Error::UnsupportedVersion);
    }
    let opcode = buf[1];
    if opcode & RESPONSE_BIT == 0 {
        return Err(Error::NatPmp(format!(
            "expected response bit in opcode: {}",
            opcode
        )));
    }
    let result_code = u16::from_be_bytes([buf[2], buf[3]]);
    let epoch = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let internal_port = u16::from_be_bytes([buf[8], buf[9]]);
    let external_port = u16::from_be_bytes([buf[10], buf[11]]);
    let lifetime = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
    Ok(MappingResponse {
        opcode,
        result_code,
        epoch,
        internal_port,
        external_port,
        lifetime,
    })
}

// ── Network I/O ──

/// Send a NAT-PMP request to the gateway and wait for a response.
///
/// Uses exponential backoff retransmission starting at 250ms, doubling
/// each attempt up to [`MAX_ATTEMPTS`].
pub async fn send_request(gateway: Ipv4Addr, request: &[u8]) -> Result<Vec<u8>> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    socket
        .connect(std::net::SocketAddr::from((gateway, NATPMP_PORT)))
        .await?;

    let mut timeout_ms = INITIAL_TIMEOUT_MS;
    let mut buf = vec![0u8; 64];

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
                tracing::debug!(attempt, timeout_ms, "NAT-PMP request timed out, retrying");
                timeout_ms *= 2;
            }
        }
    }
    Err(Error::Timeout)
}

/// Request a TCP port mapping via NAT-PMP.
pub async fn map_tcp(
    gateway: Ipv4Addr,
    internal_port: u16,
    external_port: u16,
    lifetime: u32,
) -> Result<MappingResponse> {
    let req = encode_mapping_request(OP_MAP_TCP, internal_port, external_port, lifetime);
    let resp = send_request(gateway, &req).await?;
    let decoded = decode_mapping_response(&resp)?;
    if decoded.result_code != 0 {
        return Err(Error::MappingRefused);
    }
    Ok(decoded)
}

/// Request a UDP port mapping via NAT-PMP.
pub async fn map_udp(
    gateway: Ipv4Addr,
    internal_port: u16,
    external_port: u16,
    lifetime: u32,
) -> Result<MappingResponse> {
    let req = encode_mapping_request(OP_MAP_UDP, internal_port, external_port, lifetime);
    let resp = send_request(gateway, &req).await?;
    let decoded = decode_mapping_response(&resp)?;
    if decoded.result_code != 0 {
        return Err(Error::MappingRefused);
    }
    Ok(decoded)
}

/// Delete a TCP port mapping (lifetime = 0).
pub async fn delete_tcp_mapping(gateway: Ipv4Addr, internal_port: u16) -> Result<()> {
    let req = encode_mapping_request(OP_MAP_TCP, internal_port, 0, 0);
    let _ = send_request(gateway, &req).await;
    Ok(())
}

/// Delete a UDP port mapping (lifetime = 0).
pub async fn delete_udp_mapping(gateway: Ipv4Addr, internal_port: u16) -> Result<()> {
    let req = encode_mapping_request(OP_MAP_UDP, internal_port, 0, 0);
    let _ = send_request(gateway, &req).await;
    Ok(())
}

/// Query the external IP address via NAT-PMP (opcode 0).
pub async fn query_external_ip(gateway: Ipv4Addr) -> Result<Ipv4Addr> {
    let req = encode_external_addr_request();
    let resp = send_request(gateway, &req).await?;
    let decoded = decode_external_addr_response(&resp)?;
    Ok(decoded.external_ip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn encode_external_addr_request_format() {
        assert_eq!(encode_external_addr_request(), [0, 0]);
    }

    #[test]
    fn encode_mapping_request_format() {
        let buf = encode_mapping_request(OP_MAP_TCP, 6881, 6881, 7200);
        assert_eq!(buf[0], VERSION); // version
        assert_eq!(buf[1], OP_MAP_TCP); // opcode
        assert_eq!(buf[2], 0); // reserved
        assert_eq!(buf[3], 0); // reserved
        assert_eq!(u16::from_be_bytes([buf[4], buf[5]]), 6881); // internal port
        assert_eq!(u16::from_be_bytes([buf[6], buf[7]]), 6881); // external port
        assert_eq!(u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]), 7200); // lifetime
    }

    #[test]
    fn decode_external_addr_response_valid() {
        let mut buf = [0u8; 12];
        buf[0] = VERSION;
        buf[1] = RESPONSE_BIT | OP_EXTERNAL_ADDR; // 128
        buf[2..4].copy_from_slice(&0u16.to_be_bytes()); // result_code = 0
        buf[4..8].copy_from_slice(&1000u32.to_be_bytes()); // epoch
        buf[8] = 203;
        buf[9] = 0;
        buf[10] = 113;
        buf[11] = 5; // 203.0.113.5

        let resp = decode_external_addr_response(&buf).unwrap();
        assert_eq!(resp.result_code, 0);
        assert_eq!(resp.epoch, 1000);
        assert_eq!(resp.external_ip, Ipv4Addr::new(203, 0, 113, 5));
    }

    #[test]
    fn decode_mapping_response_valid() {
        let mut buf = [0u8; 16];
        buf[0] = VERSION;
        buf[1] = RESPONSE_BIT | OP_MAP_TCP; // 130
        buf[2..4].copy_from_slice(&0u16.to_be_bytes()); // result_code = 0
        buf[4..8].copy_from_slice(&2000u32.to_be_bytes()); // epoch
        buf[8..10].copy_from_slice(&6881u16.to_be_bytes()); // internal_port
        buf[10..12].copy_from_slice(&6881u16.to_be_bytes()); // external_port
        buf[12..16].copy_from_slice(&7200u32.to_be_bytes()); // lifetime

        let resp = decode_mapping_response(&buf).unwrap();
        assert_eq!(resp.opcode, RESPONSE_BIT | OP_MAP_TCP);
        assert_eq!(resp.result_code, 0);
        assert_eq!(resp.epoch, 2000);
        assert_eq!(resp.internal_port, 6881);
        assert_eq!(resp.external_port, 6881);
        assert_eq!(resp.lifetime, 7200);
    }
}
