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

mod actor;
/// BEP 44 immutable and mutable item storage.
pub mod bep44;
/// Compact node encoding (26-byte IPv4, 38-byte IPv6).
pub mod compact;
/// DHT error types.
pub mod error;
/// KRPC message encoding and decoding (BEP 5).
pub mod krpc;
/// BEP 42 node ID generation and validation.
pub mod node_id;
/// Per-info_hash peer storage and announce token management.
pub mod peer_store;
/// Kademlia routing table with k-buckets.
pub mod routing_table;
/// DHT item storage backend.
pub mod storage;

pub use actor::{DhtConfig, DhtHandle, DhtStats, SampleInfohashesResult};
pub use bep44::{
    ImmutableItem, MAX_SALT_SIZE, MAX_VALUE_SIZE, MutableItem, build_signing_buffer,
    compute_mutable_target,
};
pub use compact::{
    COMPACT_NODE_SIZE, COMPACT_NODE6_SIZE, CompactNodeInfo, CompactNodeInfo6, encode_compact_nodes,
    encode_compact_nodes6, parse_compact_nodes, parse_compact_nodes6,
};
pub use error::{Error, Result};
pub use krpc::{
    GetPeersResponse, KrpcBody, KrpcMessage, KrpcQuery, KrpcResponse, SampleInfohashesResponse,
    TransactionId,
};
pub use node_id::{
    ExternalIpVoter, IpVoteSource, generate_node_id, is_bep42_exempt, is_valid_node_id,
};
pub use routing_table::{NodeStatus, RoutingTable};
pub use storage::{DhtStorage, InMemoryDhtStorage};
