use bytes::Bytes;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::hash::Id20;

/// Wrapper for `url-list` that handles both a single string and a list of strings.
#[derive(Debug, Clone, Default)]
pub struct UrlList(pub Vec<String>);

impl<'de> Deserialize<'de> for UrlList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UrlListVisitor;

        impl<'de> de::Visitor<'de> for UrlListVisitor {
            type Value = UrlList;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string or list of strings")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<UrlList, E> {
                Ok(UrlList(vec![v.to_owned()]))
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<UrlList, E> {
                let s = std::str::from_utf8(v).map_err(de::Error::custom)?;
                Ok(UrlList(vec![s.to_owned()]))
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<UrlList, A::Error> {
                let mut urls = Vec::new();
                while let Some(url) = seq.next_element::<String>()? {
                    urls.push(url);
                }
                Ok(UrlList(urls))
            }
        }

        deserializer.deserialize_any(UrlListVisitor)
    }
}

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
    /// BEP 19 web seed URLs (GetRight-style).
    pub url_list: Vec<String>,
    /// BEP 17 HTTP seed URLs (Hoffman-style).
    pub httpseeds: Vec<String>,
    /// Raw info dict bytes for BEP 9 metadata serving.
    pub info_bytes: Option<Bytes>,
    /// PEM-encoded SSL CA certificate from the info dict, if present.
    pub ssl_cert: Option<Vec<u8>>,
}

/// The "info" dictionary from a .torrent file.
#[derive(Debug, Clone, Deserialize, Serialize)]
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
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub length: Option<u64>,
    /// Files (multi-file mode).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub files: Option<Vec<FileEntry>>,
    /// Private flag.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub private: Option<i64>,
    /// Source tag (private tracker identification).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
    /// BEP 35 / SSL torrent: PEM-encoded X.509 CA certificate.
    /// When present, all peer connections must use TLS with certs chaining to this CA.
    #[serde(rename = "ssl-cert", skip_serializing_if = "Option::is_none", default)]
    #[serde(with = "serde_bytes")]
    pub ssl_cert: Option<Vec<u8>>,
}

/// A file entry in multi-file mode.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileEntry {
    /// File length in bytes.
    pub length: u64,
    /// Path components (e.g., ["dir", "file.txt"]).
    pub path: Vec<String>,
    /// BEP 47 file attributes ("p"=pad, "h"=hidden, "x"=executable, "l"=symlink).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub attr: Option<String>,
    /// File modification time (unix timestamp).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mtime: Option<i64>,
    /// Symlink target path components.
    #[serde(rename = "symlink path", skip_serializing_if = "Option::is_none", default)]
    pub symlink_path: Option<Vec<String>>,
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
    /// BEP 19: web seed URL(s) — single string or list.
    #[serde(rename = "url-list", default)]
    url_list: UrlList,
    /// BEP 17: HTTP seed URLs.
    #[serde(default)]
    httpseeds: Vec<String>,
}

