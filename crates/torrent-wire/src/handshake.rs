use bytes::{Buf, BufMut, Bytes, BytesMut};

use torrent_core::Id20;

use crate::error::{Error, Result};

/// BitTorrent protocol string.
const PROTOCOL: &[u8] = b"BitTorrent protocol";

/// Total handshake size: 1 + 19 + 8 + 20 + 20 = 68 bytes.
pub const HANDSHAKE_SIZE: usize = 68;

/// Peer wire handshake (BEP 3).
///
/// Format: `<pstrlen><pstr><reserved><info_hash><peer_id>`
/// - pstrlen: 1 byte (19)
/// - pstr: 19 bytes ("BitTorrent protocol")
/// - reserved: 8 bytes (extension flags)
/// - info_hash: 20 bytes
/// - peer_id: 20 bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// 8-byte reserved field for extension flags.
    pub reserved: [u8; 8],
    /// Info hash of the torrent.
    pub info_hash: Id20,
    /// Peer ID.
    pub peer_id: Id20,
}

impl Handshake {
    /// Create a new handshake with extension protocol support (BEP 10).
    pub fn new(info_hash: Id20, peer_id: Id20) -> Self {
        let mut reserved = [0u8; 8];
        // Bit 20 (byte 5, bit 4) = Extension Protocol (BEP 10)
        reserved[5] |= 0x10;
        Handshake {
            reserved,
            info_hash,
            peer_id,
        }
    }

    /// Check if the peer supports the Extension Protocol (BEP 10).
    pub fn supports_extensions(&self) -> bool {
        self.reserved[5] & 0x10 != 0
    }

    /// Check if the peer supports DHT (BEP 5).
    pub fn supports_dht(&self) -> bool {
        self.reserved[7] & 0x01 != 0
    }

    /// Enable DHT support flag.
    pub fn with_dht(mut self) -> Self {
        self.reserved[7] |= 0x01;
        self
    }

    /// Check if the peer supports Fast Extension (BEP 6).
    pub fn supports_fast(&self) -> bool {
        self.reserved[7] & 0x04 != 0
    }

    /// Enable Fast Extension flag.
    pub fn with_fast(mut self) -> Self {
        self.reserved[7] |= 0x04;
        self
    }

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(HANDSHAKE_SIZE);
        buf.put_u8(19);
        buf.put_slice(PROTOCOL);
        buf.put_slice(&self.reserved);
        buf.put_slice(self.info_hash.as_bytes());
        buf.put_slice(self.peer_id.as_bytes());
        buf.freeze()
    }

    /// Parse from bytes. Input must be exactly 68 bytes.
    pub fn from_bytes(mut data: &[u8]) -> Result<Self> {
        if data.len() < HANDSHAKE_SIZE {
            return Err(Error::InvalidHandshake(format!(
                "need {} bytes, got {}",
                HANDSHAKE_SIZE,
                data.len()
            )));
        }

        let pstrlen = data.get_u8();
        if pstrlen != 19 {
            return Err(Error::InvalidHandshake(format!(
                "pstrlen {pstrlen}, expected 19"
            )));
        }

        let pstr = &data[..19];
        if pstr != PROTOCOL {
            return Err(Error::InvalidHandshake("wrong protocol string".into()));
        }
        data.advance(19);

        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&data[..8]);
        data.advance(8);

        let info_hash =
            Id20::from_bytes(&data[..20]).map_err(|e| Error::InvalidHandshake(e.to_string()))?;
        data.advance(20);

        let peer_id =
            Id20::from_bytes(&data[..20]).map_err(|e| Error::InvalidHandshake(e.to_string()))?;

        Ok(Handshake {
            reserved,
            info_hash,
            peer_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_round_trip() {
        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let peer_id = Id20::from_hex("0102030405060708091011121314151617181920").unwrap();

        let hs = Handshake::new(info_hash, peer_id);
        assert!(hs.supports_extensions());

        let bytes = hs.to_bytes();
        assert_eq!(bytes.len(), HANDSHAKE_SIZE);

        let parsed = Handshake::from_bytes(&bytes).unwrap();
        assert_eq!(hs, parsed);
    }

    #[test]
    fn handshake_dht_flag() {
        let hs = Handshake::new(Id20::ZERO, Id20::ZERO).with_dht();
        assert!(hs.supports_dht());
        assert!(hs.supports_extensions());

        let parsed = Handshake::from_bytes(&hs.to_bytes()).unwrap();
        assert!(parsed.supports_dht());
    }

    #[test]
    fn handshake_too_short() {
        assert!(Handshake::from_bytes(&[0u8; 10]).is_err());
    }

    #[test]
    fn handshake_fast_flag() {
        let hs = Handshake::new(Id20::ZERO, Id20::ZERO).with_fast();
        assert!(hs.supports_fast());
        assert!(hs.supports_extensions());
        // Ensure DHT and fast don't interfere
        let hs2 = hs.with_dht();
        assert!(hs2.supports_fast());
        assert!(hs2.supports_dht());

        let parsed = Handshake::from_bytes(&hs2.to_bytes()).unwrap();
        assert!(parsed.supports_fast());
        assert!(parsed.supports_dht());
    }
}
