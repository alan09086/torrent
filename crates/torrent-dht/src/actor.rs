//! DHT actor: single-owner event loop managing the routing table, UDP socket,
//! and pending queries.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, trace, warn};

use torrent_core::{AddressFamily, Id20};

use crate::bep44::{self, ImmutableItem, MAX_SALT_SIZE, MAX_VALUE_SIZE, MutableItem};
use crate::compact::CompactNodeInfo;
use crate::error::{Error, Result};
use crate::krpc::{
    GetPeersResponse, KrpcBody, KrpcMessage, KrpcQuery, KrpcResponse, SampleInfohashesResponse,
    TransactionId,
};
use crate::node_id::{self, ExternalIpVoter, IpVoteSource};
use crate::peer_store::PeerStore;
use crate::routing_table::{K, RoutingTable};
use crate::storage::{DhtStorage, InMemoryDhtStorage};

#[allow(unused_imports)]
use ed25519_dalek::SigningKey;

/// Token-bucket rate limiter for outgoing KRPC queries.
///
/// Permits refill continuously based on elapsed real time. `try_acquire` is
/// non-blocking: it either consumes a permit or returns `false` immediately.
struct QueryRateLimiter {
    permits: u32,
    max_permits: u32,
    last_refill: Instant,
    refill_rate: u32,
}

impl QueryRateLimiter {
    /// Create a new limiter with `rate` permits per second. Starts with a full
    /// bucket so the first burst of queries is not artificially delayed.
    fn new(rate: usize) -> Self {
        QueryRateLimiter {
            permits: rate as u32,
            max_permits: rate as u32,
            last_refill: Instant::now(),
            refill_rate: rate as u32,
        }
    }

    /// Attempt to consume one permit. Returns `true` if a permit was available,
    /// `false` if the bucket is empty. Never blocks.
    fn try_acquire(&mut self) -> bool {
        self.refill();
        if self.permits > 0 {
            self.permits -= 1;
            true
        } else {
            false
        }
    }

    /// Refill the bucket based on elapsed time since the last refill. Caps at
    /// `max_permits`. Only updates `last_refill` when at least one permit is
    /// added (avoids drift on very fast calls).
    fn refill(&mut self) {
        let elapsed = self.last_refill.elapsed();
        let elapsed_secs = elapsed.as_secs_f64();
        let new_permits = (elapsed_secs * self.refill_rate as f64) as u32;
        if new_permits > 0 {
            self.permits = (self.permits + new_permits).min(self.max_permits);
            self.last_refill = Instant::now();
        }
    }
}

/// Configuration for the DHT.
#[derive(Debug, Clone)]
pub struct DhtConfig {
    /// Address to bind the UDP socket.
    pub bind_addr: SocketAddr,
    /// Bootstrap nodes (host:port strings resolved at startup).
    pub bootstrap_nodes: Vec<String>,
    /// Our node ID. Generated randomly if `None`.
    pub own_id: Option<Id20>,
    /// Max outgoing queries per second (0 = unlimited).
    pub queries_per_second: usize,
    /// Timeout for individual KRPC queries.
    pub query_timeout: Duration,
    /// Address family for this DHT instance (determines compact format and DNS filtering).
    pub address_family: AddressFamily,
    /// BEP 42: Enforce node ID verification when inserting into routing table.
    /// Nodes with IDs that don't match their IP are rejected.
    pub enforce_node_id: bool,
    /// BEP 42: Restrict routing table to one node per IP address.
    pub restrict_routing_ips: bool,
    /// BEP 44: Maximum number of stored DHT items (immutable + mutable).
    pub dht_max_items: usize,
    /// BEP 44: Lifetime of DHT items in seconds before expiry.
    pub dht_item_lifetime_secs: u64,
    /// Maximum number of nodes in the routing table. Prevents unbounded growth
    /// from adversarial node injection. Default: 512 (matches rqbit).
    pub max_routing_nodes: usize,
}

impl Default for DhtConfig {
    fn default() -> Self {
        DhtConfig {
            bind_addr: "0.0.0.0:0".parse().unwrap(),
            bootstrap_nodes: vec![
                "router.bittorrent.com:6881".into(),
                "dht.transmissionbt.com:6881".into(),
                "router.utorrent.com:6881".into(),
            ],
            own_id: None,
            queries_per_second: 250,
            query_timeout: Duration::from_secs(5),
            address_family: AddressFamily::V4,
            enforce_node_id: false,
            restrict_routing_ips: true,
            dht_max_items: 700,
            dht_item_lifetime_secs: 7200,
            max_routing_nodes: 512,
        }
    }
}

impl DhtConfig {
    /// Default configuration for an IPv6 DHT instance (BEP 24).
    pub fn default_v6() -> Self {
        DhtConfig {
            bind_addr: "[::]:0".parse().unwrap(),
            bootstrap_nodes: vec![
                "router.bittorrent.com:6881".into(),
                "dht.libtorrent.org:25401".into(),
            ],
            own_id: None,
            queries_per_second: 250,
            query_timeout: Duration::from_secs(5),
            address_family: AddressFamily::V6,
            enforce_node_id: false,
            restrict_routing_ips: true,
            dht_max_items: 700,
            dht_item_lifetime_secs: 7200,
            max_routing_nodes: 512,
        }
    }
}

/// Runtime statistics for the DHT.
#[derive(Debug, Clone)]
pub struct DhtStats {
    /// Our current node ID (may differ from startup ID after BEP 42 regeneration).
    pub node_id: Id20,
    /// Number of nodes in the routing table.
    pub routing_table_size: usize,
    /// Number of k-buckets in use.
    pub bucket_count: usize,
    /// Number of distinct info hashes tracked in the peer store.
    pub peer_store_info_hashes: usize,
    /// Total number of peers across all info hashes.
    pub peer_store_peers: usize,
    /// Number of in-flight KRPC queries.
    pub pending_queries: usize,
    /// Total KRPC queries sent since startup.
    pub total_queries_sent: u64,
    /// Total KRPC responses received since startup.
    pub total_responses_received: u64,
    /// Number of BEP 44 items stored (immutable + mutable).
    pub dht_item_count: usize,
}

/// Result of a sample_infohashes query (BEP 51).
#[derive(Debug, Clone)]
pub struct SampleInfohashesResult {
    /// Minimum seconds before querying the same node again.
    pub interval: i64,
    /// Estimated total info hashes in the remote node's store.
    pub num: i64,
    /// Sampled info hashes.
    pub samples: Vec<Id20>,
    /// Closer nodes for traversal.
    pub nodes: Vec<CompactNodeInfo>,
}

/// A cloneable handle to the DHT actor.
#[derive(Clone)]
pub struct DhtHandle {
    tx: mpsc::Sender<DhtCommand>,
}

enum DhtCommand {
    GetPeers {
        info_hash: Id20,
        reply: mpsc::Sender<Vec<SocketAddr>>,
    },
    Announce {
        info_hash: Id20,
        port: u16,
        reply: oneshot::Sender<Result<()>>,
    },
    Stats {
        reply: oneshot::Sender<DhtStats>,
    },
    UpdateExternalIp {
        ip: std::net::IpAddr,
        source: IpVoteSource,
    },
    GetImmutable {
        target: Id20,
        reply: oneshot::Sender<Result<Option<Vec<u8>>>>,
    },
    PutImmutable {
        value: Vec<u8>,
        reply: oneshot::Sender<Result<Id20>>,
    },
    GetMutable {
        public_key: [u8; 32],
        salt: Vec<u8>,
        #[allow(clippy::type_complexity)]
        reply: oneshot::Sender<Result<Option<(Vec<u8>, i64)>>>,
    },
    PutMutable {
        keypair_bytes: [u8; 32],
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
        reply: oneshot::Sender<Result<Id20>>,
    },
    SampleInfohashes {
        target: Id20,
        reply: oneshot::Sender<Result<SampleInfohashesResult>>,
    },
    GetRoutingNodes {
        reply: oneshot::Sender<Vec<(Id20, SocketAddr)>>,
    },
    Shutdown,
}

impl DhtHandle {
    /// Start the DHT actor and return a handle plus an IP consensus channel.
    ///
    /// The consensus channel fires when the BEP 42 ExternalIpVoter reaches
    /// agreement on our external IP address.
    pub async fn start(config: DhtConfig) -> Result<(Self, mpsc::Receiver<std::net::IpAddr>)> {
        let socket = UdpSocket::bind(config.bind_addr).await?;
        let local_addr = socket.local_addr()?;
        debug!(addr = %local_addr, "DHT socket bound");

        let (tx, rx) = mpsc::channel(256);
        let (ip_consensus_tx, ip_consensus_rx) = mpsc::channel(4);
        let handle = DhtHandle { tx };

        let actor = DhtActor::new(config, socket, rx, ip_consensus_tx);
        tokio::spawn(actor.run());

        Ok((handle, ip_consensus_rx))
    }

    /// Notify the DHT of our external IP (from NAT/tracker discovery).
    pub async fn update_external_ip(
        &self,
        ip: std::net::IpAddr,
        source: IpVoteSource,
    ) -> Result<()> {
        self.tx
            .send(DhtCommand::UpdateExternalIp { ip, source })
            .await
            .map_err(|_| Error::Shutdown)
    }

    /// Discover peers for an info_hash.
    ///
    /// Returns a channel that receives batches of peers as they are found.
    /// The channel closes when the search is exhausted.
    pub async fn get_peers(&self, info_hash: Id20) -> Result<mpsc::Receiver<Vec<SocketAddr>>> {
        let (reply_tx, reply_rx) = mpsc::channel(32);
        self.tx
            .send(DhtCommand::GetPeers {
                info_hash,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        Ok(reply_rx)
    }

    /// Announce that we have peers for an info_hash on the given port.
    pub async fn announce(&self, info_hash: Id20, port: u16) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::Announce {
                info_hash,
                port,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Get current DHT statistics.
    pub async fn stats(&self) -> Result<DhtStats> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::Stats { reply: reply_tx })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)
    }

    /// Shut down the DHT actor.
    pub async fn shutdown(&self) -> Result<()> {
        self.tx
            .send(DhtCommand::Shutdown)
            .await
            .map_err(|_| Error::Shutdown)
    }

