//! Stream a file from a torrent (AsyncRead + AsyncSeek).
//!
//! Usage: `cargo run --example stream -- "magnet:?xt=urn:btih:..."`

use torrent::prelude::*;
use std::env;
use tokio::io::AsyncReadExt;

#[tokio::main]
async fn main() -> torrent::Result<()> {
    let magnet_uri = env::args().nth(1).expect("usage: stream <magnet-uri>");
    let magnet = Magnet::parse(&magnet_uri)?;

    let session = ClientBuilder::new()
        .download_dir(".")
        .start()
        .await?;

    let info_hash = session.add_magnet(magnet).await?;

    // Wait for metadata
    loop {
        let stats = session.torrent_stats(info_hash).await?;
        if !matches!(stats.state, TorrentState::FetchingMetadata) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Open a FileStream for the first file
    let mut stream = session.open_file(info_hash, 0).await?;
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    println!("Read {} bytes from stream", n);

    session.shutdown().await?;
    Ok(())
}
