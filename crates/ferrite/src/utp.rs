//! uTP (BEP 29) Micro Transport Protocol.
//!
//! Re-exports from [`ferrite_utp`] — a UDP-based reliable transport with
//! LEDBAT congestion control that yields to competing flows.

pub use ferrite_utp::{Error, UtpConfig, UtpListener, UtpSocket, UtpStream};
