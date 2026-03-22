mod create;
mod download;
mod info;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "torrent", version, about = "BitTorrent client")]
struct Cli {
    /// Log level (error, warn, info, debug, trace)
    #[arg(short, long, default_value = "error")]
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
        /// Disable DHT
        #[arg(long)]
        no_dht: bool,
        /// Path to JSON settings file
        #[arg(long)]
        config: Option<PathBuf>,
        /// Seed after completion instead of exiting
        #[arg(long)]
        seed: bool,
        /// Listen port
        #[arg(short, long, default_value = "42020")]
        port: u16,
        /// Quiet mode — suppress progress output
        #[arg(short, long)]
        quiet: bool,
        /// Number of tokio worker threads (0 = auto)
        #[arg(long, default_value_t = 0)]
        workers: usize,
        /// Disable core affinity pinning
        #[arg(long)]
        no_pin_cores: bool,
        /// Overwrite existing files
        #[arg(long)]
        overwrite: bool,
        /// Only list torrent contents, don't download
        #[arg(short, long)]
        list: bool,
        /// Initial peers to connect to (host:port)
        #[arg(long)]
        initial_peers: Vec<String>,
        /// Disable tracker announces
        #[arg(long)]
        disable_trackers: bool,
        /// Use io_uring for disk writes (Linux only, requires io-uring feature)
        #[arg(long)]
        io_uring: bool,
        /// Enable O_DIRECT for io_uring writes (implies --io-uring)
        #[arg(long)]
        direct_io: bool,
        /// io_uring submission queue depth (default: 256)
        #[arg(long)]
        uring_sq_depth: Option<u32>,
        /// HTTP API port (0 = disabled)
        #[arg(long, default_value_t = 0)]
        api_port: u16,
        /// HTTP API bind address
        #[arg(long, default_value = "127.0.0.1")]
        api_bind: String,
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
                .unwrap_or_else(|_| cli.log_level.parse().unwrap_or_else(|_| "error".into())),
        )
        .with_target(false)
        .init();

    let exit_code = match cli.command {
        Command::Download {
            source,
            output,
            no_dht,
            config,
            seed,
            port,
            quiet,
            workers,
            no_pin_cores,
            overwrite: _,
            list: _,
            initial_peers: _,
            disable_trackers: _,
            io_uring,
            direct_io,
            uring_sq_depth,
            api_port,
            api_bind,
        } => {
            let mut settings = if let Some(ref config_path) = config {
                let data = std::fs::read_to_string(config_path).unwrap_or_else(|e| {
                    eprintln!("error: failed to read config {}: {e}", config_path.display());
                    std::process::exit(1);
                });
                let s: torrent::session::Settings =
                    serde_json::from_str(&data).unwrap_or_else(|e| {
                        eprintln!("error: failed to parse settings JSON: {e}");
                        std::process::exit(1);
                    });
                if let Err(e) = s.validate() {
                    eprintln!("error: invalid settings: {e}");
                    std::process::exit(1);
                }
                s
            } else {
                torrent::session::Settings::default()
            };

            if workers != 0 {
                settings.runtime_worker_threads = workers;
            }
            if no_pin_cores {
                settings.pin_cores = false;
            }
            if io_uring || direct_io {
                settings.storage_mode = torrent::core::StorageMode::IoUring;
            }
            if direct_io {
                settings.io_uring_direct_io = true;
            }
            if let Some(depth) = uring_sq_depth {
                settings.io_uring_sq_depth = depth;
                if !io_uring && !direct_io {
                    settings.storage_mode = torrent::core::StorageMode::IoUring;
                }
            }

            let rt = download::build_runtime(&settings);
            let result = rt.block_on(download::run(download::DownloadOpts {
                source: &source,
                output: &output,
                no_dht,
                seed,
                port,
                quiet,
                settings,
                api_port,
                api_bind,
            }));

            // Force-shutdown the runtime like rqbit does — kills any dangling
            // tasks (peer connections, DHT, etc.) that would otherwise hang.
            rt.shutdown_timeout(Duration::from_secs(1));

            match result {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        Command::Create {
            path,
            output,
            tracker,
            private,
            piece_size,
        } => match create::run(&path, &output, &tracker, private, piece_size) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        },
        Command::Info { path } => match info::run(&path) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        },
    };

    std::process::exit(exit_code);
}
