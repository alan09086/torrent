use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// 20-byte identifier used for SHA1 info-hashes and peer IDs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id20(pub [u8; 20]);

/// 32-byte identifier used for SHA-256 (BitTorrent v2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id32(pub [u8; 32]);

// ---- Id20 ----

impl Id20 {
    /// All zeros.
    pub const ZERO: Self = Id20([0u8; 20]);

    /// Create from a hex string (40 chars).
    pub fn from_hex(s: &str) -> Result<Self, Error> {
        let bytes = hex::decode(s).map_err(|e| Error::InvalidHex(e.to_string()))?;
        Self::from_bytes(&bytes)
    }

    /// Create from a base32 string (32 chars, used in magnet links).
    pub fn from_base32(s: &str) -> Result<Self, Error> {
        let bytes = data_encoding::BASE32_NOPAD
            .decode(s.as_bytes())
            .map_err(|e| Error::InvalidHex(format!("base32: {e}")))?;
        Self::from_bytes(&bytes)
    }

    /// Create from a byte slice, validating length.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let arr: [u8; 20] = bytes.try_into().map_err(|_| Error::InvalidHashLength {
            expected: 20,
            got: bytes.len(),
        })?;
        Ok(Id20(arr))
    }

    /// Encode as lowercase hex string.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Encode as base32 string (no padding), used in magnet links.
    pub fn to_base32(&self) -> String {
        data_encoding::BASE32_NOPAD.encode(&self.0)
    }

    /// XOR distance to another Id20 (used in DHT/Kademlia).
    pub fn xor_distance(&self, other: &Id20) -> Id20 {
        let mut result = [0u8; 20];
        for (i, byte) in result.iter_mut().enumerate() {
            *byte = self.0[i] ^ other.0[i];
        }
        Id20(result)
    }

    /// Return the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl fmt::Debug for Id20 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id20({})", self.to_hex())
    }
}

impl fmt::Display for Id20 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl AsRef<[u8]> for Id20 {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<[u8; 20]> for Id20 {
    fn from(arr: [u8; 20]) -> Self {
        Id20(arr)
    }
}

impl Serialize for Id20 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id20 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = serde_bytes::deserialize(deserializer)?;
        Id20::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}

// ---- Id32 ----

impl Id32 {
    /// All zeros.
    pub const ZERO: Self = Id32([0u8; 32]);

    /// BEP 52 multihash prefix: SHA-256 function code (0x12) + digest length (0x20 = 32).
    const MULTIHASH_PREFIX: [u8; 2] = [0x12, 0x20];

    /// Create from a hex string (64 chars).
    pub fn from_hex(s: &str) -> Result<Self, Error> {
        let bytes = hex::decode(s).map_err(|e| Error::InvalidHex(e.to_string()))?;
        Self::from_bytes(&bytes)
    }

    /// Create from a base32 string (used in v2 magnet links).
    pub fn from_base32(s: &str) -> Result<Self, Error> {
        let bytes = data_encoding::BASE32_NOPAD
            .decode(s.as_bytes())
            .map_err(|e| Error::InvalidHex(format!("base32: {e}")))?;
        Self::from_bytes(&bytes)
    }

    /// Create from a byte slice, validating length.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let arr: [u8; 32] = bytes.try_into().map_err(|_| Error::InvalidHashLength {
            expected: 32,
            got: bytes.len(),
        })?;
        Ok(Id32(arr))
    }

    /// Encode as lowercase hex string.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Encode as base32 string (no padding), used in v2 magnet links.
    pub fn to_base32(&self) -> String {
        data_encoding::BASE32_NOPAD.encode(&self.0)
    }

    /// Encode as multihash hex string for BEP 52 magnet URIs (`urn:btmh:`).
    ///
    /// Format: `1220` prefix (SHA-256 function code + 32-byte length) + 64 hex chars.
    pub fn to_multihash_hex(&self) -> String {
        let mut buf = Vec::with_capacity(34);
        buf.extend_from_slice(&Self::MULTIHASH_PREFIX);
        buf.extend_from_slice(&self.0);
        hex::encode(buf)
    }

    /// Decode from a multihash hex string (BEP 52 magnet format).
    ///
    /// Validates the `1220` prefix (SHA-256 function code + 32-byte length).
    pub fn from_multihash_hex(s: &str) -> Result<Self, Error> {
        let bytes = hex::decode(s).map_err(|e| Error::InvalidHex(e.to_string()))?;
        if bytes.len() != 34 {
            return Err(Error::InvalidHashLength {
                expected: 34,
                got: bytes.len(),
            });
        }
        if bytes[0] != Self::MULTIHASH_PREFIX[0] || bytes[1] != Self::MULTIHASH_PREFIX[1] {
            return Err(Error::InvalidHex(format!(
                "invalid multihash prefix: expected 1220, got {:02x}{:02x}",
                bytes[0], bytes[1]
            )));
        }
        Self::from_bytes(&bytes[2..])
    }

    /// Return the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Id32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id32({})", self.to_hex())
    }
}

impl fmt::Display for Id32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl AsRef<[u8]> for Id32 {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<[u8; 32]> for Id32 {
    fn from(arr: [u8; 32]) -> Self {
        Id32(arr)
    }
}

impl Serialize for Id32 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id32 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = serde_bytes::deserialize(deserializer)?;
        Id32::from_bytes(&bytes).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id20_hex_round_trip() {
        let hex_str = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";
        let id = Id20::from_hex(hex_str).unwrap();
        assert_eq!(id.to_hex(), hex_str);
    }

    #[test]
    fn id20_base32_round_trip() {
        let id = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let b32 = id.to_base32();
        let id2 = Id20::from_base32(&b32).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn id20_xor_distance() {
        let a = Id20::from_hex("0000000000000000000000000000000000000001").unwrap();
        let b = Id20::from_hex("0000000000000000000000000000000000000003").unwrap();
        let dist = a.xor_distance(&b);
        assert_eq!(
            dist,
            Id20::from_hex("0000000000000000000000000000000000000002").unwrap()
        );
    }

    #[test]
    fn id20_xor_self_is_zero() {
        let a = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        assert_eq!(a.xor_distance(&a), Id20::ZERO);
    }

    #[test]
    fn id20_invalid_hex() {
        assert!(Id20::from_hex("not_hex").is_err());
        assert!(Id20::from_hex("aabbcc").is_err()); // too short
    }

    #[test]
    fn id20_display() {
        let id = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        assert_eq!(
            format!("{id}"),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );
    }

    #[test]
    fn id32_hex_round_trip() {
        let hex_str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let id = Id32::from_hex(hex_str).unwrap();
        assert_eq!(id.to_hex(), hex_str);
    }

    #[test]
    fn id32_base32_round_trip() {
        let id =
            Id32::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap();
        let b32 = id.to_base32();
        let id2 = Id32::from_base32(&b32).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn id32_multihash_round_trip() {
        let id =
            Id32::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap();
        let mh = id.to_multihash_hex();
        // Must start with "1220" (SHA-256 function code + 32-byte length)
        assert!(mh.starts_with("1220"));
        assert_eq!(mh.len(), 68); // 2 prefix bytes + 32 hash bytes = 34 bytes = 68 hex chars
        let id2 = Id32::from_multihash_hex(&mh).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn id32_multihash_reject_wrong_function_code() {
        // Use 0x11 instead of 0x12 (wrong function code)
        let bad = "1120e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(Id32::from_multihash_hex(bad).is_err());
    }
}
