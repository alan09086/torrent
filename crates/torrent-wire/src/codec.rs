use bytes::{Buf, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::Error;
use crate::message::Message;

/// Maximum message size (16 MiB) — protects against malicious peers.
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Tokio codec for length-prefixed BitTorrent messages.
///
/// Wire format: `<4-byte big-endian length><payload>`
/// where payload = `<message-id><message-body>` (or empty for keep-alive).
pub struct MessageCodec {
    max_size: usize,
}

impl MessageCodec {
    /// Create a codec with the default maximum message size (16 MiB).
    pub fn new() -> Self {
        MessageCodec {
            max_size: MAX_MESSAGE_SIZE,
        }
    }

    /// Set the maximum allowed message size.
    pub fn with_max_size(mut self, max: usize) -> Self {
        self.max_size = max;
        self
    }
}

impl Default for MessageCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for MessageCodec {
    type Item = Message;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, Error> {
        // Need at least 4 bytes for the length prefix
        if src.len() < 4 {
            return Ok(None);
        }

        // Peek at the length (don't advance yet)
        let length = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;

        if length > self.max_size {
            return Err(Error::MessageTooLarge {
                size: length,
                max: self.max_size,
            });
        }

        // Check if we have the full message
        let total = 4 + length;
        if src.len() < total {
            // Reserve space for the rest
            src.reserve(total - src.len());
            return Ok(None);
        }

        // Consume length prefix
        src.advance(4);
        // Take the payload
        let payload = src.split_to(length);

        Message::from_payload(payload).map(Some)
    }
}

impl Encoder<Message> for MessageCodec {
    type Error = Error;

    fn encode(&mut self, msg: Message, dst: &mut BytesMut) -> Result<(), Error> {
        msg.encode_into(dst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn codec_decode_single_message() {
        let mut codec = MessageCodec::new();
        let msg = Message::Have { index: 42 };
        let wire = msg.to_bytes();

        let mut buf = BytesMut::from(wire.as_ref());
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert!(buf.is_empty());
    }

    #[test]
    fn codec_decode_partial_then_complete() {
        let mut codec = MessageCodec::new();
        let msg = Message::Request {
            index: 1,
            begin: 0,
            length: 16384,
        };
        let wire = msg.to_bytes();

        // Feed partial data first
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&wire[..5]); // only length + part of payload
        assert!(codec.decode(&mut buf).unwrap().is_none());

        // Feed the rest
        buf.extend_from_slice(&wire[5..]);
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn codec_decode_multiple_messages() {
        let mut codec = MessageCodec::new();
        let msg1 = Message::Choke;
        let msg2 = Message::Have { index: 7 };

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&msg1.to_bytes());
        buf.extend_from_slice(&msg2.to_bytes());

        let d1 = codec.decode(&mut buf).unwrap().unwrap();
        let d2 = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(d1, msg1);
        assert_eq!(d2, msg2);
    }

    #[test]
    fn codec_decode_keepalive() {
        let mut codec = MessageCodec::new();
        let mut buf = BytesMut::from(&[0u8, 0, 0, 0][..]);
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, Message::KeepAlive);
    }

    #[test]
    fn codec_reject_oversized() {
        let mut codec = MessageCodec::new().with_max_size(100);
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&200u32.to_be_bytes()); // length = 200 > max 100
        buf.extend_from_slice(&vec![0u8; 200]);

        let result = codec.decode(&mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn codec_encode() {
        let mut codec = MessageCodec::new();
        let msg = Message::Piece {
            index: 0,
            begin: 0,
            data: Bytes::from_static(b"data"),
        };

        let mut buf = BytesMut::new();
        codec.encode(msg.clone(), &mut buf).unwrap();

        // Decode it back
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn codec_insufficient_header() {
        let mut codec = MessageCodec::new();
        let mut buf = BytesMut::from(&[0u8, 0][..]); // only 2 bytes
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }
}
