//! UPnP IGD / NAT-PMP / PCP port mapping for ferrite.
//!
//! Provides automatic NAT traversal so peers behind routers can receive
//! inbound connections. Tries PCP first (newest), falls back to NAT-PMP,
//! then UPnP IGD as a last resort.

pub mod error;
pub mod gateway;
pub mod natpmp;
pub mod pcp;
pub mod upnp;

pub use error::{Error, Result};
