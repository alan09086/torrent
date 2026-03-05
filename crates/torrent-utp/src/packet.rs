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
    /// Regular data payload.
    Data = 0,
    /// Graceful connection close.
    Fin = 1,
    /// ACK-only (no payload).
    State = 2,
    /// Forceful connection reset.
    Reset = 3,
    /// Connection initiation.
    Syn = 4,
}

impl PacketType {
    /// Converts a raw byte to a `PacketType`, returning an error for unknown values.
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

/// Extension type field values (BEP 29 + libtorrent extensions).
///
/// BEP 29 defines types 0 (none) and 1 (SACK). libtorrent adds type 3
/// (close reason). Unknown types are preserved and skipped gracefully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionType {
    /// No extension (end of chain).
    None,
    /// Selective ACK bitmask (BEP 29).
    Sack,
    /// Close reason (libtorrent extension, type 3).
    /// Carries a 4-byte payload: 2 reserved bytes + 2-byte reason code.
    CloseReason,
    /// Unknown extension — skipped gracefully during parsing.
    Unknown(u8),
}

impl ExtensionType {
    /// Converts a raw byte to an `ExtensionType`. Unknown types are preserved
    /// rather than rejected, since real-world peers (libtorrent, qBittorrent)
    /// send extensions beyond BEP 29.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::None,
            1 => Self::Sack,
            3 => Self::CloseReason,
            _ => Self::Unknown(v),
        }
    }

    /// Returns the wire byte for this extension type.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Sack => 1,
            Self::CloseReason => 3,
            Self::Unknown(v) => v,
        }
    }
}

/// BEP 29 packet header (20 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Packet type (Data, Fin, State, Reset, Syn).
    pub packet_type: PacketType,
    /// First extension type in the chain, or `None`.
    pub extension: ExtensionType,
    /// Connection identifier.
    pub connection_id: u16,
    /// Sender timestamp in microseconds.
    pub timestamp_us: u32,
    /// Difference between local time and last received timestamp.
    pub timestamp_diff_us: u32,
    /// Advertised receive window in bytes.
    pub wnd_size: u32,
    /// Sequence number of this packet.
    pub seq_nr: SeqNr,
    /// Last sequence number received by sender.
    pub ack_nr: SeqNr,
}

impl Header {
    /// Serializes the header into a 20-byte buffer.
    pub fn encode(&self, buf: &mut BytesMut) {
        let type_ver = ((self.packet_type as u8) << 4) | VERSION;
        buf.put_u8(type_ver);
        buf.put_u8(self.extension.to_u8());
        buf.put_u16(self.connection_id);
        buf.put_u32(self.timestamp_us);
        buf.put_u32(self.timestamp_diff_us);
        buf.put_u32(self.wnd_size);
        buf.put_u16(self.seq_nr.0);
        buf.put_u16(self.ack_nr.0);
    }

    /// Parses a header from a byte slice, returning an error if too short or invalid.
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
        let extension = ExtensionType::from_u8(buf.get_u8());
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

/// Complete uTP packet: header + optional extensions + payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Fixed-size packet header.
    pub header: Header,
    /// Optional SACK bitmask extension (type 1).
    pub sack: Option<Bytes>,
    /// Optional close reason code (libtorrent extension type 3).
    pub close_reason: Option<u16>,
    /// Data payload (empty for control packets).
    pub payload: Bytes,
}

impl Packet {
    /// Serializes the entire packet (header + extensions + payload) into bytes.
    pub fn encode(&self) -> Bytes {
        let sack_ext_len = match &self.sack {
            Some(sack) => 2 + sack.len(), // next_ext(1) + len(1) + bitmask
            None => 0,
        };
        let close_ext_len = if self.close_reason.is_some() {
            2 + 4
        } else {
            0
        };
        let total = HEADER_SIZE + sack_ext_len + close_ext_len + self.payload.len();
        let mut buf = BytesMut::with_capacity(total);

        self.header.encode(&mut buf);

        // Encode SACK extension if present
        if let Some(sack) = &self.sack {
            // next_ext points to close_reason if present, otherwise none
            let next = if self.close_reason.is_some() {
                ExtensionType::CloseReason.to_u8()
            } else {
                0
            };
            buf.put_u8(next);
            buf.put_u8(sack.len() as u8);
            buf.put_slice(sack);
        }

        // Encode close reason extension if present
        if let Some(reason) = self.close_reason {
            buf.put_u8(0); // next extension = none
            buf.put_u8(4); // 4 bytes: 2 reserved + 2 reason
            buf.put_u16(0); // reserved
            buf.put_u16(reason);
        }

        buf.put_slice(&self.payload);
        buf.freeze()
    }

    /// Parses a complete packet from raw bytes.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut cursor: &[u8] = data;
        let header = Header::decode(&mut cursor)?;

        // Parse extension chain
        let mut sack = None;
        let mut close_reason = None;
        let mut ext = header.extension;

        while ext != ExtensionType::None {
            if cursor.len() < 2 {
                return Err(Error::InvalidPacket(
                    "extension header truncated".to_string(),
                ));
            }
            let next_ext = ExtensionType::from_u8(cursor.get_u8());
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
                ExtensionType::CloseReason => {
                    // libtorrent format: 2 reserved bytes + 2-byte reason code
                    if ext_len >= 4 {
                        let reason = u16::from_be_bytes([cursor[2], cursor[3]]);
                        close_reason = Some(reason);
                    }
                }
                ExtensionType::Unknown(_) => {
                    // Skip unknown extensions gracefully (advance past data below)
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
            close_reason,
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
            close_reason: None,
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
    fn close_reason_roundtrip() {
        let packet = Packet {
            header: Header {
                extension: ExtensionType::CloseReason,
                ..sample_header()
            },
            sack: None,
            close_reason: Some(42),
            payload: Bytes::new(),
        };

        let encoded = packet.encode();
        let decoded = Packet::decode(&encoded).unwrap();

        assert_eq!(decoded.close_reason, Some(42));
        assert_eq!(decoded.sack, None);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn sack_with_close_reason_roundtrip() {
        let sack_data = Bytes::from_static(&[0xFF, 0x00, 0xAA, 0x55]);
        let packet = Packet {
            header: Header {
                extension: ExtensionType::Sack,
                ..sample_header()
            },
            sack: Some(sack_data.clone()),
            close_reason: Some(256),
            payload: Bytes::from_static(b"data"),
        };

        let encoded = packet.encode();
        let decoded = Packet::decode(&encoded).unwrap();

        assert_eq!(decoded.sack, Some(sack_data));
        assert_eq!(decoded.close_reason, Some(256));
        assert_eq!(decoded.payload, Bytes::from_static(b"data"));
    }

    #[test]
    fn unknown_extension_skipped_gracefully() {
        // Build a packet with extension type 99 (unknown) in header
        let mut buf = BytesMut::new();
        let hdr = Header {
            extension: ExtensionType::Unknown(99),
            ..sample_header()
        };
        hdr.encode(&mut buf);
        // Extension chain: next=0 (none), len=2, data=[0xAB, 0xCD]
        buf.put_u8(0); // next extension = none
        buf.put_u8(2); // length
        buf.put_u8(0xAB);
        buf.put_u8(0xCD);
        buf.put_slice(b"payload");

        let decoded = Packet::decode(&buf).unwrap();
        assert_eq!(decoded.sack, None);
        assert_eq!(decoded.close_reason, None);
        assert_eq!(decoded.payload, Bytes::from_static(b"payload"));
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
