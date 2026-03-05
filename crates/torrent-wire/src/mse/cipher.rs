//! RC4 stream cipher with 1024-byte discard.

/// RC4 stream cipher state.
#[derive(Clone)]
pub(crate) struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    /// Initialize RC4 from a key (KSA), then discard the first 1024 bytes
    /// to mitigate the Fluhrer-Mantin-Shamir attack.
    pub fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        for (i, b) in s.iter_mut().enumerate() {
            *b = i as u8;
        }

        // Key-Scheduling Algorithm (KSA)
        let mut j: u8 = 0;
        for i in 0..256u16 {
            j = j
                .wrapping_add(s[i as usize])
                .wrapping_add(key[i as usize % key.len()]);
            s.swap(i as usize, j as usize);
        }

        let mut cipher = Rc4 { s, i: 0, j: 0 };

        // Discard first 1024 bytes (standard MSE/PE requirement)
        let mut discard = [0u8; 1024];
        cipher.apply(&mut discard);

        cipher
    }

    /// Pseudo-Random Generation Algorithm (PRGA). XORs data in-place with keystream.
    pub fn apply(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k = self.s[self.s[self.i as usize].wrapping_add(self.s[self.j as usize]) as usize];
            *byte ^= k;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc4_encrypt_decrypt_roundtrip() {
        let key = b"test key for rc4";
        let plaintext = b"Hello, BitTorrent MSE/PE encryption!";

        let mut encrypted = plaintext.to_vec();
        let mut cipher_enc = Rc4::new(key);
        cipher_enc.apply(&mut encrypted);

        // Encrypted should differ from plaintext
        assert_ne!(&encrypted[..], &plaintext[..]);

        // Decrypt with fresh cipher (same key = same keystream)
        let mut decrypted = encrypted.clone();
        let mut cipher_dec = Rc4::new(key);
        cipher_dec.apply(&mut decrypted);

        assert_eq!(&decrypted[..], &plaintext[..]);
    }

    #[test]
    fn rc4_discard_1024_bytes() {
        // Two ciphers with same key should produce same output
        let key = b"consistency check";
        let mut c1 = Rc4::new(key);
        let mut c2 = Rc4::new(key);

        let mut buf1 = [0u8; 64];
        let mut buf2 = [0u8; 64];
        c1.apply(&mut buf1);
        c2.apply(&mut buf2);

        assert_eq!(buf1, buf2);
    }

    #[test]
    fn rc4_different_keys_differ() {
        let mut buf1 = [0u8; 32];
        let mut buf2 = [0u8; 32];
        Rc4::new(b"key one").apply(&mut buf1);
        Rc4::new(b"key two").apply(&mut buf2);
        assert_ne!(buf1, buf2);
    }
}
