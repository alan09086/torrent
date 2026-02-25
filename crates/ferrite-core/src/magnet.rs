use url::Url;

use crate::error::Error;
use crate::hash::Id20;

/// Parsed magnet link (BEP 9).
///
/// Format: `magnet:?xt=urn:btih:<info-hash>&dn=<name>&tr=<tracker>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Magnet {
    /// The 20-byte info hash.
    pub info_hash: Id20,
    /// Display name (optional).
    pub display_name: Option<String>,
    /// Tracker URLs.
    pub trackers: Vec<String>,
    /// Peer addresses (x.pe parameter).
    pub peers: Vec<String>,
}

impl Magnet {
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

        let mut info_hash = None;
        let mut display_name = None;
        let mut trackers = Vec::new();
        let mut peers = Vec::new();

        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "xt" => {
                    if let Some(hash_str) = value.strip_prefix("urn:btih:") {
                        info_hash = Some(parse_info_hash(hash_str)?);
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
                _ => {} // Ignore unknown parameters
            }
        }

        let info_hash =
            info_hash.ok_or_else(|| Error::InvalidMagnet("missing xt=urn:btih:".into()))?;

        Ok(Magnet {
            info_hash,
            display_name,
            trackers,
            peers,
        })
    }

    /// Convert back to a magnet URI string.
    pub fn to_uri(&self) -> String {
        let mut parts = vec![format!("magnet:?xt=urn:btih:{}", self.info_hash.to_hex())];

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

        parts.join("&")
    }
}

/// Parse an info hash from hex (40 chars) or base32 (32 chars).
fn parse_info_hash(s: &str) -> Result<Id20, Error> {
    match s.len() {
        40 => Id20::from_hex(s),
        32 => Id20::from_base32(&s.to_ascii_uppercase()),
        _ => Err(Error::InvalidMagnet(format!(
            "info hash must be 40 hex or 32 base32 chars, got {} chars",
            s.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_magnet() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test&tr=http://tracker.example.com/announce";
        let m = Magnet::parse(uri).unwrap();
        assert_eq!(
            m.info_hash,
            Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap()
        );
        assert_eq!(m.display_name.as_deref(), Some("test"));
        assert_eq!(m.trackers, vec!["http://tracker.example.com/announce"]);
    }

    #[test]
    fn parse_base32_magnet() {
        // Same hash as above but base32 encoded
        let id = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let b32 = id.to_base32();
        let uri = format!("magnet:?xt=urn:btih:{b32}");
        let m = Magnet::parse(&uri).unwrap();
        assert_eq!(m.info_hash, id);
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
        assert_eq!(m.info_hash, m2.info_hash);
        assert_eq!(m.display_name, m2.display_name);
        assert_eq!(m.trackers, m2.trackers);
    }

    #[test]
    fn reject_invalid_magnet() {
        assert!(Magnet::parse("http://example.com").is_err());
        assert!(Magnet::parse("magnet:?dn=test").is_err()); // no xt
    }
}
