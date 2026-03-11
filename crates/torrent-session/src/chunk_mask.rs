//! 256-bit fixed-size bitfield for tracking available chunks within a piece.
//!
//! A set bit means "this chunk is available for assignment" (missing AND unassigned).
//! Covers up to 256 chunks per piece — handles all standard v1 piece sizes:
//! 16 KiB to 4 MiB with 16 KiB chunks = 1 to 256 chunks.

/// 256-bit bitfield tracking chunk availability within a piece.
///
/// Each bit represents one chunk. A set bit means the chunk is available for
/// assignment (it is missing from local storage and not yet assigned to a peer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkMask {
    bits: [u64; 4],
    num_chunks: u32,
}

impl ChunkMask {
    /// Creates an empty mask with all bits clear and `num_chunks = 0`.
    #[inline]
    pub(crate) fn empty() -> Self {
        Self {
            bits: [0u64; 4],
            num_chunks: 0,
        }
    }

    /// Creates an empty mask with `num_chunks` set (for debug assertions).
    ///
    /// # Panics
    ///
    /// Panics if `num_chunks > 256`.
    #[inline]
    pub(crate) fn empty_with_capacity(num_chunks: u32) -> Self {
        assert!(num_chunks <= 256, "ChunkMask supports at most 256 chunks, got {num_chunks}");
        Self {
            bits: [0u64; 4],
            num_chunks,
        }
    }

    /// Creates a mask with bits 0..`num_chunks` all set.
    ///
    /// # Panics
    ///
    /// Panics if `num_chunks > 256`.
    #[inline]
    pub(crate) fn all(num_chunks: u32) -> Self {
        assert!(num_chunks <= 256, "ChunkMask supports at most 256 chunks, got {num_chunks}");
        if num_chunks == 0 {
            return Self::empty();
        }

        let mut bits = [0u64; 4];
        let full_words = (num_chunks / 64) as usize;
        let remainder = num_chunks % 64;

        for word in bits.iter_mut().take(full_words) {
            *word = u64::MAX;
        }
        if remainder > 0 && full_words < 4 {
            bits[full_words] = (1u64 << remainder) - 1;
        }

        Self { bits, num_chunks }
    }

    /// Sets a bit (marks chunk as available for assignment).
    ///
    /// # Debug Panics
    ///
    /// Debug-asserts that `index < num_chunks`.
    #[inline]
    pub(crate) fn set(&mut self, index: u32) {
        debug_assert!(index < self.num_chunks, "chunk index {index} out of range (num_chunks={})", self.num_chunks);
        self.bits[(index / 64) as usize] |= 1u64 << (index % 64);
    }

    /// Clears a bit (marks chunk as assigned or received).
    ///
    /// # Debug Panics
    ///
    /// Debug-asserts that `index < num_chunks`.
    #[inline]
    pub(crate) fn clear(&mut self, index: u32) {
        debug_assert!(index < self.num_chunks, "chunk index {index} out of range (num_chunks={})", self.num_chunks);
        self.bits[(index / 64) as usize] &= !(1u64 << (index % 64));
    }

    /// Returns `true` if the bit at `index` is set.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn is_set(&self, index: u32) -> bool {
        debug_assert!(index < self.num_chunks);
        (self.bits[(index / 64) as usize] >> (index % 64)) & 1 == 1
    }

    /// Returns the number of set bits (available chunks).
    #[inline]
    pub(crate) fn count_ones(&self) -> u32 {
        self.bits.iter().map(|w| w.count_ones()).sum()
    }

    /// Returns `true` if no bits are set.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.bits == [0u64; 4]
    }

    /// Returns the index of the first set bit, or `None` if empty.
    #[inline]
    #[allow(dead_code)] // Wired in Task 3 (picker rewrite).
    pub(crate) fn first_set(&self) -> Option<u32> {
        for (i, &word) in self.bits.iter().enumerate() {
            if word != 0 {
                return Some(i as u32 * 64 + word.trailing_zeros());
            }
        }
        None
    }

    /// Returns an iterator over indices of all set bits, in ascending order.
    #[inline]
    pub(crate) fn iter_set_bits(&self) -> ChunkMaskIter {
        ChunkMaskIter {
            bits: self.bits,
            word_idx: 0,
        }
    }

    /// Returns the number of chunks this mask was created for.
    #[inline]
    #[allow(dead_code)] // Wired in Task 3 (picker rewrite).
    pub(crate) fn num_chunks(&self) -> u32 {
        self.num_chunks
    }
}

/// Iterator over the indices of set bits in a [`ChunkMask`].
///
/// Uses the `trailing_zeros()` + clear-lowest-bit pattern for O(popcount) iteration.
pub(crate) struct ChunkMaskIter {
    bits: [u64; 4],
    word_idx: usize,
}

