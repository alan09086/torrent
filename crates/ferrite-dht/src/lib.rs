//! Kademlia DHT for BitTorrent (BEP 5).
//!
//! Implements the Mainline DHT protocol: KRPC message encoding, Kademlia
//! routing table, peer discovery, and announce operations.
//!
//! # Architecture
//!
//! The DHT runs as an actor: `DhtHandle::start()` spawns a background task
//! that owns all state (routing table, UDP socket, pending queries). The
//! returned `DhtHandle` is a cheap, cloneable sender for submitting commands.

pub mod error;
pub mod compact;
pub mod krpc;
pub mod routing_table;
pub mod peer_store;
pub mod node_id;
pub mod bep44;
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
pub use actor::{DhtHandle, DhtConfig, DhtStats};
pub use node_id::{generate_node_id, is_valid_node_id, is_bep42_exempt, ExternalIpVoter, IpVoteSource};
pub use storage::{DhtStorage, InMemoryDhtStorage};
