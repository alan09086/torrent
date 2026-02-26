//! A Rust BitTorrent library.
//!
//! `ferrite` is the public facade for the ferrite crate family. It re-exports
//! types from internal crates through a clean, ergonomic API.
//!
//! # Modules
//!
//! - [`bencode`] — Serde-based bencode serialization
//! - [`core`] — Hashes, metainfo, magnets, piece arithmetic

pub mod bencode;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bencode_round_trip_through_facade() {
        use serde::{Serialize, Deserialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Demo {
            name: String,
            value: i64,
        }

        let original = Demo { name: "ferrite".into(), value: 42 };
        let encoded = bencode::to_bytes(&original).unwrap();
        let decoded: Demo = bencode::from_bytes(&encoded).unwrap();
        assert_eq!(original, decoded);
    }
}
