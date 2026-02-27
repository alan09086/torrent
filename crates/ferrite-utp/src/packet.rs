use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::error::{Error, Result};
use crate::seq::SeqNr;

/// BEP 29 header size in bytes.
pub const HEADER_SIZE: usize = 20;

/// uTP protocol version.
pub const VERSION: u8 = 1;

/// Packet type field values (BEP 29).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Data = 0,
    Fin = 1,
    State = 2,
    Reset = 3,
    Syn = 4,
}

impl PacketType {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Data),
            1 => Ok(Self::Fin),
            2 => Ok(Self::State),
            3 => Ok(Self::Reset),
            4 => Ok(Self::Syn),
            _ => Err(Error::InvalidPacket(format!("unknown packet type: {v}"))),
        }
    }
}

/// Extension type field values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExtensionType {
    None = 0,
    Sack = 1,
}

impl ExtensionType {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::None),
            1 => Ok(Self::Sack),
            _ => Err(Error::InvalidPacket(format!("unknown extension type: {v}"))),
        }
    }
}

/// BEP 29 packet header (20 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub packet_type: PacketType,
    pub extension: ExtensionType,
    pub connection_id: u16,
    pub timestamp_us: u32,
    pub timestamp_diff_us: u32,
    pub wnd_size: u32,
    pub seq_nr: SeqNr,
    pub ack_nr: SeqNr,
}

impl Header {
    pub fn encode(&self, buf: &mut BytesMut) {
        let type_ver = ((self.packet_type as u8) << 4) | VERSION;
        buf.put_u8(type_ver);
        buf.put_u8(self.extension as u8);
        buf.put_u16(self.connection_id);
        buf.put_u32(self.timestamp_us);
        buf.put_u32(self.timestamp_diff_us);
        buf.put_u32(self.wnd_size);
        buf.put_u16(self.seq_nr.0);
        buf.put_u16(self.ack_nr.0);
    }

    pub fn decode(buf: &mut &[u8]) -> Result<Self> {
        if buf.len() < HEADER_SIZE {
            return Err(Error::InvalidPacket(format!(
                "packet too short: {} < {HEADER_SIZE}",
                buf.len()
            )));
        }

        let type_ver = buf.get_u8();
        let version = type_ver & 0x0F;
        if version != VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        let packet_type = PacketType::from_u8(type_ver >> 4)?;
        let extension = ExtensionType::from_u8(buf.get_u8())?;
        let connection_id = buf.get_u16();
        let timestamp_us = buf.get_u32();
        let timestamp_diff_us = buf.get_u32();
        let wnd_size = buf.get_u32();
        let seq_nr = SeqNr(buf.get_u16());
        let ack_nr = SeqNr(buf.get_u16());

        Ok(Header {
            packet_type,
            extension,
            connection_id,
            timestamp_us,
            timestamp_diff_us,
            wnd_size,
            seq_nr,
            ack_nr,
        })
    }
}

/// Complete uTP packet: header + optional SACK bitmask + payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: Header,
    pub sack: Option<Bytes>,
    pub payload: Bytes,
}

impl Packet {
    pub fn encode(&self) -> Bytes {
        let sack_ext_len = match &self.sack {
            Some(sack) => 2 + sack.len(), // next_ext(1) + len(1) + bitmask
            None => 0,
        };
        let total = HEADER_SIZE + sack_ext_len + self.payload.len();
        let mut buf = BytesMut::with_capacity(total);

        self.header.encode(&mut buf);

        // Encode SACK extension if present
        if let Some(sack) = &self.sack {
            buf.put_u8(0); // next extension = none
            buf.put_u8(sack.len() as u8);
            buf.put_slice(sack);
        }

        buf.put_slice(&self.payload);
        buf.freeze()
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut cursor: &[u8] = data;
        let header = Header::decode(&mut cursor)?;

        // Parse extension chain
        let mut sack = None;
        let mut ext = header.extension;

        while ext != ExtensionType::None {
            if cursor.len() < 2 {
                return Err(Error::InvalidPacket(
                    "extension header truncated".to_string(),
                ));
            }
            let next_ext = ExtensionType::from_u8(cursor.get_u8())?;
            let ext_len = cursor.get_u8() as usize;

            if cursor.len() < ext_len {
                return Err(Error::InvalidPacket(format!(
                    "extension data truncated: need {ext_len}, have {}",
                    cursor.len()
                )));
            }

            match ext {
                ExtensionType::Sack => {
                    sack = Some(Bytes::copy_from_slice(&cursor[..ext_len]));
                }
                ExtensionType::None => unreachable!(),
            }

            cursor.advance(ext_len);
            ext = next_ext;
        }

        let payload = Bytes::copy_from_slice(cursor);

        Ok(Packet {
            header,
            sack,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> Header {
        Header {
            packet_type: PacketType::Data,
            extension: ExtensionType::None,
            connection_id: 12345,
            timestamp_us: 1_000_000,
            timestamp_diff_us: 500,
            wnd_size: 65536,
            seq_nr: SeqNr(100),
            ack_nr: SeqNr(99),
        }
    }

    #[test]
    fn header_encode_decode_roundtrip() {
        let original = sample_header();
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        assert_eq!(buf.len(), HEADER_SIZE);

        let mut slice: &[u8] = &buf;
        let decoded = Header::decode(&mut slice).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn packet_with_sack_roundtrip() {
        let sack_data = Bytes::from_static(&[0b10101010, 0b01010101, 0b11110000, 0b00001111]);
        let packet = Packet {
            header: Header {
                extension: ExtensionType::Sack,
                ..sample_header()
            },
            sack: Some(sack_data.clone()),
            payload: Bytes::from_static(b"hello"),
        };

        let encoded = packet.encode();
        let decoded = Packet::decode(&encoded).unwrap();

        assert_eq!(decoded.header.extension, ExtensionType::Sack);
        assert_eq!(decoded.sack, Some(sack_data));
        assert_eq!(decoded.payload, Bytes::from_static(b"hello"));
    }

    #[test]
    fn packet_types_discriminant() {
        assert_eq!(PacketType::Data as u8, 0);
        assert_eq!(PacketType::Fin as u8, 1);
        assert_eq!(PacketType::State as u8, 2);
        assert_eq!(PacketType::Reset as u8, 3);
        assert_eq!(PacketType::Syn as u8, 4);
    }

    #[test]
    fn reject_invalid_version() {
        let mut buf = BytesMut::new();
        // type=0 (Data), version=2 (invalid)
        buf.put_u8(0x02);
        buf.put_u8(0); // extension
        buf.put_u16(0); // connection_id
        buf.put_u32(0); // timestamp
        buf.put_u32(0); // timestamp_diff
        buf.put_u32(0); // wnd_size
        buf.put_u16(0); // seq_nr
        buf.put_u16(0); // ack_nr

        let mut slice: &[u8] = &buf;
        let err = Header::decode(&mut slice).unwrap_err();
        assert!(matches!(err, Error::UnsupportedVersion(2)));
    }
}
