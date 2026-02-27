use crate::hash::Id20;

/// A BitTorrent peer ID (20 bytes).
///
/// Uses Azureus-style encoding: `-FE0100-` followed by 12 random bytes.
/// FE = Ferrite, 0100 = version 0.1.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub Id20);

impl PeerId {
    /// Client identifier prefix.
    const PREFIX: &'static [u8] = b"-FE0100-";

    /// Generate a new random peer ID.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 20];
        bytes[..8].copy_from_slice(Self::PREFIX);
        // Fill remaining 12 bytes with random data
        for byte in &mut bytes[8..] {
            // Simple LCG random — good enough for peer IDs.
            // In production this would use rand, but we avoid the dep for now.
            *byte = random_byte();
        }
        PeerId(Id20(bytes))
    }

    /// Return the raw 20 bytes.
    pub fn as_bytes(&self) -> &[u8; 20] {
        self.0.as_bytes()
    }

    /// Return the client prefix (e.g., "-FE0100-").
    pub fn prefix(&self) -> &[u8] {
        &self.0 .0[..8]
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show prefix as ASCII, rest as hex
        let prefix = std::str::from_utf8(&self.0 .0[..8]).unwrap_or("????????");
        let suffix = hex::encode(&self.0 .0[8..]);
        write!(f, "{prefix}{suffix}")
    }
}

/// Simple random byte using thread-local state seeded from system time.
pub(crate) fn random_byte() -> u8 {
    use std::cell::Cell;
    use std::time::SystemTime;

    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        );
    }

    STATE.with(|s| {
        // xorshift64
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_id_has_prefix() {
        let id = PeerId::generate();
        assert_eq!(id.prefix(), b"-FE0100-");
    }

    #[test]
    fn peer_ids_are_unique() {
        let a = PeerId::generate();
        let b = PeerId::generate();
        assert_ne!(a, b);
    }

    #[test]
    fn peer_id_display() {
        let id = PeerId::generate();
        let s = format!("{id}");
        assert!(s.starts_with("-FE0100-"));
        assert_eq!(s.len(), 8 + 24); // 8 ASCII prefix + 12 bytes as hex
    }
}
