# M26: Async Disk I/O + mmap Storage + ARC Disk Cache

**Goal:** Replace synchronous, blocking disk I/O with an async central actor
(DiskActor) backed by configurable storage backends (mmap/pread), write
buffering, and an ARC read cache — matching libtorrent-rasterbar's disk
subsystem architecture.

## Architecture

Central actor pattern. A single `DiskActor` tokio task receives all disk I/O
jobs via bounded mpsc channel, manages a write buffer (flushes on piece
completion), an ARC read cache, and dispatches blocking I/O to
`spawn_blocking` with semaphore-limited concurrency. Per-torrent `DiskHandle`s
provide the async API to `TorrentActor`s. `DiskManagerHandle` provides
session-level operations (register/unregister torrents, shutdown).

## Tech Stack

- tokio (spawn_blocking, mpsc, oneshot, Semaphore)
- memmap2
- bitflags
- bytes

## Components

### ferrite-core
- `StorageMode` enum: Auto, Sparse, Full, Mmap

### ferrite-storage
- `FilesystemStorage` pre-allocation mode (fallocate on Linux)
- `MmapStorage` backend using memmap2
- `ArcCache<K, V>` — Adaptive Replacement Cache (Megiddo & Modha 2003)

### ferrite-session
- `DiskJob` enum — all disk I/O operations as message types
- `DiskJobFlags` — hint bitflags (FORCE_COPY, SEQUENTIAL, VOLATILE_READ, FLUSH_PIECE)
- `DiskStats` — performance counters
- `DiskConfig` — I/O threads, cache size, write ratio, channel capacity
- `WriteBuffer` — batches writes per-piece, flushes on completion or pressure
- `DiskActor` — central tokio task processing DiskJob messages
- `DiskHandle` — per-torrent async API (write_chunk, read_chunk, verify_piece)
- `DiskManagerHandle` — session-level API (register/unregister, shutdown)

### Integration
- `SessionActor` creates `DiskManagerHandle`, registers torrents on add
- `TorrentActor` uses `DiskHandle` instead of `Arc<dyn TorrentStorage>`
- `SessionConfig` gains disk I/O fields
- `ClientBuilder` gains disk configuration methods
- Alert integration: `FileError`, `DiskStats` alerts

## Data Flow

```
TorrentActor → DiskHandle.write_chunk()
            → mpsc::Sender<DiskJob>
            → DiskActor receives DiskJob::Write
            → WriteBuffer (if not FLUSH_PIECE)
            → On piece complete: spawn_blocking { storage.write_chunk() }
            → Reply via oneshot

TorrentActor → DiskHandle.read_chunk()
            → mpsc::Sender<DiskJob>
            → DiskActor receives DiskJob::Read
            → Check WriteBuffer → Check ArcCache → spawn_blocking { storage.read_chunk() }
            → Cache result in ArcCache
            → Reply via oneshot
```

## Version

0.26.0 → 0.27.0
