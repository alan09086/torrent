use std::collections::BTreeMap;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Extension handshake (BEP 10, ext_id=0).
///
/// Exchanged after the standard handshake to negotiate extension IDs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtHandshake {
    /// Map of extension name → assigned message ID.
    #[serde(default)]
    pub m: BTreeMap<String, u8>,
    /// Client name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v: Option<String>,
    /// TCP listen port for incoming connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<u16>,
    /// Number of outstanding request queue size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reqq: Option<u32>,
    /// Total size of metadata (for ut_metadata).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_size: Option<u64>,
    /// BEP 21: upload-only flag (1 = seeder, 0 or absent = leecher).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_only: Option<u8>,
}

impl ExtHandshake {
    /// Create a handshake advertising our supported extensions.
    pub fn new() -> Self {
        let mut m = BTreeMap::new();
        m.insert("ut_metadata".into(), 1);
        m.insert("ut_pex".into(), 2);
        m.insert("lt_trackers".into(), 3);
        m.insert("ut_holepunch".into(), 4);

        ExtHandshake {
            m,
            v: Some("Ferrite 0.1.0".into()),
            p: None,
            reqq: Some(250),
            metadata_size: None,
            upload_only: None,
        }
    }

    /// Create a handshake advertising built-in + plugin extensions.
    ///
    /// Built-in extensions are assigned IDs 1–4. Plugin names are assigned
    /// IDs starting at 10, in the order provided.
    pub fn new_with_plugins(plugin_names: &[&str]) -> Self {
        let mut hs = Self::new();
        for (i, name) in plugin_names.iter().enumerate() {
            hs.m.insert((*name).into(), 10 + i as u8);
        }
        hs
    }

    /// Create a handshake advertising upload-only (BEP 21 seeder) status.
    pub fn new_upload_only() -> Self {
        let mut hs = Self::new();
        hs.upload_only = Some(1);
        hs
    }

    /// Returns true if the peer declared BEP 21 upload-only status.
    pub fn is_upload_only(&self) -> bool {
        self.upload_only.unwrap_or(0) != 0
    }

    /// Encode to bencode bytes.
    pub fn to_bytes(&self) -> Result<Bytes> {
        let data = ferrite_bencode::to_bytes(self)?;
        Ok(Bytes::from(data))
    }

    /// Decode from bencode bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Ok(ferrite_bencode::from_bytes(data)?)
    }

    /// Look up the message ID for an extension by name.
    pub fn ext_id(&self, name: &str) -> Option<u8> {
        self.m.get(name).copied()
    }
}

/// Parsed extension messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtMessage {
    /// Extension handshake (ext_id=0).
    Handshake(Bytes),
    /// ut_metadata message (BEP 9).
    Metadata(MetadataMessage),
}

/// ut_metadata message types (BEP 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataMessageType {
    Request = 0,
    Data = 1,
    Reject = 2,
}

/// ut_metadata message (BEP 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataMessage {
    pub msg_type: MetadataMessageType,
    pub piece: u32,
    /// Total metadata size (included in Data messages).
    pub total_size: Option<u64>,
    /// Metadata piece data (appended after the bencode dict for Data messages).
    pub data: Option<Bytes>,
}

/// Raw bencode structure for ut_metadata messages.
#[derive(Serialize, Deserialize)]
struct MetadataDict {
    msg_type: u8,
    piece: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_size: Option<u64>,
}

impl MetadataMessage {
    /// Create a request for metadata piece.
    pub fn request(piece: u32) -> Self {
        MetadataMessage {
            msg_type: MetadataMessageType::Request,
            piece,
            total_size: None,
            data: None,
        }
    }

    /// Create a data response for a metadata piece.
    pub fn data(piece: u32, total_size: u64, data: Bytes) -> Self {
        MetadataMessage {
            msg_type: MetadataMessageType::Data,
            piece,
            total_size: Some(total_size),
            data: Some(data),
        }
    }

    /// Create a reject for metadata piece.
    pub fn reject(piece: u32) -> Self {
        MetadataMessage {
            msg_type: MetadataMessageType::Reject,
            piece,
            total_size: None,
            data: None,
        }
    }

    /// Encode to bytes (bencode dict + optional trailing data).
    pub fn to_bytes(&self) -> Result<Bytes> {
        let dict = MetadataDict {
            msg_type: self.msg_type as u8,
            piece: self.piece,
            total_size: self.total_size,
        };
        let mut buf = ferrite_bencode::to_bytes(&dict)?;
        if let Some(ref data) = self.data {
            buf.extend_from_slice(data);
        }
        Ok(Bytes::from(buf))
    }

    /// Parse from bytes. The bencode dict may be followed by raw data.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        // Find the end of the bencode dict by scanning for the matching 'e'
        let dict_end = find_bencode_dict_end(data)?;
        let dict: MetadataDict = ferrite_bencode::from_bytes(&data[..dict_end])?;

        let msg_type = match dict.msg_type {
            0 => MetadataMessageType::Request,
            1 => MetadataMessageType::Data,
            2 => MetadataMessageType::Reject,
            n => {
                return Err(Error::InvalidExtended(format!(
                    "unknown metadata msg_type {n}"
                )))
            }
        };

        let trailing = if dict_end < data.len() {
            Some(Bytes::copy_from_slice(&data[dict_end..]))
        } else {
            None
        };

