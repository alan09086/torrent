//! Batched Have message buffer.
//!
//! Accumulates Have indices and flushes them periodically, upgrading to a
//! full Bitfield send when the count exceeds 50% of total pieces.

use std::collections::HashSet;

use torrent_storage::Bitfield;

/// Result of flushing the Have buffer.
pub(crate) enum FlushResult {
    /// Send individual Have messages for these piece indices.
    SendHaves(Vec<u32>),
    /// Send a full bitfield (more efficient when > 50% of pieces changed).
    SendBitfield(Bitfield),
}

/// Buffers Have messages for batched delivery.
pub(crate) struct HaveBuffer {
    /// Accumulated piece indices waiting to be sent.
    pending: HashSet<u32>,
    /// Whether batching is enabled (delay_ms > 0).
    enabled: bool,
    /// Total pieces in the torrent (for 50% threshold).
    num_pieces: u32,
}

#[allow(dead_code)]
impl HaveBuffer {
    pub fn new(num_pieces: u32, delay_ms: u64) -> Self {
        Self {
            pending: HashSet::new(),
            enabled: delay_ms > 0,
            num_pieces,
        }
    }

    /// Add a piece index to the buffer. No-op if batching is disabled.
    pub fn push(&mut self, index: u32) {
        if self.enabled {
            self.pending.insert(index);
        }
    }

    /// Drain pending pieces and decide how to send them.
    /// Returns `None` if nothing is pending.
    pub fn flush(&mut self, we_have: &Bitfield) -> Option<FlushResult> {
        if self.pending.is_empty() {
            return None;
        }

        let pieces: Vec<u32> = self.pending.drain().collect();

        if pieces.len() as u32 > self.num_pieces / 2 {
            Some(FlushResult::SendBitfield(we_have.clone()))
        } else {
            Some(FlushResult::SendHaves(pieces))
        }
    }

    /// Returns true if there are buffered pieces waiting to be sent.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Returns true if batching is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_mode_no_buffering() {
        let mut buf = HaveBuffer::new(10, 0);
        assert!(!buf.is_enabled());
        buf.push(3);
        // Push is a no-op when disabled
        assert!(!buf.has_pending());
        let bf = Bitfield::new(10);
        assert!(buf.flush(&bf).is_none());
    }

    #[test]
    fn batch_mode_returns_haves() {
        let mut buf = HaveBuffer::new(10, 100);
        assert!(buf.is_enabled());
        buf.push(2);
        buf.push(5);
        assert!(buf.has_pending());

        let bf = Bitfield::new(10);
        match buf.flush(&bf) {
            Some(FlushResult::SendHaves(pieces)) => {
                assert_eq!(pieces.len(), 2);
                assert!(pieces.contains(&2));
                assert!(pieces.contains(&5));
            }
            _ => panic!("expected SendHaves"),
        }

        // After flush, pending is empty
        assert!(!buf.has_pending());
    }

    #[test]
    fn large_batch_upgrades_to_bitfield() {
        let mut buf = HaveBuffer::new(10, 100);
        // Push 6 out of 10 pieces — exceeds 50% threshold
        for i in 0..6 {
            buf.push(i);
        }

        let mut bf = Bitfield::new(10);
        for i in 0..6 {
            bf.set(i);
        }

        match buf.flush(&bf) {
            Some(FlushResult::SendBitfield(result_bf)) => {
                for i in 0..6 {
                    assert!(result_bf.get(i));
                }
            }
            _ => panic!("expected SendBitfield"),
        }
    }
}
