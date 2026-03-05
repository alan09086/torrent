use std::path::Path;

pub fn run(path: &Path) -> anyhow::Result<()> {
    let data = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;

    let meta = torrent::core::torrent_from_bytes_any(&data)
        .map_err(|e| anyhow::anyhow!("failed to parse torrent: {e}"))?;

    let hashes = meta.info_hashes();
    let version = meta.version();

    println!("Name:         {}", meta_name(&meta));
    if let Some(v1) = hashes.v1 {
        println!("Info hash v1: {v1}");
    }
    if let Some(v2) = hashes.v2 {
        println!("Info hash v2: {v2}");
    }
    println!("Version:      {version:?}");

    match &meta {
        torrent::core::TorrentMeta::V1(t) => print_v1_details(t),
        torrent::core::TorrentMeta::V2(t) => print_v2_details(t),
        torrent::core::TorrentMeta::Hybrid(v1, v2) => {
            print_v1_details(v1);
            // Also show v2 file tree if different
            let v2_files = v2.info.files();
            if !v2_files.is_empty() {
                println!("V2 files:     {}", v2_files.len());
            }
        }
    }

    Ok(())
}

fn meta_name(meta: &torrent::core::TorrentMeta) -> &str {
    match meta {
        torrent::core::TorrentMeta::V1(t) => &t.info.name,
        torrent::core::TorrentMeta::V2(t) => &t.info.name,
        torrent::core::TorrentMeta::Hybrid(v1, _) => &v1.info.name,
    }
}

fn print_v1_details(t: &torrent::core::TorrentMetaV1) {
    let total_size = if let Some(len) = t.info.length {
        len
    } else if let Some(ref files) = t.info.files {
        files.iter().map(|f| f.length).sum()
    } else {
        0
    };

    let pieces = t.info.pieces.len() / 20;

    println!("Total size:   {}", format_size(total_size));
    println!("Piece size:   {}", format_size(t.info.piece_length));
    println!("Pieces:       {pieces}");

    if let Some(ref files) = t.info.files {
        println!("Files:        {}", files.len());
        for f in files {
            let path = f.path.join("/");
            println!("  {path} ({})", format_size(f.length));
        }
    } else {
        println!("Files:        1");
        println!("  {} ({})", t.info.name, format_size(total_size));
    }

    print_trackers(t.announce.as_deref(), t.announce_list.as_deref());

    println!(
        "Private:      {}",
        if t.info.private == Some(1) { "yes" } else { "no" }
    );

    if let Some(ts) = t.creation_date {
        println!("Created:      {ts} (unix)");
    }
    if let Some(ref c) = t.comment {
        println!("Comment:      {c}");
    }
    if let Some(ref by) = t.created_by {
        println!("Created by:   {by}");
    }
}

fn print_v2_details(t: &torrent::core::TorrentMetaV2) {
    let total_size: u64 = t.info.files().iter().map(|f| f.attr.length).sum();
    println!("Total size:   {}", format_size(total_size));
    println!("Piece size:   {}", format_size(t.info.piece_length));

    let files = t.info.files();
    println!("Files:        {}", files.len());
    for f in &files {
        let path = f.path.join("/");
        println!("  {path} ({})", format_size(f.attr.length));
    }

    print_trackers(t.announce.as_deref(), t.announce_list.as_deref());

    if let Some(ts) = t.creation_date {
        println!("Created:      {ts} (unix)");
    }
    if let Some(ref c) = t.comment {
        println!("Comment:      {c}");
    }
    if let Some(ref by) = t.created_by {
        println!("Created by:   {by}");
    }
}

fn print_trackers(announce: Option<&str>, announce_list: Option<&[Vec<String>]>) {
    if let Some(tiers) = announce_list {
        println!("Trackers:");
        for tier in tiers {
            for url in tier {
                println!("  {url}");
            }
        }
    } else if let Some(url) = announce {
        println!("Trackers:");
        println!("  {url}");
    }
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}
