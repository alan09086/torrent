//! Swarm integration tests for the simulated network.

use std::time::Duration;

use torrent_sim::{SimSwarmBuilder, make_seeded_storage, make_test_torrent};

/// Build a swarm, add a torrent to one node as seed, add as leecher to another,
/// introduce peers, and verify the leecher eventually learns about peers.
#[tokio::test]
async fn test_seed_leecher_peer_discovery() {
    let swarm = SimSwarmBuilder::new(2).build().await;

    let data = vec![0xAB; 32768]; // 2 pieces at 16384
    let (meta, _bytes) = make_test_torrent(&data, 16384);
    let seeded_storage = make_seeded_storage(&data, 16384);

    // Node 0 is the seed (with pre-populated storage)
    let info_hash = swarm
        .add_torrent(0, meta.clone().into(), Some(seeded_storage))
        .await;

    // Node 1 is the leecher (empty storage)
    let _ih2 = swarm.add_torrent(1, meta.into(), None).await;

    // Introduce peers so nodes know about each other
    swarm.introduce_peers(info_hash).await;

    // Give some time for peer connections to establish
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify seed has the torrent registered with pieces
    let seed_stats = swarm.torrent_stats(0, info_hash).await;
    assert!(
        seed_stats.pieces_total > 0,
        "seed should have pieces_total > 0, got {}",
        seed_stats.pieces_total,
    );

    // Verify leecher has the torrent registered
    let leecher_stats = swarm.torrent_stats(1, info_hash).await;
    assert!(
        leecher_stats.pieces_total > 0,
        "leecher should have pieces_total > 0, got {}",
        leecher_stats.pieces_total,
    );

    // The leecher should have peers_available > 0 after introduce_peers
    assert!(
        leecher_stats.peers_available > 0 || leecher_stats.peers_connected > 0,
        "leecher should know about peers after introduce_peers, \
         peers_available={}, peers_connected={}",
        leecher_stats.peers_available,
        leecher_stats.peers_connected,
    );

    swarm.shutdown().await;
}

/// Verify that a partition blocks peer connections.
#[tokio::test]
async fn test_partition_blocks_connection() {
    let swarm = SimSwarmBuilder::new(2).build().await;

    let data = vec![0xCD; 16384]; // 1 piece
    let (meta, _bytes) = make_test_torrent(&data, 16384);
    let seeded_storage = make_seeded_storage(&data, 16384);

    let info_hash = swarm
        .add_torrent(0, meta.clone().into(), Some(seeded_storage))
        .await;
    let _ih2 = swarm.add_torrent(1, meta.into(), None).await;

    // Partition the network BEFORE introducing peers
    swarm
        .network()
        .partition(vec![swarm.node_ip(0)], vec![swarm.node_ip(1)]);

    swarm.introduce_peers(info_hash).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Leecher should have 0 connected peers (partitioned — TCP connect will fail)
    let leecher_stats = swarm.torrent_stats(1, info_hash).await;
    assert_eq!(
        leecher_stats.peers_connected, 0,
        "partitioned leecher should have no connected peers"
    );

    swarm.shutdown().await;
}

/// Verify a three-node swarm can all register the same torrent.
#[tokio::test]
async fn test_swarm_three_nodes_all_register_torrent() {
    let swarm = SimSwarmBuilder::new(3).build().await;

    let data = vec![0xEF; 32768];
    let (meta, _bytes) = make_test_torrent(&data, 16384);
    let seeded_storage = make_seeded_storage(&data, 16384);

    // Node 0 is seed
    let info_hash = swarm
        .add_torrent(0, meta.clone().into(), Some(seeded_storage))
        .await;
    // Nodes 1 and 2 are leechers
    let _ih1 = swarm.add_torrent(1, meta.clone().into(), None).await;
    let _ih2 = swarm.add_torrent(2, meta.into(), None).await;

    swarm.introduce_peers(info_hash).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify all three nodes have the torrent registered
    for i in 0..3 {
        let stats = swarm.torrent_stats(i, info_hash).await;
        assert!(
            stats.pieces_total > 0,
            "node {i} should have pieces_total > 0"
        );
    }

    swarm.shutdown().await;
}

/// Verify that `num_nodes()` returns the correct count.
#[tokio::test]
async fn test_num_nodes() {
    let swarm = SimSwarmBuilder::new(5).build().await;
    assert_eq!(swarm.num_nodes(), 5);
    swarm.shutdown().await;
}

/// Build a 2-node swarm (seeder + leecher), transfer 64 KiB of data
/// (4 pieces at 16384), and verify the leecher downloads everything
/// and the seeder records upload.
#[tokio::test]
async fn test_basic_seeder_leecher_transfer() {
    let swarm = SimSwarmBuilder::new(2).build().await;

    // 64 KiB of test data = 4 pieces at 16384 bytes each
    let data = vec![0xDE; 65536];
    let (meta, _bytes) = make_test_torrent(&data, 16384);
    let seeded_storage = make_seeded_storage(&data, 16384);

    // Node 0 is the seeder (with pre-populated storage)
    let info_hash = swarm
        .add_torrent(0, meta.clone().into(), Some(seeded_storage))
        .await;

    // Node 1 is the leecher (empty storage)
    let _ih2 = swarm.add_torrent(1, meta.into(), None).await;

    // Introduce peers so nodes know about each other
    swarm.introduce_peers(info_hash).await;

    // Poll leecher stats until download completes, with 30s timeout
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let leecher_stats = swarm.torrent_stats(1, info_hash).await;
        if leecher_stats.total_done == data.len() as u64 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for leecher to complete download; \
             total_done={}, expected={}",
            leecher_stats.total_done,
            data.len(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Verify leecher has all the data
    let leecher_stats = swarm.torrent_stats(1, info_hash).await;
    assert_eq!(
        leecher_stats.total_done,
        data.len() as u64,
        "leecher should have downloaded all data"
    );

    // Verify seeder recorded upload
    let seed_stats = swarm.torrent_stats(0, info_hash).await;
    assert!(
        seed_stats.total_upload > 0,
        "seeder should have total_upload > 0, got {}",
        seed_stats.total_upload,
    );

    swarm.shutdown().await;
}

/// Verify make_seeded_storage produces storage with correct data.
#[test]
fn test_make_seeded_storage_roundtrip() {
    use torrent_storage::TorrentStorage;

    let data = vec![0x42; 32768]; // 2 pieces at 16384
    let storage = make_seeded_storage(&data, 16384);

    // Read back piece 0
    let piece0 = storage.read_piece(0).unwrap();
    assert_eq!(piece0.len(), 16384);
    assert_eq!(&piece0[..], &data[..16384]);

    // Read back piece 1
    let piece1 = storage.read_piece(1).unwrap();
    assert_eq!(piece1.len(), 16384);
    assert_eq!(&piece1[..], &data[16384..]);
}
