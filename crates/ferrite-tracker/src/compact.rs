use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use crate::error::{Error, Result};

/// Parse compact peer format (BEP 23): 6 bytes per peer (4 IP + 2 port).
pub fn parse_compact_peers(data: &[u8]) -> Result<Vec<SocketAddr>> {
    if !data.len().is_multiple_of(6) {
        return Err(Error::InvalidResponse(format!(
            "compact peers length {} is not a multiple of 6",
            data.len()
        )));
    }

    let mut peers = Vec::with_capacity(data.len() / 6);
    for chunk in data.chunks_exact(6) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        peers.push(SocketAddr::V4(SocketAddrV4::new(ip, port)));
    }
    Ok(peers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_peer() {
        // 192.168.1.1:6881
        let data = [192, 168, 1, 1, 0x1A, 0xE1];
        let peers = parse_compact_peers(&data).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].to_string(), "192.168.1.1:6881");
    }

    #[test]
    fn parse_multiple_peers() {
        let mut data = Vec::new();
        // 10.0.0.1:8080
        data.extend_from_slice(&[10, 0, 0, 1, 0x1F, 0x90]);
        // 172.16.0.1:6881
        data.extend_from_slice(&[172, 16, 0, 1, 0x1A, 0xE1]);
        let peers = parse_compact_peers(&data).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].to_string(), "10.0.0.1:8080");
        assert_eq!(peers[1].to_string(), "172.16.0.1:6881");
    }

    #[test]
    fn parse_empty() {
        let peers = parse_compact_peers(&[]).unwrap();
        assert!(peers.is_empty());
    }

    #[test]
    fn reject_invalid_length() {
        assert!(parse_compact_peers(&[1, 2, 3, 4, 5]).is_err());
    }
}
