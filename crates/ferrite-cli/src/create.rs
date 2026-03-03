use std::path::Path;

pub fn run(
    path: &Path,
    output: &Path,
    trackers: &[String],
    private: bool,
    piece_size: Option<u64>,
) -> anyhow::Result<()> {
    let mut builder = ferrite::core::CreateTorrent::new();

    if path.is_dir() {
        builder = builder.add_directory(path);
    } else {
        builder = builder.add_file(path);
    }

    for (i, url) in trackers.iter().enumerate() {
        builder = builder.add_tracker(url, i);
    }

    if private {
        builder = builder.set_private(true);
    }

    if let Some(size_kib) = piece_size {
        builder = builder.set_piece_size(size_kib * 1024);
    }

    let result = builder.generate_with_progress(|current, total| {
        if total > 0 {
            eprint!("\rHashing: {current}/{total} pieces");
        }
    })?;
    eprintln!();

    std::fs::write(output, &result.bytes)?;

    let hashes = result.meta.info_hashes();
    eprintln!("Created: {}", output.display());
    if let Some(v1) = hashes.v1 {
        eprintln!("Info hash v1: {v1}");
    }
    if let Some(v2) = hashes.v2 {
        eprintln!("Info hash v2: {v2}");
    }
    if let Some(v1) = result.meta.as_v1() {
        eprintln!("Pieces: {}", v1.info.pieces.len() / 20);
    }

    Ok(())
}
