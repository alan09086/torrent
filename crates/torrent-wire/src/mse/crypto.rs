//! MSE/PE hash functions and crypto method negotiation.

/// Verification Constant: 8 zero bytes.
pub(crate) const VC: [u8; 8] = [0u8; 8];

/// Maximum random padding length.
#[allow(dead_code)]
pub(crate) const MAX_PADDING: usize = 512;

/// Crypto method bitmask: plaintext (no encryption).
pub const CRYPTO_PLAINTEXT: u32 = 0x01;
/// Crypto method bitmask: RC4 stream cipher.
pub const CRYPTO_RC4: u32 = 0x02;

/// Compute HASH(data) = SHA1(data). Returns 20 bytes.
#[allow(dead_code)]
fn mse_hash(data: &[u8]) -> [u8; 20] {
    let hash = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data);
    let mut out = [0u8; 20];
    out.copy_from_slice(hash.as_ref());
    out
}

/// HASH(prefix + suffix) — concatenate then SHA1.
fn mse_hash2(a: &[u8], b: &[u8]) -> [u8; 20] {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(a);
    ctx.update(b);
    let hash = ctx.finish();
    let mut out = [0u8; 20];
    out.copy_from_slice(hash.as_ref());
    out
}

/// HASH(a + b + c) — concatenate three parts then SHA1.
fn mse_hash3(a: &[u8], b: &[u8], c: &[u8]) -> [u8; 20] {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(a);
    ctx.update(b);
    ctx.update(c);
    let hash = ctx.finish();
    let mut out = [0u8; 20];
    out.copy_from_slice(hash.as_ref());
    out
}

/// Compute the synchronization marker: HASH("req1" + S)
pub(crate) fn sync_marker(shared_secret: &[u8]) -> [u8; 20] {
    mse_hash2(b"req1", shared_secret)
}

/// Compute the SKEY hash: HASH("req2" + skey)
pub(crate) fn skey_hash(skey: &[u8]) -> [u8; 20] {
    mse_hash2(b"req2", skey)
}

/// Compute HASH("req3" + S)
pub(crate) fn req3_hash(shared_secret: &[u8]) -> [u8; 20] {
    mse_hash2(b"req3", shared_secret)
}

/// Compute the SKEY proof: HASH("req2" + skey) XOR HASH("req3" + S)
pub(crate) fn skey_proof(skey: &[u8], shared_secret: &[u8]) -> [u8; 20] {
    let h2 = skey_hash(skey);
    let h3 = req3_hash(shared_secret);
    let mut result = [0u8; 20];
    for i in 0..20 {
        result[i] = h2[i] ^ h3[i];
    }
    result
}

/// Compute RC4 key for the "A" direction (initiator encrypt, responder decrypt).
pub(crate) fn key_a(shared_secret: &[u8], skey: &[u8]) -> [u8; 20] {
    mse_hash3(b"keyA", shared_secret, skey)
}

/// Compute RC4 key for the "B" direction (initiator decrypt, responder encrypt).
pub(crate) fn key_b(shared_secret: &[u8], skey: &[u8]) -> [u8; 20] {
    mse_hash3(b"keyB", shared_secret, skey)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vc_is_eight_zeros() {
        assert_eq!(VC, [0u8; 8]);
    }

    #[test]
    fn hash_deterministic() {
        let h1 = mse_hash2(b"req1", b"shared_secret");
        let h2 = mse_hash2(b"req1", b"shared_secret");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_differs_for_different_input() {
        let h1 = mse_hash2(b"req1", b"secret_a");
        let h2 = mse_hash2(b"req1", b"secret_b");
        assert_ne!(h1, h2);
    }

    #[test]
    fn skey_proof_xor_correct() {
        let skey = [1u8; 20];
        let secret = [2u8; 96];
        let proof = skey_proof(&skey, &secret);

        // Verify: proof XOR req3_hash should equal skey_hash
        let h3 = req3_hash(&secret);
        let mut recovered = [0u8; 20];
        for i in 0..20 {
            recovered[i] = proof[i] ^ h3[i];
        }
        assert_eq!(recovered, skey_hash(&skey));
    }

    #[test]
    fn key_a_and_key_b_differ() {
        let secret = [0xABu8; 96];
        let skey = [0xCDu8; 20];
        assert_ne!(key_a(&secret, &skey), key_b(&secret, &skey));
    }
}