/// Parse a .torrent file from raw bytes.
///
/// Computes the info-hash by finding the raw byte span of the "info" key
/// and SHA1-hashing it directly (not the re-serialized form).
pub fn torrent_from_bytes(data: &[u8]) -> Result<TorrentMetaV1, Error> {
    // Step 1: Find the raw info dict span for hashing
    let info_span = torrent_bencode::find_dict_key_span(data, "info")?;
    let info_hash = crate::sha1(&data[info_span.clone()]);
    let info_raw = Bytes::copy_from_slice(&data[info_span]);

    // Step 2: Deserialize the full structure
    let raw: RawTorrent = torrent_bencode::from_bytes(data)?;

    // Step 3: Validate the info dict
    validate_info(&raw.info)?;

    let ssl_cert = raw.info.ssl_cert.clone();

    Ok(TorrentMetaV1 {
        info_hash,
        announce: raw.announce,
        announce_list: raw.announce_list,
        comment: raw.comment,
        created_by: raw.created_by,
        creation_date: raw.creation_date,
        info: raw.info,
        url_list: raw.url_list.0,
        httpseeds: raw.httpseeds,
        info_bytes: Some(info_raw),
        ssl_cert,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal torrent bencoded dict with extra keys sorted correctly.
    ///
    /// `before_info` contains keys that sort before "info" (e.g., "httpseeds").
    /// `after_info` contains keys that sort after "info" (e.g., "url-list").
    fn make_torrent_bytes_sorted(before_info: &[u8], after_info: &[u8]) -> Vec<u8> {
        // Minimal info dict: name, piece length, pieces (20 zero bytes), length
        let info = b"d6:lengthi1048576e4:name4:test12:piece lengthi262144e6:pieces20:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00e";
        let mut buf = Vec::new();
        buf.push(b'd');
        buf.extend_from_slice(before_info);
        buf.extend_from_slice(b"4:info");
        buf.extend_from_slice(info);
        buf.extend_from_slice(after_info);
        buf.push(b'e');
        buf
    }

    #[test]
    fn url_list_single_string() {
        // url-list sorts after info
        let data = make_torrent_bytes_sorted(
            b"",
            b"8:url-list24:http://example.com/files",
        );
        let meta = torrent_from_bytes(&data).unwrap();
        assert_eq!(meta.url_list, vec!["http://example.com/files"]);
    }

    #[test]
    fn url_list_multiple() {
        let data = make_torrent_bytes_sorted(
            b"",
            b"8:url-listl24:http://example.com/files26:http://mirror.example.com/e",
        );
        let meta = torrent_from_bytes(&data).unwrap();
        assert_eq!(meta.url_list.len(), 2);
        assert_eq!(meta.url_list[0], "http://example.com/files");
        assert_eq!(meta.url_list[1], "http://mirror.example.com/");
    }

    #[test]
    fn url_list_absent() {
        let data = make_torrent_bytes_sorted(b"", b"");
        let meta = torrent_from_bytes(&data).unwrap();
        assert!(meta.url_list.is_empty());
    }

    #[test]
    fn httpseeds_present() {
        // httpseeds sorts before info
        let data = make_torrent_bytes_sorted(
            b"9:httpseedsl28:http://seed.example.com/seede",
            b"",
        );
        let meta = torrent_from_bytes(&data).unwrap();
        assert_eq!(meta.httpseeds, vec!["http://seed.example.com/seed"]);
    }

    #[test]
    fn httpseeds_absent() {
        let data = make_torrent_bytes_sorted(b"", b"");
        let meta = torrent_from_bytes(&data).unwrap();
        assert!(meta.httpseeds.is_empty());
    }

    #[test]
    fn torrent_from_bytes_stores_raw_info_bytes() {
        let data = make_torrent_bytes_sorted(b"", b"");
        let meta = torrent_from_bytes(&data).unwrap();
        assert!(meta.info_bytes.is_some());
        let info_bytes = meta.info_bytes.unwrap();
        // Re-hashing the stored bytes should produce the same info hash
        let rehash = crate::sha1(&info_bytes);
        assert_eq!(rehash, meta.info_hash);
    }

    #[test]
    fn ssl_cert_parsed_from_info_dict() {
        // Build a torrent with ssl-cert in the info dict.
        let cert_pem = b"-----BEGIN CERTIFICATE-----\nMIIBtest\n-----END CERTIFICATE-----\n";
        let cert_len = cert_pem.len();

        // Minimal info dict with ssl-cert inserted (keys must be sorted)
        let mut info = Vec::new();
        info.extend_from_slice(b"d");
        info.extend_from_slice(b"6:lengthi1048576e");
        info.extend_from_slice(b"4:name4:test");
        info.extend_from_slice(b"12:piece lengthi262144e");
        info.extend_from_slice(b"6:pieces20:");
        info.extend_from_slice(&[0u8; 20]);
        info.extend_from_slice(format!("8:ssl-cert{}:", cert_len).as_bytes());
        info.extend_from_slice(cert_pem);
        info.extend_from_slice(b"e");

        let mut torrent = Vec::new();
        torrent.extend_from_slice(b"d4:info");
        torrent.extend_from_slice(&info);
        torrent.extend_from_slice(b"e");

        let meta = torrent_from_bytes(&torrent).unwrap();
        assert!(meta.ssl_cert.is_some());
        assert_eq!(meta.ssl_cert.as_deref().unwrap(), cert_pem);
        assert_eq!(meta.info.ssl_cert.as_deref().unwrap(), cert_pem);
    }

    #[test]
    fn ssl_cert_absent_by_default() {
        let data = make_torrent_bytes_sorted(b"", b"");
        let meta = torrent_from_bytes(&data).unwrap();
        assert!(meta.ssl_cert.is_none());
        assert!(meta.info.ssl_cert.is_none());
    }
}
