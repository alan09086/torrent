#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Parse as a BitTorrent wire protocol message payload.
    let _ = torrent_wire::Message::from_payload(data);

    // Parse as a BEP 10 extension handshake.
    let _ = torrent_wire::ExtHandshake::from_bytes(data);

    // Parse as a BEP 3 handshake (68 bytes).
    let _ = torrent_wire::Handshake::from_bytes(data);

    // Parse as a BEP 55 holepunch message.
    let _ = torrent_wire::HolepunchMessage::from_bytes(data);
});
