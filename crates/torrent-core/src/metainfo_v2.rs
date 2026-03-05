//! BitTorrent v2 metainfo types (BEP 52).
//!
//! v2 uses a nested file tree instead of a flat file list, SHA-256 instead
//! of SHA-1, and per-file Merkle hash trees with piece alignment.

use std::collections::BTreeMap;

use bytes::Bytes;
use torrent_bencode::BencodeValue;

use crate::error::Error;
use crate::file_tree::{FileTreeNode, V2FileInfo};
use crate::hash::{Id20, Id32};
use crate::info_hashes::InfoHashes;

/// v2 info dictionary (BEP 52).
///
/// Key difference from v1: files are represented as a nested tree, and `piece_length`
/// is a global value (power of 2, ≥ 16 KiB). Files are aligned to piece boundaries —
/// each file starts on a new piece, and the last piece of each file may be shorter.
#[derive(Debug, Clone)]
pub struct InfoDictV2 {
    /// Suggested name for the torrent.
    pub name: String,
    /// Global piece length in bytes. Must be a power of 2, ≥ 16384.
    pub piece_length: u64,
    /// Meta version — always 2 for v2 torrents.
    pub meta_version: u64,
    /// The nested file tree.
    pub file_tree: FileTreeNode,
    /// BEP 35 / SSL torrent: PEM-encoded X.509 CA certificate.
    /// When present, all peer connections must use TLS with certs chaining to this CA.
    pub ssl_cert: Option<Vec<u8>>,
}

impl InfoDictV2 {
    /// Get all files as a flat list.
    pub fn files(&self) -> Vec<V2FileInfo> {
        self.file_tree.flatten()
    }

    /// Total size of all files in bytes.
    pub fn total_length(&self) -> u64 {
        self.files().iter().map(|f| f.attr.length).sum()
    }

    /// Total number of pieces across all files.
    ///
    /// In v2, each file starts on a new piece boundary, so the total is the
    /// sum of per-file piece counts (not simply `ceil(total_length / piece_length)`).
    pub fn num_pieces(&self) -> u32 {
        self.files()
            .iter()
            .map(|f| file_piece_count(f.attr.length, self.piece_length))
            .sum()
    }

    /// Per-file piece ranges: `(file_info, global_piece_offset, file_piece_count)`.
    ///
    /// Each file starts at a new global piece offset due to v2 alignment.
    pub fn file_piece_ranges(&self) -> Vec<(V2FileInfo, u32, u32)> {
        let files = self.files();
        let mut result = Vec::with_capacity(files.len());
        let mut offset = 0u32;

        for file in files {
            let count = file_piece_count(file.attr.length, self.piece_length);
            result.push((file, offset, count));
            offset += count;
        }

        result
    }
}

/// Number of pieces for a single file with v2 alignment.
fn file_piece_count(file_length: u64, piece_length: u64) -> u32 {
    if file_length == 0 {
        return 0;
    }
    file_length.div_ceil(piece_length) as u32
}

/// Validate a v2 info dict.
pub fn validate_info_v2(info: &InfoDictV2) -> Result<(), Error> {
    if info.meta_version != 2 {
        return Err(Error::InvalidTorrent(format!(
            "expected meta version 2, got {}",
            info.meta_version
        )));
    }

    if info.piece_length < 16384 {
        return Err(Error::InvalidTorrent(format!(
            "piece length {} is less than minimum 16384",
            info.piece_length
        )));
    }

    if !info.piece_length.is_power_of_two() {
        return Err(Error::InvalidTorrent(format!(
            "piece length {} is not a power of 2",
            info.piece_length
        )));
    }

    Ok(())
}

/// Parsed v2 .torrent file (BEP 52).
#[derive(Debug, Clone)]
pub struct TorrentMetaV2 {
    /// Unified info hashes (v2 SHA-256, optionally truncated v1 for compat).
    pub info_hashes: InfoHashes,
    /// Raw info dict bytes for BEP 9 metadata serving.
    pub info_bytes: Option<Bytes>,
    /// Primary announce URL.
    pub announce: Option<String>,
    /// Announce list (BEP 12).
    pub announce_list: Option<Vec<Vec<String>>>,
    /// Comment.
    pub comment: Option<String>,
    /// Created by.
    pub created_by: Option<String>,
    /// Creation date (unix timestamp).
    pub creation_date: Option<i64>,
    /// v2 info dictionary.
    pub info: InfoDictV2,
    /// Piece layers: `pieces_root → concatenated SHA-256 hashes`.
    ///
    /// Each entry maps a file's Merkle root to the concatenated piece-level
    /// hashes. Only present for files larger than `piece_length`.
    pub piece_layers: BTreeMap<Id32, Vec<u8>>,
    /// PEM-encoded SSL CA certificate from the info dict, if present.
    pub ssl_cert: Option<Vec<u8>>,
}

