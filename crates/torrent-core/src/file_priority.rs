use serde::{Deserialize, Serialize};

/// Per-file download priority.
///
/// Matches libtorrent's priority scale. `Skip` means "do not download".
/// `PartialOrd`/`Ord` ordering: Skip < Low < Normal < High.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[repr(u8)]
pub enum FilePriority {
    /// Do not download this file.
    Skip = 0,
    /// Low download priority.
    Low = 1,
    /// Default download priority.
    #[default]
    Normal = 4,
    /// High download priority.
    High = 7,
}

impl From<u8> for FilePriority {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Skip,
            1 => Self::Low,
            7 => Self::High,
            _ => Self::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_normal() {
        assert_eq!(FilePriority::default(), FilePriority::Normal);
    }

    #[test]
    fn ordering_skip_less_than_high() {
        assert!(FilePriority::Skip < FilePriority::Low);
        assert!(FilePriority::Low < FilePriority::Normal);
        assert!(FilePriority::Normal < FilePriority::High);
    }

    #[test]
    fn from_u8_known_values() {
        assert_eq!(FilePriority::from(0), FilePriority::Skip);
        assert_eq!(FilePriority::from(1), FilePriority::Low);
        assert_eq!(FilePriority::from(4), FilePriority::Normal);
        assert_eq!(FilePriority::from(7), FilePriority::High);
    }

    #[test]
    fn from_u8_unknown_defaults_to_normal() {
        assert_eq!(FilePriority::from(2), FilePriority::Normal);
        assert_eq!(FilePriority::from(255), FilePriority::Normal);
    }

    #[test]
    fn repr_u8_round_trip() {
        let p = FilePriority::High;
        let v = p as u8;
        assert_eq!(v, 7);
        assert_eq!(FilePriority::from(v), p);
    }

    #[test]
    fn serde_round_trip() {
        let p = FilePriority::Low;
        let encoded = torrent_bencode::to_bytes(&p).unwrap();
        let decoded: FilePriority = torrent_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(p, decoded);
    }
}
