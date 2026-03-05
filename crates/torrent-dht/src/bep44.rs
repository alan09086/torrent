//! BEP 44: DHT arbitrary data storage types and validation.
//!
//! Defines item types, signature construction, and validation for both
//! immutable (SHA-1-keyed) and mutable (ed25519-signed) DHT items.

use ed25519_dalek::{Signature, VerifyingKey, SigningKey, Signer, Verifier};
use torrent_core::{sha1, Id20};

use crate::error::{Error, Result};

/// Maximum size of a bencoded value in a BEP 44 item (bytes).
pub const MAX_VALUE_SIZE: usize = 1000;

/// Maximum size of the salt field (bytes).
pub const MAX_SALT_SIZE: usize = 64;

/// An immutable DHT item. Key = SHA-1(bencoded value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableItem {
    /// The bencoded value (max 1000 bytes).
    pub value: Vec<u8>,
    /// SHA-1 hash of `value`, used as the lookup key / target.
    pub target: Id20,
}

impl ImmutableItem {
    /// Create an immutable item from raw bencoded bytes.
    ///
    /// Returns error if value exceeds 1000 bytes.
    pub fn new(value: Vec<u8>) -> Result<Self> {
        if value.len() > MAX_VALUE_SIZE {
            return Err(Error::Bep44ValueTooLarge {
                size: value.len(),
                max: MAX_VALUE_SIZE,
            });
        }
        let target = sha1(&value);
        Ok(ImmutableItem { value, target })
    }

    /// Validate that the value matches the expected target hash.
    pub fn verify(&self) -> bool {
        sha1(&self.value) == self.target
    }
}

/// A mutable DHT item. Key = ed25519 public key + optional salt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableItem {
    /// The bencoded value (max 1000 bytes).
    pub value: Vec<u8>,
    /// Ed25519 public key (32 bytes).
    pub public_key: [u8; 32],
    /// Ed25519 signature (64 bytes) over the signing buffer.
    pub signature: [u8; 64],
    /// Monotonically increasing sequence number.
    pub seq: i64,
    /// Optional salt (max 64 bytes). Empty vec = no salt.
    pub salt: Vec<u8>,
    /// Derived target: SHA-1(public_key + salt).
    pub target: Id20,
}

impl MutableItem {
    /// Create a mutable item and sign it with the given keypair.
    ///
    /// - `keypair`: ed25519 signing key (contains both secret and public key)
    /// - `value`: raw bencoded value (max 1000 bytes)
    /// - `seq`: sequence number (must increase with each update)
    /// - `salt`: optional salt (max 64 bytes, empty vec for none)
    pub fn create(
        keypair: &SigningKey,
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
    ) -> Result<Self> {
        if value.len() > MAX_VALUE_SIZE {
            return Err(Error::Bep44ValueTooLarge {
                size: value.len(),
                max: MAX_VALUE_SIZE,
            });
        }
        if salt.len() > MAX_SALT_SIZE {
            return Err(Error::Bep44SaltTooLarge {
                size: salt.len(),
                max: MAX_SALT_SIZE,
            });
        }

        let public_key = keypair.verifying_key().to_bytes();
        let target = compute_mutable_target(&public_key, &salt);
        let sign_buf = build_signing_buffer(&salt, seq, &value);
        let signature = keypair.sign(&sign_buf);

        Ok(MutableItem {
            value,
            public_key,
            signature: signature.to_bytes(),
            seq,
            salt,
            target,
        })
    }

    /// Verify the ed25519 signature against the public key and signing buffer.
    pub fn verify(&self) -> bool {
        let Ok(verifying_key) = VerifyingKey::from_bytes(&self.public_key) else {
            return false;
        };
        // Gap fix: Signature::from_bytes is infallible in ed25519-dalek v2
        let signature = Signature::from_bytes(&self.signature);
        let sign_buf = build_signing_buffer(&self.salt, self.seq, &self.value);
        verifying_key.verify(&sign_buf, &signature).is_ok()
    }

    /// Check that the target matches SHA-1(public_key + salt).
    pub fn verify_target(&self) -> bool {
        compute_mutable_target(&self.public_key, &self.salt) == self.target
    }
}

/// Compute the DHT target for a mutable item: `SHA-1(public_key + salt)`.
pub fn compute_mutable_target(public_key: &[u8; 32], salt: &[u8]) -> Id20 {
    let mut buf = Vec::with_capacity(32 + salt.len());
    buf.extend_from_slice(public_key);
    buf.extend_from_slice(salt);
    sha1(&buf)
}