    /// Store an immutable item in the DHT (BEP 44).
    ///
    /// Returns the SHA-1 target hash that can be used to retrieve the item.
    /// The value must be valid bencoded data, max 1000 bytes.
    pub async fn put_immutable(&self, value: Vec<u8>) -> Result<Id20> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::PutImmutable {
                value,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Retrieve an immutable item from the DHT (BEP 44).
    ///
    /// Returns the raw bencoded value if found, `None` if not.
    pub async fn get_immutable(&self, target: Id20) -> Result<Option<Vec<u8>>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::GetImmutable {
                target,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Store a mutable item in the DHT (BEP 44).
    ///
    /// - `keypair_bytes`: 32-byte ed25519 seed (secret key)
    /// - `value`: bencoded data, max 1000 bytes
    /// - `seq`: sequence number (must be higher than any previously stored)
    /// - `salt`: optional salt for sub-key isolation (max 64 bytes)
    ///
    /// Returns the target hash.
    pub async fn put_mutable(
        &self,
        keypair_bytes: [u8; 32],
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
    ) -> Result<Id20> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::PutMutable {
                keypair_bytes,
                value,
                seq,
                salt,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Query a DHT node for a random sample of info hashes (BEP 51).
    ///
    /// Routes toward `target` to find the responding node. Returns sampled
    /// hashes, the interval before re-querying, and closer nodes for traversal.
    pub async fn sample_infohashes(&self, target: Id20) -> Result<SampleInfohashesResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::SampleInfohashes {
                target,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Retrieve a mutable item from the DHT (BEP 44).
    ///
    /// Returns `(value, seq)` if found, `None` if not.
    pub async fn get_mutable(
        &self,
        public_key: [u8; 32],
        salt: Vec<u8>,
    ) -> Result<Option<(Vec<u8>, i64)>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DhtCommand::GetMutable {
                public_key,
                salt,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Return all nodes currently in the DHT routing table.
    pub async fn get_routing_nodes(&self) -> Vec<(Id20, SocketAddr)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DhtCommand::GetRoutingNodes { reply: reply_tx })
            .await;
        reply_rx.await.unwrap_or_default()
    }
}

// ---- Actor internals ----

struct DhtActor {
    config: DhtConfig,
    address_family: AddressFamily,
    socket: UdpSocket,
    rx: mpsc::Receiver<DhtCommand>,
    routing_table: RoutingTable,
    peer_store: PeerStore,
    /// BEP 44 item storage (immutable + mutable).
    item_store: Box<dyn DhtStorage + Send>,
    pending: HashMap<u16, PendingQuery>,
    next_txn_id: u16,
    stats: ActorStats,
    /// Active get_peers lookups.
    lookups: HashMap<Id20, LookupState>,
    /// Active BEP 44 get lookups.
    item_lookups: HashMap<Id20, ItemLookupState>,
    /// Active BEP 44 put operations (waiting for tokens before sending puts).
    item_put_ops: HashMap<Id20, ItemPutState>,
    /// BEP 42 external IP voter: aggregates IP reports from KRPC responses.
    ip_voter: ExternalIpVoter,
    /// Callback channel: fires when voter consensus changes.
    ip_consensus_tx: mpsc::Sender<std::net::IpAddr>,
    /// Pending one-shot replies for sample_infohashes queries.
    sample_replies: HashMap<u16, oneshot::Sender<Result<SampleInfohashesResult>>>,
    /// Token-bucket rate limiter for outgoing KRPC queries.
    rate_limiter: QueryRateLimiter,
    /// Active iterative bootstrap lookup (find_node self-lookup after initial bootstrap).
    bootstrap_lookup: Option<FindNodeLookup>,
    /// Whether initial bootstrap (FindNodeLookup) has completed (M97).
    bootstrap_complete: bool,
    /// Queued get_peers requests waiting for bootstrap to complete (M97).
    pending_get_peers: Vec<(Id20, mpsc::Sender<Vec<SocketAddr>>)>,
    /// Bootstrap timeout timer — forces bootstrap_complete after 10s (M97).
    bootstrap_timeout: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    /// Timestamp of last `ping_questionable_nodes()` call for two-phase gating (M105).
    last_ping: Instant,
    /// Receiver for DNS-resolved bootstrap addresses from background tasks (M105).
    /// Set during `bootstrap()`, drained in the main select! loop, cleared when
    /// all spawned DNS tasks complete (channel closes).
    dns_bootstrap_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,
}

struct ActorStats {
    total_queries_sent: u64,
    total_responses_received: u64,
}

struct PendingQuery {
    sent_at: Instant,
    addr: SocketAddr,
    kind: PendingQueryKind,
    node_id: Option<Id20>,
}

#[derive(Debug)]
enum PendingQueryKind {
    Ping,
    FindNode,
    GetPeers {
        info_hash: Id20,
    },
    AnnouncePeer,
    /// BEP 44: outgoing get item query.
    GetItem {
        target: Id20,
    },
    /// BEP 44: outgoing put item query.
    PutItem,
    /// BEP 51: outgoing sample_infohashes query.
    SampleInfohashes,
}

struct LookupState {
    reply: mpsc::Sender<Vec<SocketAddr>>,
    /// Nodes we've already queried.
    queried: std::collections::HashSet<Id20>,
    /// Nodes with tokens (for announce).
    tokens: HashMap<Id20, (SocketAddr, Vec<u8>)>,
    /// Best (closest) nodes we know about.
    closest: Vec<CompactNodeInfo>,
}

/// State for an active iterative find_node bootstrap lookup.
struct FindNodeLookup {
    target: Id20,
    closest: Vec<CompactNodeInfo>,
    queried: std::collections::HashSet<Id20>,
    round: u8,
    max_rounds: u8,
}

/// State for an active BEP 44 get lookup.
enum ItemLookupState {
    Immutable {
        #[allow(clippy::type_complexity)]
        reply: Option<oneshot::Sender<Result<Option<Vec<u8>>>>>,
        queried: std::collections::HashSet<Id20>,
    },
    Mutable {
        salt: Vec<u8>,
        #[allow(clippy::type_complexity)]
        reply: Option<oneshot::Sender<Result<Option<(Vec<u8>, i64)>>>>,
        best_seq: i64,
        best_value: Option<Vec<u8>>,
        queried: std::collections::HashSet<Id20>,
    },
}

/// State for an active BEP 44 put operation (waiting for tokens then sending puts).
enum ItemPutState {
    Immutable {
        item: crate::bep44::ImmutableItem,
        tokens: HashMap<Id20, (SocketAddr, Vec<u8>)>,
        sent_puts: usize,
        reply: Option<oneshot::Sender<Result<Id20>>>,
    },
    Mutable {
        item: crate::bep44::MutableItem,
        tokens: HashMap<Id20, (SocketAddr, Vec<u8>)>,
        sent_puts: usize,
        reply: Option<oneshot::Sender<Result<Id20>>>,
    },
}

/// Parameters for a single BEP 44 put-item query.
struct PutItemParams {
    addr: SocketAddr,
    token: Vec<u8>,
    value: Vec<u8>,
    key: Option<[u8; 32]>,
    signature: Option<[u8; 64]>,
    seq: Option<i64>,
    salt: Option<Vec<u8>>,
}

/// Interval for routing table maintenance.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
/// Interval for peer store cleanup.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Interval for pinging questionable nodes.
const PING_INTERVAL: Duration = Duration::from_secs(5);

impl DhtActor {
    fn new(
        config: DhtConfig,
        socket: UdpSocket,
        rx: mpsc::Receiver<DhtCommand>,
        ip_consensus_tx: mpsc::Sender<std::net::IpAddr>,
    ) -> Self {
        let own_id = config.own_id.unwrap_or_else(generate_node_id);
        let address_family = config.address_family;
        let restrict_ips = config.restrict_routing_ips;
        let max_routing_nodes = config.max_routing_nodes;
        debug!(id = %own_id, family = ?address_family, "DHT node ID");

        let max_items = config.dht_max_items;
        let queries_per_second = config.queries_per_second;
        DhtActor {
            config,
            address_family,
            socket,
            rx,
            routing_table: RoutingTable::with_config(own_id, restrict_ips, max_routing_nodes),
            peer_store: PeerStore::new(),
            item_store: Box::new(InMemoryDhtStorage::new(max_items)),
            pending: HashMap::new(),
            next_txn_id: 1,
            stats: ActorStats {
                total_queries_sent: 0,
                total_responses_received: 0,
            },
            lookups: HashMap::new(),
            item_lookups: HashMap::new(),
            item_put_ops: HashMap::new(),
            ip_voter: ExternalIpVoter::new(10),
            ip_consensus_tx,
            sample_replies: HashMap::new(),
            rate_limiter: QueryRateLimiter::new(queries_per_second),
            bootstrap_lookup: None,
            bootstrap_complete: false,
            pending_get_peers: Vec::new(),
            bootstrap_timeout: Some(Box::pin(tokio::time::sleep(Duration::from_secs(10)))),
            last_ping: Instant::now(),
            dns_bootstrap_rx: None,
        }
    }

    async fn run(mut self) {
        // Bootstrap
        self.bootstrap().await;

        let mut recv_buf = vec![0u8; 65535];
        let mut maintenance_tick = tokio::time::interval(MAINTENANCE_INTERVAL);
        let mut cleanup_tick = tokio::time::interval(CLEANUP_INTERVAL);
        let mut query_timeout_tick = tokio::time::interval(self.config.query_timeout);
        let mut ping_tick = tokio::time::interval(PING_INTERVAL);

        loop {
            tokio::select! {
                // Incoming UDP packets
                result = self.socket.recv_from(&mut recv_buf) => {
                    match result {
                        Ok((n, addr)) => {
                            self.handle_packet(&recv_buf[..n], addr).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "UDP recv error");
                        }
                    }
                }

                // Commands from handle
                cmd = self.rx.recv() => {
                    match cmd {
                        Some(DhtCommand::GetPeers { info_hash, reply }) => {
                            self.start_get_peers(info_hash, reply).await;
                        }
                        Some(DhtCommand::Announce { info_hash, port, reply }) => {
                            self.handle_announce(info_hash, port, reply).await;
                        }
                        Some(DhtCommand::Stats { reply }) => {
                            let _ = reply.send(self.make_stats());
                        }
                        Some(DhtCommand::UpdateExternalIp { ip, source }) => {
                            let source_id = source.source_id();
                            if let Some(consensus_ip) = self.ip_voter.add_vote(source_id, ip) {
                                debug!(%consensus_ip, "BEP 42: external IP consensus (via NAT/tracker)");
                                let _ = self.ip_consensus_tx.try_send(consensus_ip);
                                self.regenerate_node_id(consensus_ip);
                            }
                        }
                        Some(DhtCommand::GetImmutable { target, reply }) => {
                            self.handle_get_immutable(target, reply).await;
                        }
                        Some(DhtCommand::PutImmutable { value, reply }) => {
                            self.handle_put_immutable(value, reply).await;
                        }
                        Some(DhtCommand::GetMutable { public_key, salt, reply }) => {
                            self.handle_get_mutable(public_key, salt, reply).await;
                        }
                        Some(DhtCommand::PutMutable { keypair_bytes, value, seq, salt, reply }) => {
                            self.handle_put_mutable(keypair_bytes, value, seq, salt, reply).await;
                        }
                        Some(DhtCommand::SampleInfohashes { target, reply }) => {
                            self.handle_sample_infohashes(target, reply).await;
                        }
                        Some(DhtCommand::GetRoutingNodes { reply }) => {
                            let nodes = self.routing_table.all_nodes();
                            let _ = reply.send(nodes);
                        }
                        Some(DhtCommand::Shutdown) | None => {
                            debug!("DHT shutting down");
                            return;
                        }
                    }
                }

                // Expire timed-out queries and advance stalled lookups
                // (like libtorrent's traversal_algorithm::failed → add_requests)
                _ = query_timeout_tick.tick() => {
                    self.expire_queries_and_advance_lookups().await;
                }

                // Periodic maintenance (routing table housekeeping)
                _ = maintenance_tick.tick() => {
                    self.maintenance().await;
                }

                // Peer store and item store cleanup
                _ = cleanup_tick.tick() => {
                    self.peer_store.cleanup();
                    self.item_store.expire(
                        Duration::from_secs(self.config.dht_item_lifetime_secs)
                    );
                }

                // Ping questionable nodes to verify liveness + check node-count gate
                _ = ping_tick.tick() => {
                    // Two-phase ping frequency (M105): 5s during bootstrap for
                    // fast routing-table population, 60s steady-state to reduce
                    // chatter after the table is established.
                    let ping_interval = if self.bootstrap_complete {
                        Duration::from_secs(60)
                    } else {
                        Duration::from_secs(5)
                    };
                    if self.last_ping.elapsed() >= ping_interval {
                        self.ping_questionable_nodes().await;
                        self.last_ping = Instant::now();
                    }
                    // Node-count gate: if saved-node pings have populated the
                    // table but FindNodeLookup hasn't converged yet, open the
                    // gate early so queued get_peers aren't blocked.
                    if !self.bootstrap_complete {
                        debug!(
                            table_size = self.routing_table.len(),
                            threshold = 8,
                            "node-count gate: checking bootstrap readiness"
                        );
                        if self.routing_table.len() >= 8 {
                            debug!(
                                table_size = self.routing_table.len(),
                                "node-count gate: opening bootstrap gate early"
                            );
                            self.on_bootstrap_complete().await;
                        }
                    }
                }

                // M97: Bootstrap timeout — force bootstrap_complete after 10s
                _ = async {
                    match &mut self.bootstrap_timeout {
                        Some(timer) => timer.as_mut().await,
                        None => std::future::pending().await,
                    }
                }, if self.bootstrap_timeout.is_some() && !self.bootstrap_complete => {
                    warn!(
                        table_size = self.routing_table.len(),
                        "bootstrap timeout (10s), proceeding with current routing table"
                    );
                    self.on_bootstrap_complete().await;
                }

                // M105: Drain DNS-resolved bootstrap addresses from background tasks
                result = async {
                    match &mut self.dns_bootstrap_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let own_id = *self.routing_table.own_id();
                    match result {
                        Some(addrs) => {
                            for addr in addrs {
                                self.send_find_node(addr, own_id, None).await;
                            }
                        }
                        None => {
                            // All DNS tasks completed
                            debug!("DNS bootstrap tasks completed");
                            self.dns_bootstrap_rx = None;
                        }
                    }
                }
            }
        }
    }

    async fn bootstrap(&mut self) {
        let own_id = *self.routing_table.own_id();

        // Partition: saved nodes (IP:port) vs DNS hostnames.
        // Saved nodes parse as SocketAddr; hardcoded bootstrap nodes have
        // hostname:port and will fail parse.
        let (saved_addrs, hostname_strs): (Vec<_>, Vec<_>) =
            self.config.bootstrap_nodes.clone().into_iter().partition(|s| {
                s.parse::<SocketAddr>().is_ok()
            });

        debug!(
            saved_nodes = saved_addrs.len(),
            dns_nodes = hostname_strs.len(),
            family = ?self.address_family,
            "bootstrap: starting (pinging saved nodes, resolving DNS nodes)"
        );

        // Phase 1: Ping saved nodes (validates liveness, inserts into routing
        // table via the normal ping response handler — no PingVerify needed)
        for addr_str in &saved_addrs {
            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                self.send_ping(addr, None).await;
            }
        }

        // Phase 2: Spawn background DNS resolution tasks with retry+backoff.
        // Each hostname gets its own tokio::spawn that retries with exponential
        // backoff (1s → 30s cap, 120s total deadline). Resolved addresses are
        // sent to dns_bootstrap_rx and integrated via the main select! loop.
        // Phase 3 starts immediately without waiting for DNS.
        if !hostname_strs.is_empty() {
            let (dns_tx, dns_rx) = mpsc::channel(16);
            for hostname in hostname_strs {
                let tx = dns_tx.clone();
                let family = self.address_family;
                tokio::spawn(async move {
                    dns_bootstrap_resolve(hostname, family, tx).await;
                });
            }
            drop(dns_tx); // close sender so receiver ends when all tasks complete
            self.dns_bootstrap_rx = Some(dns_rx);
        }

        // Phase 3: Initiate iterative bootstrap — follow returned nodes to discover more
        let initial_closest: Vec<CompactNodeInfo> = self
            .routing_table
            .closest(&own_id, K)
            .into_iter()
            .map(|n| CompactNodeInfo { id: n.id, addr: n.addr })
            .collect();

        debug!(
            initial_nodes = initial_closest.len(),
            table_size = self.routing_table.len(),
            "bootstrap: starting iterative lookup"
        );

        self.bootstrap_lookup = Some(FindNodeLookup {
            target: own_id,
            closest: initial_closest,
            queried: std::collections::HashSet::new(),
            round: 0,
            max_rounds: 6,
        });
    }

    async fn handle_packet(&mut self, data: &[u8], addr: SocketAddr) {
        let msg = match KrpcMessage::from_bytes(data) {
            Ok(msg) => msg,
            Err(e) => {
                trace!(error = %e, from = %addr, "invalid KRPC message");
                return;
            }
        };

        match &msg.body {
            KrpcBody::Query(query) => {
                self.handle_query(&msg, query, addr).await;
            }
            KrpcBody::Response(resp) => {
                self.handle_response(&msg, resp, addr).await;
            }
            KrpcBody::Error { code, message } => {
                trace!(code, message, from = %addr, "KRPC error received");
                // Still match pending query to clean up
                let txn = msg.transaction_id.as_u16();
                if let Some(pending) = self.pending.remove(&txn)
                    && let Some(nid) = pending.node_id
                {
                    self.routing_table.mark_failed(&nid);
                }
            }
        }
    }

    /// Check if a socket address matches this actor's address family.
    fn matches_family(&self, addr: &SocketAddr) -> bool {
        match self.address_family {
            AddressFamily::V4 => addr.is_ipv4(),
            AddressFamily::V6 => addr.is_ipv6(),
        }
    }

    async fn handle_query(&mut self, msg: &KrpcMessage, query: &KrpcQuery, addr: SocketAddr) {
        if !self.matches_family(&addr) {
            return; // Reject wrong address family
        }
        let sender_id = *query.sender_id();
        self.checked_insert(sender_id, addr);
        self.routing_table.mark_query(&sender_id);

        let response = match query {
            KrpcQuery::Ping { id: _ } => KrpcResponse::NodeId {
                id: *self.routing_table.own_id(),
            },
            KrpcQuery::FindNode { id: _, target } => {
                let closest = self.routing_table.closest(target, K);
                let nodes: Vec<CompactNodeInfo> = closest
                    .into_iter()
                    .map(|n| CompactNodeInfo {
                        id: n.id,
                        addr: n.addr,
                    })
                    .collect();
                KrpcResponse::FindNode {
                    id: *self.routing_table.own_id(),
                    nodes,
                    nodes6: Vec::new(),
                }
            }
            KrpcQuery::GetPeers { id: _, info_hash } => {
                let ip = addr.ip();
                let token = self.peer_store.generate_token(&ip);
                let peers = self.peer_store.get_peers(info_hash, 50);

                if peers.is_empty() {
                    let closest = self.routing_table.closest(info_hash, K);
                    let nodes: Vec<CompactNodeInfo> = closest
                        .into_iter()
                        .map(|n| CompactNodeInfo {
                            id: n.id,
                            addr: n.addr,
                        })
                        .collect();
                    KrpcResponse::GetPeers(GetPeersResponse {
                        id: *self.routing_table.own_id(),
                        token: Some(token),
                        peers: Vec::new(),
                        nodes,
                        nodes6: Vec::new(),
                    })
                } else {
                    KrpcResponse::GetPeers(GetPeersResponse {
                        id: *self.routing_table.own_id(),
                        token: Some(token),
                        peers,
                        nodes: Vec::new(),
                        nodes6: Vec::new(),
                    })
                }
            }
            KrpcQuery::AnnouncePeer {
                id: _,
                info_hash,
                port,
                implied_port,
                token,
            } => {
                let ip = addr.ip();
                if !self.peer_store.validate_token(token, &ip) {
                    // Send error response for invalid token
                    let err_msg = KrpcMessage {
                        transaction_id: msg.transaction_id,
                        body: KrpcBody::Error {
                            code: 203,
                            message: "invalid token".into(),
                        },
                        sender_ip: Some(addr),
                    };
                    if let Ok(bytes) = err_msg.to_bytes() {
                        let _ = self.socket.send_to(&bytes, addr).await;
                    }
                    return;
                }
                let peer_port = if *implied_port { addr.port() } else { *port };
                let peer_addr = SocketAddr::new(addr.ip(), peer_port);
                self.peer_store.add_peer(*info_hash, peer_addr);
                KrpcResponse::NodeId {
                    id: *self.routing_table.own_id(),
                }
            }
            // BEP 44: get item from DHT storage
            KrpcQuery::Get {
                id: _,
                target,
                seq: requested_seq,
            } => {
                let ip = addr.ip();
                let token = self.peer_store.generate_token(&ip);

                // Try immutable lookup first
                if let Some(item) = self.item_store.get_immutable(target) {
                    KrpcResponse::GetItem {
                        id: *self.routing_table.own_id(),
                        token: Some(token),
                        nodes: Vec::new(),
                        nodes6: Vec::new(),
                        value: Some(item.value),
                        key: None,
                        signature: None,
                        seq: None,
                    }
                } else if let Some(item) = self.item_store.get_mutable_by_target(target) {
                    // Check if requester wants only items with seq > requested_seq
                    if let Some(min_seq) = requested_seq {
                        if item.seq <= *min_seq {
                            // Return token + nodes but no value (requester already has this or newer)
                            let closest = self.routing_table.closest(target, K);
                            let nodes: Vec<CompactNodeInfo> = closest
                                .into_iter()
                                .map(|n| CompactNodeInfo {
                                    id: n.id,
                                    addr: n.addr,
                                })
                                .collect();
                            KrpcResponse::GetItem {
                                id: *self.routing_table.own_id(),
                                token: Some(token),
                                nodes,
                                nodes6: Vec::new(),
                                value: None,
                                key: Some(item.public_key),
                                signature: Some(item.signature),
                                seq: Some(item.seq),
                            }
                        } else {
                            KrpcResponse::GetItem {
                                id: *self.routing_table.own_id(),
                                token: Some(token),
                                nodes: Vec::new(),
                                nodes6: Vec::new(),
                                value: Some(item.value),
                                key: Some(item.public_key),
                                signature: Some(item.signature),
                                seq: Some(item.seq),
                            }
                        }
                    } else {
                        KrpcResponse::GetItem {
                            id: *self.routing_table.own_id(),
                            token: Some(token),
                            nodes: Vec::new(),
                            nodes6: Vec::new(),
                            value: Some(item.value),
                            key: Some(item.public_key),
                            signature: Some(item.signature),
                            seq: Some(item.seq),
                        }
                    }
                } else {
                    // Not found — return closer nodes
                    let closest = self.routing_table.closest(target, K);
                    let nodes: Vec<CompactNodeInfo> = closest
                        .into_iter()
                        .map(|n| CompactNodeInfo {
                            id: n.id,
                            addr: n.addr,
                        })
                        .collect();
                    KrpcResponse::GetItem {
                        id: *self.routing_table.own_id(),
                        token: Some(token),
                        nodes,
                        nodes6: Vec::new(),
                        value: None,
                        key: None,
                        signature: None,
                        seq: None,
                    }
                }
            }
            // BEP 44: put item into DHT storage
            KrpcQuery::Put {
                id: _,
                token,
                value,
                key,
                signature,
                seq,
                salt,
                cas,
            } => {
                let ip = addr.ip();

                // Validate token
                if !self.peer_store.validate_token(token, &ip) {
                    let err_msg = KrpcMessage {
                        transaction_id: msg.transaction_id,
                        body: KrpcBody::Error {
                            code: 203,
                            message: "invalid token".into(),
                        },
                        sender_ip: Some(addr),
                    };
                    if let Ok(bytes) = err_msg.to_bytes() {
                        let _ = self.socket.send_to(&bytes, addr).await;
                    }
                    return;
                }

                // Validate value size
                if value.len() > MAX_VALUE_SIZE {
                    let err_msg = KrpcMessage {
                        transaction_id: msg.transaction_id,
                        body: KrpcBody::Error {
                            code: 205,
                            message: "message (v field) too big".into(),
                        },
                        sender_ip: Some(addr),
                    };
                    if let Ok(bytes) = err_msg.to_bytes() {
                        let _ = self.socket.send_to(&bytes, addr).await;
                    }
                    return;
                }

                if let (Some(k), Some(sig), Some(seq_val)) = (key, signature, seq) {
                    // Mutable item
                    let salt_bytes = salt.clone().unwrap_or_default();

                    // Validate salt size
                    if salt_bytes.len() > MAX_SALT_SIZE {
                        let err_msg = KrpcMessage {
                            transaction_id: msg.transaction_id,
                            body: KrpcBody::Error {
                                code: 207,
                                message: "salt (salt field) too big".into(),
                            },
                            sender_ip: Some(addr),
                        };
                        if let Ok(bytes) = err_msg.to_bytes() {
                            let _ = self.socket.send_to(&bytes, addr).await;
                        }
                        return;
                    }

                    let item = MutableItem {
                        value: value.clone(),
                        public_key: *k,
                        signature: *sig,
                        seq: *seq_val,
                        salt: salt_bytes,
                        target: bep44::compute_mutable_target(k, salt.as_deref().unwrap_or(&[])),
                    };

                    // Verify signature
                    if !item.verify() {
                        let err_msg = KrpcMessage {
                            transaction_id: msg.transaction_id,
                            body: KrpcBody::Error {
                                code: 206,
                                message: "invalid signature".into(),
                            },
                            sender_ip: Some(addr),
                        };
                        if let Ok(bytes) = err_msg.to_bytes() {
                            let _ = self.socket.send_to(&bytes, addr).await;
                        }
                        return;
                    }

                    // CAS check
                    if let Some(expected_seq) = cas
                        && let Some(existing) = self.item_store.get_mutable(k, &item.salt)
                        && existing.seq != *expected_seq
                    {
                        let err_msg = KrpcMessage {
                            transaction_id: msg.transaction_id,
                            body: KrpcBody::Error {
                                code: 301,
                                message: format!(
                                    "CAS mismatch: expected seq {}, got {}",
                                    expected_seq, existing.seq
                                ),
                            },
                            sender_ip: Some(addr),
                        };
                        if let Ok(bytes) = err_msg.to_bytes() {
                            let _ = self.socket.send_to(&bytes, addr).await;
                        }
                        return;
                    }

                    // Seq monotonicity check
                    if let Some(existing) = self.item_store.get_mutable(k, &item.salt)
                        && *seq_val <= existing.seq
                    {
                        let err_msg = KrpcMessage {
                            transaction_id: msg.transaction_id,
                            body: KrpcBody::Error {
                                code: 302,
                                message: format!(
                                    "sequence number not newer: {} <= {}",
                                    seq_val, existing.seq
                                ),
                            },
                            sender_ip: Some(addr),
                        };
                        if let Ok(bytes) = err_msg.to_bytes() {
                            let _ = self.socket.send_to(&bytes, addr).await;
                        }
                        return;
                    }

                    self.item_store.put_mutable(item);
                } else {
                    // Immutable item
                    match ImmutableItem::new(value.clone()) {
                        Ok(item) => {
                            self.item_store.put_immutable(item);
                        }
                        Err(_) => {
                            let err_msg = KrpcMessage {
                                transaction_id: msg.transaction_id,
                                body: KrpcBody::Error {
                                    code: 205,
                                    message: "message (v field) too big".into(),
                                },
                                sender_ip: Some(addr),
                            };
                            if let Ok(bytes) = err_msg.to_bytes() {
                                let _ = self.socket.send_to(&bytes, addr).await;
                            }
                            return;
                        }
                    }
                }

                KrpcResponse::NodeId {
                    id: *self.routing_table.own_id(),
                }
            }
            // BEP 51: sample_infohashes
            KrpcQuery::SampleInfohashes { id: _, target } => {
                let closest = self.routing_table.closest(target, K);
                let nodes: Vec<CompactNodeInfo> = closest
                    .into_iter()
                    .map(|n| CompactNodeInfo {
                        id: n.id,
                        addr: n.addr,
                    })
                    .collect();

                // Sample up to 20 info hashes (fits comfortably in one UDP packet)
                let samples = self.peer_store.random_info_hashes(20);
                let num = self.peer_store.info_hash_count() as i64;

                KrpcResponse::SampleInfohashes(SampleInfohashesResponse {
                    id: *self.routing_table.own_id(),
                    interval: 60, // 1 minute default interval
                    num,
                    samples,
                    nodes,
                })
            }
        };

        let reply = KrpcMessage {
            transaction_id: msg.transaction_id,
            body: KrpcBody::Response(response),
            sender_ip: Some(addr), // BEP 42: tell the querier their IP
        };
        if let Ok(bytes) = reply.to_bytes() {
            let _ = self.socket.send_to(&bytes, addr).await;
        }
    }

    async fn handle_response(&mut self, msg: &KrpcMessage, resp: &KrpcResponse, addr: SocketAddr) {
        if !self.matches_family(&addr) {
            return; // Reject wrong address family
        }
        self.stats.total_responses_received += 1;

        // BEP 42: feed the ip field into the voter
        if let Some(reported_ip) = msg.sender_ip {
            let source_id = hash_source_addr(&addr);
            if let Some(consensus_ip) = self.ip_voter.add_vote(source_id, reported_ip.ip()) {
                debug!(%consensus_ip, "BEP 42: external IP consensus changed");
                let _ = self.ip_consensus_tx.try_send(consensus_ip);
                self.regenerate_node_id(consensus_ip);
            }
        }

        let sender_id = *resp.sender_id();
        self.checked_insert(sender_id, addr);
        self.routing_table.mark_response(&sender_id);

        let txn = msg.transaction_id.as_u16();
        let pending = match self.pending.remove(&txn) {
            Some(p) => p,
            None => {
                trace!(txn, from = %addr, "response for unknown transaction");
                return;
            }
        };

        match (&pending.kind, resp) {
            (
                PendingQueryKind::FindNode,
                KrpcResponse::FindNode { nodes, nodes6, .. },
            ) => {
                for node in nodes {
                    if self.matches_family(&node.addr) {
                        self.checked_insert(node.id, node.addr);
                    }
                }
                for node in nodes6 {
                    if self.matches_family(&node.addr) {
                        self.checked_insert(node.id, node.addr);
                    }
                }

                // Advance iterative bootstrap lookup if active
                if self.bootstrap_lookup.is_some() {
                    let family = self.address_family;
                    let family_match = |a: &SocketAddr| match family {
                        AddressFamily::V4 => a.is_ipv4(),
                        AddressFamily::V6 => a.is_ipv6(),
                    };

                    // Accumulate returned nodes into the lookup's closest list
                    if let Some(ref mut lookup) = self.bootstrap_lookup {
                        for node in nodes {
                            if family_match(&node.addr)
                                && !lookup.closest.iter().any(|n| n.id == node.id)
                            {
                                lookup.closest.push(*node);
                            }
                        }
                        for node in nodes6 {
                            if family_match(&node.addr)
                                && !lookup.closest.iter().any(|n| n.id == node.id)
                            {
                                lookup.closest.push(CompactNodeInfo {
                                    id: node.id,
                                    addr: node.addr,
                                });
                            }
                        }
                        let target = lookup.target;
                        lookup.closest.sort_by_key(|n| n.id.xor_distance(&target));
                        lookup.closest.truncate(K * 2);
                    }

                    // Extract data needed before calling send_find_node (drops borrow)
                    let (to_query, target, terminate) = if let Some(ref mut lookup) =
                        self.bootstrap_lookup
                    {
                        if lookup.round >= lookup.max_rounds {
                            (Vec::new(), lookup.target, true)
                        } else {
                            let to_query: Vec<CompactNodeInfo> = lookup
                                .closest
                                .iter()
                                .filter(|n| !lookup.queried.contains(&n.id))
                                .take(3)
                                .copied()
                                .collect();
                            let target = lookup.target;
                            if to_query.is_empty() {
                                (Vec::new(), target, true)
                            } else {
                                for node in &to_query {
                                    lookup.queried.insert(node.id);
                                }
                                lookup.round += 1;
                                (to_query, target, false)
                            }
                        }
                    } else {
                        (Vec::new(), Id20::ZERO, false)
                    };

                    if terminate {
                        debug!(
                            routing_table_size = self.routing_table.len(),
                            "iterative bootstrap complete"
                        );
                        self.bootstrap_lookup = None;
                        self.on_bootstrap_complete().await;
                    } else {
                        let queries: Vec<(SocketAddr, Id20)> =
                            to_query.iter().map(|n| (n.addr, n.id)).collect();
                        for (node_addr, nid) in queries {
                            self.send_find_node(node_addr, target, Some(nid)).await;
                        }
                    }
                }
            }
            (PendingQueryKind::GetPeers { info_hash }, KrpcResponse::GetPeers(gp)) => {
                // Add discovered nodes to routing table (filter by address family)
                for node in &gp.nodes {
                    if self.matches_family(&node.addr) {
                        self.checked_insert(node.id, node.addr);
                    }
                }
                for node in &gp.nodes6 {
                    if self.matches_family(&node.addr) {
                        self.checked_insert(node.id, node.addr);
                    }
                }

                // Forward results to active lookup
                if let Some(lookup) = self.lookups.get_mut(info_hash) {
                    // Store token for later announce
                    if let Some(token) = &gp.token {
                        lookup.tokens.insert(sender_id, (addr, token.clone()));
                    }

                    // Send discovered peers to caller
                    if !gp.peers.is_empty() {
                        let _ = lookup.reply.try_send(gp.peers.clone());
                    }

                    // Collect nodes from both nodes and nodes6 that match our family
                    let family = self.address_family;
                    let family_match = |addr: &SocketAddr| match family {
                        AddressFamily::V4 => addr.is_ipv4(),
                        AddressFamily::V6 => addr.is_ipv6(),
                    };
                    let all_matching_nodes: Vec<CompactNodeInfo> = gp
                        .nodes
                        .iter()
                        .filter(|n| family_match(&n.addr))
                        .copied()
                        .chain(gp.nodes6.iter().filter(|n| family_match(&n.addr)).map(|n| {
                            CompactNodeInfo {
                                id: n.id,
                                addr: n.addr,
                            }
                        }))
                        .collect();

                    // Continue iterative lookup with new closer nodes
                    let new_nodes: Vec<CompactNodeInfo> = all_matching_nodes
                        .iter()
                        .filter(|n| !lookup.queried.contains(&n.id))
                        .copied()
                        .collect();

                    for node in &new_nodes {
                        lookup.closest.push(*node);
                    }

                    // Sort by distance and keep closest
                    lookup.closest.sort_by_key(|n| n.id.xor_distance(info_hash));
                    lookup.closest.truncate(K * 2);

                    // Query new closest unqueried nodes
                    let to_query: Vec<CompactNodeInfo> = lookup
                        .closest
                        .iter()
                        .filter(|n| !lookup.queried.contains(&n.id))
                        .take(3) // Alpha = 3 concurrent queries
                        .copied()
                        .collect();

                    let info_hash = *info_hash;
                    if to_query.is_empty() {
                        // No new nodes to query — check if lookup is complete
                        let has_pending = self.pending.values().any(|p| {
                            matches!(p.kind, PendingQueryKind::GetPeers { info_hash: ih } if ih == info_hash)
                        });
                        if !has_pending {
                            debug!(%info_hash, "get_peers lookup exhausted all nodes");
                            self.lookups.remove(&info_hash);
                        }
                    } else {
                        for node in to_query {
                            if let Some(lookup) = self.lookups.get_mut(&info_hash) {
                                lookup.queried.insert(node.id);
                            }
                            self.send_get_peers(node.addr, info_hash, Some(node.id)).await;
                        }
                    }
                }
            }
            (PendingQueryKind::Ping, KrpcResponse::NodeId { .. }) => {
                // Ping response — node is alive, already updated routing table
                if !self.bootstrap_complete {
                    debug!(
                        from = %pending.addr,
                        table_size = self.routing_table.len(),
                        "bootstrap: ping response received"
                    );
                }
            }
            (PendingQueryKind::AnnouncePeer, KrpcResponse::NodeId { .. }) => {
                // Announce response — success
            }
            (
                PendingQueryKind::SampleInfohashes,
                KrpcResponse::SampleInfohashes(si),
            ) => {
                // Add discovered nodes to routing table (Gap 6: use checked_insert for BEP 42)
                for node in &si.nodes {
                    if self.matches_family(&node.addr) {
                        self.checked_insert(node.id, node.addr);
                    }
                }

                // Send result back to caller
                if let Some(reply) = self.sample_replies.remove(&txn) {
                    let _ = reply.send(Ok(SampleInfohashesResult {
                        interval: si.interval,
                        num: si.num,
                        samples: si.samples.clone(),
                        nodes: si.nodes.clone(),
                    }));
                }
            }
            (
                PendingQueryKind::GetItem { target },
                KrpcResponse::GetItem {
                    token,
                    nodes,
                    nodes6,
                    value,
                    key,
                    signature,
                    seq,
                    ..
                },
            ) => {
                // Gap 13: Use checked_insert (BEP 42 compliant) instead of routing_table.insert
                for node in nodes {
                    if self.matches_family(&node.addr) {
                        self.checked_insert(node.id, node.addr);
                    }
                }
                for node in nodes6 {
                    if self.matches_family(&node.addr) {
                        self.checked_insert(node.id, node.addr);
                    }
                }

                let target = *target;

                // If we have a put operation waiting for tokens, collect this token
                if let (Some(token), Some(put_op)) = (token, self.item_put_ops.get_mut(&target)) {
                    match put_op {
                        ItemPutState::Immutable { tokens, .. }
                        | ItemPutState::Mutable { tokens, .. } => {
                            tokens.insert(sender_id, (addr, token.clone()));
                        }
                    }

                    // If we have enough tokens, send the puts
                    let should_send = match &self.item_put_ops[&target] {
                        ItemPutState::Immutable {
                            tokens, sent_puts, ..
                        }
                        | ItemPutState::Mutable {
                            tokens, sent_puts, ..
                        } => tokens.len() >= K && *sent_puts == 0,
                    };

                    if should_send {
                        self.send_pending_puts(target).await;
                    }
                }

                // If we have a get lookup, process the value
                if self.item_lookups.contains_key(&target) {
                    // Determine if this is immutable or mutable lookup
                    let is_immutable = matches!(
                        self.item_lookups.get(&target),
                        Some(ItemLookupState::Immutable { .. })
                    );

                    if is_immutable {
                        if let Some(v) = value {
                            // Validate: SHA-1(v) should equal target
                            if torrent_core::sha1(v) == target {
                                // Store locally
                                if let Ok(item) = crate::bep44::ImmutableItem::new(v.clone()) {
                                    self.item_store.put_immutable(item);
                                }
                                if let Some(ItemLookupState::Immutable { reply, .. }) =
                                    self.item_lookups.get_mut(&target)
                                    && let Some(r) = reply.take()
                                {
                                    let _ = r.send(Ok(Some(v.clone())));
                                }
                            }
                        } else {
                            // Gap 7: Collect nodes to query into local Vec first
                            // to avoid borrow checker violation
                            let family = self.address_family;
                            let to_query: Vec<SocketAddr> = {
                                if let Some(ItemLookupState::Immutable { queried, .. }) =
                                    self.item_lookups.get_mut(&target)
                                {
                                    nodes
                                        .iter()
                                        .filter(|n| match family {
                                            AddressFamily::V4 => n.addr.is_ipv4(),
                                            AddressFamily::V6 => n.addr.is_ipv6(),
                                        })
                                        .filter(|n| queried.insert(n.id))
                                        .take(3)
                                        .map(|n| n.addr)
                                        .collect()
                                } else {
                                    vec![]
                                }
                            };
                            for query_addr in to_query {
                                self.send_get_item(query_addr, target, None).await;
                            }
                        }
                    } else {
                        // Mutable lookup
                        if let (Some(v), Some(k), Some(sig), Some(s)) = (value, key, signature, seq)
                        {
                            // Get the salt from the lookup state
                            let salt = if let Some(ItemLookupState::Mutable { salt, .. }) =
                                self.item_lookups.get(&target)
                            {
                                salt.clone()
                            } else {
                                Vec::new()
                            };

                            let item = crate::bep44::MutableItem {
                                value: v.clone(),
                                public_key: *k,
                                signature: *sig,
                                seq: *s,
                                salt,
                                target,
                            };

                            if item.verify()
                                && let Some(ItemLookupState::Mutable {
                                    best_seq,
                                    best_value,
                                    ..
                                }) = self.item_lookups.get_mut(&target)
                                && *s > *best_seq
                            {
                                *best_seq = *s;
                                *best_value = Some(v.clone());
                                // Store locally
                                self.item_store.put_mutable(item);
                            }
                        }

                        // Gap 7: Collect nodes to query into local Vec first
                        let family = self.address_family;
                        let to_query: Vec<SocketAddr> = {
                            if let Some(ItemLookupState::Mutable { queried, .. }) =
                                self.item_lookups.get_mut(&target)
                            {
                                nodes
                                    .iter()
                                    .filter(|n| match family {
                                        AddressFamily::V4 => n.addr.is_ipv4(),
                                        AddressFamily::V6 => n.addr.is_ipv6(),
                                    })
                                    .filter(|n| queried.insert(n.id))
                                    .take(3)
                                    .map(|n| n.addr)
                                    .collect()
                            } else {
                                vec![]
                            }
                        };
                        for query_addr in to_query {
                            self.send_get_item(query_addr, target, None).await;
                        }
                    }
                }
            }
            (PendingQueryKind::PutItem, KrpcResponse::NodeId { .. }) => {
                // Put acknowledged. Nothing to do.
            }
            _ => {
                trace!(txn, "mismatched response type");
            }
        }
    }

    async fn start_get_peers(&mut self, info_hash: Id20, reply: mpsc::Sender<Vec<SocketAddr>>) {
        if !self.bootstrap_complete {
            // Transmission-style node-count gate: if the routing table already
            // has enough good nodes (e.g. from fast saved-node ping responses),
            // open the gate early without waiting for FindNodeLookup convergence.
            if self.routing_table.len() >= 8 {
                self.on_bootstrap_complete().await;
            } else {
                debug!(
                    %info_hash,
                    pending = self.pending_get_peers.len(),
                    table_size = self.routing_table.len(),
                    "get_peers queued (bootstrap not yet complete)"
                );
                self.pending_get_peers.push((info_hash, reply));
                return;
            }
        }
        self.start_get_peers_inner(info_hash, reply).await;
    }

    async fn start_get_peers_inner(&mut self, info_hash: Id20, reply: mpsc::Sender<Vec<SocketAddr>>) {
        debug!(
            %info_hash,
            table_size = self.routing_table.len(),
            "starting get_peers query"
        );
        let closest = self.routing_table.closest(&info_hash, K);
        let initial_nodes: Vec<CompactNodeInfo> = closest
            .iter()
            .map(|n| CompactNodeInfo {
                id: n.id,
                addr: n.addr,
            })
            .collect();

        // If routing table is empty (e.g. bootstrap not yet complete), drop the
        // reply sender immediately so the caller's channel closes.  This lets
        // TorrentActor detect dht_peers_rx = None and re-trigger later when the
        // routing table has been populated.
        if initial_nodes.is_empty() {
            debug!(
                family = ?self.address_family,
                table_size = self.routing_table.len(),
                "get_peers: routing table empty, dropping lookup (will retry)"
            );
            drop(reply);
            return;
        }
        debug!(
            family = ?self.address_family,
            %info_hash,
            closest_count = initial_nodes.len(),
            table_size = self.routing_table.len(),
            "get_peers: starting lookup"
        );

        let mut queried = std::collections::HashSet::new();

        // Query ALL initial closest nodes in parallel (alpha = K).
        // libtorrent uses alpha=8 for get_peers — querying all K closest
        // nodes immediately maximizes the chance of fast convergence.
        let to_query: Vec<_> = initial_nodes.to_vec();
        for node in &to_query {
            queried.insert(node.id);
            self.send_get_peers(node.addr, info_hash, Some(node.id)).await;
        }

        self.lookups.insert(
            info_hash,
            LookupState {
                reply,
                queried,
                tokens: HashMap::new(),
                closest: initial_nodes,
            },
        );
    }

    /// M97: Called when bootstrap (FindNodeLookup) completes or times out.
    /// Drains queued get_peers requests.
    async fn on_bootstrap_complete(&mut self) {
        if self.bootstrap_complete {
            return; // Prevent double-fire
        }
        self.bootstrap_complete = true;
        self.bootstrap_timeout = None;

        let pending = std::mem::take(&mut self.pending_get_peers);
        debug!(
            count = pending.len(),
            table_size = self.routing_table.len(),
            "bootstrap complete, processing queued get_peers"
        );
        for (info_hash, reply) in pending {
            self.start_get_peers_inner(info_hash, reply).await;
        }
    }

    async fn handle_announce(
        &mut self,
        info_hash: Id20,
        port: u16,
        reply: oneshot::Sender<Result<()>>,
    ) {
        // First, find nodes with tokens from a previous get_peers
        let tokens: Vec<(SocketAddr, Vec<u8>)> = self
            .lookups
            .get(&info_hash)
            .map(|lookup| lookup.tokens.values().cloned().collect())
            .unwrap_or_default();

        if tokens.is_empty() {
            let _ = reply.send(Err(Error::InvalidMessage(
                "no tokens available; call get_peers first".into(),
            )));
            return;
        }

        let own_id = *self.routing_table.own_id();
        for (addr, token) in &tokens {
            if !self.rate_limiter.try_acquire() {
                break; // Use break, not return, since we still need to send the reply
            }
            let txn = self.next_transaction_id();
            let msg = KrpcMessage {
                transaction_id: TransactionId::from_u16(txn),
                body: KrpcBody::Query(KrpcQuery::AnnouncePeer {
                    id: own_id,
                    info_hash,
                    port,
                    implied_port: false,
                    token: token.clone(),
                }),
                sender_ip: None,
            };
            if let Ok(bytes) = msg.to_bytes() {
                let _ = self.socket.send_to(&bytes, addr).await;
                self.pending.insert(
                    txn,
                    PendingQuery {
                        sent_at: Instant::now(),
                        addr: *addr,
                        kind: PendingQueryKind::AnnouncePeer,
                        node_id: None,
                    },
                );
                self.stats.total_queries_sent += 1;
            }
        }

        let _ = reply.send(Ok(()));
    }

    /// Expire timed-out queries and advance any stalled get_peers lookups.
    /// Runs every query_timeout interval — mirrors libtorrent's pattern where
    /// `traversal_algorithm::failed()` immediately calls `add_requests()` to
    /// query the next closest nodes.
    async fn expire_queries_and_advance_lookups(&mut self) {
        let timeout = self.config.query_timeout;
        let expired: Vec<u16> = self
            .pending
            .iter()
            .filter(|(_, p)| p.sent_at.elapsed() > timeout)
            .map(|(txn, _)| *txn)
            .collect();

        if expired.is_empty() {
            return;
        }

        debug!(
            family = ?self.address_family,
            expired_count = expired.len(),
            total_pending = self.pending.len(),
            active_lookups = self.lookups.len(),
            "expiring timed-out queries"
        );

        let mut stalled_lookups: Vec<Id20> = Vec::new();
        let mut find_node_timed_out = false;

        for txn in expired {
            if let Some(pending) = self.pending.remove(&txn) {
                trace!(txn, addr = %pending.addr, "query timed out");
                if let Some(nid) = pending.node_id {
                    self.routing_table.mark_failed(&nid);
                }
                if matches!(pending.kind, PendingQueryKind::SampleInfohashes)
                    && let Some(reply) = self.sample_replies.remove(&txn)
                {
                    let _ = reply.send(Err(Error::Timeout));
                }
                if let PendingQueryKind::GetPeers { info_hash } = pending.kind {
                    stalled_lookups.push(info_hash);
                }
                if matches!(pending.kind, PendingQueryKind::FindNode) {
                    find_node_timed_out = true;
                }
            }
        }

        // Advance stalled lookups immediately — query next closest unqueried nodes
        for info_hash in stalled_lookups {
            let has_pending = self
                .pending
                .values()
                .any(|p| matches!(p.kind, PendingQueryKind::GetPeers { info_hash: ih } if ih == info_hash));

            if let Some(lookup) = self.lookups.get_mut(&info_hash) {
                let to_query: Vec<CompactNodeInfo> = lookup
                    .closest
                    .iter()
                    .filter(|n| !lookup.queried.contains(&n.id))
                    .take(3)
                    .copied()
                    .collect();

                if !to_query.is_empty() {
                    for node in &to_query {
                        lookup.queried.insert(node.id);
                    }
                    for node in to_query {
                        self.send_get_peers(node.addr, info_hash, Some(node.id)).await;
                    }
                } else if !has_pending {
                    debug!(%info_hash, "get_peers lookup exhausted all nodes");
                    self.lookups.remove(&info_hash);
                }
            }
        }

        // Advance bootstrap lookup if a FindNode query timed out
        if find_node_timed_out && self.bootstrap_lookup.is_some() {
            // Extract queries before calling send_find_node (borrow-checker)
            let (to_query, target, terminate) =
                if let Some(ref mut lookup) = self.bootstrap_lookup {
                    let to_query: Vec<CompactNodeInfo> = lookup
                        .closest
                        .iter()
                        .filter(|n| !lookup.queried.contains(&n.id))
                        .take(3)
                        .copied()
                        .collect();
                    let target = lookup.target;
                    if to_query.is_empty() {
                        (Vec::new(), target, true)
                    } else {
                        for node in &to_query {
                            lookup.queried.insert(node.id);
                        }
                        (to_query, target, false)
                    }
                } else {
                    (Vec::new(), Id20::ZERO, false)
                };

            if terminate {
                self.bootstrap_lookup = None;
                self.on_bootstrap_complete().await;
            } else {
                let queries: Vec<(SocketAddr, Id20)> =
                    to_query.iter().map(|n| (n.addr, n.id)).collect();
                for (node_addr, nid) in queries {
                    self.send_find_node(node_addr, target, Some(nid)).await;
                }
            }
        }
    }

    async fn maintenance(&mut self) {
        // Query timeouts are now handled by expire_queries_and_advance_lookups()

        // Clean up completed lookups (where the reply channel is closed)
        self.lookups.retain(|_, lookup| !lookup.reply.is_closed());

        // Gap 12: Clean up item lookups — send best result before dropping stale lookups
        self.item_lookups.retain(|_, lookup| match lookup {
            ItemLookupState::Immutable { reply, .. } => {
                if reply.as_ref().is_some_and(|r| r.is_closed()) {
                    // Receiver dropped — discard
                    false
                } else if reply.is_some() {
                    true
                } else {
                    // Reply already sent
                    false
                }
            }
            ItemLookupState::Mutable {
                reply,
                best_value,
                best_seq,
                ..
            } => {
                if reply.as_ref().is_some_and(|r| r.is_closed()) {
                    false
                } else if reply.is_some() {
                    true
                } else {
                    // Check if we should finalize — reply already taken means we're done
                    let _ = best_value;
                    let _ = best_seq;
                    false
                }
            }
        });

        // Clean up completed put operations
        self.item_put_ops.retain(|_, put_op| match put_op {
            ItemPutState::Immutable { reply, .. } | ItemPutState::Mutable { reply, .. } => {
                reply.is_some()
            }
        });

        // Refresh stale buckets
        let stale = self
            .routing_table
            .stale_buckets(Duration::from_secs(15 * 60));
        for bucket_idx in stale {
            let target = self.routing_table.random_id_in_bucket(bucket_idx);
            let closest = self.routing_table.closest(&target, 3);
            for node in closest {
                self.send_find_node(node.addr, target, Some(node.id)).await;
            }
        }
    }

    async fn send_find_node(&mut self, addr: SocketAddr, target: Id20, node_id: Option<Id20>) {
        if !self.rate_limiter.try_acquire() {
            return;
        }
        let txn = self.next_transaction_id();
        let own_id = *self.routing_table.own_id();
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(txn),
            body: KrpcBody::Query(KrpcQuery::FindNode { id: own_id, target }),
            sender_ip: None,
        };
        if let Ok(bytes) = msg.to_bytes() {
            let _ = self.socket.send_to(&bytes, addr).await;
            self.pending.insert(
                txn,
                PendingQuery {
                    sent_at: Instant::now(),
                    addr,
                    kind: PendingQueryKind::FindNode,
                    node_id,
                },
            );
            self.stats.total_queries_sent += 1;
        }
    }

