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
    Have {
        /// Piece index.
        index: u32,
    },
    /// Peer's complete bitfield.
    Bitfield(Bytes),
    /// Request a block: piece index, byte offset within piece, length.
    Request {
        /// Piece index.
        index: u32,
        /// Byte offset within the piece.
        begin: u32,
        /// Requested block length in bytes.
        length: u32,
    },
    /// A data block: piece index, byte offset, data.
    Piece {
        /// Piece index.
        index: u32,
        /// Byte offset within the piece.
        begin: u32,
        /// Block payload.
        data: Bytes,
    },
    /// Cancel a previously sent request.
    Cancel {
        /// Piece index.
        index: u32,
        /// Byte offset within the piece.
        begin: u32,
        /// Block length in bytes.
        length: u32,
    },
    /// DHT port (BEP 5).
    Port(u16),
    /// Extension message (BEP 10). ext_id=0 is handshake.
    Extended {
        /// Extension message ID (0 = handshake).
        ext_id: u8,
        /// Bencoded extension payload.
        payload: Bytes,
    },
    /// BEP 6: Suggest a piece for the peer to download.
    SuggestPiece(u32),
    /// BEP 6: We have all pieces.
    HaveAll,
    /// BEP 6: We have no pieces.
    HaveNone,
    /// BEP 6: Reject a request from the peer.
    RejectRequest {
        /// Piece index.
        index: u32,
        /// Byte offset within the piece.
        begin: u32,
        /// Block length in bytes.
        length: u32,
    },
    /// BEP 6: Piece index the peer is allowed to request while choked.
    AllowedFast(u32),
    /// BEP 52: Request hashes from a file's Merkle tree.
    HashRequest {
        /// File root hash identifying the Merkle tree.
        pieces_root: torrent_core::Id32,
        /// Tree layer (0 = leaf/block layer).
        base: u32,
        /// Starting node index within the layer.
        index: u32,
        /// Number of consecutive hashes requested.
        count: u32,
        /// Number of uncle proof layers to include.
        proof_layers: u32,
    },
    /// BEP 52: Response with hashes and uncle proof.
    Hashes {
        /// File root hash identifying the Merkle tree.
        pieces_root: torrent_core::Id32,
        /// Tree layer (0 = leaf/block layer).
        base: u32,
        /// Starting node index within the layer.
        index: u32,
        /// Number of consecutive hashes in the response.
        count: u32,
        /// Number of uncle proof layers included.
        proof_layers: u32,
        /// Hash values followed by uncle proof hashes.
        hashes: Vec<torrent_core::Id32>,
    },
    /// BEP 52: Reject a hash request.
    HashReject {
        /// File root hash identifying the Merkle tree.
        pieces_root: torrent_core::Id32,
        /// Tree layer that was requested.
        base: u32,
        /// Starting node index that was requested.
        index: u32,
        /// Number of hashes that was requested.
        count: u32,
        /// Number of proof layers that was requested.
        proof_layers: u32,
    },
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

// BEP 6 Fast Extension
const ID_SUGGEST_PIECE: u8 = 0x0D;
const ID_HAVE_ALL: u8 = 0x0E;
const ID_HAVE_NONE: u8 = 0x0F;
const ID_REJECT_REQUEST: u8 = 0x10;
const ID_ALLOWED_FAST: u8 = 0x11;

