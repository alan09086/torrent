//! TorrentActor (single-owner event loop) and TorrentHandle (cloneable public API).
//!
//! The actor owns all per-torrent state (chunk tracking, piece selection, choking,
//! peer management) and communicates with spawned PeerTasks via channels.
//! The handle is a thin wrapper around an mpsc sender.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use ferrite_core::{
    torrent_from_bytes, FilePriority, Id20, Lengths, Magnet, PeerId, TorrentMetaV1, DEFAULT_CHUNK_SIZE,
};
use ferrite_dht::DhtHandle;
use ferrite_storage::{Bitfield, ChunkTracker, MemoryStorage, TorrentStorage};

use crate::choker::{Choker, PeerInfo};
use crate::end_game::EndGame;
use crate::metadata::MetadataDownloader;
use crate::peer::run_peer;
use crate::peer_state::PeerState;
use crate::piece_selector::PieceSelector;
use crate::tracker_manager::TrackerManager;
use crate::types::{PeerCommand, PeerEvent, TorrentCommand, TorrentConfig, TorrentState, TorrentStats};

/// Shared global rate limiter bucket.
type SharedBucket = Arc<std::sync::Mutex<crate::rate_limiter::TokenBucket>>;

/// Cloneable handle for interacting with a running torrent.
#[derive(Clone)]
pub struct TorrentHandle {
    cmd_tx: mpsc::Sender<TorrentCommand>,
}

