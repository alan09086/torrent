//! Auto-detection of torrent format (v1 vs v2).
//!
//! Inspects the info dict for `meta version = 2` to dispatch between
//! `torrent_from_bytes()` (v1) and `torrent_v2_from_bytes()` (v2).
//! M35 will add a `Hybrid` variant for v1+v2 torrents.

use ferrite_bencode::BencodeValue;

use crate::error::Error;
use crate::info_hashes::InfoHashes;
use crate::metainfo::TorrentMetaV1;
use crate::metainfo_v2::TorrentMetaV2;

/// A parsed torrent file — either v1 or v2.
#[derive(Debug, Clone)]
pub enum TorrentMeta {
    /// BitTorrent v1 (BEP 3).
    V1(TorrentMetaV1),
    /// BitTorrent v2 (BEP 52).
    V2(TorrentMetaV2),
}

impl TorrentMeta {
    /// Get the unified info hashes.
    pub fn info_hashes(&self) -> InfoHashes {
        match self {
            TorrentMeta::V1(t) => InfoHashes::v1_only(t.info_hash),
            TorrentMeta::V2(t) => t.info_hashes.clone(),
        }
    }

    /// Whether this is a v1 torrent.
    pub fn is_v1(&self) -> bool {
        matches!(self, TorrentMeta::V1(_))
    }

    /// Whether this is a v2 torrent.
    pub fn is_v2(&self) -> bool {
        matches!(self, TorrentMeta::V2(_))
    }
}

/// Auto-detect and parse a .torrent file as v1 or v2.
///
/// Detection: if the info dict contains `meta version` with value 2, parse as v2.
/// Otherwise parse as v1.
pub fn torrent_from_bytes_any(data: &[u8]) -> Result<TorrentMeta, Error> {
    if is_v2(data)? {
        Ok(TorrentMeta::V2(crate::metainfo_v2::torrent_v2_from_bytes(
            data,
        )?))
    } else {
        Ok(TorrentMeta::V1(crate::metainfo::torrent_from_bytes(data)?))
    }
}

/// Check if a torrent's info dict contains `meta version = 2`.
fn is_v2(data: &[u8]) -> Result<bool, Error> {
    let root: BencodeValue = ferrite_bencode::from_bytes(data)?;
    let root_dict = root
        .as_dict()
        .ok_or_else(|| Error::InvalidTorrent("torrent must be a dict".into()))?;
    let info = root_dict
        .get(b"info".as_ref())
        .and_then(|v| v.as_dict())
        .ok_or_else(|| Error::InvalidTorrent("missing or invalid 'info' dict".into()))?;

    Ok(info
        .get(b"meta version".as_ref())
        .and_then(|v| v.as_int())
        == Some(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detect_v1() {
        // Build a v1 torrent
        let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi256e6:pieces20:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00ee";
        let meta = torrent_from_bytes_any(data).unwrap();
        assert!(meta.is_v1());
        assert!(!meta.is_v2());
    }

    #[test]
    fn auto_detect_v2() {
        use std::collections::BTreeMap;

        // Build a minimal v2 torrent
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
        info_map.insert(b"piece length".to_vec(), BencodeValue::Integer(16384));

        let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

        let data = ferrite_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap();
        let meta = torrent_from_bytes_any(&data).unwrap();
        assert!(meta.is_v2());
        assert!(!meta.is_v1());
    }

    #[test]
    fn enum_queries_work() {
        let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi256e6:pieces20:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00ee";
        let meta = torrent_from_bytes_any(data).unwrap();
        let hashes = meta.info_hashes();
        assert!(hashes.has_v1());
        assert!(!hashes.has_v2());
    }
}
