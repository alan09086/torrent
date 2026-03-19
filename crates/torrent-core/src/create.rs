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

use crate::detect::TorrentMeta;
use crate::error::{Error, Result};
use crate::file_tree::{FileTreeNode, V2FileAttr};
use crate::hash::{Id20, Id32};
use crate::info_hashes::InfoHashes;
use crate::merkle::MerkleTree;
use crate::metainfo::{FileEntry, InfoDict, TorrentMetaV1};
use crate::metainfo_v2::{InfoDictV2, TorrentMetaV2};
use crate::torrent_version::TorrentVersion;

/// Auto-select piece size based on total content size (libtorrent-style).
pub fn auto_piece_size(total: u64) -> u64 {
    if total <= 10_485_760 {
        32 * 1024 // ≤ 10 MiB → 32 KiB
    } else if total <= 104_857_600 {
        64 * 1024 // ≤ 100 MiB → 64 KiB
    } else if total <= 1_073_741_824 {
        256 * 1024 // ≤ 1 GiB → 256 KiB
    } else if total <= 10_737_418_240 {
        512 * 1024 // ≤ 10 GiB → 512 KiB
    } else if total <= 107_374_182_400 {
        1024 * 1024 // ≤ 100 GiB → 1 MiB
    } else if total <= 1_099_511_627_776 {
        2 * 1024 * 1024 // ≤ 1 TiB → 2 MiB
    } else {
        4 * 1024 * 1024 // > 1 TiB → 4 MiB
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
    /// Parsed torrent metadata (V1, V2, or Hybrid depending on `set_version()`).
    pub meta: TorrentMeta,
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
    version: TorrentVersion,
    ssl_cert: Option<Vec<u8>>,
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
            version: TorrentVersion::V1Only,
            ssl_cert: None,
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
                    t.duration_since(UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs() as i64)
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
            walk_directory(
                &canonical,
                &[],
                &mut files,
                self.include_mtime,
                self.include_symlinks,
            );
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

    /// Set the torrent version to create.
    ///
    /// - `V1Only` (default) — standard SHA-1 .torrent (BEP 3)
    /// - `Hybrid` — combined v1+v2 with both SHA-1 pieces and SHA-256 Merkle trees (BEP 52)
    /// - `V2Only` — pure v2 with SHA-256 Merkle trees only (BEP 52)
    pub fn set_version(mut self, version: TorrentVersion) -> Self {
        self.version = version;
        self
    }

    /// Set the SSL CA certificate (PEM-encoded) for SSL torrent creation.
    /// When set, the `ssl-cert` key is written into the info dict.
    pub fn set_ssl_cert(mut self, cert_pem: Vec<u8>) -> Self {
        self.ssl_cert = Some(cert_pem);
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
        let piece_size = self
            .piece_size
            .unwrap_or_else(|| auto_piece_size(total_size));

        // Build announce / announce-list (needed by all paths)
        let (announce, announce_list) = build_tracker_lists(&self.trackers);

        // Creation date
        let creation_date = self.creation_date.or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        });

        match self.version {
            TorrentVersion::V2Only => {
                // ── V2-only output (BEP 52) ──
                // Skip SHA-1 entirely — v2 uses SHA-256 Merkle trees only
                let v2_out = build_v2_output(
                    &files_with_pads,
                    piece_size,
                    &name,
                    self.private,
                    &self.source,
                    &self.ssl_cert,
                )?;

                // Build outer torrent dict
                let mut outer = build_outer_dict(
                    &announce,
                    &announce_list,
                    &self.comment,
                    &self.creator,
                    creation_date,
                    &self.http_seeds,
                    &v2_out.info_bytes,
                    &self.dht_nodes,
                    &self.web_seeds,
                )?;

                // piece layers
                if !v2_out.piece_layers_raw.is_empty() {
                    let mut pl_dict = std::collections::BTreeMap::new();
                    for (root, layer) in &v2_out.piece_layers_raw {
                        pl_dict.insert(
                            root.clone(),
                            torrent_bencode::BencodeValue::Bytes(layer.clone()),
                        );
                    }
                    outer.insert(
                        b"piece layers".to_vec(),
                        torrent_bencode::BencodeValue::Dict(pl_dict),
                    );
                }

                let bytes = torrent_bencode::to_bytes(&torrent_bencode::BencodeValue::Dict(outer))
                    .map_err(|e| Error::CreateTorrent(format!("serialize v2 torrent: {e}")))?;

                let ssl_cert = self.ssl_cert;
                let meta_v2 = TorrentMetaV2 {
                    info_hashes: InfoHashes::v2_only(v2_out.info_hash_v2),
                    info_bytes: Some(Bytes::from(v2_out.info_bytes)),
                    announce: self.trackers.first().map(|(url, _)| url.clone()),
                    announce_list,
                    comment: self.comment,
                    created_by: self.creator,
                    creation_date,
                    info: v2_out.info_dict_v2,
                    piece_layers: v2_out.piece_layers,
                    ssl_cert,
                };

                Ok(CreateTorrentResult {
                    meta: TorrentMeta::V2(meta_v2),
                    bytes,
                })
            }

            TorrentVersion::V1Only => {
                // ── Standard v1 output ──
                let num_pieces = if total_size == 0 {
                    0
                } else {
                    total_size.div_ceil(piece_size) as usize
                };
                let pieces = hash_sha1_pieces(
                    &files_with_pads,
                    piece_size,
                    total_size,
                    num_pieces,
                    &self.pre_hashes,
                    &mut cb,
                )?;
                let info = build_v1_info_dict(
                    &files_with_pads,
                    &name,
                    piece_size,
                    &pieces,
                    is_single_file,
                    self.private,
                    &self.source,
                    &self.ssl_cert,
                );

                let info_bytes = torrent_bencode::to_bytes(&info)
                    .map_err(|e| Error::CreateTorrent(format!("serialize info: {e}")))?;
                let info_hash = crate::sha1(&info_bytes);

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

                let bytes = torrent_bencode::to_bytes(&output)
                    .map_err(|e| Error::CreateTorrent(format!("serialize torrent: {e}")))?;

                let ssl_cert = self.ssl_cert;
                let meta_v1 = TorrentMetaV1 {
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
                    ssl_cert,
                };

                Ok(CreateTorrentResult {
                    meta: TorrentMeta::V1(meta_v1),
                    bytes,
                })
            }

            TorrentVersion::Hybrid => {
                // ── Hybrid v1+v2 output ──
                // Step 1: SHA-1 piece hashing (v1 component)
                let num_pieces = if total_size == 0 {
                    0
                } else {
                    total_size.div_ceil(piece_size) as usize
                };
                let pieces = hash_sha1_pieces(
                    &files_with_pads,
                    piece_size,
                    total_size,
                    num_pieces,
                    &self.pre_hashes,
                    &mut cb,
                )?;
                let info = build_v1_info_dict(
                    &files_with_pads,
                    &name,
                    piece_size,
                    &pieces,
                    is_single_file,
                    self.private,
                    &self.source,
                    &self.ssl_cert,
                );

                // Step 2: v2 Merkle data
                let v2_out = build_v2_output(
                    &files_with_pads,
                    piece_size,
                    &name,
                    self.private,
                    &self.source,
                    &self.ssl_cert,
                )?;

                // Step 3: Build merged info dict with both v1 and v2 keys
                let mut merged =
                    std::collections::BTreeMap::<Vec<u8>, torrent_bencode::BencodeValue>::new();

                // v2: file tree
                merged.insert(
                    b"file tree".to_vec(),
                    v2_out.info_dict_v2.file_tree.to_bencode(),
                );

                // v1: files list or single-file length
                if is_single_file {
                    merged.insert(
                        b"length".to_vec(),
                        torrent_bencode::BencodeValue::Integer(
                            info.length.expect("single-file info dict must have length") as i64,
                        ),
                    );
                } else {
                    let file_list: Vec<torrent_bencode::BencodeValue> = info
                        .files
                        .as_ref()
                        .expect("multi-file info dict must have files")
                        .iter()
                        .map(|f| {
                            let mut d = std::collections::BTreeMap::new();
                            d.insert(
                                b"length".to_vec(),
                                torrent_bencode::BencodeValue::Integer(f.length as i64),
                            );
                            let path: Vec<torrent_bencode::BencodeValue> = f
                                .path
                                .iter()
                                .map(|p| {
                                    torrent_bencode::BencodeValue::Bytes(p.as_bytes().to_vec())
                                })
                                .collect();
                            d.insert(b"path".to_vec(), torrent_bencode::BencodeValue::List(path));
                            if let Some(ref attr) = f.attr {
                                d.insert(
                                    b"attr".to_vec(),
                                    torrent_bencode::BencodeValue::Bytes(attr.as_bytes().to_vec()),
                                );
                            }
                            if let Some(mtime) = f.mtime {
                                d.insert(
                                    b"mtime".to_vec(),
                                    torrent_bencode::BencodeValue::Integer(mtime),
                                );
                            }
                            if let Some(ref sl) = f.symlink_path {
                                let sl_list: Vec<torrent_bencode::BencodeValue> = sl
                                    .iter()
                                    .map(|s| {
                                        torrent_bencode::BencodeValue::Bytes(s.as_bytes().to_vec())
                                    })
                                    .collect();
                                d.insert(
                                    b"symlink path".to_vec(),
                                    torrent_bencode::BencodeValue::List(sl_list),
                                );
                            }
                            torrent_bencode::BencodeValue::Dict(d)
                        })
                        .collect();
                    merged.insert(
                        b"files".to_vec(),
                        torrent_bencode::BencodeValue::List(file_list),
                    );
                }

                // v2: meta version
                merged.insert(
                    b"meta version".to_vec(),
                    torrent_bencode::BencodeValue::Integer(2),
                );

                // shared: name
                merged.insert(
                    b"name".to_vec(),
                    torrent_bencode::BencodeValue::Bytes(info.name.as_bytes().to_vec()),
                );

                // shared: piece length
                merged.insert(
                    b"piece length".to_vec(),
                    torrent_bencode::BencodeValue::Integer(piece_size as i64),
                );

                // v1: pieces (concatenated SHA-1 hashes)
                merged.insert(
                    b"pieces".to_vec(),
                    torrent_bencode::BencodeValue::Bytes(pieces),
                );

                // optional: private
                if self.private {
                    merged.insert(
                        b"private".to_vec(),
                        torrent_bencode::BencodeValue::Integer(1),
                    );
                }

                // optional: source
                if let Some(ref source) = self.source {
                    merged.insert(
                        b"source".to_vec(),
                        torrent_bencode::BencodeValue::Bytes(source.as_bytes().to_vec()),
                    );
                }

                // optional: ssl-cert
                if let Some(ref cert) = self.ssl_cert {
                    merged.insert(
                        b"ssl-cert".to_vec(),
                        torrent_bencode::BencodeValue::Bytes(cert.clone()),
                    );
                }

                // Serialize merged info dict -> compute both hashes
                let merged_info_bytes =
                    torrent_bencode::to_bytes(&torrent_bencode::BencodeValue::Dict(merged))
                        .map_err(|e| Error::CreateTorrent(format!("serialize hybrid info: {e}")))?;
                let info_hash_v1 = crate::sha1(&merged_info_bytes);
                let info_hash_v2 = crate::sha256(&merged_info_bytes);

                // Build outer torrent dict
                let mut outer = build_outer_dict(
                    &announce,
                    &announce_list,
                    &self.comment,
                    &self.creator,
                    creation_date,
                    &self.http_seeds,
                    &merged_info_bytes,
                    &self.dht_nodes,
                    &self.web_seeds,
                )?;

                // piece layers
                if !v2_out.piece_layers_raw.is_empty() {
                    let mut pl_dict = std::collections::BTreeMap::new();
                    for (root, layer) in &v2_out.piece_layers_raw {
                        pl_dict.insert(
                            root.clone(),
                            torrent_bencode::BencodeValue::Bytes(layer.clone()),
                        );
                    }
                    outer.insert(
                        b"piece layers".to_vec(),
                        torrent_bencode::BencodeValue::Dict(pl_dict),
                    );
                }

                let bytes = torrent_bencode::to_bytes(&torrent_bencode::BencodeValue::Dict(outer))
                    .map_err(|e| Error::CreateTorrent(format!("serialize hybrid torrent: {e}")))?;

                // Build TorrentMetaV1 component
                let ssl_cert = self.ssl_cert;
                let meta_v1 = TorrentMetaV1 {
                    info_hash: info_hash_v1,
                    announce: self.trackers.first().map(|(url, _)| url.clone()),
                    announce_list: announce_list.clone(),
                    comment: self.comment.clone(),
                    created_by: self.creator.clone(),
                    creation_date,
                    info,
                    url_list: self.web_seeds.clone(),
                    httpseeds: self.http_seeds.clone(),
                    info_bytes: Some(Bytes::from(merged_info_bytes.clone())),
                    ssl_cert: ssl_cert.clone(),
                };

                // Build TorrentMetaV2 component
                let meta_v2 = TorrentMetaV2 {
                    info_hashes: InfoHashes::hybrid(info_hash_v1, info_hash_v2),
                    info_bytes: Some(Bytes::from(merged_info_bytes)),
                    announce: meta_v1.announce.clone(),
                    announce_list: meta_v1.announce_list.clone(),
                    comment: self.comment,
                    created_by: self.creator,
                    creation_date,
                    info: v2_out.info_dict_v2,
                    piece_layers: v2_out.piece_layers,
                    ssl_cert,
                };

                Ok(CreateTorrentResult {
                    meta: TorrentMeta::Hybrid(Box::new(meta_v1), Box::new(meta_v2)),
                    bytes,
                })
            }
        }
    }
}

