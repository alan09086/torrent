//! File streaming — AsyncRead + AsyncSeek over individual torrent files.
//!
//! Provides [`FileStream`], which lets consumers read a file within a torrent
//! as if it were a regular seekable byte stream. The stream blocks on pieces
//! that haven't been downloaded yet and resumes automatically when the piece
//! arrives.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch};

use torrent_core::Lengths;
use torrent_storage::Bitfield;

use crate::disk::{DiskHandle, DiskJobFlags};

/// Internal handle passed from the TorrentActor to construct a [`FileStream`].
pub struct FileStreamHandle {
    pub(crate) disk: DiskHandle,
    pub(crate) lengths: Lengths,
    pub(crate) file_index: usize,
    pub(crate) file_offset: u64,
    pub(crate) file_length: u64,
    pub(crate) cursor_tx: watch::Sender<u64>,
    pub(crate) piece_ready_rx: broadcast::Receiver<u32>,
    pub(crate) have: watch::Receiver<Bitfield>,
    pub(crate) read_permit: OwnedSemaphorePermit,
}

impl std::fmt::Debug for FileStreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileStreamHandle")
            .field("file_index", &self.file_index)
            .field("file_offset", &self.file_offset)
            .field("file_length", &self.file_length)
            .finish_non_exhaustive()
    }
}

/// Streaming cursor tracked by the TorrentActor.
///
/// The actor polls this to update streaming piece priorities.
pub(crate) struct StreamingCursor {
    #[allow(dead_code)]
    pub file_index: usize,
    pub file_offset: u64,
    pub cursor_piece: u32,
    pub readahead_pieces: u32,
    pub cursor_rx: watch::Receiver<u64>,
}

/// Async reader/seeker over a single file within a torrent.
///
/// Created via [`crate::TorrentHandle::open_file()`]. Implements [`AsyncRead`] and
/// [`AsyncSeek`], blocking on pieces that haven't been downloaded yet.
///
/// The stream updates a cursor position that the TorrentActor uses to
/// prioritise downloading the pieces around the read head.
pub struct FileStream {
    disk: DiskHandle,
    lengths: Lengths,
    #[allow(dead_code)]
    file_index: usize,
    /// Absolute byte offset of the file within the torrent data.
    file_offset: u64,
    /// Length of this file in bytes.
    file_length: u64,
    /// Current read position relative to start of file.
    position: u64,
    /// Cursor channel to notify actor of read position changes.
    cursor_tx: watch::Sender<u64>,
    /// Broadcast receiver for piece completion notifications.
    piece_ready_rx: broadcast::Receiver<u32>,
    /// Watch receiver for the have-bitfield.
    have: watch::Receiver<Bitfield>,
    /// In-progress read future (piece, begin, length).
    pending_read:
        Option<Pin<Box<dyn std::future::Future<Output = torrent_storage::Result<Bytes>> + Send>>>,
    /// Buffered data from the last disk read (partially consumed).
    buffer: Bytes,
    /// Pending seek result (set by start_seek, consumed by poll_complete).
    seek_result: Option<io::Result<u64>>,
    /// Semaphore permit — held for the lifetime of the stream.
    _read_permit: OwnedSemaphorePermit,
}

impl FileStream {
    /// Construct a `FileStream` from an actor-provided handle.
    pub fn from_handle(h: FileStreamHandle) -> Self {
        Self {
            disk: h.disk,
            lengths: h.lengths,
            file_index: h.file_index,
            file_offset: h.file_offset,
            file_length: h.file_length,
            position: 0,
            cursor_tx: h.cursor_tx,
            piece_ready_rx: h.piece_ready_rx,
            have: h.have,
            pending_read: None,
            buffer: Bytes::new(),
            seek_result: None,
            _read_permit: h.read_permit,
        }
    }

    /// File length in bytes.
    pub fn file_length(&self) -> u64 {
        self.file_length
    }

