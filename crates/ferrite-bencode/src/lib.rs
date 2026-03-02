#![warn(missing_docs)]
//! Serde-based bencode codec for BitTorrent.
//!
//! Bencode is the serialization format used throughout BitTorrent for .torrent
//! files, tracker responses, and DHT messages. It has four types:
//!
//! - **Integers**: `i42e`, `i-1e`, `i0e`
//! - **Byte strings**: `4:spam` (length-prefixed)
//! - **Lists**: `l<values>e`
//! - **Dictionaries**: `d<key><value>...e` (keys are byte strings, sorted)
//!
//! # Usage
//!
//! ```
//! use serde::{Serialize, Deserialize};
//! use ferrite_bencode::{to_bytes, from_bytes};
//!
//! #[derive(Serialize, Deserialize, PartialEq, Debug)]
//! struct Torrent {
//!     announce: String,
//!     #[serde(rename = "piece length")]
//!     piece_length: i64,
//! }
//!
//! let torrent = Torrent {
//!     announce: "http://tracker.example.com/announce".into(),
//!     piece_length: 262144,
//! };
//!
//! let encoded = to_bytes(&torrent).unwrap();
//! let decoded: Torrent = from_bytes(&encoded).unwrap();
//! assert_eq!(torrent, decoded);
//! ```

mod error;
mod ser;
mod de;
mod span;
mod value;

pub use error::{Error, Result};
pub use ser::Serializer;
pub use de::Deserializer;
pub use span::find_dict_key_span;
pub use value::BencodeValue;

/// Serialize a value to bencode bytes.
pub fn to_bytes<T: serde::Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut serializer = Serializer::new(&mut buf);
    serde::Serialize::serialize(value, &mut serializer)?;
    Ok(buf)
}

/// Deserialize a value from bencode bytes.
pub fn from_bytes<'de, T: serde::Deserialize<'de>>(bytes: &'de [u8]) -> Result<T> {
    let mut deserializer = Deserializer::new(bytes);
    let value = serde::Deserialize::deserialize(&mut deserializer)?;
    deserializer.finish()?;
    Ok(value)
}
