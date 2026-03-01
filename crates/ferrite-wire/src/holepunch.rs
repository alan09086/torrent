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
    /// Initiator -> Relay: "please connect me to this peer."
    Rendezvous = 0x00,
    /// Relay -> Both parties: "initiate simultaneous connect to this peer."
    Connect = 0x01,
    /// Relay -> Initiator: "cannot complete the rendezvous."
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
