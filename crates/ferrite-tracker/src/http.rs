use std::collections::HashMap;

use serde::Deserialize;

use ferrite_core::Id20;

use crate::compact::{parse_compact_peers, parse_compact_peers6};
use crate::error::{Error, Result};
use crate::{AnnounceEvent, AnnounceRequest, AnnounceResponse, ScrapeInfo};

/// HTTP tracker client (BEP 3).
pub struct HttpTracker {
    client: reqwest::Client,
}

/// Raw HTTP announce response (bencode).
#[derive(Debug, Clone)]
pub struct HttpAnnounceResponse {
    pub response: AnnounceResponse,
    /// Tracker ID (some trackers return this for subsequent requests).
    pub tracker_id: Option<String>,
    /// Warning message from tracker.
    pub warning: Option<String>,
}

/// Raw bencode response from HTTP tracker.
#[derive(Deserialize)]
struct RawHttpResponse {
    interval: u32,
    #[serde(default)]
    complete: Option<u32>,
    #[serde(default)]
    incomplete: Option<u32>,
    #[serde(with = "serde_bytes")]
    peers: Vec<u8>,
    /// Compact IPv6 peers (BEP 7): 18 bytes each (16 IP + 2 port).
    #[serde(with = "serde_bytes", default)]
    peers6: Vec<u8>,
    #[serde(default, rename = "failure reason")]
    failure_reason: Option<String>,
    #[serde(default, rename = "warning message")]
    warning_message: Option<String>,
    #[serde(default, rename = "tracker id")]
    tracker_id: Option<String>,
}

