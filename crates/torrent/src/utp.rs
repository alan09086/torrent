//! uTP (BEP 29) Micro Transport Protocol.
//!
//! Re-exports from [`torrent_utp`] — a UDP-based reliable transport with
//! LEDBAT congestion control that yields to competing flows.

pub use torrent_utp::{Error, UtpConfig, UtpListener, UtpSocket, UtpStream};