impl TorrentMetaV2 {
    /// Validate that piece layers match the file tree.
    ///
    /// Each file larger than `piece_length` must have a corresponding piece layer
    /// with the correct number of hashes.
    pub fn validate_piece_layers(&self) -> Result<(), Error> {
        for file in self.info.files() {
            if file.attr.length <= self.info.piece_length {
                continue; // Small files don't need piece layers
            }

            let root = file.attr.pieces_root.ok_or_else(|| {
                Error::InvalidTorrent(format!(
                    "file {:?} has length {} but no pieces_root",
                    file.path, file.attr.length
                ))
            })?;

            let layer = self.piece_layers.get(&root).ok_or_else(|| {
                Error::InvalidTorrent(format!(
                    "missing piece layer for file {:?} (root: {})",
                    file.path, root
                ))
            })?;

            let expected_pieces =
                file_piece_count(file.attr.length, self.info.piece_length) as usize;
            let actual_hashes = layer.len() / 32;

            if layer.len() % 32 != 0 {
                return Err(Error::InvalidTorrent(format!(
                    "piece layer for {:?} has length {} which is not a multiple of 32",
                    file.path,
                    layer.len()
                )));
            }

            if actual_hashes != expected_pieces {
                return Err(Error::InvalidTorrent(format!(
                    "piece layer for {:?} has {} hashes, expected {}",
                    file.path, actual_hashes, expected_pieces
                )));
            }
        }
        Ok(())
    }

    /// Get piece hashes for a file by its Merkle root.
    pub fn file_piece_hashes(&self, pieces_root: &Id32) -> Option<Vec<Id32>> {
        let layer = self.piece_layers.get(pieces_root)?;
        Some(
            layer
                .chunks_exact(32)
                .map(|chunk| {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(chunk);
                    Id32(hash)
                })
                .collect(),
        )
    }

    /// Access piece layer by file index (libtorrent parity).
    pub fn piece_layer_for_file(&self, file_index: usize) -> Option<Vec<Id32>> {
        let files = self.info.files();
        let file = files.get(file_index)?;
        let root = file.attr.pieces_root.as_ref()?;
        self.file_piece_hashes(root)
    }

    /// Take and clear piece layers (memory optimization, libtorrent parity).
    ///
    /// Returns the layers and clears them from the struct. Useful after
    /// verification is complete to free memory.
    pub fn take_piece_layers(&mut self) -> BTreeMap<Id32, Vec<u8>> {
        std::mem::take(&mut self.piece_layers)
    }
}

