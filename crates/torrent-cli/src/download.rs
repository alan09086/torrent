use anyhow::Context;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use torrent::core::{DEFAULT_CHUNK_SIZE, Lengths, TorrentMeta};
use torrent::session::SessionState;
use torrent::storage::{FilesystemStorage, TorrentStorage};

pub struct DownloadOpts<'a> {
    pub source: &'a str,
    pub output: &'a Path,
    pub no_dht: bool,
    pub config: Option<&'a Path>,
    pub seed: bool,
    pub port: u16,
    pub quiet: bool,
}

pub async fn run(opts: DownloadOpts<'_>) -> anyhow::Result<()> {
    let DownloadOpts {
        source,
        output,
        no_dht,
        config,
        seed,
        port,
        quiet,
    } = opts;

    // Global state file for DHT node persistence across sessions
    let state_path = state_file_path();

    // Load settings
    let mut builder = if let Some(config_path) = config {
        let data = std::fs::read_to_string(config_path)
            .with_context(|| format!("failed to read config: {}", config_path.display()))?;
        let settings: torrent::session::Settings =
            serde_json::from_str(&data).with_context(|| "failed to parse settings JSON")?;
        settings.validate().with_context(|| "invalid settings")?;
        torrent::ClientBuilder::from_settings(settings)
    } else {
        torrent::ClientBuilder::new()
    };

    builder = builder.listen_port(port).download_dir(output);

    if no_dht {
        builder = builder.enable_dht(false);
    }

    // Load saved DHT state from previous session for instant peer discovery
    if let Some((saved_nodes, saved_node_id)) = load_dht_state(&state_path) {
        if !quiet {
            eprintln!(
                "Loaded {} saved DHT nodes from previous session",
                saved_nodes.len()
            );
        }
        builder = builder.dht_saved_nodes(saved_nodes);
        if let Some(id) = saved_node_id {
            builder = builder.dht_node_id(id);
        }
    }

    let session = builder.start().await?;

    // Subscribe for all alerts (raw broadcast receiver so we can try_recv)
    let mut alerts = session.subscribe();

    // Parse source and add torrent
    let info_hash = if source.starts_with("magnet:") {
        let magnet = torrent::core::Magnet::parse(source)
            .map_err(|e| anyhow::anyhow!("invalid magnet URI: {e}"))?;
        if let Some(ref name) = magnet.display_name {
            eprintln!("Adding: {name}");
        }
        session.add_magnet(magnet).await?
    } else {
        let data = std::fs::read(source)
            .with_context(|| format!("failed to read torrent file: {source}"))?;
        let meta = torrent::core::torrent_from_bytes_any(&data)
            .map_err(|e| anyhow::anyhow!("failed to parse torrent: {e}"))?;
        let ih = meta.info_hashes().best_v1();
        let storage = make_filesystem_storage(&meta, output)?;
        session.add_torrent(meta, Some(storage)).await?;
        ih
    };

    // Shutdown on SIGINT (Ctrl-C) or SIGTERM (kill / systemd stop / benchmark timeout)
    let shutdown = Arc::new(AtomicBool::new(false));
    let s = shutdown.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
        s.store(true, Ordering::SeqCst);
    });

    // Progress bar
    let pb = if quiet {
        None
    } else {
        let pb = ProgressBar::new(100);
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} [{bar:40.cyan/blue}] {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );
        Some(pb)
    };

    // Poll loop
    let mut finished = false;
    let mut peak_peers: usize = 0;
    let mut last_download_rate: u64 = 0;
    let mut last_save = Instant::now();
    const SAVE_INTERVAL: Duration = Duration::from_secs(60);
    loop {
        if shutdown.load(Ordering::SeqCst) {
            if let Some(ref pb) = pb {
                pb.finish_with_message("shutting down...");
            }
            eprintln!("\nShutting down...");
            save_session_state(&session, &state_path, quiet).await;
            session.shutdown().await?;
            tokio::time::sleep(Duration::from_secs(1)).await;
            break;
        }

        // Drain alerts (non-blocking)
        while let Ok(alert) = alerts.try_recv() {
            if let torrent::session::AlertKind::TorrentFinished { info_hash: ih } = alert.kind
                && ih == info_hash
            {
                finished = true;
            }
        }

        if finished {
            if let Some(ref pb) = pb {
                pb.set_position(100);
                pb.finish_with_message("download complete!");
            }
            eprintln!(
                "torrent: peak_peers={peak_peers} final_speed={:.1}MB/s",
                last_download_rate as f64 / 1_048_576.0
            );
            if seed {
                eprintln!("Seeding... press Ctrl-C to stop.");
                tokio::signal::ctrl_c().await?;
                eprintln!("\nShutting down...");
            }
            save_session_state(&session, &state_path, quiet).await;
            session.shutdown().await?;
            tokio::time::sleep(Duration::from_secs(1)).await;
            break;
        }

        // Update progress
        if let Ok(stats) = session.torrent_stats(info_hash).await {
            peak_peers = peak_peers.max(stats.peers_connected);
            last_download_rate = stats.download_rate;

            // Fallback completion check via progress (in case alert is missed)
            if stats.progress >= 1.0 {
                finished = true;
            }

            if let Some(ref pb) = pb {
                let pct = (stats.progress * 100.0) as u64;
                pb.set_position(pct);

                let done = format_size(stats.total_done);
                let total = format_size(stats.total_wanted);
                let down_rate = format_rate(stats.download_rate);
                let up_rate = format_rate(stats.upload_rate);
                let peers = stats.peers_connected;

                pb.set_message(format!(
                    "{:.1}% | {done}/{total} | down {down_rate} up {up_rate} | {peers} peers",
                    stats.progress * 100.0,
                ));
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;

        if last_save.elapsed() >= SAVE_INTERVAL {
            save_session_state(&session, &state_path, quiet).await;
            last_save = Instant::now();
        }
    }

    Ok(())
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn make_filesystem_storage(
    meta: &TorrentMeta,
    output: &Path,
) -> anyhow::Result<Arc<dyn TorrentStorage>> {
    let (file_paths, file_lengths, total_length, piece_length) = if let Some(v1) = meta.as_v1() {
        let files = v1.info.files();
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|f| f.path.iter().collect::<PathBuf>())
            .collect();
        let lengths: Vec<u64> = files.iter().map(|f| f.length).collect();
        (paths, lengths, v1.info.total_length(), v1.info.piece_length)
    } else if let Some(v2) = meta.as_v2() {
        let files = v2.info.files();
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|f| f.path.iter().collect::<PathBuf>())
            .collect();
        let lengths: Vec<u64> = files.iter().map(|f| f.attr.length).collect();
        (paths, lengths, v2.info.total_length(), v2.info.piece_length)
    } else {
        anyhow::bail!("torrent has no file metadata");
    };

    let lengths_calc = Lengths::new(total_length, piece_length, DEFAULT_CHUNK_SIZE);
    let storage =
        FilesystemStorage::new(output, file_paths, file_lengths, lengths_calc, None, false)
            .map_err(|e| anyhow::anyhow!("failed to create storage: {e}"))?;
    Ok(Arc::new(storage))
}

