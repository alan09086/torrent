//! Kademlia DHT for BitTorrent peer discovery (BEP 5).
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