        Ok(MetadataMessage {
            msg_type,
            piece: dict.piece,
            total_size: dict.total_size,
            data: trailing,
        })
    }
}

/// Find the end position of a bencode dictionary.
fn find_bencode_dict_end(data: &[u8]) -> Result<usize> {
    if data.first() != Some(&b'd') {
        return Err(Error::InvalidExtended("expected bencode dict".into()));
    }
    let mut pos = 1;
    let mut depth = 1u32;

    while pos < data.len() && depth > 0 {
        match data[pos] {
            b'd' | b'l' => {
                depth += 1;
                pos += 1;
            }
            b'e' => {
                depth -= 1;
                pos += 1;
            }
            b'i' => {
                pos += 1;
                while pos < data.len() && data[pos] != b'e' {
                    pos += 1;
                }
                pos += 1; // skip 'e'
            }
            b'0'..=b'9' => {
                // byte string: parse length, skip content
                let len_start = pos;
                while pos < data.len() && data[pos] != b':' {
                    pos += 1;
                }
                let len: usize = std::str::from_utf8(&data[len_start..pos])
                    .map_err(|_| Error::InvalidExtended("bad string length".into()))?
                    .parse()
                    .map_err(|_| Error::InvalidExtended("bad string length".into()))?;
                pos += 1 + len; // skip ':' + content
            }
            b => {
                return Err(Error::InvalidExtended(format!(
                    "unexpected byte {b:#04x} at position {pos}"
                )));
            }
        }
    }

    if depth != 0 {
        return Err(Error::InvalidExtended("unterminated dict".into()));
    }
    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_handshake_round_trip() {
        let hs = ExtHandshake::new();
        let bytes = hs.to_bytes().unwrap();
        let parsed = ExtHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(hs.m, parsed.m);
        assert_eq!(hs.v, parsed.v);
        assert_eq!(hs.reqq, parsed.reqq);
    }

    #[test]
    fn ext_handshake_ext_id_lookup() {
        let hs = ExtHandshake::new();
        assert_eq!(hs.ext_id("ut_metadata"), Some(1));
        assert_eq!(hs.ext_id("ut_pex"), Some(2));
        assert_eq!(hs.ext_id("lt_trackers"), Some(3));
        assert_eq!(hs.ext_id("ut_holepunch"), Some(4));
        assert_eq!(hs.ext_id("unknown"), None);
    }

    #[test]
    fn ext_handshake_upload_only_round_trip() {
        let hs = ExtHandshake::new_upload_only();
        assert!(hs.is_upload_only());
        let bytes = hs.to_bytes().unwrap();
        let parsed = ExtHandshake::from_bytes(&bytes).unwrap();
        assert!(parsed.is_upload_only());
        assert_eq!(parsed.upload_only, Some(1));
    }

    #[test]
    fn ext_handshake_no_upload_only_default() {
        let hs = ExtHandshake::new();
        assert!(!hs.is_upload_only());
        assert_eq!(hs.upload_only, None);
    }

    #[test]
    fn ext_handshake_with_plugins() {
        let hs = ExtHandshake::new_with_plugins(&["ut_comment", "ut_holepunch"]);
        // Built-ins unchanged
        assert_eq!(hs.ext_id("ut_metadata"), Some(1));
        assert_eq!(hs.ext_id("ut_pex"), Some(2));
        assert_eq!(hs.ext_id("lt_trackers"), Some(3));
        // Plugins at 10+
        assert_eq!(hs.ext_id("ut_comment"), Some(10));
        assert_eq!(hs.ext_id("ut_holepunch"), Some(11));
    }

    #[test]
    fn ext_handshake_with_plugins_round_trip() {
        let hs = ExtHandshake::new_with_plugins(&["ut_echo"]);
        let bytes = hs.to_bytes().unwrap();
        let parsed = ExtHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.ext_id("ut_echo"), Some(10));
        assert_eq!(parsed.ext_id("ut_metadata"), Some(1));
    }

    #[test]
    fn ext_handshake_no_plugins() {
        let hs = ExtHandshake::new_with_plugins(&[]);
        assert_eq!(hs.m.len(), 4); // only built-ins
    }

    #[test]
    fn metadata_request_round_trip() {
        let msg = MetadataMessage::request(3);
        let bytes = msg.to_bytes().unwrap();
        let parsed = MetadataMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.msg_type, MetadataMessageType::Request);
        assert_eq!(parsed.piece, 3);
        assert!(parsed.data.is_none());
    }

    #[test]
    fn metadata_data_with_trailing() {
        let msg = MetadataMessage {
            msg_type: MetadataMessageType::Data,
            piece: 0,
            total_size: Some(31415),
            data: Some(Bytes::from_static(b"raw metadata bytes here")),
        };
        let bytes = msg.to_bytes().unwrap();
        let parsed = MetadataMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.msg_type, MetadataMessageType::Data);
        assert_eq!(parsed.piece, 0);
        assert_eq!(parsed.total_size, Some(31415));
        assert_eq!(parsed.data.as_deref(), Some(b"raw metadata bytes here".as_ref()));
    }

    #[test]
    fn metadata_reject() {
        let msg = MetadataMessage::reject(5);
        let bytes = msg.to_bytes().unwrap();
        let parsed = MetadataMessage::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.msg_type, MetadataMessageType::Reject);
        assert_eq!(parsed.piece, 5);
    }
}
