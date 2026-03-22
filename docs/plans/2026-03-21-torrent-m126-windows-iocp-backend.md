# M126: Windows IOCP Backend

## Context

M122 shipped the io_uring backend for Linux. Windows users currently use the standard
`PosixDiskIo` path which calls `FilesystemStorage::write_chunk_vectored()` — on Windows
this falls through to the non-Linux `seek + write_all` fallback (no `pwritev` on Windows).

IOCP (I/O Completion Ports) is Windows' mature async I/O primitive, used by libtorrent
and most high-performance Windows network/disk applications. It provides kernel-managed
thread pool dispatch for overlapped I/O operations — conceptually similar to io_uring's
completion queue.

### Why IOCP for Disk I/O

Without IOCP, Windows disk writes use synchronous `WriteFile` which:
1. Blocks the calling thread for the entire I/O duration
2. Cannot be vectored (no `writev` equivalent without IOCP)
3. Cannot bypass the page cache without `FILE_FLAG_NO_BUFFERING`

With IOCP:
1. `WriteFileGather` / overlapped `WriteFile` — non-blocking submission
2. `GetQueuedCompletionStatusEx` — batched completion reaping
3. `FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING` — async direct I/O

## Design

### Architecture: Same Decorator Pattern as io_uring

`IocpDiskIo` wraps `PosixDiskIo`, overrides `write_block_direct()` (and in M126,
reads too). Identical fallback strategy — graceful degradation to inner backend.

```rust
#[cfg(target_os = "windows")]
pub(crate) struct IocpDiskIo {
    inner: PosixDiskIo,
    iocp: HANDLE,  // I/O Completion Port
    iocp_states: RwLock<HashMap<Id20, IocpTorrentState>>,
    config: IocpConfig,
    iocp_write_bytes: AtomicU64,
}
```

### IocpConfig

```rust
pub struct IocpConfig {
    /// Number of concurrent I/O threads (0 = system default). Default: 0.
    pub concurrent_threads: u32,
    /// Enable FILE_FLAG_NO_BUFFERING (direct I/O). Default: false.
    pub enable_direct_io: bool,
}
```

### IocpTorrentState

Pre-opened `HANDLE` per file (Windows equivalent of `RawFd`):

```rust
pub struct IocpTorrentState {
    handles: Vec<HANDLE>,
    file_map: FileMap,
}

impl IocpTorrentState {
    pub fn open_files(
        base_dir: &Path,
        file_paths: &[PathBuf],
        iocp: HANDLE,
        direct_io: bool,
    ) -> io::Result<Self> {
        // CreateFileW with FILE_FLAG_OVERLAPPED
        // Associate each handle with the IOCP via CreateIoCompletionPort
    }
}
```

### Write Path

```rust
fn iocp_write(
    &self,
    state: &IocpTorrentState,
    piece: u32,
    begin: u32,
    s0: &[u8],
    s1: &[u8],
) -> crate::Result<()> {
    let segments = state.file_map.chunk_segments(piece, begin, total);

    for seg in &segments {
        let handle = state.handles[seg.file_index];

        // Build OVERLAPPED with file offset.
        let mut overlapped: OVERLAPPED = zeroed();
        overlapped.u.s_mut().Offset = (seg.file_offset & 0xFFFFFFFF) as u32;
        overlapped.u.s_mut().OffsetHigh = (seg.file_offset >> 32) as u32;

        // Concatenate s0/s1 slice for this segment (Windows WriteFile
        // doesn't have vectored write without scatter/gather).
        let data = extract_segment_data(s0, s1, pos, seg.len);

        WriteFile(handle, data.as_ptr(), data.len(), &mut written, &mut overlapped);
    }

    // Wait for all completions via GetQueuedCompletionStatusEx.
    let mut entries = vec![OVERLAPPED_ENTRY::default(); segments.len()];
    GetQueuedCompletionStatusEx(self.iocp, &mut entries, segments.len(), INFINITE, FALSE);

    // Check results...
}
```

### StorageMode::Iocp

Add variant to `StorageMode` (not cfg-gated, like IoUring):
```rust
/// IOCP kernel async I/O (Windows).
/// Falls back to standard I/O when IOCP is unavailable.
Iocp,
```

### CLI Flag

`--iocp` on Download command — sets `StorageMode::Iocp`.

### Factory Integration

```rust
#[cfg(target_os = "windows")]
if config.storage_mode == StorageMode::Iocp {
    match IocpDiskIo::new(config, iocp_config) {
        Ok(backend) => return Arc::new(backend),
        Err(e) => warn!("IOCP init failed, falling back to posix: {e}"),
    }
}
```

## Dependencies

- `windows-sys` crate (lightweight FFI bindings, no COM overhead)
- Or `winapi` crate (older but widely used)
- Prefer `windows-sys` — maintained by Microsoft, `no_std` compatible

## Files Modified

| File | Change |
|------|--------|
| `crates/torrent-storage/src/iocp_backend.rs` | **NEW** — IocpConfig, IocpTorrentState |
| `crates/torrent-session/src/iocp_backend.rs` | **NEW** — IocpDiskIo impl |
| `crates/torrent-core/src/storage_mode.rs` | Add `Iocp` variant |
| `crates/torrent-session/src/disk_backend.rs` | Factory IOCP branch |
| `crates/torrent-session/src/settings.rs` | IOCP settings fields |
| `crates/torrent-cli/src/main.rs` | `--iocp` flag |

## Task Breakdown

### Task 1: IocpConfig + IocpTorrentState (torrent-storage)
### Task 2: IocpDiskIo struct + constructor + DiskIoBackend delegation
### Task 3: iocp_write() — overlapped WriteFile + completion reaping
### Task 4: iocp_read() — overlapped ReadFile (both read_chunk and read_piece)
### Task 5: StorageMode::Iocp + factory + CLI + settings
### Task 6: Tests (10-12 new, Windows-only)

## Success Criteria

- IOCP backend works on Windows 10+
- Graceful fallback on failure
- `--iocp` CLI flag enables the backend
- Performance parity with or better than synchronous WriteFile
- 10-12 new tests (cfg(target_os = "windows"))

## Risk: Cross-compilation Testing

This milestone requires a Windows build environment. Options:
- GitHub Actions `windows-latest` runner for CI
- Cross-compilation via `x86_64-pc-windows-msvc` target (requires MSVC linker)
- Local Windows VM or WSL2 with Windows host for integration testing