// BEP 52 Hash Messages
const ID_HASH_REQUEST: u8 = 21;
const ID_HASHES: u8 = 22;
const ID_HASH_REJECT: u8 = 23;

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
            Message::SuggestPiece(index) => {
                let mut buf = BytesMut::with_capacity(9);
                buf.put_u32(5);
                buf.put_u8(ID_SUGGEST_PIECE);
                buf.put_u32(*index);
                buf.freeze()
            }
            Message::HaveAll => fixed_msg(ID_HAVE_ALL),
            Message::HaveNone => fixed_msg(ID_HAVE_NONE),
            Message::RejectRequest {
                index,
                begin,
                length,
            } => triple_msg(ID_REJECT_REQUEST, *index, *begin, *length),
            Message::AllowedFast(index) => {
                let mut buf = BytesMut::with_capacity(9);
                buf.put_u32(5);
                buf.put_u8(ID_ALLOWED_FAST);
                buf.put_u32(*index);
                buf.freeze()
            }
            Message::HashRequest {
                pieces_root,
                base,
                index,
                count,
                proof_layers,
            }
            | Message::HashReject {
                pieces_root,
                base,
                index,
                count,
                proof_layers,
            } => {
                let id = match self {
                    Message::HashRequest { .. } => ID_HASH_REQUEST,
                    _ => ID_HASH_REJECT,
                };
                let mut buf = BytesMut::with_capacity(53);
                buf.put_u32(49); // 1 + 32 + 4*4
                buf.put_u8(id);
                buf.put_slice(&pieces_root.0);
                buf.put_u32(*base);
                buf.put_u32(*index);
                buf.put_u32(*count);
                buf.put_u32(*proof_layers);
                buf.freeze()
            }
            Message::Hashes {
                pieces_root,
                base,
                index,
                count,
                proof_layers,
                hashes,
            } => {
                let hash_bytes = hashes.len() * 32;
                let payload_len = 1 + 32 + 16 + hash_bytes;
                let mut buf = BytesMut::with_capacity(4 + payload_len);
                buf.put_u32(payload_len as u32);
                buf.put_u8(ID_HASHES);
                buf.put_slice(&pieces_root.0);
                buf.put_u32(*base);
                buf.put_u32(*index);
                buf.put_u32(*count);
                buf.put_u32(*proof_layers);
                for h in hashes {
                    buf.put_slice(&h.0);
                }
                buf.freeze()
            }
        }
    }

    /// Encode this message (with length prefix) directly into a buffer.
    ///
    /// Unlike [`to_bytes`](Self::to_bytes), this writes directly into `dst`
    /// without allocating an intermediate `Bytes`, avoiding a double-copy
    /// when used with `tokio_util::codec::Encoder`.
    pub fn encode_into(&self, dst: &mut BytesMut) {
        match self {
            Message::KeepAlive => {
                dst.put_u32(0);
            }
            Message::Choke => encode_fixed_into(dst, ID_CHOKE),
            Message::Unchoke => encode_fixed_into(dst, ID_UNCHOKE),
            Message::Interested => encode_fixed_into(dst, ID_INTERESTED),
            Message::NotInterested => encode_fixed_into(dst, ID_NOT_INTERESTED),
            Message::Have { index } => {
                dst.put_u32(5);
                dst.put_u8(ID_HAVE);
                dst.put_u32(*index);
            }
            Message::Bitfield(bits) => {
                dst.reserve(5 + bits.len());
                dst.put_u32(1 + bits.len() as u32);
                dst.put_u8(ID_BITFIELD);
                dst.put_slice(bits);
            }
            Message::Request {
                index,
                begin,
                length,
            } => encode_triple_into(dst, ID_REQUEST, *index, *begin, *length),
            Message::Piece { index, begin, data } => {
                dst.reserve(13 + data.len());
                dst.put_u32(9 + data.len() as u32);
                dst.put_u8(ID_PIECE);
                dst.put_u32(*index);
                dst.put_u32(*begin);
                dst.put_slice(data);
            }
            Message::Cancel {
                index,
                begin,
                length,
            } => encode_triple_into(dst, ID_CANCEL, *index, *begin, *length),
            Message::Port(port) => {
                dst.put_u32(3);
                dst.put_u8(ID_PORT);
                dst.put_u16(*port);
            }
            Message::Extended { ext_id, payload } => {
                dst.reserve(6 + payload.len());
                dst.put_u32(2 + payload.len() as u32);
                dst.put_u8(ID_EXTENDED);
                dst.put_u8(*ext_id);
                dst.put_slice(payload);
            }
            Message::SuggestPiece(index) => {
                dst.put_u32(5);
                dst.put_u8(ID_SUGGEST_PIECE);
                dst.put_u32(*index);
            }
            Message::HaveAll => encode_fixed_into(dst, ID_HAVE_ALL),
            Message::HaveNone => encode_fixed_into(dst, ID_HAVE_NONE),
            Message::RejectRequest {
                index,
                begin,
                length,
            } => encode_triple_into(dst, ID_REJECT_REQUEST, *index, *begin, *length),
            Message::AllowedFast(index) => {
                dst.put_u32(5);
                dst.put_u8(ID_ALLOWED_FAST);
                dst.put_u32(*index);
            }
            Message::HashRequest {
                pieces_root,
                base,
                index,
                count,
                proof_layers,
            }
            | Message::HashReject {
                pieces_root,
                base,
                index,
                count,
                proof_layers,
            } => {
                let id = match self {
                    Message::HashRequest { .. } => ID_HASH_REQUEST,
                    _ => ID_HASH_REJECT,
                };
                dst.put_u32(49); // 1 + 32 + 4*4
                dst.put_u8(id);
                dst.put_slice(&pieces_root.0);
                dst.put_u32(*base);
                dst.put_u32(*index);
                dst.put_u32(*count);
                dst.put_u32(*proof_layers);
            }
            Message::Hashes {
                pieces_root,
                base,
                index,
                count,
                proof_layers,
                hashes,
            } => {
                let hash_bytes = hashes.len() * 32;
                let payload_len = 1 + 32 + 16 + hash_bytes;
                dst.reserve(4 + payload_len);
                dst.put_u32(payload_len as u32);
                dst.put_u8(ID_HASHES);
                dst.put_slice(&pieces_root.0);
                dst.put_u32(*base);
                dst.put_u32(*index);
                dst.put_u32(*count);
                dst.put_u32(*proof_layers);
                for h in hashes {
                    dst.put_slice(&h.0);
                }
            }
        }
    }

    /// Parse a message from its payload (after the 4-byte length prefix has
    /// been consumed). `payload` is everything after the length prefix.
    pub fn from_payload(payload: BytesMut) -> Result<Self> {
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
            ID_BITFIELD => Ok(Message::Bitfield(payload.freeze().slice(1..))),
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
                let index = read_u32(body);
                let begin = read_u32(&body[4..]);
                Ok(Message::Piece {
                    index,
                    begin,
                    data: payload.freeze().slice(9..),
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
                let ext_id = body[0];
                Ok(Message::Extended {
                    ext_id,
                    payload: payload.freeze().slice(2..),
                })
            }
            ID_SUGGEST_PIECE => {
                ensure_len(body, 4, "SuggestPiece")?;
                Ok(Message::SuggestPiece(read_u32(body)))
            }
            ID_HAVE_ALL => Ok(Message::HaveAll),
            ID_HAVE_NONE => Ok(Message::HaveNone),
            ID_REJECT_REQUEST => {
                ensure_len(body, 12, "RejectRequest")?;
                Ok(Message::RejectRequest {
                    index: read_u32(body),
                    begin: read_u32(&body[4..]),
                    length: read_u32(&body[8..]),
                })
            }
            ID_ALLOWED_FAST => {
                ensure_len(body, 4, "AllowedFast")?;
                Ok(Message::AllowedFast(read_u32(body)))
            }
            ID_HASH_REQUEST | ID_HASH_REJECT => {
                ensure_len(body, 48, "HashRequest/Reject")?;
                let mut root = [0u8; 32];
                root.copy_from_slice(&body[..32]);
                let pieces_root = torrent_core::Id32(root);
                let base = read_u32(&body[32..]);
                let index = read_u32(&body[36..]);
                let count = read_u32(&body[40..]);
                let proof_layers = read_u32(&body[44..]);
                if id == ID_HASH_REQUEST {
                    Ok(Message::HashRequest {
                        pieces_root,
                        base,
                        index,
                        count,
                        proof_layers,
                    })
                } else {
                    Ok(Message::HashReject {
                        pieces_root,
                        base,
                        index,
                        count,
                        proof_layers,
                    })
                }
            }
            ID_HASHES => {
                ensure_len(body, 48, "Hashes")?;
                let mut root = [0u8; 32];
                root.copy_from_slice(&body[..32]);
                let pieces_root = torrent_core::Id32(root);
                let base = read_u32(&body[32..]);
                let index = read_u32(&body[36..]);
                let count = read_u32(&body[40..]);
                let proof_layers = read_u32(&body[44..]);
                let hash_data = &body[48..];
                if !hash_data.len().is_multiple_of(32) {
                    return Err(Error::MessageTooShort {
                        expected: 48 + 32,
                        got: body.len(),
                    });
                }
                let hashes = hash_data
                    .chunks_exact(32)
                    .map(|chunk| {
                        let mut h = [0u8; 32];
                        h.copy_from_slice(chunk);
                        torrent_core::Id32(h)
                    })
                    .collect();
                Ok(Message::Hashes {
                    pieces_root,
                    base,
                    index,
                    count,
                    proof_layers,
                    hashes,
                })
            }
            _ => Err(Error::InvalidMessageId(id)),
        }
    }
}

