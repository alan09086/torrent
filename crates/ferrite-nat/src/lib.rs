#![warn(missing_docs)]
//! NAT traversal via PCP (RFC 6887), NAT-PMP (RFC 6886), and UPnP IGD.
//!
//! Provides automatic NAT traversal so peers behind routers can receive
//! inbound connections. Tries PCP first (newest), falls back to NAT-PMP,
//! then UPnP IGD as a last resort.

/// Background port mapping actor and handle.
pub mod actor;
/// Error types for NAT operations.
pub mod error;
/// Default gateway and local IP discovery.
pub mod gateway;
/// NAT-PMP (RFC 6886) protocol implementation.
pub mod natpmp;
/// PCP (RFC 6887) protocol implementation.
pub mod pcp;
/// UPnP IGD port mapping.
pub mod upnp;

pub use actor::{NatConfig, NatEvent, NatHandle};
pub use error::{Error, Result};
