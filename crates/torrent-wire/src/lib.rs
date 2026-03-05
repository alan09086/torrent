#![warn(missing_docs)]
//! BitTorrent peer wire protocol: handshake, messages, BEP 6/9/10/21/52 extensions, MSE/PE encryption.
//!
//! Provides message types, handshake, and a tokio codec for framed I/O.

mod codec;
mod error;
mod extended;
mod handshake;
mod holepunch;
mod message;
pub mod mse;
pub mod ssl;

pub use codec::MessageCodec;
pub use error::{Error, Result};
pub use extended::{ExtHandshake, ExtMessage, MetadataMessage, MetadataMessageType};
pub use handshake::Handshake;
pub use holepunch::{HolepunchError, HolepunchMessage, HolepunchMsgType};
pub use message::{Message, allowed_fast_set, allowed_fast_set_for_ip};
pub use ssl::{
    SslConfig, accept_tls, build_client_config, build_server_config, connect_tls,
    generate_self_signed_cert,
};
