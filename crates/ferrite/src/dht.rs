//! Kademlia DHT for BitTorrent peer discovery (BEP 5) and storage (BEP 44).
//!
//! Re-exports from [`ferrite_dht`].

pub use ferrite_dht::{
    // Actor handle and configuration
    DhtHandle,
    DhtConfig,
    DhtStats,
    // Compact node encoding (26-byte IPv4 / 38-byte IPv6)
    CompactNodeInfo,
    CompactNodeInfo6,
    // Routing table
    RoutingTable,
    // KRPC protocol messages
    KrpcMessage,
    KrpcBody,
    KrpcQuery,
    KrpcResponse,
    GetPeersResponse,
    TransactionId,
    // Error types
    Error,
    Result,
};

// Re-export items from public sub-modules
pub use ferrite_dht::compact::{
    parse_compact_nodes, encode_compact_nodes,
    parse_compact_nodes6, encode_compact_nodes6,
};
pub use ferrite_dht::peer_store::PeerStore;
pub use ferrite_dht::routing_table::K;

// BEP 42: Node ID generation/verification and IP voting
pub use ferrite_dht::node_id::{
    generate_node_id, is_valid_node_id, is_bep42_exempt,
    ExternalIpVoter, IpVoteSource,
};

// BEP 44: DHT storage (put/get for immutable and mutable items)
pub use ferrite_dht::bep44::{
    ImmutableItem, MutableItem,
    compute_mutable_target, build_signing_buffer,
    MAX_VALUE_SIZE, MAX_SALT_SIZE,
};
pub use ferrite_dht::storage::{DhtStorage, InMemoryDhtStorage};

// Re-export ed25519 types for BEP 44 mutable item signing
pub use ed25519_dalek::{SigningKey as Ed25519SigningKey, VerifyingKey as Ed25519VerifyingKey};