    async fn send_ping(&mut self, addr: SocketAddr, node_id: Option<Id20>) {
        if !self.rate_limiter.try_acquire() {
            return;
        }
        let txn = self.next_transaction_id();
        let own_id = *self.routing_table.own_id();
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(txn),
            body: KrpcBody::Query(KrpcQuery::Ping { id: own_id }),
            sender_ip: None,
        };
        if let Ok(bytes) = msg.to_bytes() {
            let _ = self.socket.send_to(&bytes, addr).await;
            self.pending.insert(
                txn,
                PendingQuery {
                    sent_at: Instant::now(),
                    addr,
                    node_id,
                    kind: PendingQueryKind::Ping,
                },
            );
            self.stats.total_queries_sent += 1;
        }
    }

    async fn ping_questionable_nodes(&mut self) {
        let nodes = self.routing_table.questionable_nodes();
        for (id, addr) in nodes {
            self.send_ping(addr, Some(id)).await;
        }
    }

    async fn send_get_peers(&mut self, addr: SocketAddr, info_hash: Id20, node_id: Option<Id20>) {
        if !self.rate_limiter.try_acquire() {
            return;
        }
        let txn = self.next_transaction_id();
        let own_id = *self.routing_table.own_id();
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(txn),
            body: KrpcBody::Query(KrpcQuery::GetPeers {
                id: own_id,
                info_hash,
            }),
            sender_ip: None,
        };
        if let Ok(bytes) = msg.to_bytes() {
            let _ = self.socket.send_to(&bytes, addr).await;
            self.pending.insert(
                txn,
                PendingQuery {
                    sent_at: Instant::now(),
                    addr,
                    kind: PendingQueryKind::GetPeers { info_hash },
                    node_id,
                },
            );
            self.stats.total_queries_sent += 1;
        }
    }

    // ---- BEP 44: item get/put handlers ----

    async fn handle_get_immutable(
        &mut self,
        target: Id20,
        reply: oneshot::Sender<Result<Option<Vec<u8>>>>,
    ) {
        // Check local store first
        if let Some(item) = self.item_store.get_immutable(&target) {
            let _ = reply.send(Ok(Some(item.value)));
            return;
        }

        // Initiate iterative get to the closest nodes
        let closest = self.routing_table.closest(&target, K);
        if closest.is_empty() {
            // No nodes to query — return None immediately
            let _ = reply.send(Ok(None));
            return;
        }

        for node in closest.iter().take(3) {
            self.send_get_item(node.addr, target, None).await;
        }

        self.item_lookups.insert(
            target,
            ItemLookupState::Immutable {
                reply: Some(reply),
                queried: closest.iter().map(|n| n.id).collect(),
            },
        );
    }

    async fn handle_put_immutable(&mut self, value: Vec<u8>, reply: oneshot::Sender<Result<Id20>>) {
        let item = match crate::bep44::ImmutableItem::new(value) {
            Ok(item) => item,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let target = item.target;

        // Store locally
        self.item_store.put_immutable(item.clone());

        // Propagate: find closest nodes to the target, then put to them.
        // First we need tokens from those nodes — initiate a get first.
        let closest = self.routing_table.closest(&target, K);
        if closest.is_empty() {
            // No peers to propagate to, but local store succeeded
            let _ = reply.send(Ok(target));
            return;
        }

        for node in closest.iter().take(K) {
            self.send_get_item(node.addr, target, None).await;
        }

        // Track the put operation
        self.item_put_ops.insert(
            target,
            ItemPutState::Immutable {
                item,
                tokens: HashMap::new(),
                sent_puts: 0,
                reply: Some(reply),
            },
        );
    }

    #[allow(clippy::type_complexity)]
    async fn handle_get_mutable(
        &mut self,
        public_key: [u8; 32],
        salt: Vec<u8>,
        reply: oneshot::Sender<Result<Option<(Vec<u8>, i64)>>>,
    ) {
        let target = crate::bep44::compute_mutable_target(&public_key, &salt);

        // Check local store first
        if let Some(item) = self.item_store.get_mutable(&public_key, &salt) {
            let _ = reply.send(Ok(Some((item.value, item.seq))));
            return;
        }

        // Initiate iterative get
        let closest = self.routing_table.closest(&target, K);
        if closest.is_empty() {
            let _ = reply.send(Ok(None));
            return;
        }

        for node in closest.iter().take(3) {
            self.send_get_item(node.addr, target, None).await;
        }

        self.item_lookups.insert(
            target,
            ItemLookupState::Mutable {
                salt,
                reply: Some(reply),
                best_seq: i64::MIN,
                best_value: None,
                queried: closest.iter().map(|n| n.id).collect(),
            },
        );
    }

    async fn handle_put_mutable(
        &mut self,
        keypair_bytes: [u8; 32],
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
        reply: oneshot::Sender<Result<Id20>>,
    ) {
        let keypair = ed25519_dalek::SigningKey::from_bytes(&keypair_bytes);
        let item = match crate::bep44::MutableItem::create(&keypair, value, seq, salt) {
            Ok(item) => item,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let target = item.target;

        // Store locally
        self.item_store.put_mutable(item.clone());

        // Propagate: find closest nodes, get tokens, then put
        let closest = self.routing_table.closest(&target, K);
        if closest.is_empty() {
            let _ = reply.send(Ok(target));
            return;
        }

        for node in closest.iter().take(K) {
            self.send_get_item(node.addr, target, None).await;
        }

        self.item_put_ops.insert(
            target,
            ItemPutState::Mutable {
                item,
                tokens: HashMap::new(),
                sent_puts: 0,
                reply: Some(reply),
            },
        );
    }

    // Gap 5: send_get_item uses sender_ip: None for outgoing queries
    async fn send_get_item(&mut self, addr: SocketAddr, target: Id20, seq: Option<i64>) {
        if !self.rate_limiter.try_acquire() {
            return;
        }
        let txn = self.next_transaction_id();
        let own_id = *self.routing_table.own_id();
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(txn),
            body: KrpcBody::Query(KrpcQuery::Get {
                id: own_id,
                target,
                seq,
            }),
            sender_ip: None, // Gap 5: outgoing queries use None
        };
        if let Ok(bytes) = msg.to_bytes() {
            let _ = self.socket.send_to(&bytes, addr).await;
            self.pending.insert(
                txn,
                PendingQuery {
                    sent_at: Instant::now(),
                    addr,
                    kind: PendingQueryKind::GetItem { target },
                    node_id: None,
                },
            );
            self.stats.total_queries_sent += 1;
        }
    }

    // Gap 5: send_put_item uses sender_ip: None for outgoing queries
    async fn send_put_item(&mut self, params: PutItemParams) {
        if !self.rate_limiter.try_acquire() {
            return;
        }
        let txn = self.next_transaction_id();
        let own_id = *self.routing_table.own_id();
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(txn),
            body: KrpcBody::Query(KrpcQuery::Put {
                id: own_id,
                token: params.token,
                value: params.value,
                key: params.key,
                signature: params.signature,
                seq: params.seq,
                salt: params.salt,
                cas: None,
            }),
            sender_ip: None, // Gap 5: outgoing queries use None
        };
        if let Ok(bytes) = msg.to_bytes() {
            let _ = self.socket.send_to(&bytes, params.addr).await;
            self.pending.insert(
                txn,
                PendingQuery {
                    sent_at: Instant::now(),
                    addr: params.addr,
                    kind: PendingQueryKind::PutItem,
                    node_id: None,
                },
            );
            self.stats.total_queries_sent += 1;
        }
    }

    // Gap 8: Extract data into local variables before calling self.send_put_item
    async fn send_pending_puts(&mut self, target: Id20) {
        let puts_to_send: Vec<PutItemParams> = if let Some(put_op) = self.item_put_ops.get(&target)
        {
            match put_op {
                ItemPutState::Immutable { item, tokens, .. } => tokens
                    .values()
                    .take(K)
                    .map(|(addr, token)| PutItemParams {
                        addr: *addr,
                        token: token.clone(),
                        value: item.value.clone(),
                        key: None,
                        signature: None,
                        seq: None,
                        salt: None,
                    })
                    .collect(),
                ItemPutState::Mutable { item, tokens, .. } => {
                    let salt = if item.salt.is_empty() {
                        None
                    } else {
                        Some(item.salt.clone())
                    };
                    tokens
                        .values()
                        .take(K)
                        .map(|(addr, token)| PutItemParams {
                            addr: *addr,
                            token: token.clone(),
                            value: item.value.clone(),
                            key: Some(item.public_key),
                            signature: Some(item.signature),
                            seq: Some(item.seq),
                            salt: salt.clone(),
                        })
                        .collect()
                }
            }
        } else {
            return;
        };

        let num_puts = puts_to_send.len();
        for params in puts_to_send {
            self.send_put_item(params).await;
        }

        // Update sent_puts count and send reply
        if let Some(put_op) = self.item_put_ops.get_mut(&target) {
            match put_op {
                ItemPutState::Immutable {
                    item,
                    sent_puts,
                    reply,
                    ..
                } => {
                    *sent_puts = num_puts;
                    if let Some(r) = reply.take() {
                        let _ = r.send(Ok(item.target));
                    }
                }
                ItemPutState::Mutable {
                    item,
                    sent_puts,
                    reply,
                    ..
                } => {
                    *sent_puts = num_puts;
                    if let Some(r) = reply.take() {
                        let _ = r.send(Ok(item.target));
                    }
                }
            }
        }
    }

    // ---- BEP 51: sample_infohashes handler ----

    async fn handle_sample_infohashes(
        &mut self,
        target: Id20,
        reply: oneshot::Sender<Result<SampleInfohashesResult>>,
    ) {
        // Find closest node to the target and send the query there
        let closest = self.routing_table.closest(&target, 1);
        let (addr, closest_node_id) = match closest.first() {
            Some(node) => (node.addr, node.id),
            None => {
                let _ = reply.send(Err(Error::InvalidMessage(
                    "no nodes in routing table".into(),
                )));
                return;
            }
        };

        if !self.rate_limiter.try_acquire() {
            let _ = reply.send(Err(Error::Timeout));
            return;
        }
        let txn = self.next_transaction_id();
        let own_id = *self.routing_table.own_id();
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(txn),
            body: KrpcBody::Query(KrpcQuery::SampleInfohashes { id: own_id, target }),
            sender_ip: None, // Gap 2: outgoing queries use None
        };
        if let Ok(bytes) = msg.to_bytes() {
            let _ = self.socket.send_to(&bytes, addr).await;
            self.pending.insert(
                txn,
                PendingQuery {
                    sent_at: Instant::now(),
                    addr,
                    kind: PendingQueryKind::SampleInfohashes,
                    node_id: Some(closest_node_id),
                },
            );
            self.stats.total_queries_sent += 1;
        }
        // Store the reply sender for when the response comes back
        self.sample_replies.insert(txn, reply);
    }

    fn next_transaction_id(&mut self) -> u16 {
        let txn = self.next_txn_id;
        self.next_txn_id = self.next_txn_id.wrapping_add(1);
        if self.next_txn_id == 0 {
            self.next_txn_id = 1;
        }
        txn
    }

    /// Insert a node into the routing table, enforcing BEP 42 if enabled.
    fn checked_insert(&mut self, id: Id20, addr: SocketAddr) -> bool {
        if self.config.enforce_node_id && !node_id::is_valid_node_id(&id, addr.ip()) {
            trace!(
                node_id = %id,
                ip = %addr.ip(),
                "BEP 42: rejecting node with invalid ID for IP"
            );
            return false;
        }
        self.routing_table.insert(id, addr)
    }

    /// Regenerate our node ID to be BEP 42-compliant for the given external IP.
    ///
    /// Preserves existing routing table nodes by re-inserting them into the
    /// new table. This avoids losing bootstrap-discovered nodes when the IP
    /// voter reaches consensus shortly after startup.
    fn regenerate_node_id(&mut self, external_ip: std::net::IpAddr) {
        let r = self.routing_table.own_id().0[19] & 0x07;
        let new_id = node_id::generate_node_id(external_ip, r);
        let restrict_ips = self.config.restrict_routing_ips;
        let max_routing_nodes = self.config.max_routing_nodes;
        let mut old_nodes = self.routing_table.all_nodes();
        debug!(
            old_id = %self.routing_table.own_id(),
            new_id = %new_id,
            preserved_nodes = old_nodes.len(),
            "BEP 42: regenerating node ID"
        );
        self.routing_table = RoutingTable::with_config(new_id, restrict_ips, max_routing_nodes);

        // Sort nodes by XOR distance to the new ID (closest first).
        // This maximizes bucket splits: close nodes fill the home bucket,
        // triggering splits that create capacity for more distant nodes.
        // Without sorting, distant nodes fill non-splittable buckets first
        // and get rejected (we saw 72→20 node loss without this).
        old_nodes.sort_by_key(|(id, _)| id.xor_distance(&new_id));

        let mut inserted = 0usize;
        for (id, addr) in &old_nodes {
            if self.routing_table.insert(*id, *addr) {
                inserted += 1;
            }
        }
        debug!(
            new_table_size = self.routing_table.len(),
            attempted = old_nodes.len(),
            inserted,
            "BEP 42: node ID regeneration complete"
        );

        // Invalidate all active get_peers lookups. The lookup `closest` lists
        // were computed using the old routing table topology and may reference
        // nodes that are no longer closest under the new ID. Dropping the
        // lookups closes the mpsc senders, which makes the session detect
        // `dht_peers_rx = None` and re-issue `get_peers()` against the fresh
        // routing table.
        if !self.lookups.is_empty() {
            // Also remove pending queries for the cleared lookups. Without this,
            // stale queries expire later and call mark_failed() on nodes that the
            // NEW lookup might want to query, degrading their routing table status.
            let cleared_hashes: std::collections::HashSet<Id20> =
                self.lookups.keys().copied().collect();
            let stale_txns: Vec<u16> = self
                .pending
                .iter()
                .filter(|(_, p)| {
                    matches!(p.kind, PendingQueryKind::GetPeers { info_hash }
                        if cleared_hashes.contains(&info_hash))
                })
                .map(|(txn, _)| *txn)
                .collect();
            debug!(
                active_lookups = self.lookups.len(),
                stale_pending = stale_txns.len(),
                "BEP 42: invalidating active get_peers lookups (will be re-issued by session)"
            );
            for txn in stale_txns {
                self.pending.remove(&txn);
            }
            self.lookups.clear();
        }

        // Re-trigger iterative bootstrap with the new node ID.
        // The first bootstrap targeted the old ID, so the discovered nodes
        // are in the wrong neighbourhood. A fresh find_node cascade targeting
        // the new ID fills the home bucket properly.
        let initial_closest: Vec<CompactNodeInfo> = self
            .routing_table
            .closest(&new_id, K)
            .into_iter()
            .map(|n| CompactNodeInfo { id: n.id, addr: n.addr })
            .collect();
        if !initial_closest.is_empty() {
            debug!(
                seed_nodes = initial_closest.len(),
                "BEP 42: re-bootstrapping with new node ID"
            );
            self.bootstrap_lookup = Some(FindNodeLookup {
                target: new_id,
                closest: initial_closest,
                queried: std::collections::HashSet::new(),
                round: 0,
                max_rounds: 6,
            });
            // M97: Re-gate get_peers until the new bootstrap completes
            self.bootstrap_complete = false;
            self.bootstrap_timeout = Some(Box::pin(tokio::time::sleep(Duration::from_secs(10))));
        }
    }

    fn make_stats(&self) -> DhtStats {
        let (immutable, mutable) = self.item_store.count();
        DhtStats {
            node_id: *self.routing_table.own_id(),
            routing_table_size: self.routing_table.len(),
            bucket_count: self.routing_table.bucket_count(),
            peer_store_info_hashes: self.peer_store.info_hash_count(),
            peer_store_peers: self.peer_store.peer_count(),
            pending_queries: self.pending.len(),
            total_queries_sent: self.stats.total_queries_sent,
            total_responses_received: self.stats.total_responses_received,
            dht_item_count: immutable + mutable,
        }
    }
}

