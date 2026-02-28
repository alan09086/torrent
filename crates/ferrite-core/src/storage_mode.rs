/// How torrent files are allocated on disk.
///
/// Matches libtorrent's `storage_mode_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageMode {
    /// Auto-select: mmap on 64-bit, pread/pwrite on 32-bit.
    #[default]
    Auto,
    /// Sparse file allocation (set_len, no pre-allocation).
    Sparse,
    /// Full pre-allocation (fallocate / write zeros).
    Full,
    /// Memory-mapped file I/O (64-bit recommended).
    Mmap,
}
