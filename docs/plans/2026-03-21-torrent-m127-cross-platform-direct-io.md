# M127: Cross-Platform Direct I/O

## Context

M122 added `O_DIRECT` support on Linux via io_uring. M126 adds
`FILE_FLAG_NO_BUFFERING` on Windows via IOCP. macOS has no kernel async disk I/O
primitive, but it does have `F_NOCACHE` — an `fcntl()` flag that disables the
unified buffer cache for a file descriptor.

This milestone unifies direct I/O across all platforms so `--direct-io` works
everywhere, and wires it into `FilesystemStorage` for the standard pwritev path
(not just io_uring/IOCP).

### Platform Direct I/O Mechanisms

| Platform | Mechanism | Granularity | Alignment Required |
|----------|-----------|-------------|-------------------|
| Linux | `O_DIRECT` on `open()` | Per-fd | Yes (512 or 4096) |
| macOS | `F_NOCACHE` via `fcntl()` | Per-fd | No |
| Windows | `FILE_FLAG_NO_BUFFERING` on `CreateFile` | Per-fd | Yes (sector size) |
| FreeBSD | `O_DIRECT` on `open()` | Per-fd | Yes |

### When Direct I/O Helps

- **Large sequential writes** (downloading) — avoids polluting the page cache with
  data that won't be re-read soon, leaving cache for other applications
- **Memory-constrained systems** — reduces RSS by not double-buffering in kernel + userspace
- **SSD workloads** — reduces write amplification from cache writeback

### When Direct I/O Hurts

- **Small random reads** (seeding random pieces) — bypasses readahead, every read hits disk
- **Hash verification** — reads full pieces sequentially, cache would help for re-verification

## Design

### FilesystemStorage Direct I/O Support

Add a `direct_io: bool` field to `FilesystemStorage`. When true:

**Linux/FreeBSD**: Open files with `O_DIRECT` flag in `open_file()`.
**macOS**: After opening, call `fcntl(fd, F_NOCACHE, 1)`.
**Windows**: Already handled by IOCP backend's `FILE_FLAG_NO_BUFFERING`.

```rust
impl FilesystemStorage {
    fn open_file(&self, index: usize) -> Result<MutexGuard<'_, Option<File>>> {
        let mut guard = self.files[index].lock();
        if guard.is_none() {
            let full = self.base_dir.join(&self.file_paths[index]);
            let mut opts = File::options();
            opts.read(true).write(true);

            #[cfg(target_os = "linux")]
            if self.direct_io {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(libc::O_DIRECT);
            }

            let f = opts.open(&full)?;

            #[cfg(target_os = "macos")]
            if self.direct_io {
                use std::os::unix::io::AsRawFd;
                unsafe { libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1); }
            }

            *guard = Some(f);
        }
        Ok(guard)
    }
}
```

### Alignment Handling

`O_DIRECT` on Linux requires writes aligned to the filesystem block size (typically
4096 bytes). For BitTorrent, the standard block size is 16384 bytes (16 KiB) which
is always 4096-aligned. The last block of a piece may be smaller — if it's not
aligned, fall back to buffered write for that block only.

```rust
fn needs_alignment_fallback(begin: u32, len: usize) -> bool {
    // 16 KiB blocks are always aligned. Only the last block may be unaligned.
    len % 4096 != 0 || (begin as usize) % 4096 != 0
}
```

macOS `F_NOCACHE` does NOT require alignment — it just disables caching.

### Settings

Wire `--direct-io` to set both:
- `io_uring_direct_io: true` (Linux io_uring path)
- `filesystem_direct_io: true` (standard pwritev path, all platforms)

New field in `Settings`:
```rust
/// Enable direct I/O for filesystem storage (bypasses kernel page cache).
/// Linux: O_DIRECT, macOS: F_NOCACHE, Windows: FILE_FLAG_NO_BUFFERING.
#[serde(default)]
pub filesystem_direct_io: bool,
```

### FilesystemStorage Constructor

Add `direct_io` parameter:
```rust
pub fn new(
    base_dir: &Path,
    file_paths: Vec<PathBuf>,
    file_lengths: Vec<u64>,
    lengths: Lengths,
    file_priorities: Option<&[FilePriority]>,
    mode: PreallocateMode,
    direct_io: bool,  // NEW
) -> Result<Self>
```

## Files Modified

| File | Change |
|------|--------|
| `crates/torrent-storage/src/filesystem.rs` | Add `direct_io` field, update `open_file()` |
| `crates/torrent-session/src/settings.rs` | Add `filesystem_direct_io` |
| `crates/torrent-cli/src/main.rs` | Wire `--direct-io` to both settings |

## Task Breakdown

### Task 1: Add direct_io field to FilesystemStorage, update constructor
### Task 2: Platform-specific open_file() with O_DIRECT / F_NOCACHE
### Task 3: Alignment fallback for unaligned last-block writes
### Task 4: Settings + CLI wiring
### Task 5: Tests (4-6 new) — direct IO write/read, alignment fallback, macOS F_NOCACHE

## Success Criteria

- `--direct-io` works on Linux (O_DIRECT), macOS (F_NOCACHE), Windows (via IOCP)
- Unaligned writes fall back gracefully
- 4-6 new tests pass
- No regression on standard (non-direct) I/O path
