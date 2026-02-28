//! Torrent creation (BEP 3, 12, 17, 19, 27, 47).
//!
//! Create `.torrent` files from local files or directories using a builder API.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::hash::Id20;
use crate::metainfo::{FileEntry, InfoDict, TorrentMetaV1};

/// Auto-select piece size based on total content size (libtorrent-style).
pub fn auto_piece_size(total: u64) -> u64 {
    if total <= 10_485_760 {
        32 * 1024                              // ≤ 10 MiB → 32 KiB
    } else if total <= 104_857_600 {
        64 * 1024                              // ≤ 100 MiB → 64 KiB
    } else if total <= 1_073_741_824 {
        256 * 1024                             // ≤ 1 GiB → 256 KiB
    } else if total <= 10_737_418_240 {
        512 * 1024                             // ≤ 10 GiB → 512 KiB
    } else if total <= 107_374_182_400 {
        1024 * 1024                            // ≤ 100 GiB → 1 MiB
    } else if total <= 1_099_511_627_776 {
        2 * 1024 * 1024                        // ≤ 1 TiB → 2 MiB
    } else {
        4 * 1024 * 1024                        // > 1 TiB → 4 MiB
    }
}

/// Collected info about a file to include in the torrent.
struct InputFile {
    /// Absolute path on disk (empty for pad files).
    disk_path: PathBuf,
    /// Path components within the torrent.
    torrent_path: Vec<String>,
    /// File length in bytes.
    length: u64,
    /// Modification time (unix timestamp).
    mtime: Option<i64>,
    /// BEP 47 file attributes.
    attr: Option<String>,
    /// Symlink target path components.
    symlink_path: Option<Vec<String>>,
    /// Whether this is a pad file.
    is_pad: bool,
}

/// Result of torrent creation.
#[derive(Debug)]
pub struct CreateTorrentResult {
    /// Parsed torrent metadata.
    pub meta: TorrentMetaV1,
    /// Raw `.torrent` file bytes.
    pub bytes: Vec<u8>,
}

/// Serializable wrapper for the outer `.torrent` dict.
#[derive(Serialize)]
struct TorrentOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    announce: Option<String>,
    #[serde(rename = "announce-list", skip_serializing_if = "Option::is_none")]
    announce_list: Option<Vec<Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    #[serde(rename = "created by", skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
    #[serde(rename = "creation date", skip_serializing_if = "Option::is_none")]
    creation_date: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    httpseeds: Option<Vec<String>>,
    info: InfoDict,
    #[serde(skip_serializing_if = "Option::is_none")]
    nodes: Option<Vec<(String, u16)>>,
    #[serde(rename = "url-list", skip_serializing_if = "Option::is_none")]
    url_list: Option<Vec<String>>,
}

/// Builder for creating `.torrent` files.
pub struct CreateTorrent {
    files: Vec<InputFile>,
    name: Option<String>,
    piece_size: Option<u64>,
    comment: Option<String>,
    creator: Option<String>,
    creation_date: Option<i64>,
    private: bool,
    source: Option<String>,
    trackers: Vec<(String, usize)>,
    web_seeds: Vec<String>,
    http_seeds: Vec<String>,
    dht_nodes: Vec<(String, u16)>,
    pad_file_limit: Option<u64>,
    include_mtime: bool,
    include_symlinks: bool,
    pre_hashes: HashMap<u32, Id20>,
}

