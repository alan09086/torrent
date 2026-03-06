//! HTTP/web seeding (BEP 19 GetRight-style, BEP 17 Hoffman-style).
//!
//! Each `WebSeedTask` manages a single HTTP URL, receiving piece download commands
//! and sending piece data or errors back to the `TorrentActor` via `PeerEvent`.

use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use torrent_core::{Id20, Lengths};
use torrent_storage::file_map::FileMap;

use crate::types::PeerEvent;

// ── Types ────────────────────────────────────────────────────────────

/// A single HTTP GET request needed to fetch (part of) a piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebSeedRequest {
    /// Full URL to fetch.
    pub url: String,
    /// Start of the Range header (byte offset within the file).
    pub range_start: u64,
    /// End of the Range header (inclusive).
    pub range_end: u64,
    /// Offset within the piece buffer where this data goes.
    pub piece_offset: u32,
    /// Expected number of bytes.
    pub length: u32,
}

/// Commands sent from the `TorrentActor` to a `WebSeedTask`.
#[derive(Debug)]
pub(crate) enum WebSeedCommand {
    FetchPiece(u32),
    Shutdown,
}

/// Errors specific to web seed downloads.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WebSeedError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("HTTP status {0}")]
    HttpStatus(u16),
    #[error("length mismatch: expected {expected}, got {got}")]
    LengthMismatch { expected: u32, got: u32 },
    #[error("retry after {0} seconds")]
    RetryAfter(u64),
}

/// Whether this web seed speaks BEP 19 (GetRight) or BEP 17 (Hoffman).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebSeedMode {
    /// BEP 19: standard HTTP server with static files.
    GetRight,
    /// BEP 17: piece-aware server script.
    Hoffman,
}

// ── URL Builder ──────────────────────────────────────────────────────

/// Constructs HTTP requests for downloading a piece from a web seed.
#[derive(Debug, Clone)]
pub(crate) struct WebSeedUrlBuilder {
    /// Base URL (from `url-list` or `httpseeds`).
    base_url: String,
    /// For single-file: the file name. For multi-file: not used (file_paths used instead).
    single_file: bool,
    /// Torrent name (directory name for multi-file).
    torrent_name: String,
    /// Pre-computed relative file paths (for multi-file BEP 19).
    file_paths: Vec<String>,
}

impl WebSeedUrlBuilder {
    /// Create a builder for a single-file torrent.
    pub fn single(base_url: String, file_name: String) -> Self {
        Self {
            base_url,
            single_file: true,
            torrent_name: file_name,
            file_paths: Vec::new(),
        }
    }

    /// Create a builder for a multi-file torrent.
    pub fn multi(base_url: String, torrent_name: String, file_paths: Vec<String>) -> Self {
        Self {
            base_url,
            single_file: false,
            torrent_name,
            file_paths,
        }
    }

    /// Generate HTTP requests needed to download a piece via BEP 19.
    pub fn requests_for_piece(
        &self,
        piece: u32,
        lengths: &Lengths,
        file_map: &FileMap,
    ) -> Vec<WebSeedRequest> {
        if self.single_file {
            self.requests_single_file(piece, lengths)
        } else {
            self.requests_multi_file(piece, lengths, file_map)
        }
    }

    fn requests_single_file(&self, piece: u32, lengths: &Lengths) -> Vec<WebSeedRequest> {
        let offset = lengths.piece_offset(piece);
        let size = lengths.piece_size(piece);
        if size == 0 {
            return Vec::new();
        }

        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/{}", self.torrent_name);

        vec![WebSeedRequest {
            url,
            range_start: offset,
            range_end: offset + size as u64 - 1,
            piece_offset: 0,
            length: size,
        }]
    }

