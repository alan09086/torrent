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
        /// Number of tokio worker threads (0 = auto)
        #[arg(long, default_value_t = 0)]
        workers: usize,
        /// Disable core affinity pinning
        #[arg(long)]
        no_pin_cores: bool,
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

fn main() {
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
            workers,
            no_pin_cores,
        } => {
            // Load config once, apply CLI overrides
            let mut settings = if let Some(ref config_path) = config {
                let data = std::fs::read_to_string(config_path)
                    .unwrap_or_else(|e| {
                        eprintln!("Error: failed to read config {}: {e}", config_path.display());
                        std::process::exit(1);
                    });
                let s: torrent::session::Settings = serde_json::from_str(&data)
                    .unwrap_or_else(|e| {
                        eprintln!("Error: failed to parse settings JSON: {e}");
                        std::process::exit(1);
                    });
                if let Err(e) = s.validate() {
                    eprintln!("Error: invalid settings: {e}");
                    std::process::exit(1);
                }
                s
            } else {
                torrent::session::Settings::default()
            };

            // Apply CLI overrides
            if workers != 0 {
                settings.runtime_worker_threads = workers;
            }
            if no_pin_cores {
                settings.pin_cores = false;
            }

            // Build runtime with core pinning
            let rt = download::build_runtime(&settings);
            rt.block_on(download::run(download::DownloadOpts {
                source: &source,
                output: &output,
                no_dht,
                seed,
                port,
                quiet,
                settings,
            }))
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