impl Iterator for ChunkMaskIter {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        while self.word_idx < 4 {
            let word = self.bits[self.word_idx];
            if word != 0 {
                let bit = word.trailing_zeros();
                // Clear the lowest set bit: word & (word - 1)
                self.bits[self.word_idx] = word & (word - 1);
                return Some(self.word_idx as u32 * 64 + bit);
            }
            self.word_idx += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_mask() {
        let m = ChunkMask::empty();
        assert!(m.is_empty());
        assert_eq!(m.count_ones(), 0);
        assert_eq!(m.first_set(), None);
        assert_eq!(m.iter_set_bits().count(), 0);
    }

    #[test]
    fn all_32_chunks() {
        // Arch ISO: 512 KiB piece / 16 KiB chunk = 32 chunks
        let m = ChunkMask::all(32);
        assert!(!m.is_empty());
        assert_eq!(m.count_ones(), 32);
        let bits: Vec<u32> = m.iter_set_bits().collect();
        assert_eq!(bits, (0u32..32).collect::<Vec<_>>());
    }

    #[test]
    fn all_64_chunks() {
        // Boundary test: 1 MiB piece / 16 KiB = 64 chunks (exactly one word)
        let m = ChunkMask::all(64);
        assert_eq!(m.count_ones(), 64);
        let bits: Vec<u32> = m.iter_set_bits().collect();
        assert_eq!(bits, (0u32..64).collect::<Vec<_>>());
    }

    #[test]
    fn all_256_chunks() {
        // Max capacity: 4 MiB piece / 16 KiB = 256 chunks
        let m = ChunkMask::all(256);
        assert_eq!(m.count_ones(), 256);
        let bits: Vec<u32> = m.iter_set_bits().collect();
        assert_eq!(bits, (0u32..256).collect::<Vec<_>>());
    }

    #[test]
    fn all_1_chunk() {
        let m = ChunkMask::all(1);
        assert_eq!(m.count_ones(), 1);
        assert_eq!(m.first_set(), Some(0));
        let bits: Vec<u32> = m.iter_set_bits().collect();
        assert_eq!(bits, vec![0]);
    }

    #[test]
    fn set_and_clear() {
        let mut m = ChunkMask::empty_with_capacity(64);
        assert_eq!(m.count_ones(), 0);

        m.set(5);
        assert!(m.is_set(5));
        assert_eq!(m.count_ones(), 1);

        m.set(63);
        assert!(m.is_set(63));
        assert_eq!(m.count_ones(), 2);

        m.clear(5);
        assert!(!m.is_set(5));
        assert_eq!(m.count_ones(), 1);

        m.clear(63);
        assert_eq!(m.count_ones(), 0);
        assert!(m.is_empty());
    }

    #[test]
    fn clear_all_one_by_one() {
        let mut m = ChunkMask::all(32);
        for i in 0..32 {
            assert!(!m.is_empty());
            m.clear(i);
        }
        assert!(m.is_empty());
        assert_eq!(m.count_ones(), 0);
    }

    #[test]
    fn iter_after_clears() {
        let mut m = ChunkMask::all(32);
        // Clear even-indexed chunks
        for i in (0..32).step_by(2) {
            m.clear(i);
        }
        let bits: Vec<u32> = m.iter_set_bits().collect();
        let expected: Vec<u32> = (0u32..32).filter(|i| i % 2 != 0).collect();
        assert_eq!(bits, expected);
    }

    #[test]
    fn cross_word_boundary() {
        // 128 chunks spans words 0 and 1
        let mut m = ChunkMask::all(128);
        assert_eq!(m.count_ones(), 128);

        // Clear bits at word 0/1 boundary
        m.clear(63); // last bit of word 0
        m.clear(64); // first bit of word 1
        assert_eq!(m.count_ones(), 126);
        assert!(!m.is_set(63));
        assert!(!m.is_set(64));
        assert!(m.is_set(62));
        assert!(m.is_set(65));
    }

    #[test]
    fn first_set_after_clearing_low() {
        let mut m = ChunkMask::all(32);
        m.clear(0);
        m.clear(1);
        m.clear(2);
        assert_eq!(m.first_set(), Some(3));
    }

    #[test]
    fn first_set_in_second_word() {
        let mut m = ChunkMask::all(128);
        // Clear all of word 0 (bits 0..64)
        for i in 0..64 {
            m.clear(i);
        }
        assert_eq!(m.first_set(), Some(64));
    }

    #[test]
    #[should_panic]
    fn panics_on_257() {
        let _ = ChunkMask::all(257);
    }

    #[test]
    fn non_power_of_two() {
        // 17 chunks — indices 0..=16 (not a round power of two)
        let m = ChunkMask::all(17);
        assert_eq!(m.count_ones(), 17);
        let bits: Vec<u32> = m.iter_set_bits().collect();
        assert_eq!(bits, (0u32..17).collect::<Vec<_>>());
        // All 17 bits (indices 0..=16) must be set
        assert!(m.is_set(0));
        assert!(m.is_set(16));
        assert_eq!(m.num_chunks(), 17);
        // Verify no stray bits beyond index 16 by checking count is exactly 17
        assert_eq!(m.count_ones(), 17);
    }
}