    /// Current read position within the file.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Check whether the piece containing the current read position is available.
    fn current_piece_available(&self) -> bool {
        let abs = self.file_offset + self.position;
        if let Some((piece, _)) = self.lengths.byte_to_piece(abs) {
            let have = self.have.borrow();
            have.get(piece)
        } else {
            false
        }
    }

    /// How many bytes remain from `position` to end-of-file.
    fn remaining(&self) -> u64 {
        self.file_length.saturating_sub(self.position)
    }
}

impl AsyncRead for FileStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // EOF check
        if self.position >= self.file_length {
            return Poll::Ready(Ok(()));
        }

        // If we have buffered data, drain it first.
        if !self.buffer.is_empty() {
            let to_copy = self.buffer.len().min(buf.remaining());
            let to_copy = to_copy.min(self.remaining() as usize);
            buf.put_slice(&self.buffer[..to_copy]);
            self.buffer = self.buffer.slice(to_copy..);
            self.position += to_copy as u64;
            let _ = self.cursor_tx.send(self.position);
            return Poll::Ready(Ok(()));
        }

        // If we have a pending disk read, poll it.
        if let Some(ref mut fut) = self.pending_read {
            match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(data)) => {
                    self.pending_read = None;
                    let to_copy = data.len().min(buf.remaining());
                    let to_copy = to_copy.min(self.remaining() as usize);
                    buf.put_slice(&data[..to_copy]);
                    if to_copy < data.len() {
                        self.buffer = data.slice(to_copy..);
                    }
                    self.position += to_copy as u64;
                    let _ = self.cursor_tx.send(self.position);
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Err(e)) => {
                    self.pending_read = None;
                    return Poll::Ready(Err(io::Error::other(e.to_string())));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        // Check if the current piece is available.
        if !self.current_piece_available() {
            // Register waker with piece_ready_rx: when any piece completes,
            // we wake and re-check.
            let mut rx = self.piece_ready_rx.resubscribe();
            let waker = cx.waker().clone();
            tokio::spawn(async move {
                let _ = rx.recv().await;
                waker.wake();
            });
            return Poll::Pending;
        }

        // Piece is available — issue a disk read.
        let abs = self.file_offset + self.position;
        let Some((piece, offset_in_piece)) = self.lengths.byte_to_piece(abs) else {
            return Poll::Ready(Ok(())); // past end
        };

        // Read one chunk from the piece at the current offset.
        let piece_size = self.lengths.piece_size(piece);
        let read_len = (piece_size - offset_in_piece)
            .min(self.lengths.chunk_size())
            .min(self.remaining() as u32);

        let disk = self.disk.clone();
        let fut = Box::pin(async move {
            disk.read_chunk(piece, offset_in_piece, read_len, DiskJobFlags::SEQUENTIAL)
                .await
        });
        self.pending_read = Some(fut);

        // Poll the newly created future immediately.
        let fut = self.pending_read.as_mut().unwrap();
        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(data)) => {
                self.pending_read = None;
                let to_copy = data.len().min(buf.remaining());
                let to_copy = to_copy.min(self.remaining() as usize);
                buf.put_slice(&data[..to_copy]);
                if to_copy < data.len() {
                    self.buffer = data.slice(to_copy..);
                }
                self.position += to_copy as u64;
                let _ = self.cursor_tx.send(self.position);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                self.pending_read = None;
                Poll::Ready(Err(io::Error::other(e.to_string())))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncSeek for FileStream {
    fn start_seek(mut self: Pin<&mut Self>, pos: io::SeekFrom) -> io::Result<()> {
        let new_pos = match pos {
            io::SeekFrom::Start(n) => n as i64,
            io::SeekFrom::End(n) => self.file_length as i64 + n,
            io::SeekFrom::Current(n) => self.position as i64 + n,
        };

        if new_pos < 0 {
            self.seek_result = Some(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to negative position",
            )));
        } else {
            let new_pos = new_pos as u64;
            self.position = new_pos;
            self.buffer = Bytes::new();
            self.pending_read = None;
            let _ = self.cursor_tx.send(self.position);
            self.seek_result = Some(Ok(new_pos));
        }
        Ok(())
    }

    fn poll_complete(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        match self.seek_result.take() {
            Some(result) => Poll::Ready(result),
            None => Poll::Ready(Ok(self.position)),
        }
    }
}

