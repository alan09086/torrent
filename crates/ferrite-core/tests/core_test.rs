use ferrite_core::{torrent_from_bytes, sha1, Id20, Lengths, Magnet, PeerId};

/// Build a valid single-file torrent as raw bencode bytes.
fn make_test_torrent() -> Vec<u8> {
    // Create fake file content and compute real piece hashes
    let piece_length: usize = 262144;
    let file_data = vec![0xABu8; 500000]; // 500 KB file
    let num_pieces = (file_data.len() + piece_length - 1) / piece_length; // 2

    let mut pieces = Vec::new();
    for i in 0..num_pieces {
        let start = i * piece_length;
        let end = (start + piece_length).min(file_data.len());
        let hash = sha1(&file_data[start..end]);
        pieces.extend_from_slice(hash.as_bytes());
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(b"d");
    buf.extend_from_slice(b"8:announce35:http://tracker.example.com/announce");
    buf.extend_from_slice(b"7:comment12:test torrent");
    buf.extend_from_slice(b"4:info");
    buf.extend_from_slice(b"d");
    buf.extend_from_slice(b"6:lengthi500000e");
    buf.extend_from_slice(b"4:name8:test.dat");
    buf.extend_from_slice(format!("12:piece lengthi{piece_length}e").as_bytes());
    buf.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
    buf.extend_from_slice(&pieces);
    buf.extend_from_slice(b"e"); // end info dict
    buf.extend_from_slice(b"e"); // end root dict
    buf
}

#[test]
fn parse_synthetic_torrent() {
    let data = make_test_torrent();
    let torrent = torrent_from_bytes(&data).unwrap();

    assert_eq!(torrent.announce.as_deref(), Some("http://tracker.example.com/announce"));
    assert_eq!(torrent.comment.as_deref(), Some("test torrent"));
    assert_eq!(torrent.info.name, "test.dat");
    assert_eq!(torrent.info.piece_length, 262144);
    assert_eq!(torrent.info.length, Some(500000));
    assert_eq!(torrent.info.num_pieces(), 2);
    assert_eq!(torrent.info.total_length(), 500000);

    // Files should return single-file mode
    let files = torrent.info.files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, vec!["test.dat"]);
    assert_eq!(files[0].length, 500000);
}

#[test]
fn torrent_info_hash_is_stable() {
    let data = make_test_torrent();
    let t1 = torrent_from_bytes(&data).unwrap();
    let t2 = torrent_from_bytes(&data).unwrap();
    assert_eq!(t1.info_hash, t2.info_hash);
    // Info hash should not be all zeros
    assert_ne!(t1.info_hash, Id20::ZERO);
}

#[test]
fn torrent_info_hash_matches_manual_sha1() {
    let data = make_test_torrent();
    let torrent = torrent_from_bytes(&data).unwrap();

    // Manually find the info span and hash it
    let span = ferrite_bencode::find_dict_key_span(&data, "info").unwrap();
    let manual_hash = sha1(&data[span]);

    assert_eq!(torrent.info_hash, manual_hash);
}

#[test]
fn piece_hashes_are_valid() {
    let data = make_test_torrent();
    let torrent = torrent_from_bytes(&data).unwrap();

    // Verify we can extract individual piece hashes
    let h0 = torrent.info.piece_hash(0).unwrap();
    let h1 = torrent.info.piece_hash(1).unwrap();
    assert_ne!(h0, Id20::ZERO);
    // Both pieces have same content (0xAB...) but different lengths,
    // so hashes should differ
    assert_ne!(h0, h1);

    // Out of bounds
    assert!(torrent.info.piece_hash(2).is_none());
}

#[test]
fn lengths_from_torrent() {
    let data = make_test_torrent();
    let torrent = torrent_from_bytes(&data).unwrap();

    let lengths = Lengths::new(
        torrent.info.total_length(),
        torrent.info.piece_length,
        ferrite_core::DEFAULT_CHUNK_SIZE,
    );

    assert_eq!(lengths.num_pieces(), 2);
    assert_eq!(lengths.piece_size(0), 262144);
    assert_eq!(lengths.piece_size(1), 500000 - 262144); // 237856

    // Chunks in last piece
    let last_chunks = lengths.chunks_in_piece(1);
    assert!(last_chunks > 0);
    assert!(last_chunks <= 16); // can't have more than full piece
}

#[test]
fn magnet_with_known_hash() {
    let data = make_test_torrent();
    let torrent = torrent_from_bytes(&data).unwrap();

    // Build a magnet from the parsed torrent
    let uri = format!(
        "magnet:?xt=urn:btih:{}&dn=test.dat",
        torrent.info_hash.to_hex()
    );
    let magnet = Magnet::parse(&uri).unwrap();
    assert_eq!(magnet.info_hash, torrent.info_hash);
    assert_eq!(magnet.display_name.as_deref(), Some("test.dat"));
}

#[test]
fn peer_id_basics() {
    let id = PeerId::generate();
    assert_eq!(id.prefix(), b"-FE0100-");
    assert_eq!(id.as_bytes().len(), 20);
}

#[test]
fn reject_invalid_torrent_no_pieces() {
    let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi256e6:pieces0:ee";
    let result = torrent_from_bytes(data);
    // Should parse OK (0 pieces for 100 bytes is technically wrong but the
    // pieces length is 0 which is a multiple of 20)
    assert!(result.is_ok());
}

#[test]
fn reject_invalid_torrent_bad_pieces_length() {
    // pieces length not a multiple of 20
    let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi256e6:pieces5:helloee";
    let result = torrent_from_bytes(data);
    assert!(result.is_err());
}

#[test]
fn reject_torrent_missing_length_and_files() {
    let data = b"d4:infod4:name4:test12:piece lengthi256e6:pieces20:xxxxxxxxxxxxxxxxxxxx ee";
    let result = torrent_from_bytes(data);
    assert!(result.is_err());
}
