#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Parse as a .torrent file (v1, v2, or hybrid — auto-detect).
    let _ = torrent_core::torrent_from_bytes_any(data);

    // Parse as a v1 .torrent file specifically.
    let _ = torrent_core::torrent_from_bytes(data);

    // Parse as a magnet URI (treat bytes as UTF-8).
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = torrent_core::Magnet::parse(s);
    }
});
