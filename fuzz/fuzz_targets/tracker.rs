#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Parse compact IPv4 peer list (tracker announce response).
    let _ = torrent_tracker::parse_compact_peers(data);

    // Parse compact IPv6 peer list.
    let _ = torrent_tracker::parse_compact_peers6(data);

    // Parse UDP tracker connect response (needs >= 16 bytes + transaction ID).
    if data.len() >= 4 {
        let tid = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let _ = torrent_tracker::UdpTracker::parse_connect_response(&data[4..], tid);
    }
});
