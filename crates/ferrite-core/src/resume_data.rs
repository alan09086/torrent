use serde::{Deserialize, Serialize};

fn default_neg_one() -> i64 {
    -1
}

/// A partial piece that was in progress when the torrent was paused/stopped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnfinishedPiece {
    /// Piece index.
    pub piece: i64,
    /// Bitmask of which blocks within the piece have been downloaded.
    #[serde(with = "serde_bytes")]
    pub bitmask: Vec<u8>,
}

/// libtorrent-compatible fast-resume data in bencode format.
///
/// This struct matches libtorrent's resume file format so that resume data
/// can be read/written by both Ferrite and libtorrent-based clients.
/// Every field uses `#[serde(rename = "...")]` to match libtorrent's exact
/// bencode dictionary keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FastResumeData {
    /// Always "libtorrent resume file".
    #[serde(rename = "file-format")]
    pub file_format: String,

    /// Always 1.
    #[serde(rename = "file-version")]
    pub file_version: i64,

    /// 20-byte SHA1 info hash.
    #[serde(rename = "info-hash")]
    #[serde(with = "serde_bytes")]
    pub info_hash: Vec<u8>,

    /// Torrent name.
    #[serde(rename = "name")]
    pub name: String,

    /// Path where files are saved.
    #[serde(rename = "save_path")]
    pub save_path: String,

    /// Bitfield indicating which pieces are complete.
    #[serde(rename = "pieces")]
    #[serde(with = "serde_bytes")]
    pub pieces: Vec<u8>,

    /// Partially downloaded pieces.
    #[serde(rename = "unfinished")]
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub unfinished: Vec<UnfinishedPiece>,

    /// Total bytes uploaded.
    #[serde(rename = "total_uploaded")]
    pub total_uploaded: i64,

    /// Total bytes downloaded.
    #[serde(rename = "total_downloaded")]
    pub total_downloaded: i64,

    /// Total time active in seconds.
    #[serde(rename = "active_time")]
    pub active_time: i64,

    /// Total time spent seeding in seconds.
    #[serde(rename = "seeding_time")]
    pub seeding_time: i64,

    /// Total time in finished state in seconds.
    #[serde(rename = "finished_time")]
    pub finished_time: i64,

    /// POSIX timestamp when the torrent was added.
    #[serde(rename = "added_time")]
    pub added_time: i64,

    /// POSIX timestamp when the torrent completed.
    #[serde(rename = "completed_time")]
    #[serde(default)]
    pub completed_time: i64,

    /// POSIX timestamp of last download activity.
    #[serde(rename = "last_download")]
    #[serde(default)]
    pub last_download: i64,

    /// POSIX timestamp of last upload activity.
    #[serde(rename = "last_upload")]
    #[serde(default)]
    pub last_upload: i64,

    /// Whether the torrent is paused (0 or 1).
    #[serde(rename = "paused")]
    #[serde(default)]
    pub paused: i64,

    /// Whether the torrent is auto-managed.
    #[serde(rename = "auto_managed")]
    #[serde(default)]
    pub auto_managed: i64,

    /// Queue position (-1 = not queued).
    #[serde(rename = "queue_position")]
    #[serde(default = "default_neg_one")]
    pub queue_position: i64,

    /// Whether sequential download is enabled.
    #[serde(rename = "sequential_download")]
    #[serde(default)]
    pub sequential_download: i64,

    /// Whether seed mode is enabled.
    #[serde(rename = "seed_mode")]
    #[serde(default)]
    pub seed_mode: i64,

    /// Tracker tiers (list of lists of tracker URLs).
    #[serde(rename = "trackers")]
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub trackers: Vec<Vec<String>>,

    /// Compact IPv4 peers (6 bytes each: 4 IP + 2 port).
    #[serde(rename = "peers")]
    #[serde(with = "serde_bytes")]
    #[serde(default)]
    pub peers: Vec<u8>,

    /// Compact IPv6 peers (18 bytes each: 16 IP + 2 port).
    #[serde(rename = "peers6")]
    #[serde(with = "serde_bytes")]
    #[serde(default)]
    pub peers6: Vec<u8>,

    /// Per-file priority values.
    #[serde(rename = "file_priority")]
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub file_priority: Vec<i64>,

    /// Per-piece priority values.
    #[serde(rename = "piece_priority")]
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub piece_priority: Vec<i64>,

    /// Upload rate limit in bytes/sec (-1 = unlimited).
    #[serde(rename = "upload_rate_limit")]
    #[serde(default)]
    pub upload_rate_limit: i64,

    /// Download rate limit in bytes/sec (-1 = unlimited).
    #[serde(rename = "download_rate_limit")]
    #[serde(default)]
    pub download_rate_limit: i64,

    /// Max connections for this torrent (-1 = unlimited).
    #[serde(rename = "max_connections")]
    #[serde(default)]
    pub max_connections: i64,

    /// Max upload slots for this torrent (-1 = unlimited).
    #[serde(rename = "max_uploads")]
    #[serde(default)]
    pub max_uploads: i64,

    /// Raw bencoded info dictionary (for magnet links that have resolved).
    #[serde(rename = "info")]
    #[serde(with = "serde_bytes")]
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub info: Option<Vec<u8>>,

    /// BEP 19 web seed URLs (GetRight-style).
    #[serde(rename = "url_seeds")]
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub url_seeds: Vec<String>,

    /// BEP 17 HTTP seed URLs (Hoffman-style).
    #[serde(rename = "http_seeds")]
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub http_seeds: Vec<String>,
}