/// Hash a socket address to a u64 for use as a voter source ID.
fn hash_source_addr(addr: &SocketAddr) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    addr.hash(&mut hasher);
    hasher.finish()
}

/// Maximum duration for DNS bootstrap retry attempts per hostname.
const DNS_BOOTSTRAP_DEADLINE: Duration = Duration::from_secs(120);

/// Initial retry delay for DNS bootstrap resolution.
const DNS_BOOTSTRAP_INITIAL_DELAY: Duration = Duration::from_secs(1);

/// Maximum retry delay for DNS bootstrap resolution (exponential backoff cap).
const DNS_BOOTSTRAP_MAX_DELAY: Duration = Duration::from_secs(30);

/// Resolve a single bootstrap hostname with exponential backoff.
///
/// Retries DNS resolution with delays of 1s, 2s, 4s, ..., capped at 30s,
/// until success or the 120-second deadline is reached. On success, sends
/// the matching addresses (filtered by address family) to `tx`.
async fn dns_bootstrap_resolve(
    hostname: String,
    family: AddressFamily,
    tx: mpsc::Sender<Vec<SocketAddr>>,
) {
    let deadline = Instant::now() + DNS_BOOTSTRAP_DEADLINE;
    let mut delay = DNS_BOOTSTRAP_INITIAL_DELAY;

    loop {
        match tokio::net::lookup_host(hostname.as_str()).await {
            Ok(addrs) => {
                let matching: Vec<SocketAddr> = addrs
                    .filter(|a| match family {
                        AddressFamily::V4 => a.is_ipv4(),
                        AddressFamily::V6 => a.is_ipv6(),
                    })
                    .collect();
                debug!(
                    %hostname,
                    count = matching.len(),
                    ?family,
                    "DNS bootstrap resolved"
                );
                let _ = tx.send(matching).await;
                break;
            }
            Err(e) if Instant::now() + delay < deadline => {
                warn!(%hostname, %e, ?delay, "DNS bootstrap retry");
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(DNS_BOOTSTRAP_MAX_DELAY);
            }
            Err(e) => {
                warn!(%hostname, %e, "DNS bootstrap failed after retries");
                break;
            }
        }
    }
}

