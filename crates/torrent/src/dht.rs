//! Kademlia DHT for BitTorrent peer discovery (BEP 5) and storage (BEP 44).
//!
//! Re-exports from [`torrent_dht`].

pub use torrent_dht::{
    // Compact node encoding (26-byte IPv4 / 38-byte IPv6)
    CompactNodeInfo,
    CompactNodeInfo6,
    DhtConfig,
    // Actor handle and configuration
    DhtHandle,
    DhtStats,
    // Error types
    Error,
    GetPeersResponse,
    KrpcBody,
    // KRPC protocol messages
    KrpcMessage,
    KrpcQuery,
    KrpcResponse,
    Result,
    // Routing table
    RoutingTable,
    TransactionId,
};

// Re-export items from public sub-modules
pub use torrent_dht::compact::{
    encode_compact_nodes, encode_compact_nodes6, parse_compact_nodes, parse_compact_nodes6,
};
pub use torrent_dht::peer_store::PeerStore;
pub use torrent_dht::routing_table::K;

// BEP 42: Node ID generation/verification and IP voting
pub use torrent_dht::node_id::{
    ExternalIpVoter, IpVoteSource, generate_node_id, is_bep42_exempt, is_valid_node_id,
};

// BEP 44: DHT storage (put/get for immutable and mutable items)
pub use torrent_dht::bep44::{
    ImmutableItem, MAX_SALT_SIZE, MAX_VALUE_SIZE, MutableItem, build_signing_buffer,
    compute_mutable_target,
};
pub use torrent_dht::storage::{DhtStorage, InMemoryDhtStorage};

// BEP 51: DHT infohash indexing (sample_infohashes query)
pub use torrent_dht::SampleInfohashesResult;
pub use torrent_dht::krpc::SampleInfohashesResponse;

// Re-export ed25519 types for BEP 44 mutable item signing
pub use ed25519_dalek::{SigningKey as Ed25519SigningKey, VerifyingKey as Ed25519VerifyingKey};