impl CreateTorrent {
    /// Create a new torrent builder.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            name: None,
            piece_size: None,
            comment: None,
            creator: None,
            creation_date: None,
            private: false,
            source: None,
            trackers: Vec::new(),
            web_seeds: Vec::new(),
            http_seeds: Vec::new(),
            dht_nodes: Vec::new(),
            pad_file_limit: None,
            include_mtime: false,
            include_symlinks: false,
            pre_hashes: HashMap::new(),
        }
    }

    /// Add a single file to the torrent.
    pub fn add_file(mut self, path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        if let Ok(canonical) = fs::canonicalize(path)
            && let Ok(meta) = fs::metadata(&canonical)
        {
            let file_name = canonical
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let mtime = if self.include_mtime {
                meta.modified().ok().and_then(|t| {
                    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
                })
            } else {
                None
            };
            let attr = detect_attr(&canonical, &meta);
            self.files.push(InputFile {
                disk_path: canonical,
                torrent_path: vec![file_name],
                length: meta.len(),
                mtime,
                attr,
                symlink_path: None,
                is_pad: false,
            });
        }
        self
    }

    /// Add all files from a directory recursively.
    pub fn add_directory(mut self, path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        if let Ok(canonical) = fs::canonicalize(path) {
            let mut files = Vec::new();
            walk_directory(&canonical, &[], &mut files, self.include_mtime, self.include_symlinks);
            files.sort_by(|a, b| a.torrent_path.cmp(&b.torrent_path));
            self.files.extend(files);
        }
        self
    }

    /// Set the torrent name (defaults to the file/directory name).
    pub fn set_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the piece size in bytes (must be a power of 2, ≥ 16384).
    pub fn set_piece_size(mut self, bytes: u64) -> Self {
        self.piece_size = Some(bytes);
        self
    }

    /// Set the comment field.
    pub fn set_comment(mut self, s: impl Into<String>) -> Self {
        self.comment = Some(s.into());
        self
    }

    /// Set the creator field.
    pub fn set_creator(mut self, s: impl Into<String>) -> Self {
        self.creator = Some(s.into());
        self
    }

    /// Set the creation date (unix timestamp). Defaults to current time.
    pub fn set_creation_date(mut self, ts: i64) -> Self {
        self.creation_date = Some(ts);
        self
    }

    /// Set the private flag (BEP 27).
    pub fn set_private(mut self, private: bool) -> Self {
        self.private = private;
        self
    }

    /// Set the source tag (private tracker identification).
    pub fn set_source(mut self, s: impl Into<String>) -> Self {
        self.source = Some(s.into());
        self
    }

    /// Add a tracker URL at the given tier.
    pub fn add_tracker(mut self, url: impl Into<String>, tier: usize) -> Self {
        self.trackers.push((url.into(), tier));
        self
    }

    /// Add a BEP 19 web seed URL (GetRight-style).
    pub fn add_web_seed(mut self, url: impl Into<String>) -> Self {
        self.web_seeds.push(url.into());
        self
    }

    /// Add a BEP 17 HTTP seed URL (Hoffman-style).
    pub fn add_http_seed(mut self, url: impl Into<String>) -> Self {
        self.http_seeds.push(url.into());
        self
    }

    /// Add a DHT bootstrap node (BEP 5).
    pub fn add_dht_node(mut self, host: impl Into<String>, port: u16) -> Self {
        self.dht_nodes.push((host.into(), port));
        self
    }

    /// Set pad file alignment (BEP 47).
    ///
    /// - `None` — no padding (default)
    /// - `Some(0)` — pad after every file
    /// - `Some(n)` — pad after files with length > n
    pub fn set_pad_file_limit(mut self, limit: Option<u64>) -> Self {
        self.pad_file_limit = limit;
        self
    }

    /// Include file modification times in the torrent.
    pub fn include_mtime(mut self, enabled: bool) -> Self {
        self.include_mtime = enabled;
        self
    }

    /// Follow and record symlinks.
    pub fn include_symlinks(mut self, enabled: bool) -> Self {
        self.include_symlinks = enabled;
        self
    }

    /// Set a pre-computed SHA1 hash for a piece (skips disk read during generation).
    pub fn set_hash(mut self, piece: u32, hash: Id20) -> Self {
        self.pre_hashes.insert(piece, hash);
        self
    }

    /// Generate the `.torrent` file.
    pub fn generate(self) -> Result<CreateTorrentResult> {
        self.generate_with_progress(|_, _| {})
    }

    /// Generate the `.torrent` file with a progress callback `(current_piece, total_pieces)`.
    pub fn generate_with_progress(
        self,
        mut cb: impl FnMut(usize, usize),
    ) -> Result<CreateTorrentResult> {
        if self.files.is_empty() {
            return Err(Error::CreateTorrent("no files added".into()));
        }

        // Validate piece size if explicitly set
        if let Some(ps) = self.piece_size
            && (ps < 16384 || !ps.is_power_of_two())
        {
            return Err(Error::CreateTorrent(
                "piece size must be a power of 2 and at least 16384".into(),
            ));
        }

        // Determine name
        let name = self.name.unwrap_or_else(|| {
            self.files[0]
                .torrent_path
                .first()
                .cloned()
                .unwrap_or_else(|| "torrent".into())
        });

        let is_single_file = self.files.len() == 1 && !self.files[0].is_pad;

        // Build file list with pad files
        let files_with_pads = if is_single_file {
            self.files
        } else {
            insert_pad_files(self.files, self.pad_file_limit, self.piece_size)
        };

        // Compute total size and piece size
        let total_size: u64 = files_with_pads.iter().map(|f| f.length).sum();
        let piece_size = self.piece_size.unwrap_or_else(|| auto_piece_size(total_size));
        let num_pieces = if total_size == 0 {
            0
        } else {
            total_size.div_ceil(piece_size) as usize
        };

        // Hash pieces
        let mut pieces = Vec::with_capacity(num_pieces * 20);
        let mut piece_buf = vec![0u8; piece_size as usize];
        let mut current_file_idx = 0;
        let mut current_file_offset = 0u64;
        let mut current_file_handle: Option<fs::File> = None;
        let mut piece_index = 0u32;

        while (piece_index as usize) < num_pieces {
            // Check for pre-computed hash
            if let Some(&hash) = self.pre_hashes.get(&piece_index) {
                // Skip disk reads for this piece — advance file cursors
                let remaining_in_piece = if (piece_index as usize) == num_pieces - 1 {
                    (total_size - (piece_index as u64) * piece_size) as usize
                } else {
                    piece_size as usize
                };
                advance_cursors(
                    &files_with_pads,
                    remaining_in_piece,
                    &mut current_file_idx,
                    &mut current_file_offset,
                    &mut current_file_handle,
                );
                pieces.extend_from_slice(hash.as_bytes());
                cb(piece_index as usize + 1, num_pieces);
                piece_index += 1;
                continue;
            }

            // Fill piece buffer from files
            let mut buf_offset = 0;
            let piece_end = if (piece_index as usize) == num_pieces - 1 {
                (total_size - (piece_index as u64) * piece_size) as usize
            } else {
                piece_size as usize
            };

            while buf_offset < piece_end {
                if current_file_idx >= files_with_pads.len() {
                    break;
                }
                let file = &files_with_pads[current_file_idx];
                let remaining_in_file = file.length - current_file_offset;
                let to_read = (piece_end - buf_offset).min(remaining_in_file as usize);

                if file.is_pad {
                    // Pad files are implicit zeros
                    piece_buf[buf_offset..buf_offset + to_read].fill(0);
                } else {
                    // Open file handle if needed
                    if current_file_handle.is_none() {
                        current_file_handle = Some(fs::File::open(&file.disk_path)?);
                        if current_file_offset > 0 {
                            use std::io::Seek;
                            current_file_handle
                                .as_mut()
                                .unwrap()
                                .seek(std::io::SeekFrom::Start(current_file_offset))?;
                        }
                    }
                    let handle = current_file_handle.as_mut().unwrap();
                    handle.read_exact(&mut piece_buf[buf_offset..buf_offset + to_read])?;
                }

                buf_offset += to_read;
                current_file_offset += to_read as u64;

                if current_file_offset >= file.length {
                    current_file_idx += 1;
                    current_file_offset = 0;
                    current_file_handle = None;
                }
            }

            let hash = crate::sha1(&piece_buf[..piece_end]);
            pieces.extend_from_slice(hash.as_bytes());
            cb(piece_index as usize + 1, num_pieces);
            piece_index += 1;
        }

        // Build InfoDict
        let info = if is_single_file {
            let f = &files_with_pads[0];
            InfoDict {
                name: name.clone(),
                piece_length: piece_size,
                pieces: pieces.clone(),
                length: Some(f.length),
                files: None,
                private: if self.private { Some(1) } else { None },
                source: self.source.clone(),
            }
        } else {
            let file_entries: Vec<FileEntry> = files_with_pads
                .iter()
                .map(|f| FileEntry {
                    length: f.length,
                    path: f.torrent_path.clone(),
                    attr: f.attr.clone(),
                    mtime: f.mtime,
                    symlink_path: f.symlink_path.clone(),
                })
                .collect();
            InfoDict {
                name: name.clone(),
                piece_length: piece_size,
                pieces: pieces.clone(),
                length: None,
                files: Some(file_entries),
                private: if self.private { Some(1) } else { None },
                source: self.source.clone(),
            }
        };

        // Compute info hash
        let info_bytes = ferrite_bencode::to_bytes(&info)
            .map_err(|e| Error::CreateTorrent(format!("serialize info: {e}")))?;
        let info_hash = crate::sha1(&info_bytes);

        // Build announce / announce-list
        let (announce, announce_list) = build_tracker_lists(&self.trackers);

        // Creation date
        let creation_date = self.creation_date.or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        });

        // Build output
        let output = TorrentOutput {
            announce,
            announce_list,
            comment: self.comment.clone(),
            created_by: self.creator.clone(),
            creation_date,
            httpseeds: if self.http_seeds.is_empty() {
                None
            } else {
                Some(self.http_seeds.clone())
            },
            info,
            nodes: if self.dht_nodes.is_empty() {
                None
            } else {
                Some(self.dht_nodes.clone())
            },
            url_list: if self.web_seeds.is_empty() {
                None
            } else {
                Some(self.web_seeds.clone())
            },
        };

        let bytes = ferrite_bencode::to_bytes(&output)
            .map_err(|e| Error::CreateTorrent(format!("serialize torrent: {e}")))?;

        let meta = TorrentMetaV1 {
            info_hash,
            announce: self.trackers.first().map(|(url, _)| url.clone()),
            announce_list: output.announce_list,
            comment: self.comment,
            created_by: self.creator,
            creation_date,
            info: output.info,
            url_list: self.web_seeds,
            httpseeds: self.http_seeds,
            info_bytes: Some(Bytes::from(info_bytes)),
        };

        Ok(CreateTorrentResult { meta, bytes })
    }
}

