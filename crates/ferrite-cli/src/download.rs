use anyhow::Context;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
    let DownloadOpts { source, output, no_dht, config, seed, port, quiet } = opts;
    // Load settings
    let mut builder = if let Some(config_path) = config {
        let data = std::fs::read_to_string(config_path)
            .with_context(|| format!("failed to read config: {}", config_path.display()))?;
        let settings: ferrite::session::Settings = serde_json::from_str(&data)
            .with_context(|| "failed to parse settings JSON")?;
        let mut b = ferrite::ClientBuilder::new();
        b = b.listen_port(settings.listen_port);
        b
    } else {
        ferrite::ClientBuilder::new()
    };

    builder = builder.listen_port(port).download_dir(output);

    if no_dht {
        builder = builder.enable_dht(false);
    }

    let session = builder.start().await?;

    // Subscribe for all alerts (raw broadcast receiver so we can try_recv)
    let mut alerts = session.subscribe();

    // Parse source and add torrent
    let info_hash = if source.starts_with("magnet:") {
        let magnet = ferrite::core::Magnet::parse(source)
            .map_err(|e| anyhow::anyhow!("invalid magnet URI: {e}"))?;
        if let Some(ref name) = magnet.display_name {
            eprintln!("Adding: {name}");
        }
        session.add_magnet(magnet).await?
    } else {
        let data = std::fs::read(source)
            .with_context(|| format!("failed to read torrent file: {source}"))?;
        let meta = ferrite::core::torrent_from_bytes_any(&data)
            .map_err(|e| anyhow::anyhow!("failed to parse torrent: {e}"))?;
        let ih = meta.info_hashes().best_v1();
        session.add_torrent(meta, None).await?;
        ih
    };

    // Ctrl-C handler
    let shutdown = Arc::new(AtomicBool::new(false));
    let s = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        s.store(true, Ordering::SeqCst);
    });

    // Progress bar
    let pb = if quiet {
        None
    } else {
        let pb = ProgressBar::new(100);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{bar:40.cyan/blue}] {msg}"
            )
            .unwrap()
            .progress_chars("#>-"),
        );
        Some(pb)
    };

    // Poll loop
    let mut finished = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            if let Some(ref pb) = pb {
                pb.finish_with_message("shutting down...");
            }
            eprintln!("\nShutting down...");
            session.shutdown().await?;
            tokio::time::sleep(Duration::from_secs(1)).await;
            break;
        }

        // Drain alerts (non-blocking)
        while let Ok(alert) = alerts.try_recv() {
            if let ferrite::session::AlertKind::TorrentFinished { info_hash: ih } = alert.kind
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
            if seed {
                eprintln!("Seeding... press Ctrl-C to stop.");
                tokio::signal::ctrl_c().await?;
                eprintln!("\nShutting down...");
            }
            session.shutdown().await?;
            tokio::time::sleep(Duration::from_secs(1)).await;
            break;
        }

        // Update progress
        if let Ok(stats) = session.torrent_stats(info_hash).await
            && let Some(ref pb) = pb
        {
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

        tokio::time::sleep(Duration::from_millis(500)).await;
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
