use std::fmt;

/// Wrapping 16-bit sequence number for uTP.
///
/// Handles wraparound comparison: sequence space is a circular ring
/// where 65535 + 1 = 0 and 0 - 1 = 65535.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SeqNr(pub u16);

impl SeqNr {
    /// Add with wrapping.
    pub fn wrapping_add(self, n: u16) -> Self {
        Self(self.0.wrapping_add(n))
    }

    /// Subtract with wrapping.
    pub fn wrapping_sub(self, n: u16) -> Self {
        Self(self.0.wrapping_sub(n))
    }

    /// Returns true if `self` precedes `other` in the circular sequence space.
    ///
    /// Uses the convention that `self` precedes `other` when the shortest
    /// distance forward from `self` to `other` is less than 2^15.
    pub fn precedes(self, other: Self) -> bool {
        let diff = other.0.wrapping_sub(self.0);
        diff > 0 && diff < 32768
    }

    /// Forward distance from `self` to `other` (wrapping).
    pub fn distance_to(self, other: Self) -> u16 {
        other.0.wrapping_sub(self.0)
    }
}

impl Ord for SeqNr {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.0 == other.0 {
            std::cmp::Ordering::Equal
        } else if self.precedes(*other) {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    }
}

impl PartialOrd for SeqNr {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for SeqNr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SeqNr({})", self.0)
    }
}

impl fmt::Display for SeqNr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u16> for SeqNr {
    fn from(v: u16) -> Self {
        Self(v)
    }
}

impl From<SeqNr> for u16 {
    fn from(s: SeqNr) -> Self {
        s.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn seq_nr_wrapping_add() {
        assert_eq!(SeqNr(65535).wrapping_add(1), SeqNr(0));
        assert_eq!(SeqNr(0).wrapping_add(100), SeqNr(100));
        assert_eq!(SeqNr(65530).wrapping_add(10), SeqNr(4));
    }

    #[test]
    fn seq_nr_precedes_normal() {
        assert!(SeqNr(5).precedes(SeqNr(10)));
        assert!(!SeqNr(10).precedes(SeqNr(5)));
        assert!(!SeqNr(5).precedes(SeqNr(5)));
    }

    #[test]
    fn seq_nr_precedes_wraparound() {
        assert!(SeqNr(65530).precedes(SeqNr(5)));
        assert!(!SeqNr(5).precedes(SeqNr(65530)));
    }

    #[test]
    fn seq_nr_ordering() {
        let mut map = BTreeMap::new();
        map.insert(SeqNr(65534), "a");
        map.insert(SeqNr(65535), "b");
        map.insert(SeqNr(0), "c");
        map.insert(SeqNr(1), "d");

        let keys: Vec<u16> = map.keys().map(|s| s.0).collect();
        assert_eq!(keys, vec![65534, 65535, 0, 1]);
    }
}
