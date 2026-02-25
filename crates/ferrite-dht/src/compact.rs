use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use ferrite_core::Id20;

use crate::error::{Error, Result};

/// A DHT node: 20-byte ID + IPv4 socket address.
///
/// Encoded as 26 bytes: 20-byte node ID, 4-byte IPv4, 2-byte port (big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactNodeInfo {
    pub id: Id20,
    pub addr: SocketAddr,
}

/// Size of one compact node info entry.
pub const COMPACT_NODE_SIZE: usize = 26;

impl CompactNodeInfo {
    /// Encode to 26 bytes.
    pub fn to_bytes(&self) -> [u8; COMPACT_NODE_SIZE] {
        let mut buf = [0u8; COMPACT_NODE_SIZE];
        buf[..20].copy_from_slice(self.id.as_bytes());
        match self.addr {
            SocketAddr::V4(v4) => {
                buf[20..24].copy_from_slice(&v4.ip().octets());
                buf[24..26].copy_from_slice(&v4.port().to_be_bytes());
            }
            SocketAddr::V6(_) => {
                // BEP 5 specifies IPv4 only; IPv6 nodes silently encode as 0.0.0.0:0
            }
        }
        buf
    }

    /// Decode from exactly 26 bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() != COMPACT_NODE_SIZE {
            return Err(Error::InvalidCompactNode(format!(
                "expected {COMPACT_NODE_SIZE} bytes, got {}",
                data.len()
            )));
        }
        let id = Id20::from_bytes(&data[..20])
            .map_err(|e| Error::InvalidCompactNode(e.to_string()))?;
        let ip = Ipv4Addr::new(data[20], data[21], data[22], data[23]);
        let port = u16::from_be_bytes([data[24], data[25]]);
        Ok(CompactNodeInfo {
            id,
            addr: SocketAddr::V4(SocketAddrV4::new(ip, port)),
        })
    }
}

/// Decode a byte slice of concatenated 26-byte compact node infos.
pub fn parse_compact_nodes(data: &[u8]) -> Result<Vec<CompactNodeInfo>> {
    if !data.len().is_multiple_of(COMPACT_NODE_SIZE) {
        return Err(Error::InvalidCompactNode(format!(
            "compact nodes length {} is not a multiple of {COMPACT_NODE_SIZE}",
            data.len()
        )));
    }
    let mut nodes = Vec::with_capacity(data.len() / COMPACT_NODE_SIZE);
    for chunk in data.chunks_exact(COMPACT_NODE_SIZE) {
        nodes.push(CompactNodeInfo::from_bytes(chunk)?);
    }
    Ok(nodes)
}

/// Encode a slice of compact node infos into bytes.
pub fn encode_compact_nodes(nodes: &[CompactNodeInfo]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(nodes.len() * COMPACT_NODE_SIZE);
    for node in nodes {
        buf.extend_from_slice(&node.to_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node() -> CompactNodeInfo {
        CompactNodeInfo {
            id: Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            addr: "192.168.1.1:6881".parse().unwrap(),
        }
    }

    #[test]
    fn round_trip_single() {
        let node = sample_node();
        let bytes = node.to_bytes();
        assert_eq!(bytes.len(), 26);
        let decoded = CompactNodeInfo::from_bytes(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn round_trip_multiple() {
        let nodes = vec![
            sample_node(),
            CompactNodeInfo {
                id: Id20::ZERO,
                addr: "10.0.0.1:8080".parse().unwrap(),
            },
        ];
        let encoded = encode_compact_nodes(&nodes);
        assert_eq!(encoded.len(), 52);
        let decoded = parse_compact_nodes(&encoded).unwrap();
        assert_eq!(nodes, decoded);
    }

    #[test]
    fn boundary_ip_and_port() {
        let node = CompactNodeInfo {
            id: Id20::from_hex("ffffffffffffffffffffffffffffffffffffffff").unwrap(),
            addr: "255.255.255.255:65535".parse().unwrap(),
        };
        let bytes = node.to_bytes();
        let decoded = CompactNodeInfo::from_bytes(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn reject_wrong_length() {
        assert!(CompactNodeInfo::from_bytes(&[0u8; 25]).is_err());
        assert!(parse_compact_nodes(&[0u8; 27]).is_err());
    }

    #[test]
    fn empty_compact_nodes() {
        let nodes = parse_compact_nodes(&[]).unwrap();
        assert!(nodes.is_empty());
    }
}