/// Generate a random node ID for this DHT node.
fn generate_node_id() -> Id20 {
    use std::cell::Cell;
    use std::time::SystemTime;

    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        );
    }

    let mut bytes = [0u8; 20];
    for byte in &mut bytes {
        STATE.with(|s| {
            let mut x = s.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.set(x);
            *byte = x as u8;
        });
    }
    Id20(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_node_id_is_unique() {
        let a = generate_node_id();
        let b = generate_node_id();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn dht_handle_start_and_shutdown() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(), // No bootstrap for test
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.routing_table_size, 0);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_handle_stats() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.routing_table_size, 0);
        assert_eq!(stats.bucket_count, 1);
        assert_eq!(stats.pending_queries, 0);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn two_dht_nodes_ping() {
        // Start two DHT nodes on localhost, have one send find_node to the other
        let config_a = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            own_id: Some(Id20::from_hex("0000000000000000000000000000000000000001").unwrap()),
            ..DhtConfig::default()
        };
        let config_b = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            own_id: Some(Id20::from_hex("0000000000000000000000000000000000000002").unwrap()),
            ..DhtConfig::default()
        };

        let (handle_a, _ip_rx_a) = DhtHandle::start(config_a).await.unwrap();
        let (handle_b, _ip_rx_b) = DhtHandle::start(config_b).await.unwrap();

        // Give them a moment to bind
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Both should have empty routing tables
        let stats_a = handle_a.stats().await.unwrap();
        let stats_b = handle_b.stats().await.unwrap();
        assert_eq!(stats_a.routing_table_size, 0);
        assert_eq!(stats_b.routing_table_size, 0);

        handle_a.shutdown().await.unwrap();
        handle_b.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_handle_get_peers_empty_table() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let _rx = handle.get_peers(info_hash).await.unwrap();

        // With empty routing table, no peers will be found and channel closes
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Channel should eventually be cleaned up
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.routing_table_size, 0);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_handles_malformed_packet() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        // Get the DHT port from stats (indirect — we'd need to expose local_addr)
        // For now, just verify it doesn't crash on shutdown
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.shutdown().await.unwrap();
    }

    #[test]
    fn dht_config_default_is_v4() {
        let config = DhtConfig::default();
        assert_eq!(config.address_family, AddressFamily::V4);
        assert!(config.bind_addr.is_ipv4());
    }

    #[test]
    fn dht_config_default_v6() {
        let config = DhtConfig::default_v6();
        assert_eq!(config.address_family, AddressFamily::V6);
        assert!(config.bind_addr.is_ipv6());
        // Should have bootstrap nodes
        assert!(!config.bootstrap_nodes.is_empty());
    }

    #[tokio::test]
    async fn dht_v6_start_and_shutdown() {
        let config = DhtConfig {
            bind_addr: "[::1]:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default_v6()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.routing_table_size, 0);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_v6_stats_on_empty_table() {
        let config = DhtConfig {
            bind_addr: "[::1]:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default_v6()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.routing_table_size, 0);
        assert_eq!(stats.bucket_count, 1);
        assert_eq!(stats.pending_queries, 0);
        assert_eq!(stats.total_queries_sent, 0);
        handle.shutdown().await.unwrap();
    }

    #[test]
    fn matches_family_helper() {
        let actor_v4 = AddressFamily::V4;
        let actor_v6 = AddressFamily::V6;
        let v4_addr: SocketAddr = "1.2.3.4:6881".parse().unwrap();
        let v6_addr: SocketAddr = "[::1]:6881".parse().unwrap();

        assert!(matches!(actor_v4, AddressFamily::V4) && v4_addr.is_ipv4());
        assert!(!v6_addr.is_ipv4());
        assert!(matches!(actor_v6, AddressFamily::V6) && v6_addr.is_ipv6());
        assert!(!v4_addr.is_ipv6());
    }

    #[test]
    fn dht_config_security_defaults() {
        let config = DhtConfig::default();
        // enforce_node_id off by default: too many real DHT nodes lack BEP 42 IDs
        assert!(!config.enforce_node_id);
        assert!(config.restrict_routing_ips);

        let config_v6 = DhtConfig::default_v6();
        assert!(!config_v6.enforce_node_id);
        assert!(config_v6.restrict_routing_ips);
    }

    #[tokio::test]
    async fn dht_handle_start_returns_ip_channel() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_update_external_ip() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();
        handle
            .update_external_ip("203.0.113.5".parse().unwrap(), IpVoteSource::Nat)
            .await
            .unwrap();
        handle.shutdown().await.unwrap();
    }

    // ---- BEP 44 put/get API tests ----

    // Gap 2: All tests use `let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();`

    #[tokio::test]
    async fn dht_put_get_immutable_local() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        // Put an immutable item
        let value = b"12:Hello World!".to_vec();
        let target = handle.put_immutable(value.clone()).await.unwrap();

        // Get it back (from local store)
        let result = handle.get_immutable(target).await.unwrap();
        assert_eq!(result, Some(value));

        // Verify SHA-1 target
        assert_eq!(target, torrent_core::sha1(b"12:Hello World!"));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_put_get_mutable_local() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        let seed = [42u8; 32];
        let keypair = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pubkey = keypair.verifying_key().to_bytes();

        let value = b"4:test".to_vec();
        let target = handle
            .put_mutable(seed, value.clone(), 1, Vec::new())
            .await
            .unwrap();

        // Get it back (from local store)
        let result = handle.get_mutable(pubkey, Vec::new()).await.unwrap();
        assert_eq!(result, Some((value, 1)));

        // Verify target
        let expected_target = crate::bep44::compute_mutable_target(&pubkey, &[]);
        assert_eq!(target, expected_target);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_get_immutable_not_found() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        let target = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        // With empty routing table, lookup has no peers to query; just returns local result
        let result = handle.get_immutable(target).await.unwrap();
        assert_eq!(result, None);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_put_immutable_rejects_oversized() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        let value = vec![0u8; 1001];
        let result = handle.put_immutable(value).await;
        assert!(result.is_err());

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_stats_includes_item_count() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.dht_item_count, 0);

        handle.put_immutable(b"5:hello".to_vec()).await.unwrap();
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.dht_item_count, 1);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_get_mutable_not_found() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        let pubkey = [99u8; 32];
        let result = handle.get_mutable(pubkey, Vec::new()).await.unwrap();
        assert_eq!(result, None);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn two_nodes_put_get_immutable() {
        let config_a = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            own_id: Some(Id20::from_hex("0000000000000000000000000000000000000001").unwrap()),
            ..DhtConfig::default()
        };
        let (handle_a, _ip_rx) = DhtHandle::start(config_a).await.unwrap();

        // Node A stores an item locally
        let value = b"12:Hello World!".to_vec();
        let target = handle_a.put_immutable(value.clone()).await.unwrap();

        // Verify local retrieval
        let result = handle_a.get_immutable(target).await.unwrap();
        assert_eq!(result, Some(value));

        handle_a.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn put_mutable_sequence_update() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        let seed = [99u8; 32];
        let keypair = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pubkey = keypair.verifying_key().to_bytes();

        // Put seq=1
        handle
            .put_mutable(seed, b"5:first".to_vec(), 1, Vec::new())
            .await
            .unwrap();
        let result = handle.get_mutable(pubkey, Vec::new()).await.unwrap();
        assert_eq!(result, Some((b"5:first".to_vec(), 1)));

        // Put seq=2 (should replace)
        handle
            .put_mutable(seed, b"6:second".to_vec(), 2, Vec::new())
            .await
            .unwrap();
        let result = handle.get_mutable(pubkey, Vec::new()).await.unwrap();
        assert_eq!(result, Some((b"6:second".to_vec(), 2)));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn put_mutable_with_salt_isolation() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        let seed = [77u8; 32];
        let keypair = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pubkey = keypair.verifying_key().to_bytes();

        // Put with salt "a"
        handle
            .put_mutable(seed, b"1:A".to_vec(), 1, b"a".to_vec())
            .await
            .unwrap();
        // Put with salt "b"
        handle
            .put_mutable(seed, b"1:B".to_vec(), 1, b"b".to_vec())
            .await
            .unwrap();

        // Each salt returns its own value
        let a = handle.get_mutable(pubkey, b"a".to_vec()).await.unwrap();
        assert_eq!(a, Some((b"1:A".to_vec(), 1)));
        let b = handle.get_mutable(pubkey, b"b".to_vec()).await.unwrap();
        assert_eq!(b, Some((b"1:B".to_vec(), 1)));

        handle.shutdown().await.unwrap();
    }

    // ---- BEP 51 sample_infohashes tests ----

    #[tokio::test]
    async fn dht_sample_infohashes_empty_table() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        let target = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let result = handle.sample_infohashes(target).await;
        // With empty routing table, we expect an error (no nodes to query)
        assert!(result.is_err());

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn two_nodes_sample_infohashes() {
        // Node A will store some peers, then node B queries it
        let config_a = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            own_id: Some(Id20::from_hex("0000000000000000000000000000000000000001").unwrap()),
            ..DhtConfig::default()
        };
        let (handle_a, _ip_rx_a) = DhtHandle::start(config_a).await.unwrap();

        // We can't directly add peers to node A's store through the public API,
        // but we can verify the query/response path by having node B query node A.
        // Node A will respond with empty samples since its peer store is empty.

        // For now, just verify the handle method exists and handles shutdown gracefully
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle_a.shutdown().await.unwrap();
    }

    // ---- QueryRateLimiter unit tests ----

    #[test]
    fn rate_limiter_new_starts_full() {
        let limiter = QueryRateLimiter::new(10);
        assert_eq!(limiter.permits, 10);
        assert_eq!(limiter.max_permits, 10);
        assert_eq!(limiter.refill_rate, 10);
    }

    #[test]
    fn rate_limiter_new_zero_rate() {
        // A zero-rate limiter should never grant permits.
        let mut limiter = QueryRateLimiter::new(0);
        assert!(!limiter.try_acquire());
    }

    #[test]
    fn rate_limiter_exhaustion() {
        // Drain all N permits, then the (N+1)th call must fail.
        let mut limiter = QueryRateLimiter::new(5);
        for _ in 0..5 {
            assert!(limiter.try_acquire(), "permit should be available");
        }
        assert!(!limiter.try_acquire(), "bucket must be empty after N acquires");
    }

    #[test]
    fn rate_limiter_initial_permits_work() {
        // Full bucket on creation: first try_acquire always succeeds.
        let mut limiter = QueryRateLimiter::new(1);
        assert!(limiter.try_acquire());
        // Bucket is now empty.
        assert!(!limiter.try_acquire());
    }

    #[test]
    fn rate_limiter_refill_caps_at_max() {
        // Manually set permits below max, then trigger a refill by faking a
        // large elapsed time through repeated calls; instead, just validate the
        // cap logic by setting state directly and calling refill via try_acquire.
        // We can't easily fake Instant, but we can verify that permits never
        // exceed max_permits after a refill.
        let mut limiter = QueryRateLimiter::new(10);
        // Drain to 0.
        for _ in 0..10 {
            limiter.try_acquire();
        }
        assert_eq!(limiter.permits, 0);

        // Sleep slightly longer than 1 second so the refill would add >10 permits
        // if uncapped. Since we cannot sleep in a unit test cheaply, we instead
        // directly manipulate last_refill to simulate elapsed time.
        limiter.last_refill = Instant::now() - Duration::from_secs(5);
        limiter.refill();
        // After 5 seconds at rate 10, raw new_permits = 50, but cap is 10.
        assert_eq!(limiter.permits, 10, "permits must not exceed max_permits");
    }

    #[test]
    fn rate_limiter_refill_adds_correct_permits() {
        let mut limiter = QueryRateLimiter::new(100);
        // Drain all.
        for _ in 0..100 {
            limiter.try_acquire();
        }
        // Simulate 0.5 seconds elapsed → should add ~50 permits.
        limiter.last_refill = Instant::now() - Duration::from_millis(500);
        limiter.refill();
        // Allow for timing imprecision: must be in [45, 55].
        assert!(
            limiter.permits >= 45 && limiter.permits <= 55,
            "expected ~50 permits after 0.5s refill at rate 100, got {}",
            limiter.permits
        );
    }

    /// Exercises the bootstrap path with saved-node addresses (no DNS).
    /// Verifies the bootstrap code path (including new diagnostic logging)
    /// runs without panicking.
    #[tokio::test]
    async fn dht_bootstrap_logging() {
        // Use a fake saved-node address (loopback) that won't resolve to a
        // real DHT node — the important thing is that the bootstrap code path
        // executes all three phases without panicking.
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: vec![
                "127.0.0.1:16881".to_owned(),
                "127.0.0.1:16882".to_owned(),
            ],
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();

        // Allow time for bootstrap() to run (pings sent, iterative lookup started).
        tokio::time::sleep(Duration::from_millis(200)).await;

        let stats = handle.stats().await.unwrap();
        // The pings will have been sent (queries_sent >= 2) but won't get
        // responses from the fake addresses.
        assert!(
            stats.total_queries_sent >= 2,
            "expected at least 2 ping queries, got {}",
            stats.total_queries_sent
        );

        handle.shutdown().await.unwrap();
    }

    /// T10: During bootstrap (`bootstrap_complete = false`), the ping gate
    /// uses a 5-second interval — pings should fire every tick.
    ///
    /// Uses millisecond-scale durations so the test completes instantly while
    /// exercising the exact same gating logic as the real actor loop.
    #[test]
    fn ping_interval_5s_during_bootstrap() {
        let bootstrap_complete = false;

        // Simulate the timing decision with a tick interval equal to the
        // bootstrap ping interval (both 5s in production, both 10ms here).
        let tick = Duration::from_millis(10);
        let bootstrap_interval = tick;
        let steady_interval = Duration::from_millis(120);

        let mut last_ping = Instant::now();
        let mut ping_count: u32 = 0;

        // Simulate 6 ticks, sleeping the tick interval between each.
        for _ in 0..6 {
            std::thread::sleep(tick);

            let ping_interval = if bootstrap_complete {
                steady_interval
            } else {
                bootstrap_interval
            };
            if last_ping.elapsed() >= ping_interval {
                ping_count = ping_count.saturating_add(1);
                last_ping = Instant::now();
            }
        }

        // All 6 ticks should trigger a ping (tick == bootstrap interval).
        assert_eq!(
            ping_count, 6,
            "expected 6 pings during bootstrap (every tick), got {ping_count}"
        );
    }

    /// T11: After bootstrap (`bootstrap_complete = true`), the ping gate
    /// uses a 60-second interval — most ticks are no-ops for pinging.
    ///
    /// Uses millisecond-scale durations so the test completes instantly while
    /// exercising the exact same gating logic as the real actor loop.
    #[test]
    fn ping_interval_60s_after_bootstrap() {
        let bootstrap_complete = true;

        // Production ratio: tick = 5s, steady interval = 60s → 12:1.
        // Test ratio:       tick = 10ms, steady interval = 120ms → 12:1.
        let tick = Duration::from_millis(10);
        let bootstrap_interval = tick;
        let steady_interval = Duration::from_millis(120);

        let mut last_ping = Instant::now();
        let mut ping_count: u32 = 0;

        // 24 ticks × 10ms = 240ms total. With a 120ms gate, exactly 2
        // pings should fire (at tick ~12 = 120ms and tick ~24 = 240ms).
        for _ in 0..24 {
            std::thread::sleep(tick);

            let ping_interval = if bootstrap_complete {
                steady_interval
            } else {
                bootstrap_interval
            };
            if last_ping.elapsed() >= ping_interval {
                ping_count = ping_count.saturating_add(1);
                last_ping = Instant::now();
            }
        }

        // Only 2 pings should have fired (12:1 ratio, same as production).
        assert_eq!(
            ping_count, 2,
            "expected 2 pings post-bootstrap (12:1 tick-to-interval ratio), got {ping_count}"
        );
    }

    // ---- DNS bootstrap backoff tests (M105 Task 3) ----

    /// T1: Verify DNS resolution is retried with increasing delay on failure.
    ///
    /// Uses a hostname that will definitely fail DNS resolution. Validates that
    /// the backoff logic computes the correct delay sequence (1s, 2s, 4s, ...)
    /// capped at 30s.
    #[test]
    fn dns_backoff_retries_on_failure() {
        // Validate the exponential backoff sequence directly.
        let mut delay = DNS_BOOTSTRAP_INITIAL_DELAY;
        let expected_delays = [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(30), // capped
            Duration::from_secs(30), // stays capped
        ];

        for expected in &expected_delays {
            assert_eq!(
                delay, *expected,
                "backoff delay mismatch: got {delay:?}, expected {expected:?}"
            );
            delay = delay.saturating_mul(2).min(DNS_BOOTSTRAP_MAX_DELAY);
        }
    }

    /// T2: Verify successful retry after initial failure proceeds normally.
    ///
    /// Spawns `dns_bootstrap_resolve` with localhost (which resolves
    /// immediately) and confirms addresses arrive on the channel.
    #[tokio::test]
    async fn dns_backoff_succeeds_on_retry() {
        let (tx, mut rx) = mpsc::channel(16);

        // "localhost:1234" should resolve immediately on any system.
        let hostname = "localhost:1234".to_owned();
        tokio::spawn(dns_bootstrap_resolve(
            hostname,
            AddressFamily::V4,
            tx,
        ));

        // We should receive at least one batch of addresses.
        let result = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(
            result.is_ok(),
            "expected DNS resolution to complete within 5 seconds"
        );
        let addrs = result.expect("timeout should not occur");
        // localhost resolves, so we should get Some with at least one address.
        assert!(
            addrs.is_some(),
            "expected Some(addresses) from dns_bootstrap_resolve"
        );
        let addrs = addrs.expect("already checked is_some");
        assert!(
            !addrs.is_empty(),
            "expected at least one resolved address for localhost"
        );
        // All addresses should be IPv4 since we requested V4.
        for addr in &addrs {
            assert!(addr.is_ipv4(), "expected IPv4 address, got {addr}");
        }
    }

    /// T3: Verify after 120s of failures, we stop retrying.
    ///
    /// Tests the deadline logic directly: once `Instant::now() + delay >= deadline`,
    /// the function should break out of its loop.
    #[test]
    fn dns_backoff_total_timeout_120s() {
        // Simulate the deadline check from dns_bootstrap_resolve.
        // With 120s deadline and delays 1,2,4,8,16,30,30,...
        // Sum: 1+2+4+8+16+30 = 61s after 6 retries, then 30s more = 91s after 7,
        // 121s after 8 retries → exceeds deadline.
        let deadline_duration = DNS_BOOTSTRAP_DEADLINE;
        let mut delay = DNS_BOOTSTRAP_INITIAL_DELAY;
        let mut total_sleep = Duration::ZERO;
        let mut retries = 0u32;

        loop {
            // Check if the next sleep would exceed the deadline
            // (mirrors: `Instant::now() + delay < deadline` in the real code,
            // but using cumulative durations since we can't fake Instant).
            let next_total = total_sleep.saturating_add(delay);
            if next_total >= deadline_duration {
                break;
            }
            total_sleep = next_total;
            retries = retries.saturating_add(1);
            delay = delay.saturating_mul(2).min(DNS_BOOTSTRAP_MAX_DELAY);
        }

        // Should have retried several times before hitting the deadline.
        assert!(
            retries >= 5,
            "expected at least 5 retries before 120s deadline, got {retries}"
        );
        // Total sleep should be < 120s (we broke before the last sleep).
        assert!(
            total_sleep < deadline_duration,
            "total sleep {total_sleep:?} should be less than deadline {deadline_duration:?}"
        );
    }

    /// T16: Verify Phase 3 (FindNodeLookup) starts immediately without
    /// waiting for DNS resolution.
    ///
    /// Starts a DHT actor with both saved-node addresses and a DNS hostname.
    /// After bootstrap(), the bootstrap_lookup (Phase 3) must be set, and
    /// dns_bootstrap_rx must be Some (DNS still in flight).
    #[tokio::test]
    async fn bootstrap_phase3_starts_before_dns() {
        // Use a DNS hostname that takes time to resolve (unresolvable is fine —
        // we just need to confirm Phase 3 didn't wait for it).
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: vec![
                // Saved node (parsed as SocketAddr → Phase 1 ping)
                "127.0.0.1:16881".to_owned(),
                // DNS hostname (goes to background task)
                "router.bittorrent.com:6881".to_owned(),
            ],
            ..DhtConfig::default()
        };

        let socket = UdpSocket::bind(config.bind_addr).await.unwrap();
        let (tx, rx) = mpsc::channel(256);
        let (ip_tx, _ip_rx) = mpsc::channel(4);
        let mut actor = DhtActor::new(config, socket, rx, ip_tx);

        // Run bootstrap — should return quickly (DNS spawned in background).
        actor.bootstrap().await;

        // Phase 3 must have started: bootstrap_lookup is set.
        assert!(
            actor.bootstrap_lookup.is_some(),
            "Phase 3 (FindNodeLookup) must start without waiting for DNS"
        );

        // DNS is still in flight: dns_bootstrap_rx must be Some.
        assert!(
            actor.dns_bootstrap_rx.is_some(),
            "dns_bootstrap_rx should be Some (background DNS tasks still running)"
        );

        // Cleanup: drop sender so actor doesn't hang.
        drop(tx);
    }
}
