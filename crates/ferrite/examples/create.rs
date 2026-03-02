//! Create a .torrent file from a file or directory.
//!
//! Usage: `cargo run --example create -- <path> [-o output.torrent]`

use ferrite::prelude::*;
use std::env;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let path = env::args()
        .nth(1)
        .expect("usage: create <path> [-o output.torrent]");
    let output = env::args()
        .position(|a| a == "-o")
        .and_then(|i| env::args().nth(i + 1))
        .unwrap_or_else(|| "output.torrent".into());

    let result = CreateTorrent::new().add_directory(&path).generate()?;

    std::fs::write(&output, &result.bytes)?;
    println!("Created {} ({} bytes)", output, result.bytes.len());
    Ok(())
}
