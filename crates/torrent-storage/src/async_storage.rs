//! Async storage trait for io_uring-based I/O (future M122).
//!
//! This module provides the async counterpart of [`TorrentStorage`](crate::TorrentStorage).
//! It is gated behind the `io-uring` feature flag and currently serves as a
//! compilation target to validate the API surface before the io_uring
//! implementation lands.

use crate::Result;

/// Async counterpart of [`TorrentStorage`](crate::TorrentStorage).
///
/// Mirrors the sync trait's API with async methods. Implementations will
/// use io_uring submission queues for zero-copy, kernel-bypassing I/O.
///
/// Object-safe via `async_trait` so it can be used as `Arc<dyn TorrentStorageAsync>`.
#[async_trait::async_trait]
pub trait TorrentStorageAsync: Send + Sync {
    /// Write a chunk of data at `(piece, begin)`.
    async fn write_chunk(&self, piece: u32, begin: u32, data: &[u8]) -> Result<()>;

    /// Read a chunk of data from `(piece, begin, length)`.
    async fn read_chunk(&self, piece: u32, begin: u32, length: u32) -> Result<Vec<u8>>;

    /// Read an entire piece.
    async fn read_piece(&self, piece: u32) -> Result<Vec<u8>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock implementation to verify the trait compiles and is object-safe.
    struct MockAsyncStorage;

    #[async_trait::async_trait]
    impl TorrentStorageAsync for MockAsyncStorage {
        async fn write_chunk(&self, _piece: u32, _begin: u32, _data: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn read_chunk(&self, _piece: u32, _begin: u32, length: u32) -> Result<Vec<u8>> {
            Ok(vec![0u8; length as usize])
        }

        async fn read_piece(&self, _piece: u32) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn async_trait_compiles() {
        // Verify the trait is object-safe (can be used as dyn).
        let _storage: Box<dyn TorrentStorageAsync> = Box::new(MockAsyncStorage);
    }
}
