//! I2P destination address.
//!
//! An I2P destination is a cryptographic identifier (~516 bytes) that serves
//! as the I2P equivalent of an IP address. It contains a 256-byte public key,
//! a 128-byte signing key, and a certificate. Conventionally encoded as Base64
//! with I2P's custom alphabet (uses `-` and `~` instead of `+` and `/`).

use std::fmt;

use serde::{Deserialize, Serialize};

/// An I2P destination address (~516 bytes, Base64-encoded for display/storage).
///
/// This is the I2P equivalent of a `SocketAddr`. Peers are identified by their
/// destination rather than by IP:port.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct I2pDestination {
    /// Raw binary destination (typically ~516 bytes).
    #[serde(with = "serde_bytes")]
    bytes: Vec<u8>,
}

/// I2P uses a modified Base64 alphabet: standard except `+` -> `-`, `/` -> `~`.
const I2P_BASE64_CHARS: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-~";

impl I2pDestination {
    /// Create from raw bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// The raw binary representation.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Encode to I2P-style Base64 string.
    pub fn to_base64(&self) -> String {
        i2p_base64_encode(&self.bytes)
    }

    /// Decode from I2P-style Base64 string.
    pub fn from_base64(s: &str) -> Result<Self, I2pDestinationError> {
        let bytes = i2p_base64_decode(s)?;
        if bytes.is_empty() {
            return Err(I2pDestinationError::Empty);
        }
        Ok(Self { bytes })
    }

    /// Compute the 52-character Base32 hash used in .b32.i2p addresses.
    ///
    /// This is SHA-256 of the destination bytes, encoded as Base32 (lowercase,
    /// no padding). The result is 52 characters.
    pub fn to_b32_address(&self) -> String {
        let hash = ferrite_core::sha256(&self.bytes);
        let mut out = String::with_capacity(52);
        base32_encode_lower(hash.as_bytes(), &mut out);
        format!("{}.b32.i2p", out)
    }

    /// Length of the raw binary destination.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the destination is empty (invalid).
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for I2pDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b64 = self.to_base64();
        if b64.len() > 16 {
            write!(f, "I2pDestination({}...{} bytes)", &b64[..16], self.bytes.len())
        } else {
            write!(f, "I2pDestination({})", b64)
        }
    }
}

impl fmt::Display for I2pDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_base64())
    }
}

/// Error type for I2P destination parsing.
#[derive(Debug, Clone, thiserror::Error)]
pub enum I2pDestinationError {
    /// The input string contains an invalid Base64 character.
    #[error("invalid Base64 character at position {0}")]
    InvalidBase64(/// Byte offset of the invalid character.
        usize),
    /// The destination was empty after decoding.
    #[error("empty destination")]
    Empty,
}

// ── I2P Base64 encode/decode ─────────────────────────────────────────

