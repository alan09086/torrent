//! Default gateway and local IP discovery.

use std::net::Ipv4Addr;

/// Parse a hex-encoded IPv4 address from `/proc/net/route` format.
///
/// The kernel stores addresses as little-endian hex u32 on x86, so
/// `"0101A8C0"` → `u32::from_str_radix` → `to_be()` → `192.168.1.1`.
pub fn parse_hex_ip(hex: &str) -> Option<Ipv4Addr> {
    let val = u32::from_str_radix(hex, 16).ok()?;
    Some(Ipv4Addr::from(val.to_be()))
}

/// Discover the default gateway by parsing `/proc/net/route`.
///
/// Looks for the entry with destination `00000000` (default route) and
/// returns the gateway IP.
pub fn default_gateway() -> Option<Ipv4Addr> {
    let contents = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in contents.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let _iface = fields.next()?;
        let destination = fields.next()?;
        let gateway = fields.next()?;

        if destination == "00000000" {
            let ip = parse_hex_ip(gateway)?;
            if !ip.is_unspecified() {
                return Some(ip);
            }
        }
    }
    None
}

/// Discover the local IP address used to reach the given gateway.
///
/// Uses the UDP socket trick: bind to `0.0.0.0:0`, connect to `gateway:80`
/// (no actual traffic is sent), then read back `local_addr()`. This reliably
/// returns the correct source IP for the gateway's network.
pub fn local_ip_for_gateway(gateway: Ipv4Addr) -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket
        .connect(std::net::SocketAddr::from((gateway, 80)))
        .ok()?;
    match socket.local_addr().ok()? {
        std::net::SocketAddr::V4(addr) => Some(*addr.ip()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_hex_ip_valid() {
        // 0101A8C0 in little-endian hex → 192.168.1.1
        assert_eq!(
            parse_hex_ip("0101A8C0"),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
    }

    #[test]
    fn parse_hex_ip_zeros() {
        assert_eq!(parse_hex_ip("00000000"), Some(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn local_ip_for_gateway_loopback() {
        // Connecting to loopback should return a local address.
        let ip = local_ip_for_gateway(Ipv4Addr::LOCALHOST);
        assert!(
            ip.is_some(),
            "expected a local address for loopback gateway"
        );
    }
}