    fn requests_multi_file(
        &self,
        piece: u32,
        _lengths: &Lengths,
        file_map: &FileMap,
    ) -> Vec<WebSeedRequest> {
        let segments = file_map.piece_segments(piece);
        let base = self.base_url.trim_end_matches('/');
        let mut piece_offset = 0u32;
        let mut requests = Vec::with_capacity(segments.len());

        for seg in &segments {
            if seg.file_index >= self.file_paths.len() {
                continue;
            }
            let file_path = &self.file_paths[seg.file_index];
            let url = format!("{base}/{}/{file_path}", self.torrent_name);
            requests.push(WebSeedRequest {
                url,
                range_start: seg.file_offset,
                range_end: seg.file_offset + seg.len as u64 - 1,
                piece_offset,
                length: seg.len,
            });
            piece_offset += seg.len;
        }

        requests
    }
}

// ── WebSeedTask ──────────────────────────────────────────────────────

/// A background task that downloads pieces from a single HTTP web seed.
pub(crate) struct WebSeedTask {
    url: String,
    mode: WebSeedMode,
    url_builder: WebSeedUrlBuilder,
    lengths: Lengths,
    file_map: FileMap,
    info_hash: Id20,
    http_client: reqwest::Client,
    cmd_rx: mpsc::Receiver<WebSeedCommand>,
    event_tx: mpsc::Sender<PeerEvent>,
}

impl WebSeedTask {
    /// Create a new web seed task.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        url: String,
        mode: WebSeedMode,
        url_builder: WebSeedUrlBuilder,
        lengths: Lengths,
        file_map: FileMap,
        info_hash: Id20,
        cmd_rx: mpsc::Receiver<WebSeedCommand>,
        event_tx: mpsc::Sender<PeerEvent>,
        security: &crate::url_guard::UrlSecurityConfig,
    ) -> Self {
        let http_client = crate::url_guard::build_http_client(security, None, "Torrent/0.60.0");

        Self {
            url,
            mode,
            url_builder,
            lengths,
            file_map,
            info_hash,
            http_client,
            cmd_rx,
            event_tx,
        }
    }

    /// Run the task loop: receive commands, download pieces, send results.
    pub async fn run(mut self) {
        debug!(url = %self.url, "web seed task started");
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                WebSeedCommand::FetchPiece(piece) => {
                    let result =
                        tokio::time::timeout(Duration::from_secs(60), self.download_piece(piece))
                            .await;

                    match result {
                        Ok(Ok(data)) => {
                            let _ = self
                                .event_tx
                                .send(PeerEvent::WebSeedPieceData {
                                    url: self.url.clone(),
                                    index: piece,
                                    data: Bytes::from(data),
                                })
                                .await;
                        }
                        Ok(Err(e)) => {
                            warn!(url = %self.url, piece, error = %e, "web seed piece download failed");
                            let _ = self
                                .event_tx
                                .send(PeerEvent::WebSeedError {
                                    url: self.url.clone(),
                                    piece,
                                    message: e.to_string(),
                                })
                                .await;
                        }
                        Err(_) => {
                            warn!(url = %self.url, piece, "web seed piece download timed out");
                            let _ = self
                                .event_tx
                                .send(PeerEvent::WebSeedError {
                                    url: self.url.clone(),
                                    piece,
                                    message: "timeout".into(),
                                })
                                .await;
                        }
                    }
                }
                WebSeedCommand::Shutdown => {
                    debug!(url = %self.url, "web seed task shutting down");
                    return;
                }
            }
        }
    }

    async fn download_piece(&self, piece: u32) -> Result<Vec<u8>, WebSeedError> {
        match self.mode {
            WebSeedMode::GetRight => self.download_piece_bep19(piece).await,
            WebSeedMode::Hoffman => self.download_piece_bep17(piece).await,
        }
    }

    /// BEP 19: download piece via standard HTTP Range requests.
    async fn download_piece_bep19(&self, piece: u32) -> Result<Vec<u8>, WebSeedError> {
        let piece_size = self.lengths.piece_size(piece) as usize;
        let requests = self
            .url_builder
            .requests_for_piece(piece, &self.lengths, &self.file_map);

        let mut buf = vec![0u8; piece_size];

        for req in &requests {
            let range = format!("bytes={}-{}", req.range_start, req.range_end);
            let response = self
                .http_client
                .get(&req.url)
                .header("Range", &range)
                .send()
                .await
                .map_err(|e| WebSeedError::Http(e.to_string()))?;

            let status = response.status().as_u16();
            if status != 200 && status != 206 {
                return Err(WebSeedError::HttpStatus(status));
            }

            let body = response
                .bytes()
                .await
                .map_err(|e| WebSeedError::Http(e.to_string()))?;

            if body.len() != req.length as usize {
                return Err(WebSeedError::LengthMismatch {
                    expected: req.length,
                    got: body.len() as u32,
                });
            }

            let start = req.piece_offset as usize;
            let end = start + req.length as usize;
            buf[start..end].copy_from_slice(&body);
        }

        Ok(buf)
    }

    /// BEP 17: download piece via parameterized URL.
    async fn download_piece_bep17(&self, piece: u32) -> Result<Vec<u8>, WebSeedError> {
        let piece_size = self.lengths.piece_size(piece);
        let encoded_hash = url_encode_info_hash(&self.info_hash);
        let url = format!(
            "{}?info_hash={}&piece={}&ranges=0-{}",
            self.url,
            encoded_hash,
            piece,
            piece_size.saturating_sub(1),
        );

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|e| WebSeedError::Http(e.to_string()))?;

        let status = response.status().as_u16();

        if status == 503 {
            // BEP 17: body contains retry-after seconds as ASCII integer.
            let body = response
                .bytes()
                .await
                .map_err(|e| WebSeedError::Http(e.to_string()))?;
            let secs = std::str::from_utf8(&body)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(60);
            return Err(WebSeedError::RetryAfter(secs));
        }

        if status != 200 {
            return Err(WebSeedError::HttpStatus(status));
        }

        let body = response
            .bytes()
            .await
            .map_err(|e| WebSeedError::Http(e.to_string()))?;

        if body.len() != piece_size as usize {
            return Err(WebSeedError::LengthMismatch {
                expected: piece_size,
                got: body.len() as u32,
            });
        }

        Ok(body.to_vec())
    }
}

