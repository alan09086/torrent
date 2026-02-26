//! Kademlia DHT for BitTorrent peer discovery (BEP 5).
//!
//! Re-exports from [`ferrite_dht`].

pub use ferrite_dht::{
    // Actor handle and configuration
    DhtHandle,
    DhtConfig,
    DhtStats,
    // Compact node encoding (26-byte format)
    CompactNodeInfo,
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
pub use ferrite_dht::compact::{parse_compact_nodes, encode_compact_nodes};
pub use ferrite_dht::peer_store::PeerStore;
pub use ferrite_dht::routing_table::K;