fn format_rate(bytes_per_sec: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;

    if bytes_per_sec >= MIB {
        format!("{:.1} MB/s", bytes_per_sec as f64 / MIB as f64)
    } else if bytes_per_sec >= KIB {
        format!("{:.1} KB/s", bytes_per_sec as f64 / KIB as f64)
    } else {
        format!("{bytes_per_sec} B/s")
    }
}

/// Return the global state file path (`$XDG_DATA_HOME/torrent/session.dat`).
///
/// Respects `XDG_DATA_HOME`, falls back to `~/.local/share/torrent`.
/// Creates the parent directory if it doesn't exist.
fn state_file_path() -> PathBuf {
    let dir = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("torrent");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("session.dat")
}

/// Load DHT state from a previously saved state file.
///
/// Returns `None` if the file doesn't exist or can't be parsed.
/// Returns (nodes, optional_node_id).
fn load_dht_state(state_path: &Path) -> Option<(Vec<String>, Option<torrent::core::Id20>)> {
    let data = std::fs::read(state_path).ok()?;
    let state: SessionState = torrent::bencode::from_bytes(&data).ok()?;
    if state.dht_nodes.is_empty() {
        return None;
    }
    let nodes: Vec<String> = state
        .dht_nodes
        .iter()
        .map(|entry| format!("{}:{}", entry.host, entry.port))
        .collect();
    let node_id = state
        .dht_node_id
        .and_then(|hex| torrent::core::Id20::from_hex(&hex).ok());
    Some((nodes, node_id))
}

