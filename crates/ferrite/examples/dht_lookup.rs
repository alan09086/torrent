//! Look up peers for an info hash via DHT.
//!
//! Usage: `cargo run --example dht_lookup -- <info-hash-hex>`

use ferrite::prelude::*;
use std::env;

#[tokio::main]
async fn main() -> ferrite::Result<()> {
    let hex = env::args()
        .nth(1)
        .expect("usage: dht_lookup <info-hash-hex>");
    let info_hash = Id20::from_hex(&hex).expect("invalid hex info hash");

    let session = ClientBuilder::new()
        .enable_dht(true)
        .download_dir("/tmp")
        .start()
        .await?;

    println!("Bootstrapping DHT...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let stats = session.session_stats().await?;
    println!("DHT nodes: {}", stats.dht_nodes);
    println!("Looking up peers for {}...", info_hash.to_hex());

    let magnet = Magnet {
        info_hashes: InfoHashes::v1_only(info_hash),
        display_name: None,
        trackers: vec![],
        peers: vec![],
        selected_files: None,
    };

    let _ih = session.add_magnet(magnet).await?;
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let ts = session.torrent_stats(info_hash).await?;
        println!(
            "Peers: {} connected, {} available",
            ts.peers_connected, ts.peers_available
        );
    }

    session.shutdown().await?;
    Ok(())
}