impl FastResumeData {
    /// Create a new `FastResumeData` with format markers pre-filled and all
    /// other fields zeroed/empty. Rate limits default to -1 (unlimited).
    pub fn new(info_hash: Vec<u8>, name: String, save_path: String) -> Self {
        Self {
            file_format: "libtorrent resume file".into(),
            file_version: 1,
            info_hash,
            name,
            save_path,
            pieces: Vec::new(),
            unfinished: Vec::new(),
            total_uploaded: 0,
            total_downloaded: 0,
            active_time: 0,
            seeding_time: 0,
            finished_time: 0,
            added_time: 0,
            completed_time: 0,
            last_download: 0,
            last_upload: 0,
            paused: 0,
            auto_managed: 0,
            queue_position: -1,
            sequential_download: 0,
            seed_mode: 0,
            trackers: Vec::new(),
            peers: Vec::new(),
            peers6: Vec::new(),
            file_priority: Vec::new(),
            piece_priority: Vec::new(),
            upload_rate_limit: -1,
            download_rate_limit: -1,
            max_connections: -1,
            max_uploads: -1,
            info: None,
            url_seeds: Vec::new(),
            http_seeds: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn fast_resume_data_bencode_round_trip() {
        let mut resume = FastResumeData::new(
            vec![0xAA; 20],
            "test-torrent".into(),
            "/downloads".into(),
        );
        resume.total_uploaded = 1024 * 1024;
        resume.total_downloaded = 2048 * 1024;
        resume.active_time = 3600;
        resume.added_time = 1700000000;
        resume.trackers = vec![
            vec!["http://tracker1.example.com/announce".into()],
            vec![
                "http://tracker2.example.com/announce".into(),
                "http://tracker3.example.com/announce".into(),
            ],
        ];
        resume.pieces = vec![0xFF; 10];

        let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
        let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(resume, decoded);
    }

    #[test]
    fn unfinished_piece_bencode_round_trip() {
        let piece = UnfinishedPiece {
            piece: 42,
            bitmask: vec![0b1010_1010, 0b0101_0101],
        };

        let encoded = ferrite_bencode::to_bytes(&piece).unwrap();
        let decoded: UnfinishedPiece = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(piece, decoded);
    }

    #[test]
    fn resume_data_with_unfinished_pieces() {
        let mut resume = FastResumeData::new(
            vec![0xBB; 20],
            "partial-torrent".into(),
            "/downloads".into(),
        );
        resume.unfinished = vec![
            UnfinishedPiece {
                piece: 5,
                bitmask: vec![0xFF, 0x0F],
            },
            UnfinishedPiece {
                piece: 12,
                bitmask: vec![0xF0],
            },
        ];

        let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
        let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(resume, decoded);
    }

    #[test]
    fn default_fields_serialize_correctly() {
        let resume = FastResumeData::new(
            vec![0x00; 20],
            "minimal".into(),
            "/tmp".into(),
        );

        let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
        let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(resume, decoded);

        // Verify default values survived the round-trip.
        assert_eq!(decoded.total_uploaded, 0);
        assert_eq!(decoded.total_downloaded, 0);
        assert_eq!(decoded.paused, 0);
        assert_eq!(decoded.upload_rate_limit, -1);
        assert_eq!(decoded.download_rate_limit, -1);
        assert_eq!(decoded.max_connections, -1);
        assert_eq!(decoded.max_uploads, -1);
        assert!(decoded.trackers.is_empty());
        assert!(decoded.unfinished.is_empty());
        assert!(decoded.file_priority.is_empty());
        assert!(decoded.info.is_none());
    }

    #[test]
    fn info_dict_embedding_round_trip() {
        let mut resume = FastResumeData::new(
            vec![0xCC; 20],
            "with-info".into(),
            "/downloads".into(),
        );
        // Simulate a raw bencoded info dict.
        resume.info = Some(b"d4:name10:test-torte12:piece lengthi262144e6:pieces20:AAAAAAAAAAAAAAAAAAAAe".to_vec());

        let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
        let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(resume, decoded);
        assert!(decoded.info.is_some());
        assert_eq!(decoded.info.unwrap().len(), resume.info.unwrap().len());
    }

    #[test]
    fn resume_data_queue_position_default() {
        let rd = FastResumeData::new(vec![0; 20], "test".into(), "/tmp".into());
        assert_eq!(rd.queue_position, -1);
    }

    #[test]
    fn format_markers_correct() {
        let resume = FastResumeData::new(
            vec![0x00; 20],
            "test".into(),
            "/tmp".into(),
        );
        assert_eq!(resume.file_format, "libtorrent resume file");
        assert_eq!(resume.file_version, 1);
    }

    #[test]
    fn resume_data_url_seeds_round_trip() {
        let mut resume = FastResumeData::new(
            vec![0xDD; 20],
            "web-seed-test".into(),
            "/downloads".into(),
        );
        resume.url_seeds = vec![
            "http://example.com/files".into(),
            "http://mirror.example.com/".into(),
        ];

        let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
        let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.url_seeds, resume.url_seeds);
    }

    #[test]
    fn resume_data_http_seeds_round_trip() {
        let mut resume = FastResumeData::new(
            vec![0xEE; 20],
            "http-seed-test".into(),
            "/downloads".into(),
        );
        resume.http_seeds = vec!["http://seed.example.com/seed".into()];

        let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
        let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.http_seeds, resume.http_seeds);
    }

    #[test]
    fn resume_data_empty_seeds_not_serialized() {
        let resume = FastResumeData::new(
            vec![0x00; 20],
            "no-seeds".into(),
            "/tmp".into(),
        );
        assert!(resume.url_seeds.is_empty());
        assert!(resume.http_seeds.is_empty());

        let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
        let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert!(decoded.url_seeds.is_empty());
        assert!(decoded.http_seeds.is_empty());
    }
}
