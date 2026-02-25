use bytes::{BufMut, Bytes, BytesMut};

use crate::error::{Error, Result};

/// Standard BitTorrent peer wire messages (BEP 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Keep connection alive (no payload, length=0).
    KeepAlive,
    /// Peer is choking us.
    Choke,
    /// Peer is unchoking us.
    Unchoke,
    /// We're interested in the peer's data.
    Interested,
    /// We're not interested.
    NotInterested,
    /// Peer has piece `index`.
    Have { index: u32 },
    /// Peer's complete bitfield.
    Bitfield(Bytes),
    /// Request a block: piece index, byte offset within piece, length.
    Request { index: u32, begin: u32, length: u32 },
    /// A data block: piece index, byte offset, data.
    Piece { index: u32, begin: u32, data: Bytes },
    /// Cancel a previously sent request.
    Cancel { index: u32, begin: u32, length: u32 },
    /// DHT port (BEP 5).
    Port(u16),
    /// Extension message (BEP 10). ext_id=0 is handshake.
    Extended { ext_id: u8, payload: Bytes },
}

// Message IDs per BEP 3
const ID_CHOKE: u8 = 0;
const ID_UNCHOKE: u8 = 1;
const ID_INTERESTED: u8 = 2;
const ID_NOT_INTERESTED: u8 = 3;
const ID_HAVE: u8 = 4;
const ID_BITFIELD: u8 = 5;
const ID_REQUEST: u8 = 6;
const ID_PIECE: u8 = 7;
const ID_CANCEL: u8 = 8;
const ID_PORT: u8 = 9;
const ID_EXTENDED: u8 = 20;

impl Message {
    /// Serialize a message to bytes (length-prefix + id + payload).
    ///
    /// The returned bytes include the 4-byte length prefix.
    pub fn to_bytes(&self) -> Bytes {
        match self {
            Message::KeepAlive => {
                let mut buf = BytesMut::with_capacity(4);
                buf.put_u32(0);
                buf.freeze()
            }
            Message::Choke => fixed_msg(ID_CHOKE),
            Message::Unchoke => fixed_msg(ID_UNCHOKE),
            Message::Interested => fixed_msg(ID_INTERESTED),
            Message::NotInterested => fixed_msg(ID_NOT_INTERESTED),
            Message::Have { index } => {
                let mut buf = BytesMut::with_capacity(9);
                buf.put_u32(5);
                buf.put_u8(ID_HAVE);
                buf.put_u32(*index);
                buf.freeze()
            }
            Message::Bitfield(bits) => {
                let mut buf = BytesMut::with_capacity(5 + bits.len());
                buf.put_u32(1 + bits.len() as u32);
                buf.put_u8(ID_BITFIELD);
                buf.put_slice(bits);
                buf.freeze()
            }
            Message::Request {
                index,
                begin,
                length,
            } => triple_msg(ID_REQUEST, *index, *begin, *length),
            Message::Piece { index, begin, data } => {
                let mut buf = BytesMut::with_capacity(13 + data.len());
                buf.put_u32(9 + data.len() as u32);
                buf.put_u8(ID_PIECE);
                buf.put_u32(*index);
                buf.put_u32(*begin);
                buf.put_slice(data);
                buf.freeze()
            }
            Message::Cancel {
                index,
                begin,
                length,
            } => triple_msg(ID_CANCEL, *index, *begin, *length),
            Message::Port(port) => {
                let mut buf = BytesMut::with_capacity(7);
                buf.put_u32(3);
                buf.put_u8(ID_PORT);
                buf.put_u16(*port);
                buf.freeze()
            }
            Message::Extended { ext_id, payload } => {
                let mut buf = BytesMut::with_capacity(6 + payload.len());
                buf.put_u32(2 + payload.len() as u32);
                buf.put_u8(ID_EXTENDED);
                buf.put_u8(*ext_id);
                buf.put_slice(payload);
                buf.freeze()
            }
        }
    }

