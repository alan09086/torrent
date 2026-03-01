//! I2P support via the SAM v3.1 bridge protocol.
//!
//! Provides anonymous peer connections through the I2P network. Requires
//! a local I2P router with SAM enabled (default: 127.0.0.1:7656).

pub mod destination;
pub mod sam;

pub use destination::{I2pDestination, I2pDestinationError};
pub use sam::{SamSession, SamStream};
#[allow(unused_imports)]
pub(crate) use destination::{i2p_base64_decode, i2p_base64_encode};
