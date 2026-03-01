//! BitTorrent peer wire protocol (BEP 3, 10, 9, 11).
//!
//! Provides message types, handshake, and a tokio codec for framed I/O.

mod codec;
mod error;
mod extended;
mod handshake;
mod holepunch;
mod message;
pub mod mse;

pub use codec::MessageCodec;
pub use error::{Error, Result};
pub use extended::{ExtHandshake, ExtMessage, MetadataMessage, MetadataMessageType};
pub use handshake::Handshake;
pub use holepunch::{HolepunchError, HolepunchMessage, HolepunchMsgType};
pub use message::{allowed_fast_set, allowed_fast_set_for_ip, Message};