impl Default for CreateTorrent {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute SHA-1 piece hashes for v1 torrents.
///
/// Reads files sequentially, filling piece-sized buffers and hashing each with SHA-1.
/// Supports pre-computed hashes that skip disk reads.
fn hash_sha1_pieces(
    files: &[InputFile],
    piece_size: u64,
    total_size: u64,
    num_pieces: usize,
    pre_hashes: &HashMap<u32, Id20>,
    cb: &mut impl FnMut(usize, usize),
) -> Result<Vec<u8>> {
    let mut pieces = Vec::with_capacity(num_pieces * 20);
    let mut piece_buf = vec![0u8; piece_size as usize];
    let mut current_file_idx = 0;
    let mut current_file_offset = 0u64;
    let mut current_file_handle: Option<fs::File> = None;
    let mut piece_index = 0u32;

    while (piece_index as usize) < num_pieces {
        // Check for pre-computed hash
        if let Some(&hash) = pre_hashes.get(&piece_index) {
            let remaining_in_piece = if (piece_index as usize) == num_pieces - 1 {
                (total_size - (piece_index as u64) * piece_size) as usize
            } else {
                piece_size as usize
            };
            advance_cursors(
                files,
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
            if current_file_idx >= files.len() {
                break;
            }
            let file = &files[current_file_idx];
            let remaining_in_file = file.length - current_file_offset;
            let to_read = (piece_end - buf_offset).min(remaining_in_file as usize);

            if file.is_pad {
                piece_buf[buf_offset..buf_offset + to_read].fill(0);
            } else {
                if current_file_handle.is_none() {
                    current_file_handle = Some(fs::File::open(&file.disk_path)?);
                    if current_file_offset > 0 {
                        use std::io::Seek;
                        current_file_handle
                            .as_mut()
                            .expect("file handle just opened")
                            .seek(std::io::SeekFrom::Start(current_file_offset))?;
                    }
                }
                let handle = current_file_handle
                    .as_mut()
                    .expect("file handle just opened or already open");
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

    Ok(pieces)
}

/// Build a v1 InfoDict from the given parameters.
#[allow(clippy::too_many_arguments)]
fn build_v1_info_dict(
    files: &[InputFile],
    name: &str,
    piece_size: u64,
    pieces: &[u8],
    is_single_file: bool,
    private: bool,
    source: &Option<String>,
    ssl_cert: &Option<Vec<u8>>,
) -> InfoDict {
    if is_single_file {
        InfoDict {
            name: name.to_owned(),
            piece_length: piece_size,
            pieces: pieces.to_vec(),
            length: Some(files[0].length),
            files: None,
            private: if private { Some(1) } else { None },
            source: source.clone(),
            ssl_cert: ssl_cert.clone(),
        }
    } else {
        let file_entries: Vec<FileEntry> = files
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
            name: name.to_owned(),
            piece_length: piece_size,
            pieces: pieces.to_vec(),
            length: None,
            files: Some(file_entries),
            private: if private { Some(1) } else { None },
            source: source.clone(),
            ssl_cert: ssl_cert.clone(),
        }
    }
}

/// Output from building v2 torrent components.
struct V2Output {
    /// The v2 file tree node.
    info_dict_v2: InfoDictV2,
    /// Piece layers as `Id32 -> concatenated hashes` (for TorrentMetaV2).
    piece_layers: std::collections::BTreeMap<Id32, Vec<u8>>,
    /// Piece layers as raw bytes `(root_bytes -> concat_hashes)` (for outer dict serialization).
    piece_layers_raw: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
    /// SHA-256 hash of the serialized v2 info dict.
    info_hash_v2: Id32,
    /// Serialized v2 info dict bytes.
    info_bytes: Vec<u8>,
}

/// Build v2 torrent components: Merkle trees, file tree, info dict, piece layers.
///
/// Used by both V2Only and Hybrid paths. For Hybrid, the caller merges v1 keys
/// into the info dict and re-hashes; the `info_hash_v2` and `info_bytes` from this
/// output are only used directly by the V2Only path.
fn build_v2_output(
    files: &[InputFile],
    piece_size: u64,
    name: &str,
    private: bool,
    source: &Option<String>,
    ssl_cert: &Option<Vec<u8>>,
) -> Result<V2Output> {
    // Compute SHA-256 Merkle trees per file
    let v2_data = compute_v2_merkle_data(files, piece_size)?;

    // Build file tree
    let file_tree = build_v2_file_tree(&v2_data);

    // Build v2 info dict as BencodeValue
    let mut info_map = std::collections::BTreeMap::<Vec<u8>, torrent_bencode::BencodeValue>::new();

    info_map.insert(b"file tree".to_vec(), file_tree.to_bencode());
    info_map.insert(
        b"meta version".to_vec(),
        torrent_bencode::BencodeValue::Integer(2),
    );
    info_map.insert(
        b"name".to_vec(),
        torrent_bencode::BencodeValue::Bytes(name.as_bytes().to_vec()),
    );
    info_map.insert(
        b"piece length".to_vec(),
        torrent_bencode::BencodeValue::Integer(piece_size as i64),
    );

    if private {
        info_map.insert(
            b"private".to_vec(),
            torrent_bencode::BencodeValue::Integer(1),
        );
    }
    if let Some(src) = source {
        info_map.insert(
            b"source".to_vec(),
            torrent_bencode::BencodeValue::Bytes(src.as_bytes().to_vec()),
        );
    }
    if let Some(cert) = ssl_cert {
        info_map.insert(
            b"ssl-cert".to_vec(),
            torrent_bencode::BencodeValue::Bytes(cert.clone()),
        );
    }

    // Serialize and hash
    let info_bytes = torrent_bencode::to_bytes(&torrent_bencode::BencodeValue::Dict(info_map))
        .map_err(|e| Error::CreateTorrent(format!("serialize v2 info: {e}")))?;
    let info_hash_v2 = crate::sha256(&info_bytes);

    // Build piece layers (both typed and raw forms)
    let mut piece_layers = std::collections::BTreeMap::<Id32, Vec<u8>>::new();
    let mut piece_layers_raw = std::collections::BTreeMap::<Vec<u8>, Vec<u8>>::new();
    for fd in &v2_data {
        if let Some(root) = &fd.pieces_root
            && !fd.piece_layer.is_empty()
        {
            let concat: Vec<u8> = fd
                .piece_layer
                .iter()
                .flat_map(|h| h.as_bytes())
                .copied()
                .collect();
            piece_layers_raw.insert(root.as_bytes().to_vec(), concat.clone());
            piece_layers.insert(*root, concat);
        }
    }

    let info_dict_v2 = InfoDictV2 {
        name: name.to_owned(),
        piece_length: piece_size,
        meta_version: 2,
        file_tree,
        ssl_cert: ssl_cert.clone(),
    };

    Ok(V2Output {
        info_dict_v2,
        piece_layers,
        piece_layers_raw,
        info_hash_v2,
        info_bytes,
    })
}

/// Build the outer torrent dict shared by V2Only and Hybrid paths.
///
/// Does NOT include "piece layers" — the caller adds those after this returns,
/// because the piece layers dict is path-specific.
#[allow(clippy::too_many_arguments)]
fn build_outer_dict(
    announce: &Option<String>,
    announce_list: &Option<Vec<Vec<String>>>,
    comment: &Option<String>,
    creator: &Option<String>,
    creation_date: Option<i64>,
    http_seeds: &[String],
    info_bytes: &[u8],
    dht_nodes: &[(String, u16)],
    web_seeds: &[String],
) -> Result<std::collections::BTreeMap<Vec<u8>, torrent_bencode::BencodeValue>> {
    let mut outer = std::collections::BTreeMap::<Vec<u8>, torrent_bencode::BencodeValue>::new();

    if let Some(url) = announce {
        outer.insert(
            b"announce".to_vec(),
            torrent_bencode::BencodeValue::Bytes(url.as_bytes().to_vec()),
        );
    }
    if let Some(al) = announce_list {
        let al_val: Vec<torrent_bencode::BencodeValue> = al
            .iter()
            .map(|tier| {
                let t: Vec<torrent_bencode::BencodeValue> = tier
                    .iter()
                    .map(|u| torrent_bencode::BencodeValue::Bytes(u.as_bytes().to_vec()))
                    .collect();
                torrent_bencode::BencodeValue::List(t)
            })
            .collect();
        outer.insert(
            b"announce-list".to_vec(),
            torrent_bencode::BencodeValue::List(al_val),
        );
    }
    if let Some(c) = comment {
        outer.insert(
            b"comment".to_vec(),
            torrent_bencode::BencodeValue::Bytes(c.as_bytes().to_vec()),
        );
    }
    if let Some(cr) = creator {
        outer.insert(
            b"created by".to_vec(),
            torrent_bencode::BencodeValue::Bytes(cr.as_bytes().to_vec()),
        );
    }
    if let Some(cd) = creation_date {
        outer.insert(
            b"creation date".to_vec(),
            torrent_bencode::BencodeValue::Integer(cd),
        );
    }
    if !http_seeds.is_empty() {
        let seeds: Vec<torrent_bencode::BencodeValue> = http_seeds
            .iter()
            .map(|s| torrent_bencode::BencodeValue::Bytes(s.as_bytes().to_vec()))
            .collect();
        outer.insert(
            b"httpseeds".to_vec(),
            torrent_bencode::BencodeValue::List(seeds),
        );
    }
    // Info dict as re-parsed BencodeValue (preserves exact serialization for hash consistency)
    outer.insert(
        b"info".to_vec(),
        torrent_bencode::from_bytes::<torrent_bencode::BencodeValue>(info_bytes)
            .map_err(|e| Error::CreateTorrent(format!("re-parse info: {e}")))?,
    );
    if !dht_nodes.is_empty() {
        let nodes: Vec<torrent_bencode::BencodeValue> = dht_nodes
            .iter()
            .map(|(h, p)| {
                torrent_bencode::BencodeValue::List(vec![
                    torrent_bencode::BencodeValue::Bytes(h.as_bytes().to_vec()),
                    torrent_bencode::BencodeValue::Integer(*p as i64),
                ])
            })
            .collect();
        outer.insert(
            b"nodes".to_vec(),
            torrent_bencode::BencodeValue::List(nodes),
        );
    }
    if !web_seeds.is_empty() {
        let seeds: Vec<torrent_bencode::BencodeValue> = web_seeds
            .iter()
            .map(|s| torrent_bencode::BencodeValue::Bytes(s.as_bytes().to_vec()))
            .collect();
        outer.insert(
            b"url-list".to_vec(),
            torrent_bencode::BencodeValue::List(seeds),
        );
    }

    Ok(outer)
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
            walk_directory(
                &entry_path,
                &path_components,
                out,
                include_mtime,
                include_symlinks,
            );
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
                        t.duration_since(UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs() as i64)
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
                    t.duration_since(UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs() as i64)
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
fn insert_pad_files(
    files: Vec<InputFile>,
    limit: Option<u64>,
    piece_size: Option<u64>,
) -> Vec<InputFile> {
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

/// Per-file v2 Merkle data computed during hybrid torrent creation.
struct V2FileData {
    /// Torrent path components (excluding pad files).
    torrent_path: Vec<String>,
    /// File length in bytes.
    length: u64,
    /// Merkle tree root hash (None for empty files).
    pieces_root: Option<Id32>,
    /// Piece-layer hashes (only for files larger than piece_length).
    piece_layer: Vec<Id32>,
}

/// Compute SHA-256 Merkle trees for each non-pad file.
///
/// This is a second pass through the file data — the first pass computed SHA-1
/// piece hashes for v1. We re-read each file and hash 16 KiB blocks for v2.
fn compute_v2_merkle_data(files: &[InputFile], piece_size: u64) -> Result<Vec<V2FileData>> {
    let block_size = 16384u64;
    let blocks_per_piece = (piece_size / block_size) as usize;
    let mut result = Vec::new();

    for file in files {
        if file.is_pad {
            continue;
        }

        if file.length == 0 {
            result.push(V2FileData {
                torrent_path: file.torrent_path.clone(),
                length: 0,
                pieces_root: None,
                piece_layer: Vec::new(),
            });
            continue;
        }

        // Read file and SHA-256 each 16 KiB block
        let num_blocks = file.length.div_ceil(block_size) as usize;
        let mut block_hashes = Vec::with_capacity(num_blocks);
        let mut handle = fs::File::open(&file.disk_path)?;
        let mut buf = vec![0u8; block_size as usize];
        let mut remaining = file.length;

        while remaining > 0 {
            let to_read = remaining.min(block_size) as usize;
            handle.read_exact(&mut buf[..to_read])?;
            // Hash only the actual data (last block may be shorter)
            block_hashes.push(crate::sha256(&buf[..to_read]));
            remaining -= to_read as u64;
        }

        let tree = MerkleTree::from_leaves(&block_hashes);
        let root = tree.root();

        // Piece layer only needed for files spanning multiple pieces
        let piece_layer = if file.length > piece_size {
            tree.piece_layer(blocks_per_piece).to_vec()
        } else {
            Vec::new()
        };

        result.push(V2FileData {
            torrent_path: file.torrent_path.clone(),
            length: file.length,
            pieces_root: Some(root),
            piece_layer,
        });
    }

    Ok(result)
}

/// Build a v2 FileTreeNode from computed Merkle data.
///
/// Single-file torrents produce a single-entry directory.
/// Multi-file torrents build the nested path structure.
fn build_v2_file_tree(v2_data: &[V2FileData]) -> FileTreeNode {
    use std::collections::BTreeMap;

    let mut root = BTreeMap::new();

    for fd in v2_data {
        let attr = V2FileAttr {
            length: fd.length,
            pieces_root: fd.pieces_root,
        };
        let file_node = FileTreeNode::File(attr);

        // Walk path components to build nested directory structure
        let mut current = &mut root;
        for (i, component) in fd.torrent_path.iter().enumerate() {
            if i == fd.torrent_path.len() - 1 {
                // Last component: insert the file
                current.insert(component.clone(), file_node);
                break;
            }
            // Intermediate component: create/get directory
            current = match current
                .entry(component.clone())
                .or_insert_with(|| FileTreeNode::Directory(BTreeMap::new()))
            {
                FileTreeNode::Directory(children) => children,
                FileTreeNode::File(_) => unreachable!("path conflict in file tree"),
            };
        }
    }

    FileTreeNode::Directory(root)
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
        assert_eq!(
            auto_piece_size(100 * 1024 * 1024 * 1024 + 1),
            2 * 1024 * 1024
        );
        assert_eq!(auto_piece_size(1024 * 1024 * 1024 * 1024), 2 * 1024 * 1024);
        // > 1 TiB → 4 MiB
        assert_eq!(
            auto_piece_size(1024 * 1024 * 1024 * 1024 + 1),
            4 * 1024 * 1024
        );
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

        assert_eq!(result.meta.as_v1().unwrap().info.total_length(), 65536);
        assert!(result.meta.as_v1().unwrap().info.length.is_some());
        assert!(result.meta.as_v1().unwrap().info.files.is_none());

        // Round-trip: re-parse the generated bytes
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.info_hash, result.meta.as_v1().unwrap().info_hash);
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

        assert!(result.meta.as_v1().unwrap().info.files.is_some());
        let files = result.meta.as_v1().unwrap().info.files.as_ref().unwrap();
        assert_eq!(files.len(), 2);
        // Files should be sorted: aaa.txt before subdir/bbb.bin
        assert_eq!(files[0].path, vec!["aaa.txt"]);
        assert_eq!(files[1].path, vec!["subdir", "bbb.bin"]);
        assert_eq!(result.meta.as_v1().unwrap().info.total_length(), 12 + 1000);

        // Round-trip
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.info_hash, result.meta.as_v1().unwrap().info_hash);
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

        assert_eq!(result.meta.as_v1().unwrap().info.private, Some(1));
        assert_eq!(
            result.meta.as_v1().unwrap().info.source.as_deref(),
            Some("MyTracker")
        );

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
            result.meta.as_v1().unwrap().announce.as_deref(),
            Some("http://tracker1.example.com/announce")
        );
        let al = result.meta.as_v1().unwrap().announce_list.as_ref().unwrap();
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

        assert_eq!(
            result.meta.as_v1().unwrap().url_list,
            vec!["http://web.example.com/files"]
        );
        assert_eq!(
            result.meta.as_v1().unwrap().httpseeds,
            vec!["http://http.example.com/seed"]
        );

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

        let files = result.meta.as_v1().unwrap().info.files.as_ref().unwrap();
        // Should have pad files after all files except the last
        let pad_count = files
            .iter()
            .filter(|f| f.attr.as_deref() == Some("p"))
            .count();
        // aaa.txt (12 bytes) → pad, subdir/bbb.bin (1000 bytes) → no pad (last)
        assert_eq!(pad_count, 1);
        // Verify pad file attributes
        let pad = files
            .iter()
            .find(|f| f.attr.as_deref() == Some("p"))
            .unwrap();
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

        let files = result.meta.as_v1().unwrap().info.files.as_ref().unwrap();
        // aaa.txt (12 bytes, ≤ 500, no pad), subdir/bbb.bin (1000 bytes, last file, no pad)
        let pad_count = files
            .iter()
            .filter(|f| f.attr.as_deref() == Some("p"))
            .count();
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

        let files2 = result2.meta.as_v1().unwrap().info.files.as_ref().unwrap();
        let pad_count2 = files2
            .iter()
            .filter(|f| f.attr.as_deref() == Some("p"))
            .count();
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
        assert_eq!(parsed.info_hash, result.meta.as_v1().unwrap().info_hash);

        // Manual SHA1 of serialized info dict
        let info_bytes = torrent_bencode::to_bytes(&result.meta.as_v1().unwrap().info).unwrap();
        let manual_hash = crate::sha1(&info_bytes);
        assert_eq!(manual_hash, result.meta.as_v1().unwrap().info_hash);
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
        let value: torrent_bencode::BencodeValue =
            torrent_bencode::from_bytes(&result.bytes).unwrap();
        if let torrent_bencode::BencodeValue::Dict(ref d) = value {
            let nodes = d.get(b"nodes".as_ref()).unwrap();
            if let torrent_bencode::BencodeValue::List(list) = nodes {
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

        let files = result.meta.as_v1().unwrap().info.files.as_ref().unwrap();
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

        let piece0_hash = normal.meta.as_v1().unwrap().info.piece_hash(0).unwrap();

        // Now generate with pre-computed hash for piece 0
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .set_hash(0, piece0_hash)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        // Should produce identical output
        assert_eq!(
            result.meta.as_v1().unwrap().info_hash,
            normal.meta.as_v1().unwrap().info_hash
        );
        assert_eq!(
            result.meta.as_v1().unwrap().info.piece_hash(0),
            normal.meta.as_v1().unwrap().info.piece_hash(0)
        );
    }

    // ── Hybrid torrent creation tests ────────────────────────────────────

    #[test]
    fn hybrid_single_file_round_trip() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .set_version(TorrentVersion::Hybrid)
            .generate()
            .unwrap();

        // Should be a Hybrid variant
        assert!(result.meta.is_hybrid());
        assert!(result.meta.version().is_hybrid());

        // v1 component
        let v1 = result.meta.as_v1().unwrap();
        assert_eq!(v1.info.total_length(), 65536);
        assert!(v1.info.length.is_some());
        assert!(v1.info.files.is_none());

        // v2 component
        let v2 = result.meta.as_v2().unwrap();
        assert_eq!(v2.info.meta_version, 2);
        assert_eq!(v2.info.piece_length, 32768);
        // File tree should have one file
        let files = v2.info.files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].attr.length, 65536);
        // Merkle root should exist
        assert!(files[0].attr.pieces_root.is_some());

        // Both hashes should come from the same info bytes
        assert!(v2.info_hashes.v1.is_some());
        assert!(v2.info_hashes.v2.is_some());
        assert_eq!(v2.info_hashes.v1.unwrap(), v1.info_hash);

        // Round-trip: re-parse the generated bytes as v1
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.info_hash, v1.info_hash);

        // Re-parse as v2 (detect.rs should detect hybrid)
        let detected = crate::detect::torrent_from_bytes_any(&result.bytes).unwrap();
        assert!(detected.is_hybrid());
    }