impl Default for CreateTorrent {
    fn default() -> Self {
        Self::new()
    }
}

/// Advance file cursors by `bytes` without reading.
fn advance_cursors(
    files: &[InputFile],
    mut bytes: usize,
    file_idx: &mut usize,
    file_offset: &mut u64,
    file_handle: &mut Option<fs::File>,
) {
    while bytes > 0 && *file_idx < files.len() {
        let remaining = files[*file_idx].length - *file_offset;
        let skip = bytes.min(remaining as usize);
        *file_offset += skip as u64;
        bytes -= skip;
        if *file_offset >= files[*file_idx].length {
            *file_idx += 1;
            *file_offset = 0;
            *file_handle = None;
        }
    }
}

/// Detect BEP 47 file attributes from filesystem metadata.
fn detect_attr(path: &Path, meta: &fs::Metadata) -> Option<String> {
    let mut attr = String::new();

    // Hidden: file name starts with "."
    if let Some(name) = path.file_name()
        && name.to_string_lossy().starts_with('.')
    {
        attr.push('h');
    }

    // Executable: any execute bit set
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            attr.push('x');
        }
    }
    let _ = meta; // suppress unused warning on non-unix

    if attr.is_empty() { None } else { Some(attr) }
}

/// Recursively walk a directory, collecting InputFile entries.
fn walk_directory(
    base: &Path,
    prefix: &[String],
    out: &mut Vec<InputFile>,
    include_mtime: bool,
    include_symlinks: bool,
) {
    let mut entries: Vec<_> = match fs::read_dir(base) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let mut path_components = prefix.to_vec();
        path_components.push(file_name);

        let entry_path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            walk_directory(&entry_path, &path_components, out, include_mtime, include_symlinks);
        } else if file_type.is_symlink() && include_symlinks {
            // Follow symlink for size, record target
            if let Ok(meta) = fs::metadata(&entry_path) {
                let target = fs::read_link(&entry_path).ok().map(|t| {
                    t.components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                });
                let mtime = if include_mtime {
                    meta.modified().ok().and_then(|t| {
                        t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
                    })
                } else {
                    None
                };
                out.push(InputFile {
                    disk_path: fs::canonicalize(&entry_path).unwrap_or(entry_path),
                    torrent_path: path_components,
                    length: meta.len(),
                    mtime,
                    attr: Some("l".into()),
                    symlink_path: target,
                    is_pad: false,
                });
            }
        } else if file_type.is_file()
            && let Ok(meta) = fs::metadata(&entry_path)
        {
            let mtime = if include_mtime {
                meta.modified().ok().and_then(|t| {
                    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
                })
            } else {
                None
            };
            let attr = detect_attr(&entry_path, &meta);
            out.push(InputFile {
                disk_path: fs::canonicalize(&entry_path).unwrap_or(entry_path),
                torrent_path: path_components,
                length: meta.len(),
                mtime,
                attr,
                symlink_path: None,
                is_pad: false,
            });
        }
    }
}

