mod create;
mod download;
mod info;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "torrent", about = "BitTorrent client powered by torrent")]
struct Cli {
    /// Log level (error, warn, info, debug, trace)
    #[arg(short, long, default_value = "warn")]
    log_level: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download a torrent from a magnet link or .torrent file
    Download {
        /// Magnet URI or path to .torrent file
        source: String,
        /// Output directory
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
        /// Download pieces sequentially
        #[arg(long)]
        sequential: bool,
        /// Disable DHT
        #[arg(long)]
        no_dht: bool,
        /// Path to JSON settings file
        #[arg(long)]
        config: Option<PathBuf>,
        /// Seed after completion (default: exit on completion)
        #[arg(long)]
        seed: bool,
        /// Listen port
        #[arg(short, long, default_value = "6881")]
        port: u16,
        /// Quiet mode — suppress progress bar
        #[arg(short, long)]
        quiet: bool,
    },
    /// Create a .torrent file
    Create {
        /// Path to file or directory
        path: PathBuf,
        /// Output .torrent file path
        #[arg(short, long, default_value = "output.torrent")]
        output: PathBuf,
        /// Tracker URL(s) — can specify multiple: -t url1 -t url2
        #[arg(short, long)]
        tracker: Vec<String>,
        /// Create as private torrent
        #[arg(long)]
        private: bool,
        /// Piece size in KiB (auto-selected if omitted)
        #[arg(long)]
        piece_size: Option<u64>,
    },
    /// Display torrent file information
    Info {
        /// Path to .torrent file
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log_level.parse().unwrap_or_else(|_| "warn".into())),
        )
        .with_target(false)
        .init();

    let result = match cli.command {
        Command::Download {
            source,
            output,
            sequential: _,
            no_dht,
            config,
            seed,
            port,
            quiet,
        } => {
            download::run(download::DownloadOpts {
                source: &source,
                output: &output,
                no_dht,
                config: config.as_deref(),
                seed,
                port,
                quiet,
            })
            .await
        }
        Command::Create {
            path,
            output,
            tracker,
            private,
            piece_size,
        } => create::run(&path, &output, &tracker, private, piece_size),
        Command::Info { path } => info::run(&path),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