/// Parse a v2 .torrent file from raw bytes.
pub fn torrent_v2_from_bytes(data: &[u8]) -> Result<TorrentMetaV2, Error> {
    // Step 1: Find raw info dict span for hashing
    let info_span = torrent_bencode::find_dict_key_span(data, "info")?;
    let info_hash_v2 = crate::sha256(&data[info_span.clone()]);
    let info_raw = Bytes::copy_from_slice(&data[info_span]);

    // Truncated v1 hash for tracker/DHT compat
    let mut v1_truncated = [0u8; 20];
    v1_truncated.copy_from_slice(&info_hash_v2.0[..20]);
    let info_hashes = InfoHashes {
        v1: Some(Id20(v1_truncated)),
        v2: Some(info_hash_v2),
    };

    // Step 2: Parse the full structure as BencodeValue (can't use serde for v2 file tree)
    let root: BencodeValue = torrent_bencode::from_bytes(data)?;
    let root_dict = root
        .as_dict()
        .ok_or_else(|| Error::InvalidTorrent("torrent must be a dict".into()))?;

    // Step 3: Parse info dict
    let info_value = root_dict
        .get(b"info".as_ref())
        .ok_or_else(|| Error::InvalidTorrent("missing 'info' key".into()))?;
    let info_dict = info_value
        .as_dict()
        .ok_or_else(|| Error::InvalidTorrent("'info' must be a dict".into()))?;

    let name = info_dict
        .get(b"name".as_ref())
        .and_then(|v| v.as_bytes_raw())
        .and_then(|b| std::str::from_utf8(b).ok())
        .ok_or_else(|| Error::InvalidTorrent("missing or invalid 'name' in info".into()))?
        .to_owned();

    let piece_length = info_dict
        .get(b"piece length".as_ref())
        .and_then(|v| v.as_int())
        .ok_or_else(|| Error::InvalidTorrent("missing 'piece length' in info".into()))?
        as u64;

    let meta_version = info_dict
        .get(b"meta version".as_ref())
        .and_then(|v| v.as_int())
        .ok_or_else(|| Error::InvalidTorrent("missing 'meta version' in info".into()))?
        as u64;

    let file_tree_value = info_dict
        .get(b"file tree".as_ref())
        .ok_or_else(|| Error::InvalidTorrent("missing 'file tree' in info".into()))?;
    let file_tree = FileTreeNode::from_bencode(file_tree_value)?;

    let ssl_cert = info_dict
        .get(b"ssl-cert".as_ref())
        .and_then(|v| v.as_bytes_raw())
        .map(|b| b.to_vec());

    let info = InfoDictV2 {
        name,
        piece_length,
        meta_version,
        file_tree,
        ssl_cert: ssl_cert.clone(),
    };

    validate_info_v2(&info)?;

    // Step 4: Parse optional top-level keys
    let announce = root_dict
        .get(b"announce".as_ref())
        .and_then(|v| v.as_bytes_raw())
        .and_then(|b| std::str::from_utf8(b).ok())
        .map(|s| s.to_owned());

    let announce_list = root_dict.get(b"announce-list".as_ref()).and_then(|v| {
        v.as_list().map(|tiers| {
            tiers
                .iter()
                .filter_map(|tier| {
                    tier.as_list().map(|urls| {
                        urls.iter()
                            .filter_map(|u| {
                                u.as_bytes_raw()
                                    .and_then(|b| std::str::from_utf8(b).ok())
                                    .map(|s| s.to_owned())
                            })
                            .collect()
                    })
                })
                .collect()
        })
    });

    let comment = root_dict
        .get(b"comment".as_ref())
        .and_then(|v| v.as_bytes_raw())
        .and_then(|b| std::str::from_utf8(b).ok())
        .map(|s| s.to_owned());

    let created_by = root_dict
        .get(b"created by".as_ref())
        .and_then(|v| v.as_bytes_raw())
        .and_then(|b| std::str::from_utf8(b).ok())
        .map(|s| s.to_owned());

    let creation_date = root_dict
        .get(b"creation date".as_ref())
        .and_then(|v| v.as_int());

    // Step 5: Parse piece layers
    let piece_layers = parse_piece_layers(root_dict)?;

    Ok(TorrentMetaV2 {
        info_hashes,
        info_bytes: Some(info_raw),
        announce,
        announce_list,
        comment,
        created_by,
        creation_date,
        info,
        piece_layers,
        ssl_cert,
    })
}

