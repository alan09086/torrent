//! BitTorrent peer wire protocol (BEP 3, 10, 9, 11).
//!
//! Re-exports from [`torrent_wire`].

pub use torrent_wire::{
    // Error types
    Error,
    // Extension protocol (BEP 10)
    ExtHandshake,
    ExtMessage,
    // Handshake
    Handshake,
    HolepunchError,
    // BEP 55 Holepunch Extension
    HolepunchMessage,
    HolepunchMsgType,
    // Wire messages (BEP 3 + BEP 6)
    Message,
    // Tokio codec for framed I/O
    MessageCodec,
    // Metadata exchange (BEP 9)
    MetadataMessage,
    MetadataMessageType,
    Result,
    // BEP 6 Fast Extension
    allowed_fast_set,
};

// Message Stream Encryption / Protocol Encryption (MSE/PE)
pub use torrent_wire::mse;

// SSL torrent TLS transport (M42)
pub use torrent_wire::ssl;