impl TorrentHandle {
    /// Create a torrent session from parsed .torrent metadata.
    ///
    /// Spawns the actor event loop and returns a handle for sending commands.
    pub(crate) async fn from_torrent(
        meta: TorrentMetaV1,
        storage: Arc<dyn TorrentStorage>,
        config: TorrentConfig,
        dht: Option<DhtHandle>,
        global_upload_bucket: Option<SharedBucket>,
        global_download_bucket: Option<SharedBucket>,
        slot_tuner: crate::slot_tuner::SlotTuner,
    ) -> crate::Result<Self> {
        let mut config = config;
        // BEP 27: private torrents disable DHT and PEX
        if meta.info.private == Some(1) {
            config.enable_dht = false;
            config.enable_pex = false;
        }

        let num_pieces = meta.info.num_pieces() as u32;
        let lengths = Lengths::new(
            meta.info.total_length(),
            meta.info.piece_length,
            DEFAULT_CHUNK_SIZE,
        );
        let chunk_tracker = ChunkTracker::new(lengths.clone());
        let piece_selector = PieceSelector::new(num_pieces);
        let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
        let file_priorities = vec![FilePriority::Normal; file_lengths.len()];
        let wanted_pieces = crate::piece_selector::build_wanted_pieces(
            &file_priorities, &file_lengths, &lengths,
        );

        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(256);
        let our_peer_id = PeerId::generate().0;

        // Bind listener for incoming connections
        let listener = TcpListener::bind(("127.0.0.1", config.listen_port))
            .await
            .ok();

        let tracker_manager =
            TrackerManager::from_torrent(&meta, our_peer_id, config.listen_port);

        let enable_dht = config.enable_dht;

        // Start DHT peer discovery if enabled and available
        let dht_peers_rx = if enable_dht {
            if let Some(ref dht) = dht {
                match dht.get_peers(meta.info_hash).await {
                    Ok(rx) => Some(rx),
                    Err(e) => {
                        warn!("failed to start DHT get_peers: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let upload_bucket = crate::rate_limiter::TokenBucket::new(config.upload_rate_limit);
        let download_bucket = crate::rate_limiter::TokenBucket::new(config.download_rate_limit);

        let actor = TorrentActor {
            config,
            info_hash: meta.info_hash,
            our_peer_id,
            state: TorrentState::Downloading,
            storage: Some(storage),
            chunk_tracker: Some(chunk_tracker),
            lengths: Some(lengths),
            num_pieces,
            piece_selector,
            in_flight: HashSet::new(),
            file_priorities,
            wanted_pieces,
            end_game: EndGame::new(),
            peers: HashMap::new(),
            available_peers: Vec::new(),
            choker: Choker::new(4),
            metadata_downloader: None,
            downloaded: 0,
            uploaded: 0,
            cmd_rx,
            event_tx,
            event_rx,
            meta: Some(meta),
            listener,
            tracker_manager,
            dht: if enable_dht { dht } else { None },
            dht_peers_rx,
            upload_bucket,
            download_bucket,
            global_upload_bucket,
            global_download_bucket,
            slot_tuner,
            upload_bytes_interval: 0,
        };

        tokio::spawn(actor.run());
        Ok(TorrentHandle { cmd_tx })
    }

    /// Create a torrent session from a magnet link (metadata fetched via BEP 9).
    pub(crate) async fn from_magnet(
        magnet: Magnet,
        config: TorrentConfig,
        dht: Option<DhtHandle>,
        global_upload_bucket: Option<SharedBucket>,
        global_download_bucket: Option<SharedBucket>,
        slot_tuner: crate::slot_tuner::SlotTuner,
    ) -> crate::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(256);
        let our_peer_id = PeerId::generate().0;

        let listener = TcpListener::bind(("127.0.0.1", config.listen_port))
            .await
            .ok();

        let tracker_manager =
            TrackerManager::empty(magnet.info_hash, our_peer_id, config.listen_port);

        let enable_dht = config.enable_dht;

        // Start DHT peer discovery if enabled and available
        let dht_peers_rx = if enable_dht {
            if let Some(ref dht) = dht {
                match dht.get_peers(magnet.info_hash).await {
                    Ok(rx) => Some(rx),
                    Err(e) => {
                        warn!("failed to start DHT get_peers: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let upload_bucket = crate::rate_limiter::TokenBucket::new(config.upload_rate_limit);
        let download_bucket = crate::rate_limiter::TokenBucket::new(config.download_rate_limit);

        let actor = TorrentActor {
            config,
            info_hash: magnet.info_hash,
            our_peer_id,
            state: TorrentState::FetchingMetadata,
            storage: None,
            chunk_tracker: None,
            lengths: None,
            num_pieces: 0,
            piece_selector: PieceSelector::new(0),
            in_flight: HashSet::new(),
            file_priorities: Vec::new(),
            wanted_pieces: Bitfield::new(0),
            end_game: EndGame::new(),
            peers: HashMap::new(),
            available_peers: Vec::new(),
            choker: Choker::new(4),
            metadata_downloader: Some(MetadataDownloader::new(magnet.info_hash)),
            downloaded: 0,
            uploaded: 0,
            cmd_rx,
            event_tx,
            event_rx,
            meta: None,
            listener,
            tracker_manager,
            dht: if enable_dht { dht } else { None },
            dht_peers_rx,
            upload_bucket,
            download_bucket,
            global_upload_bucket,
            global_download_bucket,
            slot_tuner,
            upload_bytes_interval: 0,
        };

        tokio::spawn(actor.run());
        Ok(TorrentHandle { cmd_tx })
    }

    /// Query current torrent statistics.
    pub async fn stats(&self) -> crate::Result<TorrentStats> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::Stats { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Add peer addresses to the available-peer pool.
    pub async fn add_peers(&self, peers: Vec<SocketAddr>) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::AddPeers { peers })
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Pause the torrent session (disconnect peers, announce Stopped).
    pub async fn pause(&self) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::Pause)
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Resume a paused torrent session (reconnect, announce Started).
    pub async fn resume(&self) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::Resume)
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Gracefully shut down the torrent session.
    pub async fn shutdown(&self) -> crate::Result<()> {
        // Best-effort send; if the channel is already closed, that's fine.
        let _ = self.cmd_tx.send(TorrentCommand::Shutdown).await;
        Ok(())
    }

    /// Snapshot current torrent state into libtorrent-compatible resume data.
    pub async fn save_resume_data(&self) -> crate::Result<ferrite_core::FastResumeData> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SaveResumeData { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the download priority for a specific file.
    pub async fn set_file_priority(
        &self,
        index: usize,
        priority: ferrite_core::FilePriority,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetFilePriority { index, priority, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the current per-file priorities.
    pub async fn file_priorities(&self) -> crate::Result<Vec<ferrite_core::FilePriority>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::FilePriorities { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }
}

// ---------------------------------------------------------------------------
// TorrentActor — internal single-owner event loop
// ---------------------------------------------------------------------------

struct TorrentActor {
    config: TorrentConfig,
    info_hash: Id20,
    our_peer_id: Id20,
    state: TorrentState,

    // Optional fields (None in magnet mode until metadata arrives)
    storage: Option<Arc<dyn TorrentStorage>>,
    chunk_tracker: Option<ChunkTracker>,
    lengths: Option<Lengths>,
    num_pieces: u32,

    // Piece management
    piece_selector: PieceSelector,
    in_flight: HashSet<u32>,
    file_priorities: Vec<FilePriority>,
    wanted_pieces: Bitfield,
    end_game: EndGame,

    // Peer management
    peers: HashMap<SocketAddr, PeerState>,
    available_peers: Vec<SocketAddr>,
    choker: Choker,

    // Metadata (for magnet links)
    metadata_downloader: Option<MetadataDownloader>,

    // Parsed torrent meta (for piece hash verification)
    meta: Option<TorrentMetaV1>,

    // Stats
    downloaded: u64,
    uploaded: u64,

    // Channels
    cmd_rx: mpsc::Receiver<TorrentCommand>,
    event_tx: mpsc::Sender<PeerEvent>,
    event_rx: mpsc::Receiver<PeerEvent>,

    // TCP listener for incoming peer connections
    listener: Option<TcpListener>,

    // Tracker management
    tracker_manager: TrackerManager,

    // DHT handle (shared, optional)
    dht: Option<DhtHandle>,
    dht_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,

    // Rate limiting (M14)
    upload_bucket: crate::rate_limiter::TokenBucket,
    download_bucket: crate::rate_limiter::TokenBucket,
    global_upload_bucket: Option<SharedBucket>,
    global_download_bucket: Option<SharedBucket>,
    slot_tuner: crate::slot_tuner::SlotTuner,
    upload_bytes_interval: u64,
}

impl TorrentActor {
    /// Main event loop.
    async fn run(mut self) {
        // Verify existing pieces on startup (resume support)
        self.verify_existing_pieces();

        let mut unchoke_interval = tokio::time::interval(Duration::from_secs(10));
        let mut optimistic_interval = tokio::time::interval(Duration::from_secs(30));
        let mut connect_interval = tokio::time::interval(Duration::from_secs(30));
        let mut refill_interval = tokio::time::interval(Duration::from_millis(100));

        // Don't fire immediately for the first tick
        unchoke_interval.tick().await;
        optimistic_interval.tick().await;
        connect_interval.tick().await;
        refill_interval.tick().await;

        // Initial tracker announce (Started event) — non-blocking, fires via select! arm
        // DHT announce
        if self.state == TorrentState::Downloading
            && self.config.enable_dht
            && let Some(ref dht) = self.dht
            && let Err(e) = dht.announce(self.info_hash, self.config.listen_port).await
        {
            warn!("DHT announce failed: {e}");
        }

        loop {
            tokio::select! {
                // Commands from handle
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(TorrentCommand::AddPeers { peers }) => {
                            self.handle_add_peers(peers);
                        }
                        Some(TorrentCommand::Stats { reply }) => {
                            let _ = reply.send(self.make_stats());
                        }
                        Some(TorrentCommand::Pause) => {
                            self.handle_pause().await;
                        }
                        Some(TorrentCommand::Resume) => {
                            self.handle_resume().await;
                        }
                        Some(TorrentCommand::SaveResumeData { reply }) => {
                            let result = self.build_resume_data();
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::SetFilePriority { index, priority, reply }) => {
                            let result = self.handle_set_file_priority(index, priority);
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::FilePriorities { reply }) => {
                            let _ = reply.send(self.file_priorities.clone());
                        }
                        Some(TorrentCommand::Shutdown) | None => {
                            self.shutdown_peers().await;
                            return;
                        }
                    }
                }
                // Events from peers
                event = self.event_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_peer_event(event).await;
                    }
                }
                // Accept incoming peers
                result = accept_incoming(&self.listener) => {
                    if let Ok((stream, addr)) = result {
                        self.spawn_peer_from_stream(addr, stream);
                    }
                }
                // Unchoke timer
                _ = unchoke_interval.tick() => {
                    self.update_peer_rates();
                    // Auto upload slot tuning
                    self.slot_tuner.observe(self.upload_bytes_interval);
                    self.upload_bytes_interval = 0;
                    self.choker.set_unchoke_slots(self.slot_tuner.current_slots());
                    self.run_choker().await;
                }
                // Optimistic unchoke timer
                _ = optimistic_interval.tick() => {
                    self.rotate_optimistic();
                }
                // Connect timer
                _ = connect_interval.tick() => {
                    self.try_connect_peers();
                    // Re-trigger DHT search if exhausted and we still need peers
                    if self.dht_peers_rx.is_none()
                        && self.config.enable_dht
                        && self.available_peers.is_empty()
                        && self.peers.len() < self.config.max_peers
                        && let Some(ref dht) = self.dht
                    {
                        match dht.get_peers(self.info_hash).await {
                            Ok(rx) => self.dht_peers_rx = Some(rx),
                            Err(e) => warn!("DHT re-search failed: {e}"),
                        }
                    }
                }
                // Tracker re-announce timer
                _ = async {
                    match self.tracker_manager.next_announce_in() {
                        Some(dur) => tokio::time::sleep(dur).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if self.state != TorrentState::FetchingMetadata {
                        let left = self.calculate_left();
                        let peers = self.tracker_manager.announce(
                            ferrite_tracker::AnnounceEvent::None,
                            self.uploaded,
                            self.downloaded,
                            left,
                        ).await;
                        if !peers.is_empty() {
                            debug!(count = peers.len(), "tracker returned peers");
                            self.handle_add_peers(peers);
                        }
                    }
                }
                // DHT peer discovery
                result = async {
                    match &mut self.dht_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Some(peers) => {
                            debug!(count = peers.len(), "DHT returned peers");
                            self.handle_add_peers(peers);
                        }
                        None => {
                            // DHT search exhausted
                            debug!("DHT peer search exhausted");
                            self.dht_peers_rx = None;
                        }
                    }
                }
                // Rate limiter refill (100ms)
                _ = refill_interval.tick() => {
                    let elapsed = Duration::from_millis(100);
                    self.upload_bucket.refill(elapsed);
                    self.download_bucket.refill(elapsed);
                }
            }
        }
    }

    // ----- Command handlers -----

    fn handle_add_peers(&mut self, peers: Vec<SocketAddr>) {
        for addr in peers {
            if !self.peers.contains_key(&addr) && !self.available_peers.contains(&addr) {
                self.available_peers.push(addr);
            }
        }
    }

    fn make_stats(&self) -> TorrentStats {
        let pieces_have = self
            .chunk_tracker
            .as_ref()
            .map(|ct| ct.bitfield().count_ones())
            .unwrap_or(0);

        TorrentStats {
            state: self.state,
            downloaded: self.downloaded,
            uploaded: self.uploaded,
            pieces_have,
            pieces_total: self.num_pieces,
            peers_connected: self.peers.len(),
            peers_available: self.available_peers.len(),
        }
    }

    /// Calculate bytes remaining for tracker announce.
    fn calculate_left(&self) -> u64 {
        match (&self.meta, &self.chunk_tracker) {
            (Some(meta), Some(ct)) => {
                let total = meta.info.total_length();
                let have = ct.bitfield().count_ones() as u64;
                let pieces_total = self.num_pieces as u64;
                if pieces_total == 0 {
                    total
                } else {
                    total.saturating_sub(have * (total / pieces_total))
                }
            }
            _ => 0,
        }
    }

    async fn shutdown_peers(&mut self) {
        // Best-effort announce Stopped to trackers
        let left = self.calculate_left();
        self.tracker_manager
            .announce_stopped(self.uploaded, self.downloaded, left)
            .await;

        for peer in self.peers.values() {
            let _ = peer.cmd_tx.send(PeerCommand::Shutdown).await;
        }
    }

    async fn handle_pause(&mut self) {
        if self.state == TorrentState::Paused || self.state == TorrentState::Stopped {
            return;
        }
        let prev_state = self.state;
        self.state = TorrentState::Paused;
        // Disconnect all peers
        for peer in self.peers.values() {
            let _ = peer.cmd_tx.send(PeerCommand::Shutdown).await;
        }
        self.peers.clear();
        // Announce Stopped to trackers
        if prev_state == TorrentState::Downloading
            || prev_state == TorrentState::Seeding
            || prev_state == TorrentState::Complete
        {
            let left = self.calculate_left();
            self.tracker_manager
                .announce_stopped(self.uploaded, self.downloaded, left)
                .await;
        }
    }

    async fn handle_resume(&mut self) {
        if self.state != TorrentState::Paused {
            return;
        }
        // Determine appropriate state
        if let Some(ref ct) = self.chunk_tracker
            && ct.bitfield().count_ones() == self.num_pieces
        {
            self.state = TorrentState::Seeding;
            self.choker.set_seed_mode(true);
        } else {
            self.state = TorrentState::Downloading;
            self.choker.set_seed_mode(false);
        }
        // Re-announce Started
        let left = self.calculate_left();
        let peers = self.tracker_manager.announce(
            ferrite_tracker::AnnounceEvent::Started,
            self.uploaded,
            self.downloaded,
            left,
        ).await;
        if !peers.is_empty() {
            self.handle_add_peers(peers);
        }
        self.try_connect_peers();
    }

    // ----- Event handlers -----

    async fn handle_peer_event(&mut self, event: PeerEvent) {
        match event {
            PeerEvent::Bitfield {
                peer_addr,
                bitfield,
            } => {
                self.piece_selector.add_peer_bitfield(&bitfield);
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.bitfield = bitfield;
                }
                // Check if we're interested in this peer
                self.maybe_express_interest(peer_addr).await;
                self.request_pieces_from_peer(peer_addr).await;
            }
            PeerEvent::Have { peer_addr, index } => {
                self.piece_selector.increment(index);
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.bitfield.set(index);
                }
                self.maybe_express_interest(peer_addr).await;
                self.request_pieces_from_peer(peer_addr).await;
            }
            PeerEvent::PieceData {
                peer_addr,
                index,
                begin,
                data,
            } => {
                self.handle_piece_data(peer_addr, index, begin, &data)
                    .await;
            }
            PeerEvent::PeerChoking {
                peer_addr,
                choking,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.peer_choking = choking;
                }
                if !choking {
                    // Unchoked — try requesting pieces
                    self.request_pieces_from_peer(peer_addr).await;
                }
            }
            PeerEvent::PeerInterested {
                peer_addr,
                interested,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.peer_interested = interested;
                }
            }
            PeerEvent::ExtHandshake {
                peer_addr,
                handshake,
            } => {
                // In FetchingMetadata mode, if we learn the metadata_size, start requesting
                if self.state == TorrentState::FetchingMetadata
                    && let Some(size) = handshake.metadata_size
                    && let Some(ref mut dl) = self.metadata_downloader
                {
                    dl.set_total_size(size);
                    let missing = dl.missing_pieces();
                    for piece in missing {
                        if let Some(peer) = self.peers.get(&peer_addr) {
                            let _ = peer
                                .cmd_tx
                                .send(PeerCommand::RequestMetadata { piece })
                                .await;
                        }
                    }
                }
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.ext_handshake = Some(handshake);
                }
            }
            PeerEvent::MetadataPiece {
                peer_addr: _,
                piece,
                data,
                total_size,
            } => {
                if let Some(ref mut dl) = self.metadata_downloader {
                    dl.set_total_size(total_size);
                    let complete = dl.piece_received(piece, data);
                    if complete {
                        self.try_assemble_metadata().await;
                    }
                }
            }
            PeerEvent::MetadataReject {
                peer_addr: _,
                piece: _,
            } => {
                // Could retry from a different peer; for now, ignore.
            }
            PeerEvent::PexPeers { new_peers } => {
                if self.config.enable_pex {
                    self.handle_add_peers(new_peers);
                }
            }
            PeerEvent::IncomingRequest {
                peer_addr,
                index,
                begin,
                length,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.incoming_requests.push((index, begin, length));
                }
                self.serve_incoming_requests().await;
            }
            PeerEvent::RejectRequest {
                peer_addr,
                index,
                begin,
                length: _,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr)
                    && let Some(pos) = peer
                        .pending_requests
                        .iter()
                        .position(|&(i, b, _)| i == index && b == begin)
                {
                    peer.pending_requests.swap_remove(pos);
                }
                debug!(index, %peer_addr, "request rejected by peer");
            }
            PeerEvent::AllowedFast {
                peer_addr,
                index,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.allowed_fast.insert(index);
                }
            }
            PeerEvent::SuggestPiece {
                peer_addr: _,
                index: _,
            } => {
                // Suggestion noted; current piece selector handles rarest-first
            }
            PeerEvent::Disconnected {
                peer_addr,
                reason,
            } => {
                debug!(%peer_addr, ?reason, "peer disconnected");
                if let Some(peer) = self.peers.remove(&peer_addr) {
                    self.piece_selector.remove_peer_bitfield(&peer.bitfield);
                    // Remove pieces that only this peer was downloading
                    let peer_pieces: HashSet<u32> = peer
                        .pending_requests
                        .iter()
                        .map(|&(idx, _, _)| idx)
                        .collect();
                    for piece_idx in peer_pieces {
                        let other_has = self.peers.values().any(|p| {
                            p.pending_requests.iter().any(|&(i, _, _)| i == piece_idx)
                        });
                        if !other_has {
                            self.in_flight.remove(&piece_idx);
                        }
                    }
                    if self.end_game.is_active() {
                        self.end_game.peer_disconnected(peer_addr);
                    }
                }
            }
        }
    }

    async fn handle_piece_data(
        &mut self,
        peer_addr: SocketAddr,
        index: u32,
        begin: u32,
        data: &[u8],
    ) {
        // Write chunk to storage
        if let Some(ref storage) = self.storage
            && let Err(e) = storage.write_chunk(index, begin, data)
        {
            warn!(index, begin, "failed to write chunk: {e}");
            return;
        }

        self.downloaded += data.len() as u64;

        if let Some(peer) = self.peers.get_mut(&peer_addr) {
            if let Some(pos) = peer
                .pending_requests
                .iter()
                .position(|&(i, b, _)| i == index && b == begin)
            {
                peer.pending_requests.swap_remove(pos);
            }
            peer.download_bytes_window += data.len() as u64;
        }

        // End-game: cancel this block on all other peers
        if self.end_game.is_active() {
            let cancels = self.end_game.block_received(index, begin, peer_addr);
            for (cancel_addr, ci, cb, cl) in cancels {
                if let Some(cancel_peer) = self.peers.get_mut(&cancel_addr) {
                    let _ = cancel_peer
                        .cmd_tx
                        .send(PeerCommand::Cancel {
                            index: ci,
                            begin: cb,
                            length: cl,
                        })
                        .await;
                    if let Some(pos) = cancel_peer
                        .pending_requests
                        .iter()
                        .position(|&(i, b, _)| i == ci && b == cb)
                    {
                        cancel_peer.pending_requests.swap_remove(pos);
                    }
                }
            }
        }

        // Track chunk completion
        let piece_complete = if let Some(ref mut ct) = self.chunk_tracker {
            ct.chunk_received(index, begin)
        } else {
            false
        };

        if piece_complete {
            self.verify_and_mark_piece(index).await;
        }

        // Try to request more from this peer
        self.request_pieces_from_peer(peer_addr).await;
    }

    fn verify_existing_pieces(&mut self) {
        let storage = match &self.storage {
            Some(s) => s,
            None => return,
        };
        let meta = match &self.meta {
            Some(m) => m,
            None => return,
        };

        let mut verified_count = 0u32;
        for i in 0..self.num_pieces {
            if let Some(expected) = meta.info.piece_hash(i as usize)
                && storage.verify_piece(i, &expected).unwrap_or(false)
            {
                if let Some(ref mut ct) = self.chunk_tracker {
                    ct.mark_verified(i);
                }
                verified_count += 1;
            }
        }

        if verified_count > 0 {
            info!(verified_count, total = self.num_pieces, "resumed with existing pieces");
        }

        if verified_count == self.num_pieces {
            self.state = TorrentState::Seeding;
            self.choker.set_seed_mode(true);
            info!("all pieces verified, starting as seeder");
        }
    }

    fn build_resume_data(&self) -> crate::Result<ferrite_core::FastResumeData> {
        let pieces_bytes = match &self.chunk_tracker {
            Some(ct) => ct.bitfield().as_bytes().to_vec(),
            None => Vec::new(),
        };

        let name = self
            .meta
            .as_ref()
            .map(|m| m.info.name.clone())
            .unwrap_or_default();

        let save_path = self.config.download_dir.to_string_lossy().into_owned();

        let mut rd = ferrite_core::FastResumeData::new(
            self.info_hash.as_bytes().to_vec(),
            name,
            save_path,
        );

        rd.pieces = pieces_bytes;

        rd.total_uploaded = self.uploaded as i64;
        rd.total_downloaded = self.downloaded as i64;

        rd.paused = if self.state == TorrentState::Paused { 1 } else { 0 };
        rd.seed_mode = if self.state == TorrentState::Seeding { 1 } else { 0 };

        // Collect tracker URLs from torrent metadata
        if let Some(ref meta) = self.meta {
            if let Some(ref announce_list) = meta.announce_list {
                rd.trackers = announce_list.clone();
            } else if let Some(ref announce) = meta.announce {
                rd.trackers = vec![vec![announce.clone()]];
            }
        }

        // Collect connected peer addresses as compact bytes
        let mut peers_v4 = Vec::new();
        for addr in self.peers.keys() {
            match addr {
                std::net::SocketAddr::V4(v4) => {
                    peers_v4.extend_from_slice(&v4.ip().octets());
                    peers_v4.extend_from_slice(&v4.port().to_be_bytes());
                }
                std::net::SocketAddr::V6(_) => {
                    // IPv6 peers would go into peers6 — skip for now
                }
            }
        }
        rd.peers = peers_v4;

        // Per-file priorities
        rd.file_priority = self
            .file_priorities
            .iter()
            .map(|&p| p as u8 as i64)
            .collect();

        Ok(rd)
    }

    fn handle_set_file_priority(
        &mut self,
        index: usize,
        priority: FilePriority,
    ) -> crate::Result<()> {
        if index >= self.file_priorities.len() {
            return Err(crate::Error::InvalidFileIndex {
                index,
                count: self.file_priorities.len(),
            });
        }

        self.file_priorities[index] = priority;

        // Rebuild wanted_pieces bitfield
        if let Some(ref meta) = self.meta {
            let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
            if let Some(ref lengths) = self.lengths {
                self.wanted_pieces = crate::piece_selector::build_wanted_pieces(
                    &self.file_priorities, &file_lengths, lengths,
                );
            }
        }

        Ok(())
    }

    async fn verify_and_mark_piece(&mut self, index: u32) {
        let expected_hash = self
            .meta
            .as_ref()
            .and_then(|m| m.info.piece_hash(index as usize));

        let verified = if let (Some(storage), Some(expected)) =
            (&self.storage, expected_hash)
        {
            storage.verify_piece(index, &expected).unwrap_or(false)
        } else {
            false
        };

        if verified {
            if let Some(ref mut ct) = self.chunk_tracker {
                ct.mark_verified(index);
            }
            self.in_flight.remove(&index);
            info!(index, "piece verified");

            // Broadcast Have to all peers
            for peer in self.peers.values() {
                let _ = peer.cmd_tx.send(PeerCommand::Have(index)).await;
            }

            // Check if download is complete
            if let Some(ref ct) = self.chunk_tracker
                && ct.bitfield().count_ones() == self.num_pieces
            {
                info!("download complete, transitioning to seeding");
                self.end_game.deactivate();
                self.state = TorrentState::Seeding;
                self.choker.set_seed_mode(true);
                // Announce completion to trackers
                let _ = self
                    .tracker_manager
                    .announce_completed(self.uploaded, self.downloaded)
                    .await;
            }
        } else {
            warn!(index, "piece hash verification failed, re-requesting");
            if let Some(ref mut ct) = self.chunk_tracker {
                ct.mark_failed(index);
            }
            self.in_flight.remove(&index);
            // Hash failure in end-game: deactivate and resume normal mode
            if self.end_game.is_active() {
                self.end_game.deactivate();
                info!(index, "end-game deactivated due to hash failure");
            }
        }
    }

    async fn try_assemble_metadata(&mut self) {
        let assembled = if let Some(ref dl) = self.metadata_downloader {
            dl.assemble_and_verify()
        } else {
            return;
        };

        match assembled {
            Ok(info_bytes) => {
                // Build torrent bytes wrapping the raw info dict into a minimal torrent
                // We need to parse it as a full torrent. The info_bytes is the raw bencoded
                // info dict. We'll build a minimal torrent around it.
                // Actually, torrent_from_bytes expects a full torrent dict.
                // Let's build one:
                let mut torrent_bytes = b"d4:info".to_vec();
                torrent_bytes.extend_from_slice(&info_bytes);
                torrent_bytes.push(b'e');

                match torrent_from_bytes(&torrent_bytes) {
                    Ok(meta) => {
                        let num_pieces = meta.info.num_pieces() as u32;
                        let lengths = Lengths::new(
                            meta.info.total_length(),
                            meta.info.piece_length,
                            DEFAULT_CHUNK_SIZE,
                        );

                        // Create storage (in-memory for now)
                        let storage: Arc<dyn TorrentStorage> =
                            Arc::new(MemoryStorage::new(lengths.clone()));

                        self.storage = Some(storage);
                        self.chunk_tracker = Some(ChunkTracker::new(lengths.clone()));
                        self.lengths = Some(lengths);
                        self.num_pieces = num_pieces;
                        self.piece_selector = PieceSelector::new(num_pieces);
                        let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
                        self.meta = Some(meta);
                        self.file_priorities = vec![FilePriority::Normal; file_lengths.len()];
                        self.wanted_pieces = crate::piece_selector::build_wanted_pieces(
                            &self.file_priorities, &file_lengths, self.lengths.as_ref().unwrap(),
                        );
                        self.state = TorrentState::Downloading;
                        self.metadata_downloader = None;

                        // Populate tracker manager with newly parsed metadata
                        if let Some(ref meta) = self.meta {
                            self.tracker_manager.set_metadata(meta);
                        }

                        info!("metadata assembled, switching to Downloading");
                    }
                    Err(e) => {
                        warn!("failed to parse assembled metadata: {e}");
                    }
                }
            }
            Err(e) => {
                warn!("metadata assembly failed: {e}");
            }
        }
    }

    // ----- Piece requesting -----

    async fn request_pieces_from_peer(&mut self, peer_addr: SocketAddr) {
        if self.state != TorrentState::Downloading {
            return;
        }

        let ct = match &self.chunk_tracker {
            Some(ct) => ct,
            None => return,
        };
        if self.meta.is_none() {
            return;
        }

        // Rate limit: don't send new requests if download budget exhausted
        if !self.download_bucket.is_unlimited() && self.download_bucket.available() == 0 {
            return;
        }

        // In end-game: request single block, no pipelining
        if self.end_game.is_active() {
            self.request_end_game_block(peer_addr).await;
            return;
        }

        // Check if peer exists and is eligible for requests
        let can_request = self
            .peers
            .get(&peer_addr)
            .is_some_and(|p| !p.peer_choking && p.pending_requests.len() < self.config.target_request_queue);
        if !can_request {
            return;
        }

        let we_have = ct.bitfield().clone();

        // Collect pieces to request (must borrow self.peers immutably for bitfield)
        let peer_bitfield = match self.peers.get(&peer_addr) {
            Some(p) => p.bitfield.clone(),
            None => return,
        };

        // Try to pick and request a piece
        if let Some(peer) = self.peers.get(&peer_addr) {
            if peer.pending_requests.len() >= self.config.target_request_queue {
                return;
            }

            let picked = self.piece_selector.pick(
                &peer_bitfield,
                &we_have,
                &self.in_flight,
                &self.wanted_pieces,
            );

            let piece_index = match picked {
                Some(idx) => idx,
                None => {
                    self.check_end_game_activation();
                    return;
                }
            };

            self.in_flight.insert(piece_index);

            // Get missing chunks for this piece
            let missing_chunks = match self.chunk_tracker {
                Some(ref ct) => ct.missing_chunks(piece_index),
                None => return,
            };

            for (begin, length) in missing_chunks {
                // Check download rate limits
                if !self.download_bucket.is_unlimited() {
                    let is_local = crate::rate_limiter::is_local_network(peer_addr.ip());
                    if !is_local
                        && let Some(ref global) = self.global_download_bucket
                        && !global.lock().unwrap().try_consume(length as u64)
                    {
                        break;
                    }
                    if !self.download_bucket.try_consume(length as u64) {
                        break;
                    }
                }

                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    let _ = peer
                        .cmd_tx
                        .send(PeerCommand::Request {
                            index: piece_index,
                            begin,
                            length,
                        })
                        .await;
                    peer.pending_requests.push((piece_index, begin, length));
                }
            }
        }
    }

    fn check_end_game_activation(&mut self) {
        if self.end_game.is_active() || self.state != TorrentState::Downloading {
            return;
        }
        let Some(ref ct) = self.chunk_tracker else {
            return;
        };
        let have = ct.bitfield().count_ones();
        let in_flight = self.in_flight.len() as u32;
        if have + in_flight >= self.num_pieces && !self.in_flight.is_empty() {
            let pending: Vec<_> = self
                .peers
                .iter()
                .map(|(addr, p)| (*addr, p.pending_requests.clone()))
                .collect();
            self.end_game.activate(&pending);
            info!(
                blocks = self.end_game.block_count(),
                "end-game mode activated"
            );
        }
    }

    async fn request_end_game_block(&mut self, peer_addr: SocketAddr) {
        let can_request = self
            .peers
            .get(&peer_addr)
            .is_some_and(|p| !p.peer_choking && p.pending_requests.is_empty());
        if !can_request {
            return; // In end-game: only 1 pending request per peer (no pipelining)
        }

        let peer_bitfield = match self.peers.get(&peer_addr) {
            Some(p) => p.bitfield.clone(),
            None => return,
        };

        let block = if self.config.strict_end_game {
            self.end_game.pick_block_strict(peer_addr, &peer_bitfield, &[])
        } else {
            self.end_game.pick_block(peer_addr, &peer_bitfield)
        };

        if let Some((index, begin, length)) = block {
            self.end_game.register_request(index, begin, peer_addr);
            if let Some(peer) = self.peers.get_mut(&peer_addr) {
                let _ = peer
                    .cmd_tx
                    .send(PeerCommand::Request {
                        index,
                        begin,
                        length,
                    })
                    .await;
                peer.pending_requests.push((index, begin, length));
            }
        }
    }

    async fn maybe_express_interest(&mut self, peer_addr: SocketAddr) {
        if self.state != TorrentState::Downloading {
            return;
        }

        let dominated = self.chunk_tracker.as_ref().map(|ct| {
            let we_have = ct.bitfield();
            if let Some(peer) = self.peers.get(&peer_addr) {
                // Check if the peer has any piece we don't
                peer.bitfield
                    .ones()
                    .any(|i| !we_have.get(i))
            } else {
                false
            }
        });

        if dominated == Some(true)
            && let Some(peer) = self.peers.get_mut(&peer_addr)
            && !peer.am_interested
        {
            peer.am_interested = true;
            let _ = peer.cmd_tx.send(PeerCommand::SetInterested(true)).await;
        }
    }

    // ----- Choking -----

    fn update_peer_rates(&mut self) {
        for peer in self.peers.values_mut() {
            // Window is 10 seconds (unchoke interval)
            peer.download_rate = peer.download_bytes_window / 10;
            peer.upload_rate = peer.upload_bytes_window / 10;
            peer.download_bytes_window = 0;
            peer.upload_bytes_window = 0;
        }
    }

    async fn run_choker(&mut self) {
        let peer_infos: Vec<PeerInfo> = self
            .peers
            .values()
            .map(|p| PeerInfo {
                addr: p.addr,
                download_rate: p.download_rate,
                upload_rate: p.upload_rate,
                interested: p.peer_interested,
            })
            .collect();

        let decision = self.choker.decide(&peer_infos);

        for addr in &decision.to_unchoke {
            if let Some(peer) = self.peers.get_mut(addr)
                && peer.am_choking
            {
                peer.am_choking = false;
                let _ = peer.cmd_tx.send(PeerCommand::SetChoking(false)).await;
            }
        }

        for addr in &decision.to_choke {
            if let Some(peer) = self.peers.get_mut(addr)
                && !peer.am_choking
            {
                if peer.supports_fast {
                    let pending: Vec<(u32, u32, u32)> =
                        peer.incoming_requests.drain(..).collect();
                    for (index, begin, length) in pending {
                        let _ = peer
                            .cmd_tx
                            .send(PeerCommand::RejectRequest {
                                index,
                                begin,
                                length,
                            })
                            .await;
                    }
                }
                peer.am_choking = true;
                let _ = peer.cmd_tx.send(PeerCommand::SetChoking(true)).await;
            }
        }

        // Serve any buffered requests from newly-unchoked peers
        self.serve_incoming_requests().await;
    }

    async fn serve_incoming_requests(&mut self) {
        let storage = match &self.storage {
            Some(s) => Arc::clone(s),
            None => return,
        };
        let chunk_tracker = match &self.chunk_tracker {
            Some(ct) => ct,
            None => return,
        };

        // Collect servable requests: peer is unchoked and we have the piece
        let mut to_serve: Vec<(SocketAddr, u32, u32, u32)> = Vec::new();
        for peer in self.peers.values() {
            if peer.am_choking {
                continue;
            }
            for &(index, begin, length) in &peer.incoming_requests {
                if chunk_tracker.has_piece(index) {
                    to_serve.push((peer.addr, index, begin, length));
                }
            }
        }

        for (addr, index, begin, length) in to_serve {
            let chunk_size = length as u64;

            // Check global upload budget (skip for local peers)
            if !crate::rate_limiter::is_local_network(addr.ip())
                && let Some(ref global) = self.global_upload_bucket
                && !global.lock().unwrap().try_consume(chunk_size)
            {
                break; // global budget exhausted, serve remaining next refill
            }

            // Check per-torrent upload budget
            if !self.upload_bucket.try_consume(chunk_size) {
                break; // per-torrent budget exhausted this cycle
            }

            match storage.read_chunk(index, begin, length) {
                Ok(data) => {
                    if let Some(peer) = self.peers.get_mut(&addr) {
                        peer.incoming_requests
                            .retain(|&(i, b, l)| !(i == index && b == begin && l == length));
                        let _ = peer
                            .cmd_tx
                            .send(PeerCommand::SendPiece {
                                index,
                                begin,
                                data: Bytes::from(data),
                            })
                            .await;
                        self.uploaded += chunk_size;
                        self.upload_bytes_interval += chunk_size;
                        peer.upload_bytes_window += chunk_size;
                    }
                }
                Err(e) => {
                    warn!(index, begin, length, "failed to read chunk for upload: {e}");
                }
            }
        }

        if self.check_seed_ratio() {
            self.shutdown_peers().await;
        }
    }

    fn check_seed_ratio(&mut self) -> bool {
        if self.state != TorrentState::Seeding {
            return false;
        }
        if let Some(limit) = self.config.seed_ratio_limit
            && self.downloaded > 0
        {
            let ratio = self.uploaded as f64 / self.downloaded as f64;
            if ratio >= limit {
                info!(ratio, limit, "seed ratio reached, stopping");
                self.state = TorrentState::Stopped;
                return true;
            }
        }
        false
    }

    fn rotate_optimistic(&mut self) {
        let peer_infos: Vec<PeerInfo> = self
            .peers
            .values()
            .map(|p| PeerInfo {
                addr: p.addr,
                download_rate: p.download_rate,
                upload_rate: p.upload_rate,
                interested: p.peer_interested,
            })
            .collect();

        self.choker.rotate_optimistic(&peer_infos);
    }

    // ----- Peer connectivity -----

    fn try_connect_peers(&mut self) {
        while self.peers.len() < self.config.max_peers {
            let addr = match self.available_peers.pop() {
                Some(a) => a,
                None => break,
            };

            if self.peers.contains_key(&addr) {
                continue;
            }

            let (cmd_tx, cmd_rx) = mpsc::channel(64);
            let bitfield = self
                .chunk_tracker
                .as_ref()
                .map(|ct| ct.bitfield().clone())
                .unwrap_or_else(|| Bitfield::new(self.num_pieces));

            self.peers
                .insert(addr, PeerState::new(addr, self.num_pieces, cmd_tx));

            let info_hash = self.info_hash;
            let peer_id = self.our_peer_id;
            let num_pieces = self.num_pieces;
            let event_tx = self.event_tx.clone();
            let enable_dht = self.config.enable_dht;
            let enable_fast = self.config.enable_fast;

            tokio::spawn(async move {
                match tokio::net::TcpStream::connect(addr).await {
                    Ok(stream) => {
                        let _ = run_peer(
                            addr,
                            stream,
                            info_hash,
                            peer_id,
                            bitfield,
                            num_pieces,
                            event_tx,
                            cmd_rx,
                            enable_dht,
                            enable_fast,
                        )
                        .await;
                    }
                    Err(e) => {
                        let _ = event_tx
                            .send(PeerEvent::Disconnected {
                                peer_addr: addr,
                                reason: Some(e.to_string()),
                            })
                            .await;
                    }
                }
            });
        }
    }

    /// Spawn a peer task from an already-connected stream (for incoming connections and tests).
    fn spawn_peer_from_stream(
        &mut self,
        addr: SocketAddr,
        stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    ) {
        if self.peers.contains_key(&addr) || self.peers.len() >= self.config.max_peers {
            return;
        }

        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let bitfield = self
            .chunk_tracker
            .as_ref()
            .map(|ct| ct.bitfield().clone())
            .unwrap_or_else(|| Bitfield::new(self.num_pieces));

        self.peers
            .insert(addr, PeerState::new(addr, self.num_pieces, cmd_tx));

        let info_hash = self.info_hash;
        let peer_id = self.our_peer_id;
        let num_pieces = self.num_pieces;
        let event_tx = self.event_tx.clone();
        let enable_dht = self.config.enable_dht;
        let enable_fast = self.config.enable_fast;

        tokio::spawn(async move {
            let _ = run_peer(
                addr,
                stream,
                info_hash,
                peer_id,
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                enable_dht,
                enable_fast,
            )
            .await;
        });
    }
}

