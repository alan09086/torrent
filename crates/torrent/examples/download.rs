//! Download a torrent from a magnet link.
//!
//! Usage: `cargo run --example download -- "magnet:?xt=urn:btih:..."`

use std::env;
use torrent::prelude::*;

#[tokio::main]
async fn main() -> torrent::Result<()> {
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
            "[{:?}] {:.1}% | {:.1} KB/s down | {} seeds, {} peers",
            stats.state,
            stats.progress * 100.0,
            stats.download_rate as f64 / 1024.0,
            stats.num_seeds,
            stats.num_peers,
        );

        if stats.is_seeding || stats.is_finished {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    println!("Download complete!");
    session.shutdown().await?;
    Ok(())
}