/// Build the byte buffer that gets signed/verified for mutable items.
///
/// Without salt: `3:seqi{seq}e1:v{len}:{value}`
/// With salt: `4:salt{salt_len}:{salt}3:seqi{seq}e1:v{value_len}:{value}`
pub fn build_signing_buffer(salt: &[u8], seq: i64, value: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + value.len() + salt.len());

    if !salt.is_empty() {
        // "4:salt{len}:{salt}"
        buf.extend_from_slice(b"4:salt");
        buf.extend_from_slice(salt.len().to_string().as_bytes());
        buf.push(b':');
        buf.extend_from_slice(salt);
    }

    // "3:seqi{seq}e"
    buf.extend_from_slice(b"3:seqi");
    buf.extend_from_slice(seq.to_string().as_bytes());
    buf.push(b'e');

    // "1:v{len}:{value}"
    buf.extend_from_slice(b"1:v");
    buf.extend_from_slice(value.len().to_string().as_bytes());
    buf.push(b':');
    buf.extend_from_slice(value);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn immutable_item_hash_is_sha1_of_value() {
        let value = b"12:Hello World!".to_vec(); // bencoded string
        let item = ImmutableItem::new(value.clone()).unwrap();
        assert_eq!(item.target, sha1(&value));
        assert!(item.verify());
    }

    #[test]
    fn immutable_item_rejects_oversized_value() {
        let value = vec![0u8; MAX_VALUE_SIZE + 1];
        let err = ImmutableItem::new(value).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn immutable_item_verify_detects_corruption() {
        let mut item = ImmutableItem::new(b"5:hello".to_vec()).unwrap();
        item.value[0] = 0xFF; // corrupt
        assert!(!item.verify());
    }

    #[test]
    fn mutable_item_sign_and_verify() {
        let keypair = SigningKey::from_bytes(&[1u8; 32]);
        let value = b"12:Hello World!".to_vec();
        let item = MutableItem::create(&keypair, value, 1, Vec::new()).unwrap();
        assert!(item.verify());
        assert!(item.verify_target());
    }

    #[test]
    fn mutable_item_with_salt() {
        let keypair = SigningKey::from_bytes(&[2u8; 32]);
        let value = b"4:test".to_vec();
        let salt = b"my-salt".to_vec();
        let item = MutableItem::create(&keypair, value, 42, salt.clone()).unwrap();
        assert!(item.verify());
        assert!(item.verify_target());
        assert_eq!(item.salt, salt);
    }

    #[test]
    fn mutable_item_rejects_oversized_value() {
        let keypair = SigningKey::from_bytes(&[3u8; 32]);
        let value = vec![0u8; MAX_VALUE_SIZE + 1];
        let err = MutableItem::create(&keypair, value, 1, Vec::new()).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn mutable_item_rejects_oversized_salt() {
        let keypair = SigningKey::from_bytes(&[4u8; 32]);
        let value = b"4:test".to_vec();
        let salt = vec![0u8; MAX_SALT_SIZE + 1];
        let err = MutableItem::create(&keypair, value, 1, salt).unwrap_err();
        assert!(err.to_string().contains("salt"));
    }

    #[test]
    fn mutable_item_verify_fails_on_wrong_key() {
        let keypair = SigningKey::from_bytes(&[5u8; 32]);
        let value = b"4:test".to_vec();
        let mut item = MutableItem::create(&keypair, value, 1, Vec::new()).unwrap();
        // Replace public key with a different one
        item.public_key = SigningKey::from_bytes(&[6u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(!item.verify());
    }

    #[test]
    fn mutable_item_verify_fails_on_tampered_value() {
        let keypair = SigningKey::from_bytes(&[7u8; 32]);
        let value = b"4:test".to_vec();
        let mut item = MutableItem::create(&keypair, value, 1, Vec::new()).unwrap();
        item.value = b"5:tampr".to_vec();
        assert!(!item.verify());
    }

    #[test]
    fn mutable_target_is_sha1_of_pubkey_plus_salt() {
        let pubkey = [0xABu8; 32];
        let salt = b"hello";
        let target = compute_mutable_target(&pubkey, salt);

        let mut expected_buf = Vec::new();
        expected_buf.extend_from_slice(&pubkey);
        expected_buf.extend_from_slice(salt);
        assert_eq!(target, sha1(&expected_buf));
    }

    #[test]
    fn mutable_target_no_salt() {
        let pubkey = [0xCDu8; 32];
        let target = compute_mutable_target(&pubkey, &[]);
        // With no salt, target = SHA-1(pubkey)
        assert_eq!(target, sha1(&pubkey));
    }

    #[test]
    fn signing_buffer_no_salt() {
        let buf = build_signing_buffer(&[], 1, b"12:Hello World!");
        assert_eq!(buf, b"3:seqi1e1:v15:12:Hello World!");
    }

    #[test]
    fn signing_buffer_with_salt() {
        let buf = build_signing_buffer(b"foobar", 4, b"12:Hello World!");
        assert_eq!(buf, b"4:salt6:foobar3:seqi4e1:v15:12:Hello World!");
    }

    #[test]
    fn signing_buffer_negative_seq() {
        let buf = build_signing_buffer(&[], -1, b"1:x");
        assert_eq!(buf, b"3:seqi-1e1:v3:1:x");
    }

    #[test]
    fn sequence_number_monotonicity() {
        let keypair = SigningKey::from_bytes(&[8u8; 32]);
        let item1 = MutableItem::create(&keypair, b"4:aaaa".to_vec(), 1, Vec::new()).unwrap();
        let item2 = MutableItem::create(&keypair, b"4:bbbb".to_vec(), 2, Vec::new()).unwrap();
        assert!(item2.seq > item1.seq);
        assert!(item1.verify());
        assert!(item2.verify());
    }
}