/// Encode bytes to I2P Base64 (uses `-` and `~` instead of `+` and `/`).
pub(crate) fn i2p_base64_encode(data: &[u8]) -> String {
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(I2P_BASE64_CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(I2P_BASE64_CHARS[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            result.push(I2P_BASE64_CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(I2P_BASE64_CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }

    result
}

/// Decode I2P Base64 string to bytes.
pub(crate) fn i2p_base64_decode(s: &str) -> Result<Vec<u8>, I2pDestinationError> {
    fn char_to_val(c: u8, pos: usize) -> Result<u32, I2pDestinationError> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'-' => Ok(62),
            b'~' => Ok(63),
            b'=' => Ok(0), // padding
            _ => Err(I2pDestinationError::InvalidBase64(pos)),
        }
    }

    let bytes = s.as_bytes();
    let mut result = Vec::with_capacity(bytes.len() * 3 / 4);

    for (chunk_idx, chunk) in bytes.chunks(4).enumerate() {
        if chunk.len() < 4 {
            // Incomplete final group -- reject
            if !chunk.is_empty() {
                return Err(I2pDestinationError::InvalidBase64(chunk_idx * 4));
            }
            break;
        }

        let base = chunk_idx * 4;
        let a = char_to_val(chunk[0], base)?;
        let b = char_to_val(chunk[1], base + 1)?;
        let c = char_to_val(chunk[2], base + 2)?;
        let d = char_to_val(chunk[3], base + 3)?;

        let triple = (a << 18) | (b << 12) | (c << 6) | d;

        result.push(((triple >> 16) & 0xFF) as u8);
        if chunk[2] != b'=' {
            result.push(((triple >> 8) & 0xFF) as u8);
        }
        if chunk[3] != b'=' {
            result.push((triple & 0xFF) as u8);
        }
    }

    Ok(result)
}

/// Encode bytes as lowercase Base32 (RFC 4648, no padding).
fn base32_encode_lower(data: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bits: u64 = 0;
    let mut num_bits: u32 = 0;

    for &byte in data {
        bits = (bits << 8) | byte as u64;
        num_bits += 8;
        while num_bits >= 5 {
            num_bits -= 5;
            out.push(ALPHABET[((bits >> num_bits) & 0x1F) as usize] as char);
        }
    }

    if num_bits > 0 {
        out.push(ALPHABET[((bits << (5 - num_bits)) & 0x1F) as usize] as char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i2p_base64_roundtrip() {
        let data = vec![0u8; 516]; // typical destination size
        let encoded = i2p_base64_encode(&data);
        let decoded = i2p_base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn i2p_base64_alphabet_differs_from_standard() {
        // I2P uses `-` (62) and `~` (63) instead of `+` and `/`
        let data = vec![0xFF, 0xFE, 0xFD]; // produces high-value sextets
        let encoded = i2p_base64_encode(&data);
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
    }

    #[test]
    fn i2p_base64_decode_invalid_char() {
        let err = i2p_base64_decode("AAAA+AAA").unwrap_err();
        assert!(matches!(err, I2pDestinationError::InvalidBase64(_)));
    }

    #[test]
    fn i2p_base64_known_vector() {
        // "hello" -> aGVsbG8= in standard Base64
        // In I2P Base64, same encoding since no +/~ characters needed
        let data = b"hello";
        let encoded = i2p_base64_encode(data);
        assert_eq!(encoded, "aGVsbG8=");
        let decoded = i2p_base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn destination_from_base64_roundtrip() {
        let raw = vec![42u8; 516];
        let dest = I2pDestination::from_bytes(raw.clone());
        let b64 = dest.to_base64();
        let parsed = I2pDestination::from_base64(&b64).unwrap();
        assert_eq!(parsed.as_bytes(), raw.as_slice());
        assert_eq!(parsed, dest);
    }

    #[test]
    fn destination_from_base64_empty_rejected() {
        let err = I2pDestination::from_base64("").unwrap_err();
        assert!(matches!(err, I2pDestinationError::Empty));
    }

    #[test]
    fn destination_debug_truncated() {
        let dest = I2pDestination::from_bytes(vec![0; 516]);
        let dbg = format!("{:?}", dest);
        assert!(dbg.contains("I2pDestination("));
        assert!(dbg.contains("..."));
        assert!(dbg.contains("516 bytes"));
    }

    #[test]
    fn destination_display_is_base64() {
        let dest = I2pDestination::from_bytes(vec![1, 2, 3]);
        let display = format!("{}", dest);
        let base64 = dest.to_base64();
        assert_eq!(display, base64);
    }

    #[test]
    fn destination_b32_address() {
        let dest = I2pDestination::from_bytes(vec![0u8; 516]);
        let b32 = dest.to_b32_address();
        assert!(b32.ends_with(".b32.i2p"));
        // SHA-256 -> 32 bytes -> 52 Base32 chars
        let host = b32.strip_suffix(".b32.i2p").unwrap();
        assert_eq!(host.len(), 52);
    }

    #[test]
    fn destination_hash_and_eq() {
        use std::collections::HashSet;
        let a = I2pDestination::from_bytes(vec![1, 2, 3]);
        let b = I2pDestination::from_bytes(vec![1, 2, 3]);
        let c = I2pDestination::from_bytes(vec![4, 5, 6]);
        assert_eq!(a, b);
        assert_ne!(a, c);

        let mut set = HashSet::new();
        set.insert(a.clone());
        set.insert(b); // duplicate
        set.insert(c);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn destination_serde_roundtrip() {
        let dest = I2pDestination::from_bytes(vec![7u8; 100]);
        let json = serde_json::to_string(&dest).unwrap();
        let parsed: I2pDestination = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, dest);
    }

    #[test]
    fn base32_encode_known_vector() {
        // SHA-256 of 32 zero bytes produces a known hash
        let hash = ferrite_core::sha256(&[0u8; 32]);
        let mut out = String::new();
        base32_encode_lower(hash.as_bytes(), &mut out);
        assert_eq!(out.len(), 52); // 32 bytes -> 52 Base32 chars
        // All chars must be lowercase alphanumeric or 2-7
        assert!(out.chars().all(|c| c.is_ascii_lowercase() || ('2'..='7').contains(&c)));
    }
}
