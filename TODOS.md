# TODOS

## Next: io_uring Full Backend (M122)

M119 shipped the io_uring scaffold: `TorrentStorageAsync` trait behind the
`io-uring` feature flag, `pwritev(2)` vectored writes, and `fallocate`
sparse file pre-allocation. M122 will complete the full async I/O backend:

- **`io_uring` submission queue** — batched async pwrite/pread via io_uring SQ
- **`O_DIRECT`** — bypass page cache for large sequential writes (reduces memory pressure)
- **Configurable queue depth** — tune submission queue depth per workload
- **Async read path** — complement the existing async write path

## Decomposition / API (M121, in progress)

- TorrentActor decomposed into 4 sub-modules (state, peers, dispatch, verify)
- `TorrentSummary` + `SessionHandle` convenience API for future HTTP endpoints (M123+)