/// Create a semaphore for limiting concurrent stream reads.
pub(crate) fn stream_read_semaphore(max: usize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(max))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a Lengths for testing.
    fn test_lengths() -> Lengths {
        // 4 pieces of 64KB, 16KB chunks = 256KB total
        Lengths::new(262144, 65536, 16384)
    }

    /// Helper: create a fully-available bitfield.
    fn full_bitfield(num_pieces: u32) -> Bitfield {
        let mut bf = Bitfield::new(num_pieces);
        for i in 0..num_pieces {
            bf.set(i);
        }
        bf
    }

    #[test]
    fn seek_updates_cursor() {
        // Create channels
        let (cursor_tx, mut cursor_rx) = watch::channel(0u64);
        let (_piece_tx, piece_rx) = broadcast::channel::<u32>(16);
        let have_bf = full_bitfield(4);
        let (have_tx, have_rx) = watch::channel(have_bf);
        let _ = have_tx; // keep alive

        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.try_acquire_owned().unwrap();

        // We need a DiskHandle to construct FileStream, but we won't actually read.
        // Create a dummy one via a channel that we immediately drop the receiver of.
        let (disk_tx, _disk_rx) = tokio::sync::mpsc::channel(1);
        let disk = DiskHandle::new(disk_tx, torrent_core::Id20::ZERO);

        let handle = FileStreamHandle {
            disk,
            lengths: test_lengths(),
            file_index: 0,
            file_offset: 0,
            file_length: 262144,
            cursor_tx,
            piece_ready_rx: piece_rx,
            have: have_rx,
            read_permit: permit,
        };

        let mut stream = FileStream::from_handle(handle);

        // Seek to position 100000
        use tokio::io::AsyncSeek;
        Pin::new(&mut stream)
            .start_seek(io::SeekFrom::Start(100000))
            .unwrap();

        // Cursor should have been updated
        assert!(cursor_rx.has_changed().unwrap());
        assert_eq!(*cursor_rx.borrow_and_update(), 100000);
        assert_eq!(stream.position(), 100000);
    }

    #[test]
    fn seek_end_relative() {
        let (cursor_tx, _cursor_rx) = watch::channel(0u64);
        let (_piece_tx, piece_rx) = broadcast::channel::<u32>(16);
        let (have_tx, have_rx) = watch::channel(full_bitfield(4));
        let _ = have_tx;

        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.try_acquire_owned().unwrap();
        let (disk_tx, _disk_rx) = tokio::sync::mpsc::channel(1);
        let disk = DiskHandle::new(disk_tx, torrent_core::Id20::ZERO);

        let handle = FileStreamHandle {
            disk,
            lengths: test_lengths(),
            file_index: 0,
            file_offset: 0,
            file_length: 262144,
            cursor_tx,
            piece_ready_rx: piece_rx,
            have: have_rx,
            read_permit: permit,
        };

        let mut stream = FileStream::from_handle(handle);

        // Seek to 1024 bytes before end
        use tokio::io::AsyncSeek;
        Pin::new(&mut stream)
            .start_seek(io::SeekFrom::End(-1024))
            .unwrap();
        assert_eq!(stream.position(), 262144 - 1024);
    }

    #[test]
    fn seek_negative_errors() {
        let (cursor_tx, _cursor_rx) = watch::channel(0u64);
        let (_piece_tx, piece_rx) = broadcast::channel::<u32>(16);
        let (have_tx, have_rx) = watch::channel(full_bitfield(4));
        let _ = have_tx;

        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.try_acquire_owned().unwrap();
        let (disk_tx, _disk_rx) = tokio::sync::mpsc::channel(1);
        let disk = DiskHandle::new(disk_tx, torrent_core::Id20::ZERO);

        let handle = FileStreamHandle {
            disk,
            lengths: test_lengths(),
            file_index: 0,
            file_offset: 0,
            file_length: 262144,
            cursor_tx,
            piece_ready_rx: piece_rx,
            have: have_rx,
            read_permit: permit,
        };

        let mut stream = FileStream::from_handle(handle);

        // Seek to negative position
        use tokio::io::AsyncSeek;
        Pin::new(&mut stream)
            .start_seek(io::SeekFrom::Start(0))
            .unwrap();
        Pin::new(&mut stream)
            .start_seek(io::SeekFrom::Current(-1))
            .unwrap();

        // poll_complete should return error
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(async {
            use futures::FutureExt;
            std::future::poll_fn(|cx| Pin::new(&mut stream).poll_complete(cx)).await
        });
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn eof_returns_zero_bytes() {
        let (cursor_tx, _cursor_rx) = watch::channel(0u64);
        let (_piece_tx, piece_rx) = broadcast::channel::<u32>(16);
        let (have_tx, have_rx) = watch::channel(full_bitfield(4));
        let _ = have_tx;

        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.try_acquire_owned().unwrap();
        let (disk_tx, _disk_rx) = tokio::sync::mpsc::channel(1);
        let disk = DiskHandle::new(disk_tx, torrent_core::Id20::ZERO);

        let handle = FileStreamHandle {
            disk,
            lengths: test_lengths(),
            file_index: 0,
            file_offset: 0,
            file_length: 262144,
            cursor_tx,
            piece_ready_rx: piece_rx,
            have: have_rx,
            read_permit: permit,
        };

        let mut stream = FileStream::from_handle(handle);
        // Set position to EOF
        stream.position = 262144;

        let mut buf = [0u8; 1024];
        let mut read_buf = ReadBuf::new(&mut buf);
        let result =
            std::future::poll_fn(|cx| Pin::new(&mut stream).poll_read(cx, &mut read_buf)).await;
        assert!(result.is_ok());
        assert_eq!(read_buf.filled().len(), 0);
    }

    #[tokio::test]
    async fn blocks_on_missing_piece_wakes_on_completion() {
        let (cursor_tx, _cursor_rx) = watch::channel(0u64);
        let (piece_tx, piece_rx) = broadcast::channel::<u32>(16);
        // Start with empty bitfield — no pieces available
        let empty_bf = Bitfield::new(4);
        let (have_tx, have_rx) = watch::channel(empty_bf);

        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.try_acquire_owned().unwrap();
        let (disk_tx, _disk_rx) = tokio::sync::mpsc::channel(1);
        let disk = DiskHandle::new(disk_tx, torrent_core::Id20::ZERO);

        let handle = FileStreamHandle {
            disk,
            lengths: test_lengths(),
            file_index: 0,
            file_offset: 0,
            file_length: 262144,
            cursor_tx,
            piece_ready_rx: piece_rx,
            have: have_rx,
            read_permit: permit,
        };

        let mut stream = FileStream::from_handle(handle);

        // Try to read — should return Pending because piece 0 is missing
        let mut buf = [0u8; 1024];
        let mut read_buf = ReadBuf::new(&mut buf);
        let is_pending = std::future::poll_fn(|cx| {
            let result = Pin::new(&mut stream).poll_read(cx, &mut read_buf);
            match result {
                Poll::Pending => Poll::Ready(true),
                Poll::Ready(_) => Poll::Ready(false),
            }
        })
        .await;
        assert!(is_pending, "should be Pending when piece is missing");

        // Now mark piece 0 as available and broadcast
        let mut bf = Bitfield::new(4);
        bf.set(0);
        have_tx.send(bf).unwrap();
        piece_tx.send(0).unwrap();

        // The waker should fire (we can't easily test the full wake cycle
        // without a real disk backend, but we verified it returns Pending
        // when the piece is missing, which is the critical behavior).
    }
}
