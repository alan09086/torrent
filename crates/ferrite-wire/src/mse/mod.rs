//! Message Stream Encryption / Protocol Encryption (MSE/PE).
//!
//! Obfuscates BitTorrent traffic using Diffie-Hellman key exchange and RC4 stream cipher.

pub(crate) mod cipher;
pub(crate) mod dh;
