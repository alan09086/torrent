#![warn(missing_docs)]
//! Kademlia DHT implementation: BEP 5 routing, BEP 42 security, BEP 44 storage, BEP 51 indexing.
//!
//! Implements the Mainline DHT protocol: KRPC message encoding, Kademlia
//! routing table, peer discovery, and announce operations.
//!
//! # Architecture
//!
//! The DHT runs as an actor: `DhtHandle::start()` spawns a background task
//! that owns all state (routing table, UDP socket, pending queries). The
//! returned `DhtHandle` is a cheap, cloneable sender for submitting commands.

/// DHT error types.
pub mod error;
/// Compact node encoding (26-byte IPv4, 38-byte IPv6).
pub mod compact;
/// KRPC message encoding and decoding (BEP 5).
pub mod krpc;
/// Kademlia routing table with k-buckets.
pub mod routing_table;
/// Per-info_hash peer storage and announce token management.
pub mod peer_store;
/// BEP 42 node ID generation and validation.
pub mod node_id;
/// BEP 44 immutable and mutable item storage.
pub mod bep44;
/// DHT item storage backend.
pub mod storage;
mod actor;

pub use bep44::{
    ImmutableItem, MutableItem,
    compute_mutable_target, build_signing_buffer,
    MAX_VALUE_SIZE, MAX_SALT_SIZE,
};
pub use compact::{
    CompactNodeInfo, CompactNodeInfo6,
    parse_compact_nodes, encode_compact_nodes,
    parse_compact_nodes6, encode_compact_nodes6,
    COMPACT_NODE_SIZE, COMPACT_NODE6_SIZE,
};
pub use error::{Error, Result};
pub use krpc::{KrpcMessage, KrpcBody, KrpcQuery, KrpcResponse, GetPeersResponse, SampleInfohashesResponse, TransactionId};
pub use routing_table::RoutingTable;
pub use actor::{DhtHandle, DhtConfig, DhtStats, SampleInfohashesResult};
pub use node_id::{generate_node_id, is_valid_node_id, is_bep42_exempt, ExternalIpVoter, IpVoteSource};
pub use storage::{DhtStorage, InMemoryDhtStorage};