    /// Parse a message from its payload (after the 4-byte length prefix has
    /// been consumed). `payload` is everything after the length prefix.
    pub fn from_payload(payload: &[u8]) -> Result<Self> {
        if payload.is_empty() {
            return Ok(Message::KeepAlive);
        }

        let id = payload[0];
        let body = &payload[1..];

        match id {
            ID_CHOKE => Ok(Message::Choke),
            ID_UNCHOKE => Ok(Message::Unchoke),
            ID_INTERESTED => Ok(Message::Interested),
            ID_NOT_INTERESTED => Ok(Message::NotInterested),
            ID_HAVE => {
                ensure_len(body, 4, "Have")?;
                Ok(Message::Have {
                    index: read_u32(body),
                })
            }
            ID_BITFIELD => Ok(Message::Bitfield(Bytes::copy_from_slice(body))),
            ID_REQUEST => {
                ensure_len(body, 12, "Request")?;
                Ok(Message::Request {
                    index: read_u32(body),
                    begin: read_u32(&body[4..]),
                    length: read_u32(&body[8..]),
                })
            }
            ID_PIECE => {
                ensure_len(body, 8, "Piece")?;
                Ok(Message::Piece {
                    index: read_u32(body),
                    begin: read_u32(&body[4..]),
                    data: Bytes::copy_from_slice(&body[8..]),
                })
            }
            ID_CANCEL => {
                ensure_len(body, 12, "Cancel")?;
                Ok(Message::Cancel {
                    index: read_u32(body),
                    begin: read_u32(&body[4..]),
                    length: read_u32(&body[8..]),
                })
            }
            ID_PORT => {
                ensure_len(body, 2, "Port")?;
                Ok(Message::Port(u16::from_be_bytes([body[0], body[1]])))
            }
            ID_EXTENDED => {
                ensure_len(body, 1, "Extended")?;
                Ok(Message::Extended {
                    ext_id: body[0],
                    payload: Bytes::copy_from_slice(&body[1..]),
                })
            }
            _ => Err(Error::InvalidMessageId(id)),
        }
    }
}

fn fixed_msg(id: u8) -> Bytes {
    let mut buf = BytesMut::with_capacity(5);
    buf.put_u32(1);
    buf.put_u8(id);
    buf.freeze()
}

fn triple_msg(id: u8, a: u32, b: u32, c: u32) -> Bytes {
    let mut buf = BytesMut::with_capacity(17);
    buf.put_u32(13);
    buf.put_u8(id);
    buf.put_u32(a);
    buf.put_u32(b);
    buf.put_u32(c);
    buf.freeze()
}

fn read_u32(buf: &[u8]) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&buf[..4]);
    u32::from_be_bytes(b)
}

fn ensure_len(body: &[u8], min: usize, _name: &str) -> Result<()> {
    if body.len() < min {
        Err(Error::MessageTooShort {
            expected: min,
            got: body.len(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: Message) {
        let bytes = msg.to_bytes();
        // Skip the 4-byte length prefix for parsing
        let parsed = Message::from_payload(&bytes[4..]).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn keepalive() {
        round_trip(Message::KeepAlive);
    }

    #[test]
    fn choke_unchoke() {
        round_trip(Message::Choke);
        round_trip(Message::Unchoke);
    }

    #[test]
    fn interested() {
        round_trip(Message::Interested);
        round_trip(Message::NotInterested);
    }

    #[test]
    fn have() {
        round_trip(Message::Have { index: 42 });
    }

    #[test]
    fn bitfield() {
        round_trip(Message::Bitfield(Bytes::from_static(&[0xFF, 0x80])));
    }

    #[test]
    fn request() {
        round_trip(Message::Request {
            index: 1,
            begin: 0,
            length: 16384,
        });
    }

    #[test]
    fn piece() {
        round_trip(Message::Piece {
            index: 1,
            begin: 0,
            data: Bytes::from_static(b"hello world"),
        });
    }

    #[test]
    fn cancel() {
        round_trip(Message::Cancel {
            index: 1,
            begin: 0,
            length: 16384,
        });
    }

    #[test]
    fn port() {
        round_trip(Message::Port(6881));
    }

    #[test]
    fn extended() {
        round_trip(Message::Extended {
            ext_id: 1,
            payload: Bytes::from_static(b"test payload"),
        });
    }

    #[test]
    fn invalid_message_id() {
        assert!(Message::from_payload(&[99]).is_err());
    }
}
