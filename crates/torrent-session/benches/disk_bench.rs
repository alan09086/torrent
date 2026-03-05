use std::sync::Arc;

use bytes::Bytes;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use torrent_core::{Id20, Lengths, sha1};
use torrent_session::disk::{DiskConfig, DiskJobFlags, DiskManagerHandle};
use torrent_storage::MemoryStorage;

fn make_hash(n: u8) -> Id20 {
    let mut b = [0u8; 20];
    b[0] = n;
    Id20(b)
}

fn bench_write_and_verify(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("disk_write_16k_chunk", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (mgr, _actor) = DiskManagerHandle::new(DiskConfig::default());
                let ih = make_hash(1);
                let lengths = Lengths::new(16384, 16384, 16384);
                let storage = Arc::new(MemoryStorage::new(lengths));
                let disk = mgr.register_torrent(ih, storage).await;

                let data = Bytes::from(vec![42u8; 16384]);
                black_box(
                    disk.write_chunk(0, 0, data, DiskJobFlags::FLUSH_PIECE)
                        .await
                        .unwrap(),
                );
                mgr.shutdown().await;
            });
        });
    });

    c.bench_function("disk_verify_piece_16k", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (mgr, _actor) = DiskManagerHandle::new(DiskConfig::default());
                let ih = make_hash(2);
                let piece_data = vec![99u8; 16384];
                let expected = sha1(&piece_data);
                let lengths = Lengths::new(16384, 16384, 16384);
                let storage = Arc::new(MemoryStorage::new(lengths));
                let disk = mgr.register_torrent(ih, storage).await;

                disk.write_chunk(0, 0, Bytes::from(piece_data), DiskJobFlags::FLUSH_PIECE)
                    .await
                    .unwrap();
                black_box(
                    disk.verify_piece(0, expected, DiskJobFlags::empty())
                        .await
                        .unwrap(),
                );
                mgr.shutdown().await;
            });
        });
    });

    c.bench_function("sha1_hash_256k", |b| {
        let data = vec![0xABu8; 262144];
        b.iter(|| {
            black_box(sha1(&data));
        });
    });
}

criterion_group!(benches, bench_write_and_verify);
criterion_main!(benches);
