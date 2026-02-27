//! Message Stream Encryption / Protocol Encryption (MSE/PE).
//!
//! Obfuscates BitTorrent traffic using Diffie-Hellman key exchange and RC4 stream cipher.

pub(crate) mod cipher;
pub(crate) mod crypto;
pub(crate) mod dh;
pub mod handshake;
pub mod stream;

pub use handshake::NegotiationResult;
pub use stream::MseStream;

/// Encryption mode for peer connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncryptionMode {
    /// No encryption. Plain BitTorrent handshake.
    Disabled,
    /// Prefer encrypted, fallback to plain on failure (default).
    #[default]
    Enabled,
    /// Encrypted only. Reject unencrypted peers.
    Forced,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encryption_mode_default_is_enabled() {
        assert_eq!(EncryptionMode::default(), EncryptionMode::Enabled);
    }
}
