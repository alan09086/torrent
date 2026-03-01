//! BitTorrent peer wire protocol (BEP 3, 10, 9, 11).
//!
//! Re-exports from [`ferrite_wire`].

pub use ferrite_wire::{
    // Handshake
    Handshake,
    // Wire messages (BEP 3 + BEP 6)
    Message,
    // Tokio codec for framed I/O
    MessageCodec,
    // Extension protocol (BEP 10)
    ExtHandshake,
    ExtMessage,
    // Metadata exchange (BEP 9)
    MetadataMessage,
    MetadataMessageType,
    // BEP 55 Holepunch Extension
    HolepunchMessage,
    HolepunchMsgType,
    HolepunchError,
    // BEP 6 Fast Extension
    allowed_fast_set,
    // Error types
    Error,
    Result,
};

// Message Stream Encryption / Protocol Encryption (MSE/PE)
pub use ferrite_wire::mse;

// SSL torrent TLS transport (M42)
pub use ferrite_wire::ssl;
