/// Compact bit-vector with MSB-first ordering (matches BEP 3 wire format).
///
/// Bit `i` is stored in byte `i / 8`, at bit position `7 - (i % 8)` (MSB-first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitfield {
    data: Vec<u8>,
    len: u32,
}

impl Bitfield {
    /// Create a new bitfield with all bits cleared.
    pub fn new(len: u32) -> Self {
        let byte_len = len.div_ceil(8) as usize;
        Bitfield {
            data: vec![0; byte_len],
            len,
        }
    }

    /// Create from raw bytes (wire format). Rejects if trailing bits are set.
    pub fn from_bytes(data: Vec<u8>, len: u32) -> crate::Result<Self> {
        let expected_bytes = len.div_ceil(8) as usize;
        if data.len() != expected_bytes {
            return Err(crate::Error::InvalidBitfieldLength {
                expected: expected_bytes,
                got: data.len(),
            });
        }

        // Check that trailing bits (beyond `len`) are clear.
        let spare = (expected_bytes * 8) as u32 - len;
        if spare > 0 {
            let mask = (1u8 << spare) - 1;
            if data[expected_bytes - 1] & mask != 0 {
                return Err(crate::Error::TrailingBitsSet);
            }
        }

        Ok(Bitfield { data, len })
    }

    /// Number of bits in this bitfield.
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Whether this bitfield has zero bits.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get the value of bit `index`.
    ///
    /// Returns `false` for out-of-range indices.
    pub fn get(&self, index: u32) -> bool {
        if index >= self.len {
            return false;
        }
        let byte = self.data[(index / 8) as usize];
        let bit = 7 - (index % 8);
        byte & (1 << bit) != 0
    }

    /// Set bit `index` to 1.
    ///
    /// No-op if out of range.
    pub fn set(&mut self, index: u32) {
        if index >= self.len {
            return;
        }
        let bit = 7 - (index % 8);
        self.data[(index / 8) as usize] |= 1 << bit;
    }

    /// Clear bit `index` to 0.
    ///
    /// No-op if out of range.
    pub fn clear(&mut self, index: u32) {
        if index >= self.len {
            return;
        }
        let bit = 7 - (index % 8);
        self.data[(index / 8) as usize] &= !(1 << bit);
    }

    /// Count of set bits.
    pub fn count_ones(&self) -> u32 {
        self.data.iter().map(|b| b.count_ones()).sum()
    }

    /// True if every bit is set.
    pub fn all_set(&self) -> bool {
        self.count_ones() == self.len
    }

    /// Raw byte slice (wire format).
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Iterator over indices of set bits.
    pub fn ones(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len).filter(|&i| self.get(i))
    }

    /// Iterator over indices of cleared bits.
    pub fn zeros(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len).filter(|&i| !self.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_all_clear() {
        let bf = Bitfield::new(16);
        assert_eq!(bf.len(), 16);
        assert_eq!(bf.count_ones(), 0);
        assert!(!bf.get(0));
    }

    #[test]
    fn set_get() {
        let mut bf = Bitfield::new(32);
        bf.set(0);
        bf.set(7);
        bf.set(8);
        bf.set(31);
        assert!(bf.get(0));
        assert!(bf.get(7));
        assert!(bf.get(8));
        assert!(bf.get(31));
        assert!(!bf.get(1));
        assert!(!bf.get(30));
    }

    #[test]
    fn clear() {
        let mut bf = Bitfield::new(8);
        bf.set(3);
        assert!(bf.get(3));
        bf.clear(3);
        assert!(!bf.get(3));
    }

    #[test]
    fn count_ones() {
        let mut bf = Bitfield::new(16);
        bf.set(0);
        bf.set(5);
        bf.set(15);
        assert_eq!(bf.count_ones(), 3);
    }

    #[test]
    fn all_set() {
        let mut bf = Bitfield::new(8);
        for i in 0..8 {
            bf.set(i);
        }
        assert!(bf.all_set());
    }

    #[test]
    fn from_bytes_valid() {
        // 10 bits: 2 bytes, bits 0 and 9 set
        // byte 0: bit0 = MSB = 0x80, byte 1: bit9 = bit1-of-byte1 = 0x40
        let data = vec![0x80, 0x40];
        let bf = Bitfield::from_bytes(data, 10).unwrap();
        assert!(bf.get(0));
        assert!(!bf.get(1));
        assert!(bf.get(9));
    }

    #[test]
    fn from_bytes_trailing_rejected() {
        // 10 bits in 2 bytes: spare bits are bits 10-15 (low 6 bits of byte 1)
        // Set a trailing bit → should fail
        let data = vec![0x00, 0x01]; // bit 15 set, which is spare
        let result = Bitfield::from_bytes(data, 10);
        assert!(result.is_err());
    }

    #[test]
    fn round_trip() {
        let mut bf = Bitfield::new(20);
        bf.set(0);
        bf.set(5);
        bf.set(19);
        let bytes = bf.as_bytes().to_vec();
        let bf2 = Bitfield::from_bytes(bytes, 20).unwrap();
        assert_eq!(bf, bf2);
    }

    #[test]
    fn ones_iter() {
        let mut bf = Bitfield::new(8);
        bf.set(1);
        bf.set(3);
        bf.set(7);
        let ones: Vec<u32> = bf.ones().collect();
        assert_eq!(ones, vec![1, 3, 7]);
    }

    #[test]
    fn zeros_iter() {
        let mut bf = Bitfield::new(4);
        bf.set(0);
        bf.set(2);
        let zeros: Vec<u32> = bf.zeros().collect();
        assert_eq!(zeros, vec![1, 3]);
    }

    #[test]
    fn empty() {
        let bf = Bitfield::new(0);
        assert!(bf.is_empty());
        assert_eq!(bf.count_ones(), 0);
        assert!(bf.all_set());
        assert!(!bf.get(0));
    }

    #[test]
    fn single_bit() {
        let mut bf = Bitfield::new(1);
        assert!(!bf.get(0));
        bf.set(0);
        assert!(bf.get(0));
        assert!(bf.all_set());
        assert_eq!(bf.as_bytes(), &[0x80]);
    }
}
