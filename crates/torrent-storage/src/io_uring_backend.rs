//! io_uring storage helpers: config and pre-opened file descriptor management.

use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

/// Configuration for the io_uring disk I/O backend.
#[derive(Debug, Clone)]
pub struct IoUringConfig {
    /// Submission queue depth (number of SQEs). Default: 256.
    pub sq_depth: u32,
    /// Enable `O_DIRECT` for bypassing the kernel page cache. Default: false.
    /// When true, writes must be aligned to the filesystem block size (typically 4096).
    /// Unaligned writes fall back to regular pwritev.
    pub enable_direct_io: bool,
    /// Minimum number of segments to batch before submitting. Default: 4.
    /// Below this threshold, individual pwritev calls may be cheaper than ring overhead.
    pub batch_threshold: usize,
}

impl Default for IoUringConfig {
    fn default() -> Self {
        Self {
            sq_depth: 256,
            enable_direct_io: false,
            batch_threshold: 4,
        }
    }
}

/// Pre-opened raw file descriptors for io_uring SQE submission.
///
/// Holds one [`RawFd`] per torrent file, opened with `libc::open()` (bypassing
/// Rust's `File` abstraction for direct fd control). The [`Drop`] implementation
/// closes all file descriptors.
pub struct IoUringStorageState {
    fds: Vec<RawFd>,
}

impl IoUringStorageState {
    /// Open all torrent files and return their raw file descriptors.
    ///
    /// `flags` should be `libc::O_WRONLY | libc::O_CREAT` (plus `libc::O_DIRECT`
    /// when direct I/O is enabled).
    ///
    /// # Errors
    ///
    /// Returns an I/O error if any path contains an interior NUL byte or if
    /// `libc::open()` fails. On failure, all previously opened file descriptors
    /// are closed before returning.
    pub fn open_files(
        base_dir: &Path,
        file_paths: &[PathBuf],
        flags: libc::c_int,
    ) -> std::io::Result<Self> {
        let mut fds = Vec::with_capacity(file_paths.len());

        for path in file_paths {
            let full = base_dir.join(path);
            // as_encoded_bytes() is stable since Rust 1.74 -- gives raw bytes
            // without requiring OsStr::as_bytes() (which is Unix-only in std).
            let c_path = CString::new(full.as_os_str().as_encoded_bytes())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

            // SAFETY: c_path is a valid NUL-terminated C string. The flags and
            // mode are valid values for open(2). The returned fd is checked for
            // errors before use.
            let fd = unsafe { libc::open(c_path.as_ptr(), flags, 0o644) };
            if fd < 0 {
                // Close any already-opened fds before returning the error.
                for &opened_fd in &fds {
                    // SAFETY: opened_fd was returned by a successful libc::open()
                    // call earlier in this function and has not been closed yet.
                    unsafe {
                        libc::close(opened_fd);
                    }
                }
                return Err(std::io::Error::last_os_error());
            }
            fds.push(fd);
        }

        Ok(Self { fds })
    }

    /// Get the raw file descriptor for a given file index.
    ///
    /// # Panics
    ///
    /// Panics if `file_index >= self.fds.len()`.
    pub fn fd(&self, file_index: usize) -> RawFd {
        self.fds[file_index]
    }

    /// Number of open file descriptors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fds.len()
    }

    /// Returns true if no file descriptors are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fds.is_empty()
    }
}

impl Drop for IoUringStorageState {
    fn drop(&mut self) {
        for &fd in &self.fds {
            // SAFETY: each fd was returned by a successful libc::open() call in
            // open_files() and has not been closed since (barring external misuse
            // of the RawFd obtained via fd()).
            unsafe {
                libc::close(fd);
            }
        }
    }
}