/// Insert pad files after eligible files to align to piece boundaries.
fn insert_pad_files(files: Vec<InputFile>, limit: Option<u64>, piece_size: Option<u64>) -> Vec<InputFile> {
    let limit = match limit {
        Some(l) => l,
        None => return files,
    };

    let total_size: u64 = files.iter().map(|f| f.length).sum();
    let ps = piece_size.unwrap_or_else(|| auto_piece_size(total_size));

    let mut result = Vec::new();
    let mut offset = 0u64;
    let last_idx = files.len() - 1;

    for (i, file) in files.into_iter().enumerate() {
        let should_pad = i < last_idx && (limit == 0 || file.length > limit);
        offset += file.length;
        result.push(file);

        if should_pad {
            let remainder = offset % ps;
            if remainder != 0 {
                let padding = ps - remainder;
                result.push(InputFile {
                    disk_path: PathBuf::new(),
                    torrent_path: vec![".pad".into(), padding.to_string()],
                    length: padding,
                    mtime: None,
                    attr: Some("p".into()),
                    symlink_path: None,
                    is_pad: true,
                });
                offset += padding;
            }
        }
    }

    result
}

/// Build announce URL and announce-list from tracker list.
fn build_tracker_lists(trackers: &[(String, usize)]) -> (Option<String>, Option<Vec<Vec<String>>>) {
    if trackers.is_empty() {
        return (None, None);
    }

    let announce = Some(trackers[0].0.clone());

    // Group by tier
    let mut max_tier = 0;
    for &(_, tier) in trackers {
        if tier > max_tier {
            max_tier = tier;
        }
    }

    let mut tiers: Vec<Vec<String>> = vec![Vec::new(); max_tier + 1];
    for (url, tier) in trackers {
        tiers[*tier].push(url.clone());
    }
    let tiers: Vec<Vec<String>> = tiers.into_iter().filter(|t| !t.is_empty()).collect();

    let announce_list = if tiers.len() > 1 || tiers.first().is_some_and(|t| t.len() > 1) {
        Some(tiers)
    } else {
        None
    };

    (announce, announce_list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::torrent_from_bytes;
    use std::io::Write;

    /// Create a temp directory with test files.
    fn make_test_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // Create files with known content
        let file_a = dir.path().join("aaa.txt");
        fs::write(&file_a, b"hello world\n").unwrap();
        let sub = dir.path().join("subdir");
        fs::create_dir(&sub).unwrap();
        let file_b = sub.join("bbb.bin");
        fs::write(&file_b, vec![0u8; 1000]).unwrap();
        dir
    }

    /// Create a single test file.
    fn make_test_file() -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&vec![0xAB; 65536]).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn auto_piece_size_thresholds() {
        // ≤ 10 MiB → 32 KiB
        assert_eq!(auto_piece_size(0), 32 * 1024);
        assert_eq!(auto_piece_size(10 * 1024 * 1024), 32 * 1024);
        // ≤ 100 MiB → 64 KiB
        assert_eq!(auto_piece_size(10 * 1024 * 1024 + 1), 64 * 1024);
        assert_eq!(auto_piece_size(100 * 1024 * 1024), 64 * 1024);
        // ≤ 1 GiB → 256 KiB
        assert_eq!(auto_piece_size(100 * 1024 * 1024 + 1), 256 * 1024);
        assert_eq!(auto_piece_size(1024 * 1024 * 1024), 256 * 1024);
        // ≤ 10 GiB → 512 KiB
        assert_eq!(auto_piece_size(1024 * 1024 * 1024 + 1), 512 * 1024);
        assert_eq!(auto_piece_size(10 * 1024 * 1024 * 1024), 512 * 1024);
        // ≤ 100 GiB → 1 MiB
        assert_eq!(auto_piece_size(10 * 1024 * 1024 * 1024 + 1), 1024 * 1024);
        assert_eq!(auto_piece_size(100 * 1024 * 1024 * 1024), 1024 * 1024);
        // ≤ 1 TiB → 2 MiB
        assert_eq!(auto_piece_size(100 * 1024 * 1024 * 1024 + 1), 2 * 1024 * 1024);
        assert_eq!(auto_piece_size(1024 * 1024 * 1024 * 1024), 2 * 1024 * 1024);
        // > 1 TiB → 4 MiB
        assert_eq!(auto_piece_size(1024 * 1024 * 1024 * 1024 + 1), 4 * 1024 * 1024);
    }

    #[test]
    fn single_file_round_trip() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        assert_eq!(result.meta.info.total_length(), 65536);
        assert!(result.meta.info.length.is_some());
        assert!(result.meta.info.files.is_none());

        // Round-trip: re-parse the generated bytes
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.info_hash, result.meta.info_hash);
        assert_eq!(parsed.info.total_length(), 65536);
        assert_eq!(parsed.info.piece_length, 32768);
        assert_eq!(parsed.info.num_pieces(), 2);
    }

    #[test]
    fn multi_file_round_trip() {
        let dir = make_test_dir();
        let result = CreateTorrent::new()
            .add_directory(dir.path())
            .set_name("test-torrent")
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        assert!(result.meta.info.files.is_some());
        let files = result.meta.info.files.as_ref().unwrap();
        assert_eq!(files.len(), 2);
        // Files should be sorted: aaa.txt before subdir/bbb.bin
        assert_eq!(files[0].path, vec!["aaa.txt"]);
        assert_eq!(files[1].path, vec!["subdir", "bbb.bin"]);
        assert_eq!(result.meta.info.total_length(), 12 + 1000);

        // Round-trip
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.info_hash, result.meta.info_hash);
        assert_eq!(parsed.info.name, "test-torrent");
    }

    #[test]
    fn private_torrent_with_source() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .set_private(true)
            .set_source("MyTracker")
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        assert_eq!(result.meta.info.private, Some(1));
        assert_eq!(result.meta.info.source.as_deref(), Some("MyTracker"));

        // Round-trip
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.info.private, Some(1));
        assert_eq!(parsed.info.source.as_deref(), Some("MyTracker"));
    }

    #[test]
    fn tracker_tiers() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .add_tracker("http://tracker1.example.com/announce", 0)
            .add_tracker("http://tracker2.example.com/announce", 0)
            .add_tracker("http://tracker3.example.com/announce", 1)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        assert_eq!(
            result.meta.announce.as_deref(),
            Some("http://tracker1.example.com/announce")
        );
        let al = result.meta.announce_list.as_ref().unwrap();
        assert_eq!(al.len(), 2);
        assert_eq!(al[0].len(), 2); // tier 0: two trackers
        assert_eq!(al[1].len(), 1); // tier 1: one tracker

        // Round-trip
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.announce_list.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn web_and_http_seeds() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .add_web_seed("http://web.example.com/files")
            .add_http_seed("http://http.example.com/seed")
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        assert_eq!(result.meta.url_list, vec!["http://web.example.com/files"]);
        assert_eq!(result.meta.httpseeds, vec!["http://http.example.com/seed"]);

        // Round-trip
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.url_list, vec!["http://web.example.com/files"]);
        assert_eq!(parsed.httpseeds, vec!["http://http.example.com/seed"]);
    }

    #[test]
    fn pad_files_all() {
        let dir = make_test_dir();
        let result = CreateTorrent::new()
            .add_directory(dir.path())
            .set_name("padded")
            .set_piece_size(32768)
            .set_pad_file_limit(Some(0))
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        let files = result.meta.info.files.as_ref().unwrap();
        // Should have pad files after all files except the last
        let pad_count = files.iter().filter(|f| f.attr.as_deref() == Some("p")).count();
        // aaa.txt (12 bytes) → pad, subdir/bbb.bin (1000 bytes) → no pad (last)
        assert_eq!(pad_count, 1);
        // Verify pad file attributes
        let pad = files.iter().find(|f| f.attr.as_deref() == Some("p")).unwrap();
        assert_eq!(pad.path[0], ".pad");
    }

    #[test]
    fn pad_file_limit_threshold() {
        let dir = make_test_dir();
        // Only pad files > 500 bytes: bbb.bin (1000) gets padded, aaa.txt (12) does not
        // But aaa.txt comes first, so: aaa.txt (no pad, 12 ≤ 500), bbb.bin (last, no pad)
        let result = CreateTorrent::new()
            .add_directory(dir.path())
            .set_name("threshold")
            .set_piece_size(32768)
            .set_pad_file_limit(Some(500))
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        let files = result.meta.info.files.as_ref().unwrap();
        // aaa.txt (12 bytes, ≤ 500, no pad), subdir/bbb.bin (1000 bytes, last file, no pad)
        let pad_count = files.iter().filter(|f| f.attr.as_deref() == Some("p")).count();
        assert_eq!(pad_count, 0);

        // Now with limit=5: aaa.txt (12 > 5) → pad, bbb.bin is last → no pad
        let result2 = CreateTorrent::new()
            .add_directory(dir.path())
            .set_name("threshold2")
            .set_piece_size(32768)
            .set_pad_file_limit(Some(5))
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        let files2 = result2.meta.info.files.as_ref().unwrap();
        let pad_count2 = files2.iter().filter(|f| f.attr.as_deref() == Some("p")).count();
        assert_eq!(pad_count2, 1);
    }

    #[test]
    fn progress_callback() {
        let f = make_test_file();
        let mut calls = Vec::new();
        CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .generate_with_progress(|current, total| {
                calls.push((current, total));
            })
            .unwrap();

        // 65536 / 32768 = 2 pieces
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], (1, 2));
        assert_eq!(calls[1], (2, 2));
    }

    #[test]
    fn empty_input_error() {
        let result = CreateTorrent::new().generate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("no files"), "error: {err}");
    }

    #[test]
    fn round_trip_info_hash() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        // Re-parse
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.info_hash, result.meta.info_hash);

        // Manual SHA1 of serialized info dict
        let info_bytes = ferrite_bencode::to_bytes(&result.meta.info).unwrap();
        let manual_hash = crate::sha1(&info_bytes);
        assert_eq!(manual_hash, result.meta.info_hash);
    }

    #[test]
    fn dht_nodes() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .add_dht_node("router.bittorrent.com", 6881)
            .add_dht_node("dht.example.com", 6882)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        // Verify nodes are in the raw bytes by re-parsing with bencode
        let value: ferrite_bencode::BencodeValue =
            ferrite_bencode::from_bytes(&result.bytes).unwrap();
        if let ferrite_bencode::BencodeValue::Dict(ref d) = value {
            let nodes = d.get(b"nodes".as_ref()).unwrap();
            if let ferrite_bencode::BencodeValue::List(list) = nodes {
                assert_eq!(list.len(), 2);
            } else {
                panic!("nodes should be a list");
            }
        } else {
            panic!("top-level should be a dict");
        }
    }

    #[test]
    fn file_mtime() {
        let dir = make_test_dir();
        let result = CreateTorrent::new()
            .include_mtime(true)
            .add_directory(dir.path())
            .set_name("mtime-test")
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        let files = result.meta.info.files.as_ref().unwrap();
        // All real files should have mtime set
        for f in files {
            if f.attr.as_deref() != Some("p") {
                assert!(f.mtime.is_some(), "file {:?} should have mtime", f.path);
                assert!(f.mtime.unwrap() > 0);
            }
        }
    }

    #[test]
    fn pre_computed_hashes() {
        let f = make_test_file();

        // First, generate normally to get the real hashes
        let normal = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        let piece0_hash = normal.meta.info.piece_hash(0).unwrap();

        // Now generate with pre-computed hash for piece 0
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .set_hash(0, piece0_hash)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        // Should produce identical output
        assert_eq!(result.meta.info_hash, normal.meta.info_hash);
        assert_eq!(result.meta.info.piece_hash(0), normal.meta.info.piece_hash(0));
    }
}