fn encode_fixed_into(dst: &mut BytesMut, id: u8) {
    dst.put_u32(1);
    dst.put_u8(id);
}

fn encode_triple_into(dst: &mut BytesMut, id: u8, a: u32, b: u32, c: u32) {
    dst.put_u32(13);
    dst.put_u8(id);
    dst.put_u32(a);
    dst.put_u32(b);
    dst.put_u32(c);
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

/// BEP 6 Allowed-Fast set generation.
///
/// Generates a deterministic set of piece indices that a peer is allowed
/// to request even while choked. Uses IP masking + info_hash + SHA1.
///
/// For IPv4: masks to /24 (matching BEP 6 spec).
/// For IPv6: masks to /48 (matching libtorrent convention).
pub fn allowed_fast_set(
    info_hash: &torrent_core::Id20,
    peer_ip: std::net::Ipv4Addr,
    num_pieces: u32,
    count: usize,
) -> Vec<u32> {
    allowed_fast_set_for_ip(info_hash, std::net::IpAddr::V4(peer_ip), num_pieces, count)
}

/// BEP 6 Allowed-Fast set generation for any IP address family.
///
/// IPv4: /24 prefix mask. IPv6: /48 prefix mask (libtorrent convention).
pub fn allowed_fast_set_for_ip(
    info_hash: &torrent_core::Id20,
    peer_ip: std::net::IpAddr,
    num_pieces: u32,
    count: usize,
) -> Vec<u32> {
    use torrent_core::sha1;

    if num_pieces == 0 {
        return Vec::new();
    }

    let count = count.min(num_pieces as usize);
    let mut result = Vec::with_capacity(count);

    // Build masked IP bytes based on address family
    let masked: Vec<u8> = match peer_ip {
        std::net::IpAddr::V4(ipv4) => {
            // Mask to /24
            let o = ipv4.octets();
            vec![o[0], o[1], o[2], 0]
        }
        std::net::IpAddr::V6(ipv6) => {
            // Mask to /48: keep first 6 bytes, zero the rest
            let o = ipv6.octets();
            let mut masked = [0u8; 16];
            masked[..6].copy_from_slice(&o[..6]);
            masked.to_vec()
        }
    };

    // Initial hash: SHA1(masked_ip + info_hash)
    let mut input = Vec::with_capacity(masked.len() + 20);
    input.extend_from_slice(&masked);
    input.extend_from_slice(info_hash.as_bytes());
    let mut hash = sha1(&input);

    while result.len() < count {
        let hash_bytes = hash.as_bytes();
        // Each 20-byte hash gives us 5 candidate indices (4 bytes each)
        for i in (0..20).step_by(4) {
            if result.len() >= count {
                break;
            }
            let index = u32::from_be_bytes([
                hash_bytes[i],
                hash_bytes[i + 1],
                hash_bytes[i + 2],
                hash_bytes[i + 3],
            ]) % num_pieces;
            if !result.contains(&index) {
                result.push(index);
            }
        }
        // Re-hash for more candidates
        hash = sha1(hash.as_bytes());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: Message) {
        let bytes = msg.to_bytes();
        // Skip the 4-byte length prefix for parsing
        let parsed = Message::from_payload(BytesMut::from(&bytes[4..])).unwrap();
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
        assert!(Message::from_payload(BytesMut::from(&[99u8][..])).is_err());
    }

    #[test]
    fn suggest_piece() {
        round_trip(Message::SuggestPiece(42));
    }

    #[test]
    fn have_all() {
        round_trip(Message::HaveAll);
    }

    #[test]
    fn have_none() {
        round_trip(Message::HaveNone);
    }

    #[test]
    fn reject_request() {
        round_trip(Message::RejectRequest {
            index: 1,
            begin: 0,
            length: 16384,
        });
    }

    #[test]
    fn allowed_fast() {
        round_trip(Message::AllowedFast(7));
    }

    #[test]
    fn allowed_fast_set_deterministic() {
        use torrent_core::Id20;
        let ih = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let ip: std::net::Ipv4Addr = "192.168.1.100".parse().unwrap();
        let set1 = allowed_fast_set(&ih, ip, 1000, 10);
        let set2 = allowed_fast_set(&ih, ip, 1000, 10);
        assert_eq!(set1, set2);
        assert_eq!(set1.len(), 10);
        // All indices in range
        assert!(set1.iter().all(|&i| i < 1000));
    }

    #[test]
    fn allowed_fast_set_unique() {
        use torrent_core::Id20;
        let ih = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let ip: std::net::Ipv4Addr = "10.0.0.1".parse().unwrap();
        let set = allowed_fast_set(&ih, ip, 50, 10);
        let unique: std::collections::HashSet<u32> = set.iter().copied().collect();
        assert_eq!(set.len(), unique.len(), "all indices should be unique");
    }

    #[test]
    fn allowed_fast_set_empty_torrent() {
        use torrent_core::Id20;
        let ih = Id20::ZERO;
        let ip: std::net::Ipv4Addr = "127.0.0.1".parse().unwrap();
        assert!(allowed_fast_set(&ih, ip, 0, 10).is_empty());
    }

    #[test]
    fn allowed_fast_set_ipv6() {
        use torrent_core::Id20;
        let ih = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let ip: std::net::IpAddr = "2001:db8::1".parse().unwrap();
        let set = allowed_fast_set_for_ip(&ih, ip, 1000, 10);
        assert_eq!(set.len(), 10);
        assert!(set.iter().all(|&i| i < 1000));

        // Same /48 prefix should produce same set
        let ip2: std::net::IpAddr = "2001:db8::ffff".parse().unwrap();
        let set2 = allowed_fast_set_for_ip(&ih, ip2, 1000, 10);
        assert_eq!(set, set2);

        // Different /48 prefix should produce different set
        let ip3: std::net::IpAddr = "2001:db9::1".parse().unwrap();
        let set3 = allowed_fast_set_for_ip(&ih, ip3, 1000, 10);
        assert_ne!(set, set3);
    }

    #[test]
    fn hash_request_round_trip() {
        let msg = Message::HashRequest {
            pieces_root: torrent_core::Id32::ZERO,
            base: 7,
            index: 0,
            count: 512,
            proof_layers: 3,
        };
        round_trip(msg);
    }

    #[test]
    fn hash_reject_round_trip() {
        let msg = Message::HashReject {
            pieces_root: torrent_core::Id32::ZERO,
            base: 7,
            index: 0,
            count: 512,
            proof_layers: 3,
        };
        round_trip(msg);
    }

    #[test]
    fn hashes_round_trip() {
        let h1 = torrent_core::sha256(b"block1");
        let h2 = torrent_core::sha256(b"block2");
        let uncle = torrent_core::sha256(b"uncle");
        let msg = Message::Hashes {
            pieces_root: torrent_core::Id32::ZERO,
            base: 0,
            index: 0,
            count: 2,
            proof_layers: 1,
            hashes: vec![h1, h2, uncle],
        };
        round_trip(msg);
    }

    #[test]
    fn hash_request_exact_wire_size() {
        let msg = Message::HashRequest {
            pieces_root: torrent_core::Id32::ZERO,
            base: 0,
            index: 0,
            count: 1,
            proof_layers: 0,
        };
        let bytes = msg.to_bytes();
        // 4 (length prefix) + 1 (msg id) + 32 (root) + 4*4 (fields) = 53
        assert_eq!(bytes.len(), 53);
    }

    #[test]
    fn hashes_variable_length() {
        let h = torrent_core::sha256(b"test");
        let msg = Message::Hashes {
            pieces_root: torrent_core::Id32::ZERO,
            base: 0,
            index: 0,
            count: 1,
            proof_layers: 0,
            hashes: vec![h],
        };
        let bytes = msg.to_bytes();
        // 4 + 1 + 32 + 4*4 + 1*32 = 85
        assert_eq!(bytes.len(), 85);
    }

    #[test]
    fn hash_request_too_short() {
        // msg id 21, but only 10 bytes of body (need 48)
        let mut payload = vec![21u8];
        payload.extend_from_slice(&[0u8; 10]);
        assert!(Message::from_payload(BytesMut::from(&payload[..])).is_err());
    }

    #[test]
    fn encode_into_matches_to_bytes() {
        let messages = vec![
            Message::KeepAlive,
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have { index: 42 },
            Message::Bitfield(Bytes::from_static(b"\xff\x00")),
            Message::Request {
                index: 1,
                begin: 0,
                length: 16384,
            },
            Message::Piece {
                index: 0,
                begin: 0,
                data: Bytes::from_static(b"block data here"),
            },
            Message::Cancel {
                index: 1,
                begin: 0,
                length: 16384,
            },
            Message::Port(6881),
            Message::Extended {
                ext_id: 0,
                payload: Bytes::from_static(b"ext payload"),
            },
            Message::SuggestPiece(7),
            Message::HaveAll,
            Message::HaveNone,
            Message::RejectRequest {
                index: 1,
                begin: 0,
                length: 16384,
            },
            Message::AllowedFast(5),
            Message::HashRequest {
                pieces_root: torrent_core::Id32::ZERO,
                base: 7,
                index: 0,
                count: 512,
                proof_layers: 3,
            },
            Message::HashReject {
                pieces_root: torrent_core::Id32::ZERO,
                base: 7,
                index: 0,
                count: 512,
                proof_layers: 3,
            },
            Message::Hashes {
                pieces_root: torrent_core::Id32::ZERO,
                base: 0,
                index: 0,
                count: 2,
                proof_layers: 1,
                hashes: vec![
                    torrent_core::sha256(b"block1"),
                    torrent_core::sha256(b"block2"),
                    torrent_core::sha256(b"uncle"),
                ],
            },
        ];
        for msg in messages {
            let expected = msg.to_bytes();
            let mut buf = BytesMut::new();
            msg.encode_into(&mut buf);
            assert_eq!(&expected[..], &buf[..], "mismatch for {msg:?}");
        }
    }

    #[test]
    fn allowed_fast_set_ipv4_compat() {
        // allowed_fast_set (IPv4-only) and allowed_fast_set_for_ip with V4 should match
        use torrent_core::Id20;
        let ih = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let ipv4: std::net::Ipv4Addr = "192.168.1.100".parse().unwrap();
        let set_v4 = allowed_fast_set(&ih, ipv4, 1000, 10);
        let set_ip = allowed_fast_set_for_ip(&ih, std::net::IpAddr::V4(ipv4), 1000, 10);
        assert_eq!(set_v4, set_ip);
    }
}
