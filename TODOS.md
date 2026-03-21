# TODOS

## Next: HTTP API (M123)

- REST API for session/torrent management (list, add, remove, stats)
- WebSocket event stream for real-time progress updates
- Foundation for future GUI/TUI clients

## Completed: io_uring Full Backend (M122)

io_uring write-path backend implemented as a decorator around PosixDiskIo:

- **`IoUringDiskIo`** wraps PosixDiskIo, overrides only `write_block_direct()`
- **`Writev` SQEs** via shared `Mutex<IoUring>` ring per session
- **`IoUringStorageState`** — pre-opened `RawFd` per file via `libc::open()`
- **`O_DIRECT` support** — optional, with automatic fallback for unaligned writes
- **Configurable** — `--io-uring`, `--direct-io`, `--uring-sq-depth` CLI flags
- **Graceful fallback** — to PosixDiskIo on old kernels or init failure
- **16 new tests**, zero regression on existing 1709

### Future io_uring work

- Async read path via io_uring (currently write-only)
- SQPOLL mode (requires CAP_SYS_NICE)
- Registered buffers for zero-copy submission
- Batch write accumulation across multiple blocks
- Per-worker ring sharding if lock contention exceeds 5%

## Completed: Decomposition / API (M121)

- TorrentActor decomposed into 4 sub-modules (state, peers, dispatch, verify)
- `TorrentSummary` + `SessionHandle` convenience API for future HTTP endpoints (M123+)
