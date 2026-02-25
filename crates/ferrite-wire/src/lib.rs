//! BitTorrent peer wire protocol (BEP 3, 10, 9, 11).
//!
//! Provides message types, handshake, and a tokio codec for framed I/O.

mod codec;
mod error;
mod extended;
mod handshake;
mod message;

pub use codec::MessageCodec;
pub use error::{Error, Result};
pub use extended::{ExtHandshake, ExtMessage, MetadataMessage, MetadataMessageType};
pub use handshake::Handshake;
pub use message::Message;
