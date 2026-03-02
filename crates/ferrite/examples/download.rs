//! Download a torrent from a magnet link.
//!
//! Usage: `cargo run --example download -- "magnet:?xt=urn:btih:..."`

use ferrite::prelude::*;
use std::env;

#[tokio::main]
async fn main() -> ferrite::Result<()> {
    let magnet_uri = env::args().nth(1).expect("usage: download <magnet-uri>");
    let magnet = Magnet::parse(&magnet_uri)?;

    let session = ClientBuilder::new()
        .download_dir(".")
        .enable_dht(true)
        .start()
        .await?;

    let info_hash = session.add_magnet(magnet).await?;
    println!("Added torrent: {}", info_hash.to_hex());

    loop {
        let stats = session.torrent_stats(info_hash).await?;
        println!(
            "[{:?}] {}/{} pieces, {} peers",
            stats.state, stats.pieces_have, stats.pieces_total, stats.peers_connected,
        );

        if matches!(stats.state, TorrentState::Seeding | TorrentState::Complete) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    println!("Download complete!");
    session.shutdown().await?;
    Ok(())
}
