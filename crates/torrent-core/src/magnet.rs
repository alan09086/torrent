use url::Url;

use crate::error::Error;
use crate::file_selection::FileSelection;
use crate::hash::{Id20, Id32};
use crate::info_hashes::InfoHashes;

/// Parsed magnet link (BEP 9 + BEP 52).
///
/// Supports v1 (`urn:btih:`), v2 (`urn:btmh:`), and hybrid magnets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Magnet {
    /// Unified info hashes (v1 and/or v2).
    pub info_hashes: InfoHashes,
    /// Display name (optional).
    pub display_name: Option<String>,
    /// Tracker URLs.
    pub trackers: Vec<String>,
    /// Peer addresses (x.pe parameter).
    pub peers: Vec<String>,
    /// BEP 53 file selection (optional `so=` parameter).
    pub selected_files: Option<Vec<FileSelection>>,
}

impl Magnet {
    /// Get the best v1 info hash for backward compatibility.
    ///
    /// Returns the v1 hash directly, or truncates v2 if only v2 is available.
    pub fn info_hash(&self) -> Id20 {
        self.info_hashes.best_v1()
    }

    /// Whether this magnet contains a v2 hash.
    pub fn is_v2(&self) -> bool {
        self.info_hashes.has_v2()
    }

    /// Whether this magnet contains both v1 and v2 hashes.
    pub fn is_hybrid(&self) -> bool {
        self.info_hashes.is_hybrid()
    }

    /// Parse a magnet URI string.
    pub fn parse(uri: &str) -> Result<Self, Error> {
        // Magnet URIs use "magnet:?" which isn't a valid URL scheme for most parsers.
        // We replace "magnet:?" with "magnet://dummy?" to make it parseable.
        let normalized = if let Some(rest) = uri.strip_prefix("magnet:?") {
            format!("magnet://dummy?{rest}")
        } else {
            return Err(Error::InvalidMagnet("must start with 'magnet:?'".into()));
        };

        let url = Url::parse(&normalized)
            .map_err(|e| Error::InvalidMagnet(format!("URL parse error: {e}")))?;

        let mut v1_hash: Option<Id20> = None;
        let mut v2_hash: Option<Id32> = None;
        let mut display_name = None;
        let mut trackers = Vec::new();
        let mut peers = Vec::new();
        let mut selected_files = None;

        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "xt" => {
                    if let Some(hash_str) = value.strip_prefix("urn:btih:") {
                        v1_hash = Some(parse_v1_hash(hash_str)?);
                    } else if let Some(hash_str) = value.strip_prefix("urn:btmh:") {
                        v2_hash = Some(parse_v2_multihash(hash_str)?);
                    }
                }
                "dn" => {
                    display_name = Some(value.into_owned());
                }
                "tr" => {
                    trackers.push(value.into_owned());
                }
                "x.pe" => {
                    peers.push(value.into_owned());
                }
                "so" => {
                    if let Ok(sels) = FileSelection::parse(&value) {
                        selected_files = Some(sels);
                    }
                    // Ignore malformed so= values per BEP 53
                }
                _ => {} // Ignore unknown parameters
            }
        }

        if v1_hash.is_none() && v2_hash.is_none() {
            return Err(Error::InvalidMagnet(
                "missing xt=urn:btih: or xt=urn:btmh:".into(),
            ));
        }

        let info_hashes = InfoHashes {
            v1: v1_hash,
            v2: v2_hash,
        };

        Ok(Magnet {
            info_hashes,
            display_name,
            trackers,
            peers,
            selected_files,
        })
    }

    /// Convert back to a magnet URI string.
    pub fn to_uri(&self) -> String {
        let mut parts = Vec::new();

        // Emit v1 hash if present
        if let Some(v1) = self.info_hashes.v1 {
            parts.push(format!("magnet:?xt=urn:btih:{}", v1.to_hex()));
        }

        // Emit v2 hash if present
        if let Some(v2) = self.info_hashes.v2 {
            let prefix = if parts.is_empty() { "magnet:?" } else { "" };
            parts.push(format!(
                "{}xt=urn:btmh:{}",
                prefix,
                v2.to_multihash_hex()
            ));
        }

        if let Some(ref name) = self.display_name {
            parts.push(format!(
                "dn={}",
                url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>()
            ));
        }

        for tracker in &self.trackers {
            parts.push(format!(
                "tr={}",
                url::form_urlencoded::byte_serialize(tracker.as_bytes()).collect::<String>()
            ));
        }

        if let Some(ref sels) = self.selected_files {
            parts.push(format!("so={}", FileSelection::to_so_value(sels)));
        }

        parts.join("&")
    }
}

/// Parse a v1 info hash from hex (40 chars) or base32 (32 chars).
fn parse_v1_hash(s: &str) -> Result<Id20, Error> {
    match s.len() {
        40 => Id20::from_hex(s),
        32 => Id20::from_base32(&s.to_ascii_uppercase()),
        _ => Err(Error::InvalidMagnet(format!(
            "v1 info hash must be 40 hex or 32 base32 chars, got {} chars",
            s.len()
        ))),
    }
}