/// Helper to accept a connection from an optional listener.
/// Returns `pending` if no listener is bound, so the `select!` branch is skipped.
async fn accept_incoming(
    listener: &Option<TcpListener>,
) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)> {
    match listener {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use bytes::Bytes;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use ferrite_wire::{ExtHandshake, Handshake, Message, MessageCodec};
    use futures::{SinkExt, StreamExt};
    use tokio_util::codec::{FramedRead, FramedWrite};

    // -- Helpers --

    /// Build a valid TorrentMetaV1 from raw data with given piece length.
    fn make_test_torrent(data: &[u8], piece_length: u64) -> TorrentMetaV1 {
        use serde::Serialize;

        let mut pieces = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + piece_length as usize).min(data.len());
            let hash = ferrite_core::sha1(&data[offset..end]);
            pieces.extend_from_slice(hash.as_bytes());
            offset = end;
        }

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            info: Info<'a>,
        }

        let t = Torrent {
            info: Info {
                length: data.len() as u64,
                name: "test",
                piece_length,
                pieces: &pieces,
            },
        };

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        torrent_from_bytes(&bytes).unwrap()
    }

    fn test_config() -> TorrentConfig {
        TorrentConfig {
            listen_port: 0, // random port
            max_peers: 50,
            target_request_queue: 5,
            download_dir: std::path::PathBuf::from("/tmp"),
            enable_dht: false,
            enable_pex: false,
            enable_fast: false,
            seed_ratio_limit: None,
            strict_end_game: true,
            upload_rate_limit: 0,
            download_rate_limit: 0,
        }
    }

    fn make_storage(data: &[u8], piece_length: u64) -> Arc<MemoryStorage> {
        let lengths = Lengths::new(
            data.len() as u64,
            piece_length,
            DEFAULT_CHUNK_SIZE,
        );
        Arc::new(MemoryStorage::new(lengths))
    }

    fn make_seeded_storage(data: &[u8], piece_length: u64) -> Arc<MemoryStorage> {
        let lengths = Lengths::new(
            data.len() as u64,
            piece_length,
            DEFAULT_CHUNK_SIZE,
        );
        let storage = Arc::new(MemoryStorage::new(lengths.clone()));
        // Write data piece by piece
        let num_pieces = lengths.num_pieces();
        for p in 0..num_pieces {
            let piece_size = lengths.piece_size(p) as usize;
            let offset = lengths.piece_offset(p) as usize;
            let end = offset + piece_size;
            storage.write_chunk(p, 0, &data[offset..end]).unwrap();
        }
        storage
    }

    /// Handshake size constant.
    const HANDSHAKE_SIZE: usize = 68;

    // ---- Test 1: Create from torrent ----

    #[tokio::test]
    async fn create_from_torrent() {
        let data = vec![0xAB; 32768]; // 32 KiB
        let meta = make_test_torrent(&data, 16384); // 2 pieces
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);
        assert_eq!(stats.pieces_total, 2);
        assert_eq!(stats.pieces_have, 0);
        assert_eq!(stats.peers_connected, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 2: Create from magnet ----

    #[tokio::test]
    async fn create_from_magnet() {
        let magnet = Magnet {
            info_hash: Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            display_name: Some("test".into()),
            trackers: vec![],
            peers: vec![],
        };
        let config = test_config();

        let handle = TorrentHandle::from_magnet(magnet, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::FetchingMetadata);
        assert_eq!(stats.pieces_total, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 3: Add peers ----

    #[tokio::test]
    async fn add_peers_increases_available() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        handle
            .add_peers(vec![
                "127.0.0.1:6881".parse().unwrap(),
                "127.0.0.1:6882".parse().unwrap(),
            ])
            .await
            .unwrap();

        // Small delay for the actor to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.peers_available, 2);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 4: Stats reporting ----

    #[tokio::test]
    async fn stats_reporting() {
        let data = vec![0xAB; 65536]; // 64 KiB
        let meta = make_test_torrent(&data, 16384); // 4 pieces
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);
        assert_eq!(stats.downloaded, 0);
        assert_eq!(stats.uploaded, 0);
        assert_eq!(stats.pieces_have, 0);
        assert_eq!(stats.pieces_total, 4);
        assert_eq!(stats.peers_connected, 0);
        assert_eq!(stats.peers_available, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 5: Private torrent disables DHT/PEX ----

    #[tokio::test]
    async fn private_torrent_disables_dht_pex() {
        // Build a private torrent by embedding private=1 in the info dict
        use serde::Serialize;

        let data = vec![0xAB; 16384];
        let hash = ferrite_core::sha1(&data);
        let mut pieces = Vec::new();
        pieces.extend_from_slice(hash.as_bytes());

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
            private: i64,
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            info: Info<'a>,
        }

        let t = Torrent {
            info: Info {
                length: data.len() as u64,
                name: "private_test",
                piece_length: 16384,
                pieces: &pieces,
                private: 1,
            },
        };

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(meta.info.private, Some(1));

        let storage = make_storage(&data, 16384);
        let mut config = test_config();
        config.enable_dht = true;
        config.enable_pex = true;

        // The from_torrent constructor should disable DHT and PEX
        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        // We can't directly inspect the actor's config, but we can verify
        // the torrent was created successfully. The real test is that PEX peers
        // would be ignored and DHT not used. For now verify the handle works.
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 6: Shutdown cleanup ----

    #[tokio::test]
    async fn shutdown_cleanup() {
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        handle.shutdown().await.unwrap();

        // After shutdown, stats should fail (channel closed)
        tokio::time::sleep(Duration::from_millis(50)).await;
        let result = handle.stats().await;
        assert!(result.is_err());
    }

    // ---- Test 7: Duplicate add_peers ignored ----

    #[tokio::test]
    async fn duplicate_peers_ignored() {
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        handle.add_peers(vec![addr, addr, addr]).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        // Only one unique peer should be in available
        assert_eq!(stats.peers_available, 1);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 8: Multiple handles (Clone) share same actor ----

    #[tokio::test]
    async fn cloned_handle_shares_actor() {
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();
        let handle2 = handle.clone();

        // Add peers through one handle
        handle
            .add_peers(vec!["127.0.0.1:7777".parse().unwrap()])
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Read stats through the other
        let stats = handle2.stats().await.unwrap();
        assert_eq!(stats.peers_available, 1);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 9: Peer connection and disconnect via listener ----

    #[tokio::test]
    async fn peer_connect_and_disconnect_via_listener() {
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        // Bind a listener on a specific port so we can connect to it
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        // Drop the pre-bound listener before from_torrent binds
        drop(listener);

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        // Give the actor time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect a mock peer
        let mut stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();

        // Perform handshake
        let remote_id = Id20::from_hex("1111111111111111111111111111111111111111").unwrap();
        let remote_hs = Handshake::new(info_hash, remote_id);
        stream.write_all(&remote_hs.to_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let mut hs_buf = [0u8; HANDSHAKE_SIZE];
        stream.read_exact(&mut hs_buf).await.unwrap();
        let their_hs = Handshake::from_bytes(&hs_buf).unwrap();
        assert_eq!(their_hs.info_hash, info_hash);

        // Give time for peer to be registered
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.peers_connected, 1);

        // Drop the connection
        drop(stream);

        // Wait for disconnect event
        tokio::time::sleep(Duration::from_millis(200)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.peers_connected, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 10: Piece download and verification via injected events ----
    //
    // We test the full flow: connect a mock peer that sends bitfield, unchoke,
    // then responds to requests with correct piece data.

    #[tokio::test]
    async fn piece_download_and_verify() {
        // Create a 1-piece torrent with 16384 bytes (exactly one chunk)
        let data = vec![0xCDu8; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect mock peer
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let remote_id = Id20::from_hex("2222222222222222222222222222222222222222").unwrap();

        // Run mock seeder in a task
        let mock_data = data.clone();
        let mock_task = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);
            let mut reader = reader;
            let mut writer = writer;

            // Handshake
            let hs = Handshake::new(info_hash, remote_id);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();

            // Switch to framed
            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            // Read ext handshake from the torrent actor's peer
            let _msg = framed_read.next().await;

            // Send ext handshake back
            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
                .await
                .unwrap();

            // Send bitfield (all pieces = piece 0 set)
            let mut bf = Bitfield::new(1);
            bf.set(0);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            // Send Unchoke
            framed_write.send(Message::Unchoke).await.unwrap();

            // Wait for requests and respond with piece data
            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => {
                        let start = begin as usize;
                        let end = start + length as usize;
                        let piece_data = &mock_data[start..end];
                        framed_write
                            .send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::copy_from_slice(piece_data),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {
                        // Expected — the torrent should express interest
                    }
                    _ => {}
                }
            }
        });

        // Wait for the download to complete
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 1);
                assert_eq!(stats.pieces_total, 1);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = handle.stats().await.unwrap();
                panic!(
                    "download did not complete within 5s, state={:?}, have={}/{}",
                    stats.state, stats.pieces_have, stats.pieces_total
                );
            }
        }

        handle.shutdown().await.unwrap();
        mock_task.abort();
    }

    // ---- Test 11: Failed piece verification re-requests ----

    #[tokio::test]
    async fn failed_piece_verification() {
        // Create a 1-piece torrent
        let data = vec![0xEEu8; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect mock peer that first sends bad data, then correct data
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let remote_id = Id20::from_hex("3333333333333333333333333333333333333333").unwrap();

        let correct_data = data.clone();
        let mock_task = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);

            // Handshake
            let mut writer = writer;
            let mut reader = reader;
            let hs = Handshake::new(info_hash, remote_id);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();

            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            // Read ext handshake
            let _msg = framed_read.next().await;

            // Send ext handshake
            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
                .await
                .unwrap();

            // Bitfield: have piece 0
            let mut bf = Bitfield::new(1);
            bf.set(0);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            // Unchoke
            framed_write.send(Message::Unchoke).await.unwrap();

            let mut request_count = 0u32;
            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => {
                        request_count += 1;
                        let piece_data = if request_count <= 1 {
                            // First request: send bad data
                            vec![0xFF; length as usize]
                        } else {
                            // Subsequent: send correct data
                            let start = begin as usize;
                            let end = start + length as usize;
                            correct_data[start..end].to_vec()
                        };
                        framed_write
                            .send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::from(piece_data),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {}
                    _ => {}
                }
            }
        });

        // Wait for completion (should eventually succeed after retry)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 1);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = handle.stats().await.unwrap();
                panic!(
                    "download did not complete after retry within 5s, state={:?}, have={}",
                    stats.state, stats.pieces_have,
                );
            }
        }

        handle.shutdown().await.unwrap();
        mock_task.abort();
    }

    // ---- Test 12: Complete state transitions after all pieces ----

    #[tokio::test]
    async fn complete_transitions_state() {
        // 2-piece torrent, each 16384 bytes (one chunk each)
        let data = vec![0xBBu8; 32768];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Mock seeder with all 2 pieces
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let remote_id = Id20::from_hex("4444444444444444444444444444444444444444").unwrap();

        let mock_data = data.clone();
        let mock_task = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);
            let mut writer = writer;
            let mut reader = reader;

            let hs = Handshake::new(info_hash, remote_id);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();

            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            // Read ext handshake
            let _msg = framed_read.next().await;

            // Send ext handshake
            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
                .await
                .unwrap();

            // Bitfield: have both pieces
            let mut bf = Bitfield::new(2);
            bf.set(0);
            bf.set(1);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            framed_write.send(Message::Unchoke).await.unwrap();

            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => {
                        let abs_start = (index as usize * 16384) + begin as usize;
                        let abs_end = abs_start + length as usize;
                        let piece_data = &mock_data[abs_start..abs_end];
                        framed_write
                            .send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::copy_from_slice(piece_data),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {}
                    _ => {}
                }
            }
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 2);
                assert_eq!(stats.pieces_total, 2);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = handle.stats().await.unwrap();
                panic!(
                    "expected Complete, got {:?}, have={}/{}",
                    stats.state, stats.pieces_have, stats.pieces_total
                );
            }
        }

        handle.shutdown().await.unwrap();
        mock_task.abort();
    }

    // ---- Test 13: Multiple pieces with multi-chunk pieces ----

    #[tokio::test]
    async fn multi_chunk_piece_download() {
        // 1 piece of 32768 bytes = 2 chunks of 16384 each
        let data = vec![0xAAu8; 32768];
        let meta = make_test_torrent(&data, 32768);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 32768);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let remote_id = Id20::from_hex("5555555555555555555555555555555555555555").unwrap();

        let mock_data = data.clone();
        let mock_task = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);
            let mut writer = writer;
            let mut reader = reader;

            let hs = Handshake::new(info_hash, remote_id);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();

            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            let _msg = framed_read.next().await;

            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
                .await
                .unwrap();

            let mut bf = Bitfield::new(1);
            bf.set(0);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            framed_write.send(Message::Unchoke).await.unwrap();

            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index: _,
                        begin,
                        length,
                    } => {
                        let start = begin as usize;
                        let end = start + length as usize;
                        framed_write
                            .send(Message::Piece {
                                index: 0,
                                begin,
                                data: Bytes::copy_from_slice(&mock_data[start..end]),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {}
                    _ => {}
                }
            }
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 1);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("multi-chunk download did not complete within 5s");
            }
        }

        handle.shutdown().await.unwrap();
        mock_task.abort();
    }

    // ---- Test 14: Seeder/Leecher integration with two actors ----

    #[tokio::test]
    async fn seeder_leecher_integration() {
        // Seeder has all data, leecher has none. Connect them via TCP.
        let data = vec![0xDDu8; 32768]; // 32 KiB, 2 pieces of 16384
        let piece_length = 16384u64;
        let meta = make_test_torrent(&data, piece_length);
        let info_hash = meta.info_hash;

        // Seeder: storage pre-filled
        let seeder_storage = make_seeded_storage(&data, piece_length);

        // For the seeder, we need a from_torrent variant that starts in Complete state
        // but still serves pieces. Since our actor starts in Downloading, the seeder
        // will just be a mock that accepts and serves.

        // Use a mock seeder approach instead (manual protocol handling):
        let seeder_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let seeder_addr = seeder_listener.local_addr().unwrap();

        let seeder_task = tokio::spawn(async move {
            let (stream, _addr) = seeder_listener.accept().await.unwrap();
            let (reader, writer) = tokio::io::split(stream);
            let mut writer = writer;
            let mut reader = reader;

            // Handshake
            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();
            let their_hs = Handshake::from_bytes(&hs_buf).unwrap();
            assert_eq!(their_hs.info_hash, info_hash);

            let hs = Handshake::new(info_hash, PeerId::generate().0);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            // Read ext handshake
            let _msg = framed_read.next().await;

            // Send ext handshake
            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
                .await
                .unwrap();

            // Send bitfield (all pieces)
            let mut bf = Bitfield::new(2);
            bf.set(0);
            bf.set(1);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            // Unchoke
            framed_write.send(Message::Unchoke).await.unwrap();

            // Serve requests
            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => {
                        let piece_data = seeder_storage
                            .read_chunk(index, begin, length)
                            .unwrap();
                        framed_write
                            .send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::from(piece_data),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {}
                    _ => {}
                }
            }
        });

        // Leecher: empty storage
        let leecher_storage = make_storage(&data, piece_length);
        let leecher_meta = make_test_torrent(&data, piece_length);

        let leecher_config = test_config();
        let leecher = TorrentHandle::from_torrent(leecher_meta, leecher_storage, leecher_config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        // Add seeder as a peer
        leecher.add_peers(vec![seeder_addr]).await.unwrap();

        // Give the connect interval time to fire (it ticks every 30s).
        // Instead, wait a bit for the initial connect tick. Since our interval
        // skips the first tick, the first real connect happens at 30s.
        // That's too long for a test. Let's trigger it by polling stats.
        //
        // Actually, the connect timer fires immediately on the SECOND tick after
        // the first (which we consumed). So we need a different approach.
        // We added the available peer — but the connect interval won't fire for 30s.
        //
        // Let's just wait and check — the actor's try_connect_peers runs on the timer.
        // For this test to be practical, we need a shorter interval or a direct connect trigger.
        //
        // Actually the timer will fire every 30 seconds after the first tick we consumed.
        // For testing, we can just wait longer or use a smaller torrent.
        // Let's wait up to 35 seconds with a short poll.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let stats = leecher.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 2);
                assert_eq!(stats.pieces_total, 2);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = leecher.stats().await.unwrap();
                panic!(
                    "seeder/leecher: leecher did not complete, state={:?}, have={}/{}, connected={}, available={}",
                    stats.state, stats.pieces_have, stats.pieces_total, stats.peers_connected, stats.peers_available,
                );
            }
        }

        leecher.shutdown().await.unwrap();
        seeder_task.abort();
    }

    // ---- Test 15: Magnet stats ----

    #[tokio::test]
    async fn magnet_initial_stats() {
        let magnet = Magnet {
            info_hash: Id20::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
            display_name: Some("magnet test".into()),
            trackers: vec![],
            peers: vec![],
        };

        let handle = TorrentHandle::from_magnet(magnet, test_config(), None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::FetchingMetadata);
        assert_eq!(stats.pieces_total, 0);
        assert_eq!(stats.pieces_have, 0);
        assert_eq!(stats.downloaded, 0);
        assert_eq!(stats.uploaded, 0);
        assert_eq!(stats.peers_connected, 0);
        assert_eq!(stats.peers_available, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 16: Tracker manager is populated from torrent metadata ----

    #[tokio::test]
    async fn tracker_populated_from_metadata() {
        use serde::Serialize;

        let data = vec![0xAB; 16384];
        let hash = ferrite_core::sha1(&data);
        let mut pieces = Vec::new();
        pieces.extend_from_slice(hash.as_bytes());

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            announce: &'a str,
            info: Info<'a>,
        }

        let t = Torrent {
            announce: "http://tracker.example.com:8080/announce",
            info: Info {
                length: data.len() as u64,
                name: "test",
                piece_length: 16384,
                pieces: &pieces,
            },
        };

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert!(meta.announce.is_some());

        let storage = make_storage(&data, 16384);
        let config = test_config();

        // The torrent should start and announce to tracker (which will fail since
        // the tracker doesn't exist, but that's fine — failures are non-fatal).
        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 17: Private torrent with DHT=None works ----

    #[tokio::test]
    async fn private_torrent_no_dht_field() {
        let data = vec![0xAB; 16384];
        let hash = ferrite_core::sha1(&data);
        let mut pieces = Vec::new();
        pieces.extend_from_slice(hash.as_bytes());

        use serde::Serialize;

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
            private: i64,
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            announce: &'a str,
            info: Info<'a>,
        }

        let t = Torrent {
            announce: "http://private-tracker.example.com/announce",
            info: Info {
                length: data.len() as u64,
                name: "private_test",
                piece_length: 16384,
                pieces: &pieces,
                private: 1,
            },
        };

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(meta.info.private, Some(1));

        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 18: Magnet defers tracker announce ----

    #[tokio::test]
    async fn magnet_no_tracker_before_metadata() {
        let magnet = Magnet {
            info_hash: Id20::from_hex("cccccccccccccccccccccccccccccccccccccccc").unwrap(),
            display_name: Some("magnet test".into()),
            trackers: vec![],
            peers: vec![],
        };

        let handle = TorrentHandle::from_magnet(magnet, test_config(), None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::FetchingMetadata);

        // No tracker announces should happen in FetchingMetadata state
        // (verified by the tracker_announce arm checking state != FetchingMetadata)
        tokio::time::sleep(Duration::from_millis(50)).await;

        handle.shutdown().await.unwrap();
    }

    // ---- Test 19: Pause and resume ----

    #[tokio::test]
    async fn pause_and_resume() {
        let data = vec![0xEEu8; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();
        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.pause().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Paused);

        handle.resume().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 20: Pause already paused is noop ----

    #[tokio::test]
    async fn pause_already_paused_is_noop() {
        let data = vec![0xEEu8; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();
        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        handle.pause().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.pause().await.unwrap(); // double pause is fine
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Paused);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 21: Incoming request served from storage ----
    //
    // Phase 1: Mock seeder feeds piece 0 to the torrent so it becomes verified.
    // Phase 2: Mock leecher connects and requests piece 0, verifying upload pipeline.

    #[tokio::test]
    async fn incoming_request_served_from_storage() {
        let data = vec![0xABu8; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Phase 1: Seed the torrent with piece 0
        let seed_data = data.clone();
        let seed_stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let seeder_task = tokio::spawn({
            let info_hash = info_hash;
            async move {
                let (reader, writer) = tokio::io::split(seed_stream);
                let mut writer = writer;
                let mut reader = reader;

                let hs = Handshake::new(info_hash, Id20::from_hex("6666666666666666666666666666666666666666").unwrap());
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await; // ext handshake
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

                // Send bitfield + unchoke
                let mut bf = Bitfield::new(1);
                bf.set(0);
                framed_write.send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes()))).await.unwrap();
                framed_write.send(Message::Unchoke).await.unwrap();

                // Respond to requests
                while let Some(Ok(msg)) = framed_read.next().await {
                    match msg {
                        Message::Request { index, begin, length } => {
                            let start = begin as usize;
                            let end = start + length as usize;
                            framed_write.send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::copy_from_slice(&seed_data[start..end]),
                            }).await.unwrap();
                        }
                        Message::Interested => {}
                        _ => {}
                    }
                }
            }
        });

        // Wait for download to complete
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.pieces_have == 1 {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("piece download did not complete within 5s");
            }
        }

        // Phase 2: Connect a mock leecher to request piece 0 back
        let leech_stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let expected_data = data.clone();
        let leecher_task = tokio::spawn({
            let info_hash = info_hash;
            async move {
                let (reader, writer) = tokio::io::split(leech_stream);
                let mut writer = writer;
                let mut reader = reader;

                let hs = Handshake::new(info_hash, Id20::from_hex("7777777777777777777777777777777777777777").unwrap());
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await; // ext handshake
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

                // Send Interested and wait for Unchoke
                framed_write.send(Message::Interested).await.unwrap();

                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    tokio::select! {
                        msg = framed_read.next() => {
                            match msg {
                                Some(Ok(Message::Unchoke)) => { break; }
                                Some(Ok(_)) => {}
                                _ => panic!("connection closed before unchoke"),
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            panic!("timed out waiting for unchoke");
                        }
                    }
                }

                // Request piece 0
                framed_write.send(Message::Request { index: 0, begin: 0, length: 16384 }).await.unwrap();

                // Read Piece response
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                loop {
                    tokio::select! {
                        msg = framed_read.next() => {
                            match msg {
                                Some(Ok(Message::Piece { index, begin, data })) => {
                                    assert_eq!(index, 0);
                                    assert_eq!(begin, 0);
                                    assert_eq!(data.as_ref(), expected_data.as_slice());
                                    return; // success
                                }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => panic!("error reading: {e}"),
                                None => panic!("connection closed before piece"),
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            panic!("timed out waiting for piece data");
                        }
                    }
                }
            }
        });

        // Wait for leecher to complete
        let result = tokio::time::timeout(Duration::from_secs(20), leecher_task).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("leecher task panicked: {e}"),
            Err(_) => panic!("test timed out"),
        }

        // Verify uploaded bytes
        let stats = handle.stats().await.unwrap();
        assert!(stats.uploaded > 0, "expected uploaded > 0, got {}", stats.uploaded);

        handle.shutdown().await.unwrap();
        seeder_task.abort();
    }

    // ---- Test 22: Seed ratio limit stops torrent ----

    #[tokio::test]
    async fn seed_ratio_limit_stops_torrent() {
        // 1-piece torrent, ratio limit = 1.0
        // After downloading 16384 bytes and uploading 16384 bytes, ratio = 1.0 → stop
        let data = vec![0xCCu8; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            seed_ratio_limit: Some(1.0),
            ..test_config()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Phase 1: Seed the torrent with piece 0
        let seed_data = data.clone();
        let seed_stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let seeder_task = tokio::spawn({
            let info_hash = info_hash;
            async move {
                let (reader, writer) = tokio::io::split(seed_stream);
                let mut writer = writer;
                let mut reader = reader;

                let hs = Handshake::new(info_hash, Id20::from_hex("8888888888888888888888888888888888888888").unwrap());
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await;
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

                let mut bf = Bitfield::new(1);
                bf.set(0);
                framed_write.send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes()))).await.unwrap();
                framed_write.send(Message::Unchoke).await.unwrap();

                while let Some(Ok(msg)) = framed_read.next().await {
                    match msg {
                        Message::Request { index, begin, length } => {
                            let start = begin as usize;
                            let end = start + length as usize;
                            framed_write.send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::copy_from_slice(&seed_data[start..end]),
                            }).await.unwrap();
                        }
                        Message::Interested => {}
                        _ => {}
                    }
                }
            }
        });

        // Wait for download to complete (transitions to Seeding)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("download did not complete within 5s");
            }
        }

        // Phase 2: Connect leecher to request piece 0 — this should trigger ratio limit
        let leech_stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let leecher_task = tokio::spawn({
            let info_hash = info_hash;
            async move {
                let (reader, writer) = tokio::io::split(leech_stream);
                let mut writer = writer;
                let mut reader = reader;

                let hs = Handshake::new(info_hash, Id20::from_hex("9999999999999999999999999999999999999999").unwrap());
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await;
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

                framed_write.send(Message::Interested).await.unwrap();

                // Wait for unchoke
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    tokio::select! {
                        msg = framed_read.next() => {
                            match msg {
                                Some(Ok(Message::Unchoke)) => break,
                                Some(Ok(_)) => {}
                                _ => return, // connection may close due to ratio shutdown
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => return,
                    }
                }

                // Request piece 0
                framed_write.send(Message::Request { index: 0, begin: 0, length: 16384 }).await.unwrap();

                // Read until connection closes (the torrent may stop and disconnect us)
                while let Some(Ok(_msg)) = framed_read.next().await {}
            }
        });

        // Wait for state to become Stopped
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Stopped {
                assert!(stats.uploaded >= 16384, "expected uploaded >= 16384, got {}", stats.uploaded);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = handle.stats().await.unwrap();
                panic!(
                    "expected Stopped, got {:?}, uploaded={}, downloaded={}",
                    stats.state, stats.uploaded, stats.downloaded
                );
            }
        }

        handle.shutdown().await.unwrap();
        seeder_task.abort();
        leecher_task.abort();
    }

    // ---- Test 23: Resume with seeded storage starts as seeder ----

    #[tokio::test]
    async fn resume_with_seeded_storage() {
        let data = vec![0xDDu8; 32768]; // 2 pieces
        let meta = make_test_torrent(&data, 16384);
        let storage = make_seeded_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        // Give the actor time to verify existing pieces
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Seeding, "should start as seeder with all pieces verified");
        assert_eq!(stats.pieces_have, 2);
        assert_eq!(stats.pieces_total, 2);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: save_resume_data captures state ----

    #[tokio::test]
    async fn save_resume_data_captures_state() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        // Give actor time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        let rd = handle.save_resume_data().await.unwrap();

        assert_eq!(rd.file_format, "libtorrent resume file");
        assert_eq!(rd.file_version, 1);
        assert_eq!(rd.info_hash, info_hash.as_bytes().to_vec());
        assert_eq!(rd.name, "test");
        assert_eq!(rd.save_path, "/tmp");
        assert_eq!(rd.paused, 0);
        // No pieces downloaded yet — bitfield should be all zeros
        assert!(!rd.pieces.is_empty());
        // Stats should be zero for a freshly started torrent with no peers
        assert_eq!(rd.total_uploaded, 0);
        assert_eq!(rd.total_downloaded, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: save_resume_data for seeder ----

    #[tokio::test]
    async fn save_resume_data_seeder() {
        let data = vec![0xCD; 32768];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_seeded_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        // Give actor time to verify pieces and switch to seeding
        tokio::time::sleep(Duration::from_millis(100)).await;

        let rd = handle.save_resume_data().await.unwrap();

        assert_eq!(rd.info_hash, info_hash.as_bytes().to_vec());
        assert_eq!(rd.name, "test");
        assert_eq!(rd.seed_mode, 1, "seeder should have seed_mode=1");
        assert_eq!(rd.paused, 0);
        // All pieces should be marked in the bitfield
        // 2 pieces -> 1 byte, top 2 bits set = 0b1100_0000 = 0xC0
        assert_eq!(rd.pieces.len(), 1);
        assert_eq!(rd.pieces[0] & 0xC0, 0xC0, "both pieces should be marked complete");

        handle.shutdown().await.unwrap();
    }

    // ---- Test: save_resume_data for paused torrent ----

    #[tokio::test]
    async fn save_resume_data_paused() {
        let data = vec![0xEF; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.pause().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let rd = handle.save_resume_data().await.unwrap();
        assert_eq!(rd.paused, 1, "paused torrent should have paused=1");
        assert_eq!(rd.seed_mode, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: set_file_priority and read back ----

    #[tokio::test]
    async fn set_file_priority_and_read_back() {
        let info_bytes = b"d5:filesld6:lengthi100e4:pathl5:a.bineed6:lengthi100e4:pathl5:b.bineee4:name4:test12:piece lengthi100e6:pieces40:AAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBe";
        let mut torrent_bytes = b"d4:info".to_vec();
        torrent_bytes.extend_from_slice(info_bytes);
        torrent_bytes.push(b'e');

        let meta = ferrite_core::torrent_from_bytes(&torrent_bytes).unwrap();
        let lengths = Lengths::new(200, 100, DEFAULT_CHUNK_SIZE);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let config = TorrentConfig {
            listen_port: 0,
            ..Default::default()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        // Default priorities should all be Normal
        let prios = handle.file_priorities().await.unwrap();
        assert_eq!(prios.len(), 2);
        assert!(prios.iter().all(|p| *p == FilePriority::Normal));

        // Set file 0 to Skip
        handle
            .set_file_priority(0, FilePriority::Skip)
            .await
            .unwrap();

        let prios = handle.file_priorities().await.unwrap();
        assert_eq!(prios[0], FilePriority::Skip);
        assert_eq!(prios[1], FilePriority::Normal);

        // Invalid index should error
        let result = handle.set_file_priority(99, FilePriority::High).await;
        assert!(result.is_err());

        handle.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn resume_data_preserves_file_priorities() {
        let info_bytes = b"d5:filesld6:lengthi100e4:pathl5:a.bineed6:lengthi100e4:pathl5:b.bineee4:name4:test12:piece lengthi100e6:pieces40:AAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBe";
        let mut torrent_bytes = b"d4:info".to_vec();
        torrent_bytes.extend_from_slice(info_bytes);
        torrent_bytes.push(b'e');

        let meta = ferrite_core::torrent_from_bytes(&torrent_bytes).unwrap();
        let lengths = Lengths::new(200, 100, DEFAULT_CHUNK_SIZE);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let config = TorrentConfig {
            listen_port: 0,
            ..Default::default()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None, None, None, crate::slot_tuner::SlotTuner::disabled(4))
            .await
            .unwrap();

        // Set file priorities
        handle.set_file_priority(0, FilePriority::High).await.unwrap();
        handle.set_file_priority(1, FilePriority::Skip).await.unwrap();

        // Save resume data
        let rd = handle.save_resume_data().await.unwrap();
        assert_eq!(rd.file_priority, vec![7, 0]); // High=7, Skip=0

        // Verify bencode round-trip
        let encoded = ferrite_bencode::to_bytes(&rd).unwrap();
        let decoded: ferrite_core::FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.file_priority, vec![7, 0]);

        handle.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
