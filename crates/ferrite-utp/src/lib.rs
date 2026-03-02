#![warn(missing_docs)]
//! Micro Transport Protocol (uTP/LEDBAT) implementation (BEP 29).

/// Error types for uTP operations.
pub mod error;
/// Wrapping 16-bit sequence numbers.
pub mod seq;

/// uTP packet encoding and decoding.
pub mod packet;

/// LEDBAT congestion control.
pub mod congestion;
/// uTP connection state machine.
pub mod conn;
mod listener;
mod socket;
mod stream;

pub use error::{Error, Result};
pub use listener::UtpListener;
pub use seq::SeqNr;
pub use socket::{UtpConfig, UtpSocket};
pub use stream::UtpStream;
