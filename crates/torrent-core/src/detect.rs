//! Auto-detection of torrent format (v1, v2, or hybrid).
//!
//! Inspects the info dict for `meta version` and `pieces` to distinguish:
//! - V1: no `meta version = 2`
//! - V2: `meta version = 2` without v1 `pieces` key
//! - Hybrid: `meta version = 2` AND v1 `pieces` key present

use torrent_bencode::BencodeValue;

use crate::error::Error;
use crate::info_hashes::InfoHashes;
use crate::metainfo::TorrentMetaV1;
use crate::metainfo_v2::TorrentMetaV2;
use crate::torrent_version::TorrentVersion;

/// A parsed torrent file — v1, v2, or hybrid.
#[derive(Debug, Clone)]
pub enum TorrentMeta {
    /// BitTorrent v1 (BEP 3).
    V1(TorrentMetaV1),
    /// BitTorrent v2 (BEP 52).
    V2(TorrentMetaV2),
    /// Hybrid v1+v2 (BEP 52). Contains both metadata representations.
    /// Boxed to avoid inflating the enum size for the common V1/V2 cases.
    Hybrid(Box<TorrentMetaV1>, Box<TorrentMetaV2>),
}

impl TorrentMeta {
    /// Get the unified info hashes.
    pub fn info_hashes(&self) -> InfoHashes {
        match self {
            TorrentMeta::V1(t) => InfoHashes::v1_only(t.info_hash),
            TorrentMeta::V2(t) => t.info_hashes.clone(),
            TorrentMeta::Hybrid(v1, v2) => InfoHashes::hybrid(
                v1.info_hash,
                v2.info_hashes.v2.expect("v2 torrent must have v2 hash"),
            ),
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

    /// Whether this is a hybrid v1+v2 torrent.
    pub fn is_hybrid(&self) -> bool {
        matches!(self, TorrentMeta::Hybrid(_, _))
    }

    /// Get the protocol version enum.
    pub fn version(&self) -> TorrentVersion {
        match self {
            TorrentMeta::V1(_) => TorrentVersion::V1Only,
            TorrentMeta::V2(_) => TorrentVersion::V2Only,
            TorrentMeta::Hybrid(_, _) => TorrentVersion::Hybrid,
        }
    }

    /// Access the v1 metadata, if present (V1 or Hybrid).
    pub fn as_v1(&self) -> Option<&TorrentMetaV1> {
        match self {
            TorrentMeta::V1(v1) => Some(v1),
            TorrentMeta::Hybrid(v1, _) => Some(v1),
            TorrentMeta::V2(_) => None,
        }
    }

    /// Access the v2 metadata, if present (V2 or Hybrid).
    pub fn as_v2(&self) -> Option<&TorrentMetaV2> {
        match self {
            TorrentMeta::V2(v2) => Some(v2),
            TorrentMeta::Hybrid(_, v2) => Some(v2),
            TorrentMeta::V1(_) => None,
        }
    }

    /// Get the best v1-compatible info hash for session identification.
    ///
    /// For v1 and hybrid: returns the v1 SHA-1 info hash.
    /// For v2-only: returns the v2 SHA-256 hash truncated to 20 bytes.
    pub fn best_v1_info_hash(&self) -> crate::hash::Id20 {
        self.info_hashes().best_v1()
    }

    /// Get the SSL CA certificate bytes (PEM), if this is an SSL torrent.
    pub fn ssl_cert(&self) -> Option<&[u8]> {
        match self {
            TorrentMeta::V1(v1) => v1.ssl_cert.as_deref(),
            TorrentMeta::V2(v2) => v2.ssl_cert.as_deref(),
            TorrentMeta::Hybrid(v1, _) => v1.ssl_cert.as_deref(),
        }
    }

    /// Whether this torrent requires SSL peer connections.
    pub fn is_ssl(&self) -> bool {
        self.ssl_cert().is_some()
    }
}

impl From<TorrentMetaV1> for TorrentMeta {
    fn from(meta: TorrentMetaV1) -> Self {
        TorrentMeta::V1(meta)
    }
}

impl From<TorrentMetaV2> for TorrentMeta {
    fn from(meta: TorrentMetaV2) -> Self {
        TorrentMeta::V2(meta)
    }
}

/// Detected version of a .torrent file's info dict.
enum DetectedVersion {
    V1Only,
    V2Only,
    Hybrid,
}

/// Auto-detect and parse a .torrent file as v1, v2, or hybrid.
///
/// Detection:
/// - `meta version = 2` AND `pieces` key present -> Hybrid
/// - `meta version = 2` only -> V2
/// - Otherwise -> V1
pub fn torrent_from_bytes_any(data: &[u8]) -> Result<TorrentMeta, Error> {
    match detect_version(data)? {
        DetectedVersion::V1Only => {
            Ok(TorrentMeta::V1(crate::metainfo::torrent_from_bytes(data)?))
        }
        DetectedVersion::V2Only => Ok(TorrentMeta::V2(
            crate::metainfo_v2::torrent_v2_from_bytes(data)?,
        )),
        DetectedVersion::Hybrid => {
            let v1 = crate::metainfo::torrent_from_bytes(data)?;
            let mut v2 = crate::metainfo_v2::torrent_v2_from_bytes(data)?;
            // Override the truncated v1 hash with the REAL SHA-1 info hash.
            // In hybrid torrents, the v1 hash is SHA-1 of the raw info dict,
            // not a truncation of the SHA-256 hash.
            v2.info_hashes.v1 = Some(v1.info_hash);
            Ok(TorrentMeta::Hybrid(Box::new(v1), Box::new(v2)))
        }
    }
}

/// Detect the version of a torrent from its raw bencode bytes.
fn detect_version(data: &[u8]) -> Result<DetectedVersion, Error> {
    let root: BencodeValue = torrent_bencode::from_bytes(data)?;
    let root_dict = root
        .as_dict()
        .ok_or_else(|| Error::InvalidTorrent("torrent must be a dict".into()))?;
    let info = root_dict
        .get(b"info".as_ref())
        .and_then(|v| v.as_dict())
        .ok_or_else(|| Error::InvalidTorrent("missing or invalid 'info' dict".into()))?;

    let has_v2 = info
        .get(b"meta version".as_ref())
        .and_then(|v| v.as_int())
        == Some(2);

    let has_v1_pieces = info.get(b"pieces".as_ref()).is_some();

    Ok(match (has_v2, has_v1_pieces) {
        (true, true) => DetectedVersion::Hybrid,
        (true, false) => DetectedVersion::V2Only,
        _ => DetectedVersion::V1Only,
    })
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
        assert!(!meta.is_hybrid());
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
        info_map.insert(
            b"piece length".to_vec(),
            BencodeValue::Integer(16384),
        );

        let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

        let data = torrent_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap();
        let meta = torrent_from_bytes_any(&data).unwrap();
        assert!(meta.is_v2());
        assert!(!meta.is_v1());
        assert!(!meta.is_hybrid());
    }

    #[test]
    fn auto_detect_hybrid() {
        use std::collections::BTreeMap;

        // Build a hybrid torrent: has both `pieces` (v1) and `meta version = 2` + `file tree` (v2)
        let mut info_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();

        // v2 keys
        let mut ft_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        let mut attr_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        attr_map.insert(b"length".to_vec(), BencodeValue::Integer(16384));
        let mut file_node: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        file_node.insert(b"".to_vec(), BencodeValue::Dict(attr_map));
        ft_map.insert(b"test.dat".to_vec(), BencodeValue::Dict(file_node));

        info_map.insert(b"file tree".to_vec(), BencodeValue::Dict(ft_map));
        info_map.insert(b"meta version".to_vec(), BencodeValue::Integer(2));

        // v1 keys
        info_map.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
        info_map.insert(
            b"piece length".to_vec(),
            BencodeValue::Integer(16384),
        );
        info_map.insert(b"length".to_vec(), BencodeValue::Integer(16384));
        // 1 piece = 20 bytes of SHA-1 hash
        info_map.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0xAA; 20]));

        let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

        let data = torrent_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap();
        let meta = torrent_from_bytes_any(&data).unwrap();
        assert!(meta.is_hybrid());
        assert!(!meta.is_v1());
        assert!(!meta.is_v2());

        // Verify info_hashes has both v1 and v2
        let hashes = meta.info_hashes();
        assert!(hashes.has_v1());
        assert!(hashes.has_v2());
        assert!(hashes.is_hybrid());

        // Verify the v1 hash is the REAL SHA-1, not a truncation of SHA-256
        if let TorrentMeta::Hybrid(ref v1, ref v2) = meta {
            // v1 info hash is SHA-1 of raw info dict bytes
            assert_eq!(hashes.v1.unwrap(), v1.info_hash);
            // v2 info hash is SHA-256 of same bytes — different from v1
            assert!(hashes.v2.is_some());
            assert_ne!(
                &v1.info_hash.0[..],
                &v2.info_hashes.v2.unwrap().0[..20],
                "v1 hash should NOT be a truncation in hybrid — it's SHA-1 not SHA-256[:20]"
            );
        } else {
            panic!("expected Hybrid variant");
        }
    }

    #[test]
    fn hybrid_version_accessor() {
        let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi256e6:pieces20:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00ee";
        let meta = torrent_from_bytes_any(data).unwrap();
        assert_eq!(
            meta.version(),
            crate::torrent_version::TorrentVersion::V1Only
        );
    }

    #[test]
    fn enum_queries_work() {
        let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi256e6:pieces20:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00ee";
        let meta = torrent_from_bytes_any(data).unwrap();
        let hashes = meta.info_hashes();
        assert!(hashes.has_v1());
        assert!(!hashes.has_v2());
    }

    #[test]
    fn as_v1_accessor() {
        let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi256e6:pieces20:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00ee";
        let meta = torrent_from_bytes_any(data).unwrap();
        assert!(meta.as_v1().is_some());
        assert!(meta.as_v2().is_none());
    }

    #[test]
    fn torrent_meta_ssl_accessors() {
        // Non-SSL torrent
        let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi256e6:pieces20:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00ee";
        let meta = torrent_from_bytes_any(data).unwrap();
        assert!(!meta.is_ssl());
        assert!(meta.ssl_cert().is_none());
    }
}
