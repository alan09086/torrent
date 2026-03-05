//! Diffie-Hellman key exchange for MSE/PE (768-bit).

use num_bigint::BigUint;
use num_traits::One;

/// DH prime P (768-bit / 96 bytes), from the MSE specification.
const P_HEX: &str = "\
    FFFFFFFFFFFFFFFFC90FDAA22168C234\
    C4C6628B80DC1CD129024E088A67CC74\
    020BBEA63B139B22514A08798E3404DD\
    EF9519B3CD3A431B302B0A6DF25F1437\
    4FE1356D6D51C245E485B576625E7EC6\
    F44C42E9A63A36210000000000090563";

/// DH generator G = 2.
const G: u64 = 2;

/// Size of public key and shared secret in bytes.
pub(crate) const DH_KEY_SIZE: usize = 96;

/// Diffie-Hellman keypair (768-bit).
pub(crate) struct DhKeypair {
    /// Private key X (random 768-bit integer).
    private: BigUint,
    /// Public key Y = G^X mod P (96 bytes, big-endian).
    pub public: [u8; DH_KEY_SIZE],
}

impl DhKeypair {
    /// Generate a new random DH keypair.
    pub fn generate() -> Self {
        let p = BigUint::parse_bytes(P_HEX.as_bytes(), 16).expect("valid prime");
        let g = BigUint::from(G);

        // Generate 96 random bytes for private key
        let mut private_bytes = [0u8; DH_KEY_SIZE];
        torrent_core::random_bytes(&mut private_bytes);
        // Ensure non-zero
        private_bytes[0] |= 0x01;

        let private = BigUint::from_bytes_be(&private_bytes);
        let y = g.modpow(&private, &p);

        let y_bytes = y.to_bytes_be();
        let mut public = [0u8; DH_KEY_SIZE];
        // Pad with leading zeros if needed
        let offset = DH_KEY_SIZE.saturating_sub(y_bytes.len());
        public[offset..].copy_from_slice(&y_bytes[..y_bytes.len().min(DH_KEY_SIZE)]);

        DhKeypair { private, public }
    }

    /// Compute the shared secret S = Y_peer^X_self mod P.
    /// Returns 96 bytes big-endian.
    pub fn shared_secret(&self, peer_public: &[u8; DH_KEY_SIZE]) -> [u8; DH_KEY_SIZE] {
        let p = BigUint::parse_bytes(P_HEX.as_bytes(), 16).expect("valid prime");
        let y_peer = BigUint::from_bytes_be(peer_public);

        // Validate peer's public key: must be > 1 and < P-1
        if y_peer <= BigUint::one() || y_peer >= (&p - BigUint::one()) {
            // Degenerate key — return zeros (caller should handle)
            return [0u8; DH_KEY_SIZE];
        }

        let s = y_peer.modpow(&self.private, &p);
        let s_bytes = s.to_bytes_be();

        let mut secret = [0u8; DH_KEY_SIZE];
        let offset = DH_KEY_SIZE.saturating_sub(s_bytes.len());
        secret[offset..].copy_from_slice(&s_bytes[..s_bytes.len().min(DH_KEY_SIZE)]);
        secret
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dh_public_key_is_96_bytes() {
        let kp = DhKeypair::generate();
        assert_eq!(kp.public.len(), DH_KEY_SIZE);
        // Should not be all zeros
        assert!(kp.public.iter().any(|&b| b != 0));
    }

    #[test]
    fn dh_shared_secret_agrees() {
        let alice = DhKeypair::generate();
        let bob = DhKeypair::generate();

        let secret_alice = alice.shared_secret(&bob.public);
        let secret_bob = bob.shared_secret(&alice.public);

        assert_eq!(secret_alice, secret_bob);
        // Secret should not be all zeros
        assert!(secret_alice.iter().any(|&b| b != 0));
    }

    #[test]
    fn dh_different_keypairs_different_secrets() {
        let alice = DhKeypair::generate();
        let bob = DhKeypair::generate();
        let carol = DhKeypair::generate();

        let ab = alice.shared_secret(&bob.public);
        let ac = alice.shared_secret(&carol.public);

        assert_ne!(ab, ac);
    }
}