/// Parse the "piece layers" dict from the top-level torrent dict.
fn parse_piece_layers(
    root_dict: &BTreeMap<Vec<u8>, BencodeValue>,
) -> Result<BTreeMap<Id32, Vec<u8>>, Error> {
    let mut layers = BTreeMap::new();

    let Some(layers_value) = root_dict.get(b"piece layers".as_ref()) else {
        return Ok(layers);
    };

    let layers_dict = layers_value
        .as_dict()
        .ok_or_else(|| Error::InvalidTorrent("'piece layers' must be a dict".into()))?;

    for (key, value) in layers_dict {
        let root = Id32::from_bytes(key)?;
        let hashes = value
            .as_bytes_raw()
            .ok_or_else(|| Error::InvalidTorrent("piece layer value must be bytes".into()))?;
        layers.insert(root, hashes.to_vec());
    }

    Ok(layers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal v2 torrent as raw bencode bytes.
    fn make_v2_torrent_bytes(
        name: &str,
        piece_length: u64,
        files: &[(&str, u64, Option<[u8; 32]>)],
        piece_layers: &[([u8; 32], Vec<u8>)],
    ) -> Vec<u8> {
        let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();

        // Build info dict
        let mut info_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();

        // Build file tree using BencodeValue
        let mut ft_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        for &(fname, length, ref root) in files {
            let mut attr_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
            attr_map.insert(b"length".to_vec(), BencodeValue::Integer(length as i64));
            if let Some(root_bytes) = root {
                attr_map.insert(
                    b"pieces root".to_vec(),
                    BencodeValue::Bytes(root_bytes.to_vec()),
                );
            }

            let mut file_node: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
            file_node.insert(b"".to_vec(), BencodeValue::Dict(attr_map));

            ft_map.insert(fname.as_bytes().to_vec(), BencodeValue::Dict(file_node));
        }

        info_map.insert(b"file tree".to_vec(), BencodeValue::Dict(ft_map));
        info_map.insert(b"meta version".to_vec(), BencodeValue::Integer(2));
        info_map.insert(
            b"name".to_vec(),
            BencodeValue::Bytes(name.as_bytes().to_vec()),
        );
        info_map.insert(
            b"piece length".to_vec(),
            BencodeValue::Integer(piece_length as i64),
        );

        root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

        // Add piece layers if any
        if !piece_layers.is_empty() {
            let mut pl_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
            for (root_hash, hashes) in piece_layers {
                pl_map.insert(root_hash.to_vec(), BencodeValue::Bytes(hashes.clone()));
            }
            root_map.insert(b"piece layers".to_vec(), BencodeValue::Dict(pl_map));
        }

        torrent_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap()
    }

    #[test]
    fn parse_minimal_v2_torrent() {
        let data = make_v2_torrent_bytes("test", 16384, &[("file.txt", 1024, None)], &[]);
        let torrent = torrent_v2_from_bytes(&data).unwrap();

        assert_eq!(torrent.info.name, "test");
        assert_eq!(torrent.info.piece_length, 16384);
        assert_eq!(torrent.info.meta_version, 2);
        assert_eq!(torrent.info.total_length(), 1024);
        assert!(torrent.info_hashes.has_v2());
    }

    #[test]
    fn parse_with_piece_layers() {
        let root = [0xABu8; 32];
        // 2 pieces worth of hashes (2 * 32 bytes)
        let hashes = vec![0xCDu8; 64];
        let data = make_v2_torrent_bytes(
            "test",
            16384,
            &[("big.dat", 32768, Some(root))],
            &[(root, hashes)],
        );

        let torrent = torrent_v2_from_bytes(&data).unwrap();
        assert_eq!(torrent.piece_layers.len(), 1);
        let layer = torrent.piece_layers.get(&Id32(root)).unwrap();
        assert_eq!(layer.len(), 64);
    }

    #[test]
    fn reject_bad_meta_version() {
        // Build manually with meta_version = 1
        let mut info_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        let mut ft_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        let mut attr_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        attr_map.insert(b"length".to_vec(), BencodeValue::Integer(100));
        let mut file_node: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        file_node.insert(b"".to_vec(), BencodeValue::Dict(attr_map));
        ft_map.insert(b"f.txt".to_vec(), BencodeValue::Dict(file_node));

        info_map.insert(b"file tree".to_vec(), BencodeValue::Dict(ft_map));
        info_map.insert(b"meta version".to_vec(), BencodeValue::Integer(1));
        info_map.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
        info_map.insert(b"piece length".to_vec(), BencodeValue::Integer(16384));

        let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

        let data = torrent_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap();
        assert!(torrent_v2_from_bytes(&data).is_err());
    }

    #[test]
    fn info_bytes_populated() {
        let data = make_v2_torrent_bytes("test", 16384, &[("f.txt", 100, None)], &[]);
        let torrent = torrent_v2_from_bytes(&data).unwrap();
        assert!(torrent.info_bytes.is_some());
        // Re-hashing should produce the same v2 hash
        let rehash = crate::sha256(&torrent.info_bytes.unwrap());
        assert_eq!(rehash, torrent.info_hashes.v2.unwrap());
    }

    #[test]
    fn piece_layer_for_file_indexed() {
        let root = [0xABu8; 32];
        let hashes = vec![0xCDu8; 64]; // 2 hashes
        let data = make_v2_torrent_bytes(
            "test",
            16384,
            &[("a.txt", 100, None), ("big.dat", 32768, Some(root))],
            &[(root, hashes)],
        );

        let torrent = torrent_v2_from_bytes(&data).unwrap();

        // File 0 (small, no piece layer)
        assert!(torrent.piece_layer_for_file(0).is_none());
        // File 1 (has piece layer)
        let pieces = torrent.piece_layer_for_file(1).unwrap();
        assert_eq!(pieces.len(), 2);
    }

    // === InfoDictV2 validation tests ===

    #[test]
    fn valid_info_dict_v2() {
        let data = make_v2_torrent_bytes("test", 16384, &[("f.txt", 1000, None)], &[]);
        let torrent = torrent_v2_from_bytes(&data).unwrap();
        assert_eq!(torrent.info.meta_version, 2);
    }

    #[test]
    fn reject_piece_length_not_power_of_two() {
        let mut info_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        let mut ft_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        let mut attr_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        attr_map.insert(b"length".to_vec(), BencodeValue::Integer(100));
        let mut file_node: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        file_node.insert(b"".to_vec(), BencodeValue::Dict(attr_map));
        ft_map.insert(b"f.txt".to_vec(), BencodeValue::Dict(file_node));

        info_map.insert(b"file tree".to_vec(), BencodeValue::Dict(ft_map));
        info_map.insert(b"meta version".to_vec(), BencodeValue::Integer(2));
        info_map.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
        info_map.insert(b"piece length".to_vec(), BencodeValue::Integer(30000));

        let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

        let data = torrent_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap();
        assert!(torrent_v2_from_bytes(&data).is_err());
    }

    #[test]
    fn reject_piece_length_too_small() {
        let mut info_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        let mut ft_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        let mut attr_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        attr_map.insert(b"length".to_vec(), BencodeValue::Integer(100));
        let mut file_node: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        file_node.insert(b"".to_vec(), BencodeValue::Dict(attr_map));
        ft_map.insert(b"f.txt".to_vec(), BencodeValue::Dict(file_node));

        info_map.insert(b"file tree".to_vec(), BencodeValue::Dict(ft_map));
        info_map.insert(b"meta version".to_vec(), BencodeValue::Integer(2));
        info_map.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
        info_map.insert(b"piece length".to_vec(), BencodeValue::Integer(8192));

        let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

        let data = torrent_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap();
        assert!(torrent_v2_from_bytes(&data).is_err());
    }

    #[test]
    fn files_list_and_total_length() {
        let data = make_v2_torrent_bytes(
            "test",
            16384,
            &[("a.txt", 100, None), ("b.txt", 200, None)],
            &[],
        );
        let torrent = torrent_v2_from_bytes(&data).unwrap();
        let files = torrent.info.files();
        assert_eq!(files.len(), 2);
        assert_eq!(torrent.info.total_length(), 300);
    }

    #[test]
    fn num_pieces_with_per_file_alignment() {
        // Two files, each 16384 bytes = 1 piece each = 2 total pieces
        let data = make_v2_torrent_bytes(
            "test",
            16384,
            &[("a.dat", 16384, None), ("b.dat", 16384, None)],
            &[],
        );
        let torrent = torrent_v2_from_bytes(&data).unwrap();
        assert_eq!(torrent.info.num_pieces(), 2);

        // Two files of 1 byte each = 1 piece each = 2 total
        // (v2 aligns each file to piece boundary)
        let data2 = make_v2_torrent_bytes(
            "test",
            16384,
            &[("a.dat", 1, None), ("b.dat", 1, None)],
            &[],
        );
        let torrent2 = torrent_v2_from_bytes(&data2).unwrap();
        assert_eq!(torrent2.info.num_pieces(), 2);
    }

    // === Piece layer validation tests ===

    #[test]
    fn correct_layers_pass_validation() {
        let root = [0xABu8; 32];
        // File is 32768 bytes with piece_length 16384 = 2 pieces
        let hashes = vec![0xCDu8; 64]; // 2 * 32 bytes
        let data = make_v2_torrent_bytes(
            "test",
            16384,
            &[("big.dat", 32768, Some(root))],
            &[(root, hashes)],
        );
        let torrent = torrent_v2_from_bytes(&data).unwrap();
        assert!(torrent.validate_piece_layers().is_ok());
    }

    #[test]
    fn missing_layer_fails_validation() {
        let root = [0xABu8; 32];
        // File needs piece layer but none provided
        let data = make_v2_torrent_bytes(
            "test",
            16384,
            &[("big.dat", 32768, Some(root))],
            &[], // no layers!
        );
        let torrent = torrent_v2_from_bytes(&data).unwrap();
        assert!(torrent.validate_piece_layers().is_err());
    }

    #[test]
    fn small_file_no_layer_needed() {
        // File smaller than piece_length doesn't need a layer
        let data = make_v2_torrent_bytes("test", 16384, &[("small.txt", 100, None)], &[]);
        let torrent = torrent_v2_from_bytes(&data).unwrap();
        assert!(torrent.validate_piece_layers().is_ok());
    }

    #[test]
    fn wrong_hash_count_fails_validation() {
        let root = [0xABu8; 32];
        // File is 32768 bytes = 2 pieces, but we provide 3 hashes
        let hashes = vec![0xCDu8; 96]; // 3 * 32 bytes (wrong!)
        let data = make_v2_torrent_bytes(
            "test",
            16384,
            &[("big.dat", 32768, Some(root))],
            &[(root, hashes)],
        );
        let torrent = torrent_v2_from_bytes(&data).unwrap();
        assert!(torrent.validate_piece_layers().is_err());
    }
}