    #[test]
    fn hybrid_multi_file_round_trip() {
        let dir = make_test_dir();
        let result = CreateTorrent::new()
            .add_directory(dir.path())
            .set_name("hybrid-test")
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .set_version(TorrentVersion::Hybrid)
            .generate()
            .unwrap();

        assert!(result.meta.is_hybrid());

        let v1 = result.meta.as_v1().unwrap();
        assert!(v1.info.files.is_some());
        let v1_files = v1.info.files.as_ref().unwrap();
        assert_eq!(v1_files.len(), 2);
        assert_eq!(v1_files[0].path, vec!["aaa.txt"]);
        assert_eq!(v1_files[1].path, vec!["subdir", "bbb.bin"]);

        let v2 = result.meta.as_v2().unwrap();
        let v2_files = v2.info.files();
        assert_eq!(v2_files.len(), 2);

        // Round-trip
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.info_hash, v1.info_hash);
    }

    #[test]
    fn hybrid_has_piece_layers_for_large_file() {
        // Create a file larger than piece_size so piece_layers should be present
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&vec![0xCD; 65536]).unwrap();
        f.flush().unwrap();

        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(16384) // 4 pieces
            .set_creation_date(1000000)
            .set_version(TorrentVersion::Hybrid)
            .generate()
            .unwrap();

        assert!(result.meta.is_hybrid());
        let v2 = result.meta.as_v2().unwrap();
        // piece_layers should have an entry for this file (larger than piece_size)
        assert!(
            !v2.piece_layers.is_empty(),
            "piece_layers should not be empty for large file"
        );
    }

    #[test]
    fn hybrid_info_hash_differs_from_v1_only() {
        let f = make_test_file();

        let v1_result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        let hybrid_result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .set_version(TorrentVersion::Hybrid)
            .generate()
            .unwrap();

        // Hybrid info dict has extra keys (file tree, meta version) so hashes differ
        let v1_hash = v1_result.meta.as_v1().unwrap().info_hash;
        let hybrid_v1_hash = hybrid_result.meta.as_v1().unwrap().info_hash;
        assert_ne!(
            v1_hash, hybrid_v1_hash,
            "hybrid info dict should differ from v1-only"
        );
    }

    // ── V2-only torrent creation tests ─────────────────────────────────

    #[test]
    fn create_v2_only_single_file() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .set_version(TorrentVersion::V2Only)
            .generate()
            .expect("v2-only single file creation should succeed");

        assert!(result.meta.is_v2(), "should be V2 variant");
        let v2 = result.meta.as_v2().expect("as_v2 should succeed");
        assert_eq!(v2.info.meta_version, 2);
        assert_eq!(v2.info.piece_length, 32768);
        let files = v2.info.files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].attr.length, 65536);
    }

    #[test]
    fn create_v2_only_multi_file() {
        let dir = make_test_dir();
        let result = CreateTorrent::new()
            .add_directory(dir.path())
            .set_name("v2-multi")
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .set_version(TorrentVersion::V2Only)
            .generate()
            .expect("v2-only multi file creation should succeed");

        assert!(result.meta.is_v2(), "should be V2 variant");
        let v2 = result.meta.as_v2().expect("as_v2 should succeed");
        let files = v2.info.files();
        assert_eq!(files.len(), 2);
        // Files should be sorted: aaa.txt before subdir/bbb.bin
        assert_eq!(files[0].path, vec!["aaa.txt"]);
        assert_eq!(files[1].path, vec!["subdir", "bbb.bin"]);
        assert_eq!(files[0].attr.length, 12);
        assert_eq!(files[1].attr.length, 1000);
    }

    #[test]
    fn create_v2_only_has_piece_layers() {
        // Create a file larger than piece_size so piece_layers should be populated.
        let mut f = tempfile::NamedTempFile::new().expect("create temp file");
        f.write_all(&vec![0xCD; 65536]).expect("write temp file");
        f.flush().expect("flush temp file");

        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(16384) // 4 pieces
            .set_creation_date(1000000)
            .set_version(TorrentVersion::V2Only)
            .generate()
            .expect("v2-only creation should succeed");

        let v2 = result.meta.as_v2().expect("as_v2 should succeed");
        assert!(
            !v2.piece_layers.is_empty(),
            "piece_layers should not be empty for a file spanning multiple pieces"
        );
        // The single file's Merkle root should be the key in piece_layers.
        let files = v2.info.files();
        assert_eq!(files.len(), 1);
        let root = files[0]
            .attr
            .pieces_root
            .expect("file should have a pieces_root");
        assert!(
            v2.piece_layers.contains_key(&root),
            "piece_layers should contain an entry for the file's Merkle root"
        );
    }

    #[test]
    fn create_v2_only_no_v1_keys() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .set_creation_date(1000000)
            .set_version(TorrentVersion::V2Only)
            .generate()
            .expect("v2-only creation should succeed");

        // Re-parse the raw bytes as a generic BencodeValue to inspect keys.
        let value: torrent_bencode::BencodeValue =
            torrent_bencode::from_bytes(&result.bytes).expect("bencode parse");
        let root = value.as_dict().expect("root should be a dict");
        let info = root
            .get(b"info".as_ref())
            .expect("should have info key")
            .as_dict()
            .expect("info should be a dict");

        // v1-only keys must NOT be present.
        assert!(
            !info.contains_key(b"pieces".as_ref()),
            "v2-only should not have 'pieces'"
        );
        assert!(
            !info.contains_key(b"length".as_ref()),
            "v2-only should not have 'length'"
        );
        assert!(
            !info.contains_key(b"files".as_ref()),
            "v2-only should not have 'files'"
        );

        // v2 keys MUST be present.
        assert!(
            info.contains_key(b"file tree".as_ref()),
            "v2-only must have 'file tree'"
        );
        assert!(
            info.contains_key(b"meta version".as_ref()),
            "v2-only must have 'meta version'"
        );
        assert!(
            info.contains_key(b"name".as_ref()),
            "v2-only must have 'name'"
        );
        assert!(
            info.contains_key(b"piece length".as_ref()),
            "v2-only must have 'piece length'"
        );
    }

    #[test]
    fn create_v2_only_round_trip() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(32768)
            .set_creation_date(1000000)
            .set_version(TorrentVersion::V2Only)
            .generate()
            .expect("v2-only creation should succeed");

        // Round-trip through the auto-detect parser.
        let detected = crate::detect::torrent_from_bytes_any(&result.bytes)
            .expect("round-trip parse should succeed");

        assert!(detected.is_v2(), "detected should be V2 (not hybrid, not v1)");
        assert!(!detected.is_hybrid(), "detected should NOT be hybrid");

        let orig = result.meta.as_v2().expect("original as_v2");
        let rt = detected.as_v2().expect("round-trip as_v2");

        assert_eq!(rt.info.name, orig.info.name);
        assert_eq!(rt.info.piece_length, orig.info.piece_length);
        assert_eq!(rt.info.meta_version, orig.info.meta_version);
        assert_eq!(rt.info.files().len(), orig.info.files().len());
        assert_eq!(
            rt.info.files()[0].attr.length,
            orig.info.files()[0].attr.length
        );
    }

    #[test]
    fn create_v2_only_info_hash() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .add_file(f.path())
            .set_piece_size(65536)
            .set_creation_date(1000000)
            .set_version(TorrentVersion::V2Only)
            .generate()
            .expect("v2-only creation should succeed");

        let v2 = result.meta.as_v2().expect("as_v2 should succeed");
        let hashes = &v2.info_hashes;

        // Pure v2 should have no v1 hash and a v2 hash.
        assert!(hashes.v1.is_none(), "v2-only should have no v1 hash");
        assert!(hashes.v2.is_some(), "v2-only should have a v2 hash");

        // best_v1() should still return something (truncated from SHA-256).
        let _best = hashes.best_v1();

        // Manually compute SHA-256 of the info dict bytes and compare.
        let info_bytes = v2
            .info_bytes
            .as_ref()
            .expect("info_bytes should be present");
        let manual_hash = crate::sha256(info_bytes);
        assert_eq!(
            manual_hash,
            hashes.v2.expect("v2 hash present"),
            "manually computed SHA-256 of info dict should match info_hashes.v2"
        );
    }

    #[test]
    fn create_torrent_with_ssl_cert() {
        let cert_pem = b"-----BEGIN CERTIFICATE-----\nMIIBtest\n-----END CERTIFICATE-----\n";
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.bin");
        std::fs::write(&file_path, vec![0u8; 65536]).unwrap();

        let result = CreateTorrent::new()
            .add_file(&file_path)
            .set_ssl_cert(cert_pem.to_vec())
            .generate()
            .unwrap();

        // Parse back and verify ssl-cert round-trips
        let parsed = torrent_from_bytes(&result.bytes).unwrap();
        assert_eq!(parsed.ssl_cert.as_deref().unwrap(), cert_pem.as_slice());
        assert_eq!(
            parsed.info.ssl_cert.as_deref().unwrap(),
            cert_pem.as_slice()
        );
    }
}
