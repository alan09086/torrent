use criterion::{Criterion, black_box, criterion_group, criterion_main};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct TorrentInfo {
    name: String,
    #[serde(rename = "piece length")]
    piece_length: i64,
    #[serde(with = "serde_bytes")]
    pieces: Vec<u8>,
    length: i64,
}

fn bench_serialize(c: &mut Criterion) {
    let info = TorrentInfo {
        name: "ubuntu-24.04-desktop-amd64.iso".into(),
        piece_length: 262144,
        pieces: vec![0xAB; 20 * 1000], // 1000 piece hashes
        length: 4_000_000_000,
    };

    c.bench_function("bencode_serialize_1k_pieces", |b| {
        b.iter(|| {
            let _ = black_box(torrent_bencode::to_bytes(&info).unwrap());
        });
    });
}

fn bench_deserialize(c: &mut Criterion) {
    let info = TorrentInfo {
        name: "ubuntu-24.04-desktop-amd64.iso".into(),
        piece_length: 262144,
        pieces: vec![0xAB; 20 * 1000],
        length: 4_000_000_000,
    };
    let encoded = torrent_bencode::to_bytes(&info).unwrap();

    c.bench_function("bencode_deserialize_1k_pieces", |b| {
        b.iter(|| {
            let _: TorrentInfo = black_box(torrent_bencode::from_bytes(&encoded).unwrap());
        });
    });
}

fn bench_value_parse(c: &mut Criterion) {
    let info = TorrentInfo {
        name: "test".into(),
        piece_length: 16384,
        pieces: vec![0xFF; 20 * 5000], // 5000 piece hashes
        length: 1_000_000_000,
    };
    let encoded = torrent_bencode::to_bytes(&info).unwrap();

    c.bench_function("bencode_value_parse_5k_pieces", |b| {
        b.iter(|| {
            let _: torrent_bencode::BencodeValue =
                black_box(torrent_bencode::from_bytes(&encoded).unwrap());
        });
    });
}

criterion_group!(benches, bench_serialize, bench_deserialize, bench_value_parse);
criterion_main!(benches);