/// Percent-encode a 20-byte info hash for BEP 17 URL parameters.
pub(crate) fn url_encode_info_hash(hash: &Id20) -> String {
    let bytes = hash.as_bytes();
    let mut encoded = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        encoded.push('%');
        encoded.push_str(&format!("{b:02X}"));
    }
    encoded
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::Lengths;
    use torrent_storage::file_map::FileMap;

    #[test]
    fn url_builder_single_file_piece() {
        let builder =
            WebSeedUrlBuilder::single("http://example.com/files".into(), "movie.mkv".into());
        let lengths = Lengths::new(1048576, 262144, 16384);
        let fm = FileMap::new(vec![1048576], lengths.clone());

        let reqs = builder.requests_for_piece(0, &lengths, &fm);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].url, "http://example.com/files/movie.mkv");
        assert_eq!(reqs[0].range_start, 0);
        assert_eq!(reqs[0].range_end, 262143);
        assert_eq!(reqs[0].piece_offset, 0);
        assert_eq!(reqs[0].length, 262144);
    }

    #[test]
    fn url_builder_single_file_last_piece() {
        let builder =
            WebSeedUrlBuilder::single("http://example.com/files".into(), "movie.mkv".into());
        // 500000 total, 262144 pieces → piece 0 = 262144, piece 1 = 237856
        let lengths = Lengths::new(500000, 262144, 16384);
        let fm = FileMap::new(vec![500000], lengths.clone());

        let reqs = builder.requests_for_piece(1, &lengths, &fm);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].range_start, 262144);
        assert_eq!(reqs[0].length, 237856);
        assert_eq!(reqs[0].range_end, 499999);
    }

    #[test]
    fn url_builder_multi_file_no_span() {
        // Two files of 262144 each → each piece is within one file
        let lengths = Lengths::new(524288, 262144, 16384);
        let fm = FileMap::new(vec![262144, 262144], lengths.clone());
        let builder = WebSeedUrlBuilder::multi(
            "http://example.com".into(),
            "torrent_dir".into(),
            vec!["file1.txt".into(), "file2.txt".into()],
        );

        let reqs = builder.requests_for_piece(0, &lengths, &fm);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].url, "http://example.com/torrent_dir/file1.txt");
        assert_eq!(reqs[0].range_start, 0);
        assert_eq!(reqs[0].range_end, 262143);

        let reqs = builder.requests_for_piece(1, &lengths, &fm);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].url, "http://example.com/torrent_dir/file2.txt");
        assert_eq!(reqs[0].range_start, 0);
    }

    #[test]
    fn url_builder_multi_file_span_two() {
        // Two files: 100, 200. Single piece of 300 spans both.
        let lengths = Lengths::new(300, 300, 16384);
        let fm = FileMap::new(vec![100, 200], lengths.clone());
        let builder = WebSeedUrlBuilder::multi(
            "http://example.com".into(),
            "dir".into(),
            vec!["a.bin".into(), "b.bin".into()],
        );

        let reqs = builder.requests_for_piece(0, &lengths, &fm);
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].url, "http://example.com/dir/a.bin");
        assert_eq!(reqs[0].range_start, 0);
        assert_eq!(reqs[0].range_end, 99);
        assert_eq!(reqs[0].piece_offset, 0);
        assert_eq!(reqs[0].length, 100);

        assert_eq!(reqs[1].url, "http://example.com/dir/b.bin");
        assert_eq!(reqs[1].range_start, 0);
        assert_eq!(reqs[1].range_end, 199);
        assert_eq!(reqs[1].piece_offset, 100);
        assert_eq!(reqs[1].length, 200);
    }

    #[test]
    fn url_builder_trailing_slash() {
        let builder =
            WebSeedUrlBuilder::single("http://example.com/files/".into(), "test.bin".into());
        let lengths = Lengths::new(100, 100, 16384);
        let fm = FileMap::new(vec![100], lengths.clone());

        let reqs = builder.requests_for_piece(0, &lengths, &fm);
        assert_eq!(reqs[0].url, "http://example.com/files/test.bin");
    }

    #[test]
    fn url_encode_info_hash_format() {
        let hash = Id20::from([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x00, 0xFF, 0x01, 0x23, 0x45, 0x67,
            0x89, 0xAB, 0xCD, 0xEF, 0x00, 0xFF,
        ]);
        let encoded = url_encode_info_hash(&hash);
        assert_eq!(
            encoded,
            "%01%23%45%67%89%AB%CD%EF%00%FF%01%23%45%67%89%AB%CD%EF%00%FF"
        );
    }

    #[test]
    fn bep17_url_construction() {
        // Verify the URL format for BEP 17.
        let hash = Id20::from([0xAA; 20]);
        let encoded = url_encode_info_hash(&hash);
        let base = "http://seed.example.com/seed";
        let piece = 5u32;
        let piece_size = 262143u32; // piece_size - 1

        let url = format!("{base}?info_hash={encoded}&piece={piece}&ranges=0-{piece_size}");
        assert!(url.starts_with("http://seed.example.com/seed?info_hash=%AA"));
        assert!(url.contains("&piece=5&"));
        assert!(url.ends_with("&ranges=0-262143"));
    }

    #[test]
    fn web_seed_url_validation_global_passes() {
        let cfg = crate::url_guard::UrlSecurityConfig {
            ssrf_mitigation: true,
            allow_idna: false,
            validate_https_trackers: true,
        };
        assert!(
            crate::url_guard::validate_web_seed_url("http://cdn.example.com/files/torrent/", &cfg,)
                .is_ok()
        );
    }

    #[test]
    fn web_seed_url_validation_local_query_rejected() {
        let cfg = crate::url_guard::UrlSecurityConfig {
            ssrf_mitigation: true,
            allow_idna: false,
            validate_https_trackers: true,
        };
        assert!(crate::url_guard::validate_web_seed_url(
            "http://192.168.1.100/files/?secret=abc",
            &cfg,
        )
        .is_err());
    }
}
