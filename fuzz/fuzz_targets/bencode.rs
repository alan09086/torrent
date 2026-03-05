#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Deserialize arbitrary bytes as a BencodeValue — should never panic.
    let _ = torrent_bencode::from_bytes::<torrent_bencode::BencodeValue>(data);

    // Round-trip: if deserialization succeeds, re-serialization must not panic.
    if let Ok(val) = torrent_bencode::from_bytes::<torrent_bencode::BencodeValue>(data) {
        let _ = torrent_bencode::to_bytes(&val);
    }
});