/// Parse a v2 multihash from hex (68 chars = "1220" prefix + 64 hex).
fn parse_v2_multihash(s: &str) -> Result<Id32, Error> {
    Id32::from_multihash_hex(s).map_err(|e| Error::InvalidMagnet(format!("v2 multihash: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_magnet() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test&tr=http://tracker.example.com/announce";
        let m = Magnet::parse(uri).unwrap();
        assert_eq!(
            m.info_hash(),
            Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap()
        );
        assert_eq!(m.display_name.as_deref(), Some("test"));
        assert_eq!(m.trackers, vec!["http://tracker.example.com/announce"]);
    }

    #[test]
    fn parse_base32_magnet() {
        let id = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let b32 = id.to_base32();
        let uri = format!("magnet:?xt=urn:btih:{b32}");
        let m = Magnet::parse(&uri).unwrap();
        assert_eq!(m.info_hash(), id);
    }

    #[test]
    fn parse_multiple_trackers() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&tr=http://a.com&tr=http://b.com";
        let m = Magnet::parse(uri).unwrap();
        assert_eq!(m.trackers.len(), 2);
    }

    #[test]
    fn round_trip_uri() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test%20file&tr=http%3A%2F%2Ftracker.example.com%2Fannounce";
        let m = Magnet::parse(uri).unwrap();
        let rebuilt = m.to_uri();
        let m2 = Magnet::parse(&rebuilt).unwrap();
        assert_eq!(m.info_hash(), m2.info_hash());
        assert_eq!(m.display_name, m2.display_name);
        assert_eq!(m.trackers, m2.trackers);
    }

    #[test]
    fn reject_invalid_magnet() {
        assert!(Magnet::parse("http://example.com").is_err());
        assert!(Magnet::parse("magnet:?dn=test").is_err()); // no xt
    }

    // v2-specific tests

    #[test]
    fn parse_v2_only_magnet() {
        let hash =
            Id32::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap();
        let mh = hash.to_multihash_hex();
        let uri = format!("magnet:?xt=urn:btmh:{mh}&dn=v2test");
        let m = Magnet::parse(&uri).unwrap();
        assert!(m.is_v2());
        assert!(!m.is_hybrid());
        assert_eq!(m.info_hashes.v2, Some(hash));
        assert_eq!(m.display_name.as_deref(), Some("v2test"));
    }

    #[test]
    fn parse_hybrid_magnet() {
        let v1 = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let v2 =
            Id32::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap();
        let uri = format!(
            "magnet:?xt=urn:btih:{}&xt=urn:btmh:{}",
            v1.to_hex(),
            v2.to_multihash_hex()
        );
        let m = Magnet::parse(&uri).unwrap();
        assert!(m.is_hybrid());
        assert_eq!(m.info_hashes.v1, Some(v1));
        assert_eq!(m.info_hashes.v2, Some(v2));
        // info_hash() backward compat returns v1
        assert_eq!(m.info_hash(), v1);
    }

    #[test]
    fn v2_round_trip() {
        let hash =
            Id32::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap();
        let mh = hash.to_multihash_hex();
        let uri = format!("magnet:?xt=urn:btmh:{mh}");
        let m = Magnet::parse(&uri).unwrap();
        let rebuilt = m.to_uri();
        let m2 = Magnet::parse(&rebuilt).unwrap();
        assert_eq!(m.info_hashes, m2.info_hashes);
    }

    #[test]
    fn hybrid_round_trip() {
        let v1 = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let v2 =
            Id32::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap();
        let uri = format!(
            "magnet:?xt=urn:btih:{}&xt=urn:btmh:{}",
            v1.to_hex(),
            v2.to_multihash_hex()
        );
        let m = Magnet::parse(&uri).unwrap();
        let rebuilt = m.to_uri();
        let m2 = Magnet::parse(&rebuilt).unwrap();
        assert_eq!(m.info_hashes, m2.info_hashes);
    }

    #[test]
    fn reject_no_hash() {
        assert!(Magnet::parse("magnet:?dn=test").is_err());
    }

    #[test]
    fn parse_so_parameter() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&so=0,2,4-6";
        let m = Magnet::parse(uri).unwrap();
        let sels = m.selected_files.unwrap();
        assert_eq!(
            sels,
            vec![
                crate::file_selection::FileSelection::Single(0),
                crate::file_selection::FileSelection::Single(2),
                crate::file_selection::FileSelection::Range(4, 6),
            ]
        );
    }

    #[test]
    fn so_parameter_round_trip() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&so=0,2,4-6";
        let m = Magnet::parse(uri).unwrap();
        let rebuilt = m.to_uri();
        let m2 = Magnet::parse(&rebuilt).unwrap();
        assert_eq!(m.selected_files, m2.selected_files);
    }

    #[test]
    fn no_so_parameter_is_none() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";
        let m = Magnet::parse(uri).unwrap();
        assert!(m.selected_files.is_none());
    }

    #[test]
    fn invalid_so_parameter_ignored() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&so=abc";
        let m = Magnet::parse(uri).unwrap();
        assert!(m.selected_files.is_none());
    }

    #[test]
    fn v1_only_backward_compat() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";
        let m = Magnet::parse(uri).unwrap();
        assert!(!m.is_v2());
        assert!(!m.is_hybrid());
        assert_eq!(
            m.info_hash(),
            Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap()
        );
    }
}