pub(crate) fn build_runtime(settings: &torrent::session::Settings) -> tokio::runtime::Runtime {
    let worker_count = if settings.runtime_worker_threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    } else {
        settings.runtime_worker_threads
    };

    let pin = settings.pin_cores;
    let core_ids = if pin {
        core_affinity::get_core_ids().unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(worker_count);
    builder.enable_all();

    if pin && !core_ids.is_empty() {
        let core_ids = std::sync::Arc::new(core_ids);
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        builder.on_thread_start(move || {
            let idx = counter.fetch_add(1, Ordering::Relaxed);
            let core = core_ids[idx % core_ids.len()];
            if !core_affinity::set_for_current(core) {
                eprintln!("warning: failed to set core affinity for worker {idx}");
            }
        });
    }

    builder.build().expect("failed to build tokio runtime")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_affinity_available() {
        let core_ids = core_affinity::get_core_ids();
        assert!(
            core_ids.as_ref().is_some_and(|ids| !ids.is_empty()),
            "expected get_core_ids() to return a non-empty list, got: {core_ids:?}"
        );
    }

    #[test]
    fn build_runtime_creates_runtime() {
        let mut settings = torrent::session::Settings::default();
        settings.runtime_worker_threads = 2;
        settings.pin_cores = true;
        let rt = build_runtime(&settings);
        let result = rt.block_on(async { 42 });
        assert_eq!(result, 42);
    }

    #[test]
    fn build_runtime_no_pin() {
        let mut settings = torrent::session::Settings::default();
        settings.runtime_worker_threads = 2;
        settings.pin_cores = false;
        let rt = build_runtime(&settings);
        let result = rt.block_on(async { 42 });
        assert_eq!(result, 42);
    }

    #[test]
    fn build_runtime_auto_workers() {
        let mut settings = torrent::session::Settings::default();
        settings.runtime_worker_threads = 0; // auto-detect
        settings.pin_cores = false;
        let rt = build_runtime(&settings);
        let result = rt.block_on(async { 42 });
        assert_eq!(result, 42);
    }
}

/// Save session state (DHT nodes, etc.) to the state file.
async fn save_session_state(
    session: &torrent::session::SessionHandle,
    state_path: &Path,
    quiet: bool,
) {
    match session.save_session_state().await {
        Ok(state) => {
            if !quiet && !state.dht_nodes.is_empty() {
                eprintln!(
                    "Saving {} DHT nodes for next session",
                    state.dht_nodes.len()
                );
            }
            match torrent::bencode::to_bytes(&state) {
                Ok(bytes) => {
                    let tmp_path = state_path.with_extension("dat.tmp");
                    if let Err(e) = std::fs::write(&tmp_path, &bytes) {
                        eprintln!("Warning: failed to write state file: {e}");
                    } else if let Err(e) = std::fs::rename(&tmp_path, state_path) {
                        eprintln!("Warning: failed to rename state file: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("Warning: failed to encode session state: {e}");
                }
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to save session state: {e}");
        }
    }
}
