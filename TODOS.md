# TODOS

## Next: io_uring / Direct I/O (M101+)

With M100's deferred write queue in place, the next performance frontier is
kernel-bypass I/O:

- **`io_uring`** backend for Linux — batched async pwrite/pread, zero syscall overhead
- **`O_DIRECT`** — bypass page cache for large sequential writes (reduces memory pressure)
- **Configurable queue depth** — currently hard-coded at 512, tune per-workload

These build on the M100 architecture where each block is written via
`storage.write_chunk()` from a dedicated writer task.
