use serde::Deserialize;

use crate::error::Error;
use crate::hash::Id20;

/// Parsed .torrent file (BEP 3 metainfo, v1).
#[derive(Debug, Clone)]
pub struct TorrentMetaV1 {
    /// The info hash (SHA1 of the raw "info" dict bytes).
    pub info_hash: Id20,
    /// Primary announce URL.
    pub announce: Option<String>,
    /// Announce list (BEP 12) — list of tracker tiers.
    pub announce_list: Option<Vec<Vec<String>>>,
    /// Comment.
    pub comment: Option<String>,
    /// Created by.
    pub created_by: Option<String>,
    /// Creation date (unix timestamp).
    pub creation_date: Option<i64>,
    /// Info dictionary.
    pub info: InfoDict,
}

/// The "info" dictionary from a .torrent file.
#[derive(Debug, Clone, Deserialize)]
pub struct InfoDict {
    /// Suggested file/directory name.
    pub name: String,
    /// Piece length in bytes.
    #[serde(rename = "piece length")]
    pub piece_length: u64,
    /// Concatenated SHA1 hashes of each piece (20 bytes each).
    #[serde(with = "serde_bytes")]
    pub pieces: Vec<u8>,
    /// Length in bytes (single-file mode).
    pub length: Option<u64>,
    /// Files (multi-file mode).
    pub files: Option<Vec<FileEntry>>,
    /// Private flag.
    pub private: Option<i64>,
}

/// A file entry in multi-file mode.
#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    /// File length in bytes.
    pub length: u64,
    /// Path components (e.g., ["dir", "file.txt"]).
    pub path: Vec<String>,
}

/// High-level file info (unified from single-file and multi-file modes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// Relative path components.
    pub path: Vec<String>,
    /// File length in bytes.
    pub length: u64,
}

/// Raw top-level torrent structure for serde deserialization.
#[derive(Deserialize)]
struct RawTorrent {
    announce: Option<String>,
    #[serde(rename = "announce-list")]
    announce_list: Option<Vec<Vec<String>>>,
    comment: Option<String>,
    #[serde(rename = "created by")]
    created_by: Option<String>,
    #[serde(rename = "creation date")]
    creation_date: Option<i64>,
    info: InfoDict,
}

/// Parse a .torrent file from raw bytes.
///
/// Computes the info-hash by finding the raw byte span of the "info" key
/// and SHA1-hashing it directly (not the re-serialized form).
pub fn torrent_from_bytes(data: &[u8]) -> Result<TorrentMetaV1, Error> {
    // Step 1: Find the raw info dict span for hashing
    let info_span = ferrite_bencode::find_dict_key_span(data, "info")?;
    let info_hash = crate::sha1(&data[info_span]);

    // Step 2: Deserialize the full structure
    let raw: RawTorrent = ferrite_bencode::from_bytes(data)?;

    // Step 3: Validate the info dict
    validate_info(&raw.info)?;

    Ok(TorrentMetaV1 {
        info_hash,
        announce: raw.announce,
        announce_list: raw.announce_list,
        comment: raw.comment,
        created_by: raw.created_by,
        creation_date: raw.creation_date,
        info: raw.info,
    })
}

fn validate_info(info: &InfoDict) -> Result<(), Error> {
    if info.piece_length == 0 {
        return Err(Error::InvalidTorrent("piece length is 0".into()));
    }

    if !info.pieces.len().is_multiple_of(20) {
        return Err(Error::InvalidTorrent(format!(
            "pieces length {} is not a multiple of 20",
            info.pieces.len()
        )));
    }

    if info.length.is_none() && info.files.is_none() {
        return Err(Error::InvalidTorrent(
            "neither 'length' nor 'files' present".into(),
        ));
    }

    if info.length.is_some() && info.files.is_some() {
        return Err(Error::InvalidTorrent(
            "both 'length' and 'files' present".into(),
        ));
    }

    Ok(())
}

impl InfoDict {
    /// Total size of all files in bytes.
    pub fn total_length(&self) -> u64 {
        if let Some(length) = self.length {
            length
        } else if let Some(ref files) = self.files {
            files.iter().map(|f| f.length).sum()
        } else {
            0
        }
    }

    /// Number of pieces.
    pub fn num_pieces(&self) -> usize {
        self.pieces.len() / 20
    }

    /// Get the SHA1 hash for a specific piece.
    pub fn piece_hash(&self, index: usize) -> Option<Id20> {
        let start = index * 20;
        if start + 20 > self.pieces.len() {
            return None;
        }
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&self.pieces[start..start + 20]);
        Some(Id20(hash))
    }

    /// Get file info in a unified format.
    pub fn files(&self) -> Vec<FileInfo> {
        if let Some(length) = self.length {
            vec![FileInfo {
                path: vec![self.name.clone()],
                length,
            }]
        } else if let Some(ref files) = self.files {
            files
                .iter()
                .map(|f| {
                    let mut path = vec![self.name.clone()];
                    path.extend(f.path.clone());
                    FileInfo {
                        path,
                        length: f.length,
                    }
                })
                .collect()
        } else {
            vec![]
        }
    }
}