impl HttpTracker {
    pub fn new() -> Self {
        HttpTracker {
            client: reqwest::Client::builder()
                .user_agent("Ferrite/0.1.0")
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    /// Create an HTTP tracker client with an optional proxy.
    ///
    /// When `proxy_url` is provided (e.g. `"socks5://host:port"`), all
    /// HTTP requests are routed through it.
    pub fn with_proxy(proxy_url: Option<&str>) -> Self {
        let mut builder = reqwest::Client::builder()
            .user_agent("Ferrite/0.1.0");
        if let Some(url) = proxy_url
            && let Ok(proxy) = reqwest::Proxy::all(url)
        {
            builder = builder.proxy(proxy);
        }
        HttpTracker {
            client: builder.build().expect("failed to build HTTP client"),
        }
    }

    /// Build the announce URL with query parameters.
    pub fn build_announce_url(
        base_url: &str,
        req: &AnnounceRequest,
    ) -> Result<String> {
        let mut url = base_url.to_string();

        // URL-encode the info_hash and peer_id as raw bytes
        let info_hash_encoded = url_encode_bytes(req.info_hash.as_bytes());
        let peer_id_encoded = url_encode_bytes(req.peer_id.as_bytes());

        let separator = if url.contains('?') { '&' } else { '?' };

        url.push(separator);
        url.push_str(&format!(
            "info_hash={info_hash_encoded}&peer_id={peer_id_encoded}&port={}&uploaded={}&downloaded={}&left={}&compact=1",
            req.port, req.uploaded, req.downloaded, req.left
        ));

        match req.event {
            AnnounceEvent::None => {}
            AnnounceEvent::Started => url.push_str("&event=started"),
            AnnounceEvent::Completed => url.push_str("&event=completed"),
            AnnounceEvent::Stopped => url.push_str("&event=stopped"),
        }

        if let Some(n) = req.num_want {
            url.push_str(&format!("&numwant={n}"));
        }

        Ok(url)
    }

    /// Send an announce request to an HTTP tracker.
    pub async fn announce(
        &self,
        base_url: &str,
        req: &AnnounceRequest,
    ) -> Result<HttpAnnounceResponse> {
        let url = Self::build_announce_url(base_url, req)?;

        let response = self
            .client
            .get(&url)
            .send()
            .await?
            .bytes()
            .await?;

        let raw: RawHttpResponse = ferrite_bencode::from_bytes(&response)?;

        if let Some(failure) = raw.failure_reason {
            return Err(Error::TrackerError(failure));
        }

        let mut peers = parse_compact_peers(&raw.peers)?;

        // Merge IPv6 peers from peers6 key (BEP 7)
        if let Ok(peers6) = parse_compact_peers6(&raw.peers6) {
            peers.extend(peers6);
        }

        Ok(HttpAnnounceResponse {
            response: AnnounceResponse {
                interval: raw.interval,
                seeders: raw.complete,
                leechers: raw.incomplete,
                peers,
            },
            tracker_id: raw.tracker_id,
            warning: raw.warning_message,
        })
    }
}

/// HTTP scrape response containing per-torrent stats.
#[derive(Debug, Clone)]
pub struct HttpScrapeResponse {
    pub files: HashMap<Id20, ScrapeInfo>,
}

impl HttpTracker {
    /// Build a scrape URL from an announce URL and a list of info_hashes.
    pub fn build_scrape_url(announce_url: &str, info_hashes: &[Id20]) -> Result<String> {
        let base = crate::announce_url_to_scrape(announce_url)
            .ok_or_else(|| Error::InvalidUrl("no 'announce' in URL to convert to scrape".into()))?;
        let mut url = base;
        for (i, hash) in info_hashes.iter().enumerate() {
            let encoded = url_encode_bytes(hash.as_bytes());
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str("info_hash=");
            url.push_str(&encoded);
        }
        Ok(url)
    }

    /// Send a scrape request to an HTTP tracker.
    pub async fn scrape(
        &self,
        announce_url: &str,
        info_hashes: &[Id20],
    ) -> Result<HttpScrapeResponse> {
        let url = Self::build_scrape_url(announce_url, info_hashes)?;

        let response = self
            .client
            .get(&url)
            .send()
            .await?
            .bytes()
            .await?;

        // Parse using BencodeValue since keys are raw 20-byte hashes
        let value: ferrite_bencode::BencodeValue = ferrite_bencode::from_bytes(&response)?;
        let root = value.as_dict()
            .ok_or_else(|| Error::InvalidResponse("scrape response is not a dict".into()))?;

        let files_val = root.get(b"files".as_slice())
            .and_then(|v| v.as_dict())
            .ok_or_else(|| Error::InvalidResponse("scrape response missing 'files' dict".into()))?;

        let mut files = HashMap::new();
        for (key, val) in files_val {
            if key.len() != 20 {
                continue;
            }
            let hash = Id20::from_bytes(key)
                .map_err(|_| Error::InvalidResponse("invalid info_hash in scrape response".into()))?;
            let entry = val.as_dict()
                .ok_or_else(|| Error::InvalidResponse("scrape file entry is not a dict".into()))?;

            let complete = entry.get(b"complete".as_slice())
                .and_then(|v| v.as_int())
                .unwrap_or(0) as u32;
            let incomplete = entry.get(b"incomplete".as_slice())
                .and_then(|v| v.as_int())
                .unwrap_or(0) as u32;
            let downloaded = entry.get(b"downloaded".as_slice())
                .and_then(|v| v.as_int())
                .unwrap_or(0) as u32;

            files.insert(hash, ScrapeInfo { complete, incomplete, downloaded });
        }

        Ok(HttpScrapeResponse { files })
    }
}

impl Default for HttpTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// URL-encode raw bytes (percent-encoding).
fn url_encode_bytes(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        match b {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'.' | b'-' | b'_' | b'~' => {
                encoded.push(b as char);
            }
            _ => {
                encoded.push_str(&format!("%{b:02X}"));
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_core::Id20;

    #[test]
    fn build_announce_url_basic() {
        let req = AnnounceRequest {
            info_hash: Id20::ZERO,
            peer_id: Id20::ZERO,
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1000,
            event: AnnounceEvent::Started,
            num_want: Some(50),
            compact: true,
        };

        let url = HttpTracker::build_announce_url(
            "http://tracker.example.com/announce",
            &req,
        )
        .unwrap();

        assert!(url.starts_with("http://tracker.example.com/announce?"));
        assert!(url.contains("info_hash="));
        assert!(url.contains("port=6881"));
        assert!(url.contains("event=started"));
        assert!(url.contains("numwant=50"));
        assert!(url.contains("compact=1"));
    }

    #[test]
    fn build_scrape_url_basic() {
        let hash = Id20::ZERO;
        let url = HttpTracker::build_scrape_url(
            "http://tracker.example.com/announce",
            &[hash],
        ).unwrap();
        assert!(url.starts_with("http://tracker.example.com/scrape?info_hash="));
    }

    #[test]
    fn build_scrape_url_no_announce_in_url() {
        let hash = Id20::ZERO;
        let result = HttpTracker::build_scrape_url(
            "http://tracker.example.com/track",
            &[hash],
        );
        assert!(result.is_err());
    }

    #[test]
    fn url_encode_bytes_simple() {
        assert_eq!(url_encode_bytes(b"abc"), "abc");
        assert_eq!(url_encode_bytes(&[0xFF, 0x00]), "%FF%00");
    }

    #[test]
    fn url_encode_preserves_unreserved() {
        let unreserved = b"abcXYZ019.-_~";
        let encoded = url_encode_bytes(unreserved);
        assert_eq!(encoded, "abcXYZ019.-_~");
    }

    #[test]
    fn parse_response_with_peers6() {
        use std::net::Ipv6Addr;

        // Build a raw bencode response with both peers and peers6
        let mut peers = Vec::new();
        peers.extend_from_slice(&[192, 168, 1, 1, 0x1A, 0xE1]); // 192.168.1.1:6881

        let ip6: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let mut peers6 = Vec::new();
        peers6.extend_from_slice(&ip6.octets());
        peers6.extend_from_slice(&8080u16.to_be_bytes());

        let raw = RawHttpResponse {
            interval: 1800,
            complete: Some(10),
            incomplete: Some(5),
            peers,
            peers6,
            failure_reason: None,
            warning_message: None,
            tracker_id: None,
        };

        // Manually parse as the announce method would
        let mut result = parse_compact_peers(&raw.peers).unwrap();
        if !raw.peers6.is_empty() {
            if let Ok(v6) = parse_compact_peers6(&raw.peers6) {
                result.extend(v6);
            }
        }

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].to_string(), "192.168.1.1:6881");
        assert_eq!(
            result[1],
            "[2001:db8::1]:8080".parse::<std::net::SocketAddr>().unwrap()
        );
    }
}
