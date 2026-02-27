use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

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

/// Encode IPv4 peers to compact format (BEP 23): 6 bytes per peer (4 IP + 2 port).
///
/// IPv6 addresses are silently skipped.
pub fn encode_compact_peers(peers: &[SocketAddr]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(peers.len() * 6);
    for peer in peers {
        if let SocketAddr::V4(v4) = peer {
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
        }
    }
    buf
}

/// Parse compact IPv6 peer format (BEP 7): 18 bytes per peer (16 IP + 2 port).
pub fn parse_compact_peers6(data: &[u8]) -> Result<Vec<SocketAddr>> {
    if !data.len().is_multiple_of(18) {
        return Err(Error::InvalidResponse(format!(
            "compact peers6 length {} is not a multiple of 18",
            data.len()
        )));
    }

    let mut peers = Vec::with_capacity(data.len() / 18);
    for chunk in data.chunks_exact(18) {
        let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&chunk[..16]).unwrap());
        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
        peers.push(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)));
    }
    Ok(peers)
}

/// Encode IPv6 peers to compact format (BEP 7): 18 bytes per peer (16 IP + 2 port).
///
/// IPv4 addresses are silently skipped.
pub fn encode_compact_peers6(peers: &[SocketAddr]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(peers.len() * 18);
    for peer in peers {
        if let SocketAddr::V6(v6) = peer {
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    buf
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

    // --- encode_compact_peers (IPv4) ---

    #[test]
    fn encode_compact_peers_round_trip() {
        let peers: Vec<SocketAddr> = vec![
            "192.168.1.1:6881".parse().unwrap(),
            "10.0.0.1:8080".parse().unwrap(),
        ];
        let encoded = encode_compact_peers(&peers);
        assert_eq!(encoded.len(), 12);
        let decoded = parse_compact_peers(&encoded).unwrap();
        assert_eq!(peers, decoded);
    }

    #[test]
    fn encode_compact_peers_skips_ipv6() {
        let peers: Vec<SocketAddr> = vec![
            "192.168.1.1:6881".parse().unwrap(),
            "[::1]:6881".parse().unwrap(),
        ];
        let encoded = encode_compact_peers(&peers);
        assert_eq!(encoded.len(), 6); // only the IPv4 peer
    }

    // --- parse_compact_peers6 ---

    #[test]
    fn parse_single_peer6() {
        // [::1]:6881
        let mut data = [0u8; 18];
        data[15] = 1; // ::1
        data[16] = 0x1A;
        data[17] = 0xE1; // port 6881
        let peers = parse_compact_peers6(&data).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0], "[::1]:6881".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_multiple_peers6() {
        let mut data = Vec::new();
        // [2001:db8::1]:8080
        let ip1: Ipv6Addr = "2001:db8::1".parse().unwrap();
        data.extend_from_slice(&ip1.octets());
        data.extend_from_slice(&8080u16.to_be_bytes());
        // [fe80::42]:6881
        let ip2: Ipv6Addr = "fe80::42".parse().unwrap();
        data.extend_from_slice(&ip2.octets());
        data.extend_from_slice(&6881u16.to_be_bytes());

        let peers = parse_compact_peers6(&data).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(
            peers[0],
            "[2001:db8::1]:8080".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            peers[1],
            "[fe80::42]:6881".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn parse_empty_peers6() {
        let peers = parse_compact_peers6(&[]).unwrap();
        assert!(peers.is_empty());
    }

    #[test]
    fn reject_invalid_length_peers6() {
        assert!(parse_compact_peers6(&[0u8; 17]).is_err());
        assert!(parse_compact_peers6(&[0u8; 19]).is_err());
    }

    // --- encode_compact_peers6 ---

    #[test]
    fn encode_compact_peers6_round_trip() {
        let peers: Vec<SocketAddr> = vec![
            "[2001:db8::1]:8080".parse().unwrap(),
            "[::1]:6881".parse().unwrap(),
        ];
        let encoded = encode_compact_peers6(&peers);
        assert_eq!(encoded.len(), 36);
        let decoded = parse_compact_peers6(&encoded).unwrap();
        assert_eq!(peers, decoded);
    }

    #[test]
    fn encode_compact_peers6_skips_ipv4() {
        let peers: Vec<SocketAddr> = vec![
            "[::1]:6881".parse().unwrap(),
            "192.168.1.1:6881".parse().unwrap(),
        ];
        let encoded = encode_compact_peers6(&peers);
        assert_eq!(encoded.len(), 18); // only the IPv6 peer
    }
}
