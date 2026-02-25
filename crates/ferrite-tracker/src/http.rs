use serde::Deserialize;

use crate::compact::parse_compact_peers;
use crate::error::{Error, Result};
use crate::{AnnounceEvent, AnnounceRequest, AnnounceResponse};

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

        let peers = parse_compact_peers(&raw.peers)?;

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
}
