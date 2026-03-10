//! Message Stream Encryption / Protocol Encryption (MSE/PE).
//!
//! Obfuscates BitTorrent traffic using Diffie-Hellman key exchange and RC4 stream cipher.

pub(crate) mod cipher;
pub(crate) mod crypto;
pub(crate) mod dh;
pub mod handshake;
pub mod stream;

use serde::{Deserialize, Serialize};

pub use crypto::{CRYPTO_PLAINTEXT, CRYPTO_RC4};
pub use handshake::NegotiationResult;
pub use stream::MseStream;

/// Encryption mode for peer connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EncryptionMode {
    /// No encryption. Plain BitTorrent handshake.
    Disabled,
    /// Offer both plaintext and RC4, let peer choose. Fallback to plain on MSE failure.
    Enabled,
    /// Offer plaintext-only first; if the peer rejects, retry offering both
    /// plaintext and RC4. This avoids unnecessary RC4 overhead when the peer
    /// accepts plaintext, while still connecting to RC4-only peers via retry.
    #[default]
    PreferPlaintext,
    /// Encrypted only. Reject unencrypted peers.
    Forced,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encryption_mode_default_is_prefer_plaintext() {
        assert_eq!(EncryptionMode::default(), EncryptionMode::PreferPlaintext);
    }
}
