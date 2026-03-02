//! In-process network simulation for ferrite swarm integration testing.
//!
//! This crate provides a simulated network layer that replaces real TCP/UDP
//! sockets with in-memory channels, enabling deterministic, fast integration
//! tests of multi-peer BitTorrent swarms without real network traffic.

pub mod clock;
pub mod network;
pub mod transport;
pub mod swarm;
