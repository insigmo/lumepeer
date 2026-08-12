//! Receive-side jitter buffer (design doc §4, §11).
//!
//! Reorders and paces decoded input so that network jitter does not reach the
//! renderer, while keeping the glass-to-glass budgets of §15.

use std::collections::BTreeMap;

use crate::encode::EncodedFrame;

/// Default depth of the buffer in frames.
const DEFAULT_CAPACITY_FRAMES: usize = 8;

/// Reordering buffer keyed by capture timestamp.
#[derive(Debug)]
pub struct JitterBuffer {
    frames: BTreeMap<u64, EncodedFrame>,
    capacity: usize,
    last_emitted_us: Option<u64>,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY_FRAMES)
    }
}

impl JitterBuffer {
    /// Creates a buffer holding at most `capacity` frames.
    #[must_use]
    pub const fn new(capacity: usize) -> Self {
        Self {
            frames: BTreeMap::new(),
            capacity,
            last_emitted_us: None,
        }
    }

    /// Inserts a frame. Frames older than the last emitted one are dropped,
    /// and the oldest frame is evicted when the buffer is full.
    pub fn push(&mut self, frame: EncodedFrame) {
        if self
            .last_emitted_us
            .is_some_and(|last| frame.timestamp_us <= last)
        {
            return;
        }
        self.frames.insert(frame.timestamp_us, frame);
        while self.frames.len() > self.capacity {
            self.frames.pop_first();
        }
    }

    /// Pops the oldest frame, if any.
    pub fn pop(&mut self) -> Option<EncodedFrame> {
        let (timestamp, frame) = self.frames.pop_first()?;
        self.last_emitted_us = Some(timestamp);
        Some(frame)
    }

    /// Number of buffered frames.
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether the buffer holds no frames.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Drops everything, e.g. on `SessionStop` or revoke.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.last_emitted_us = None;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn frame(timestamp_us: u64) -> EncodedFrame {
        EncodedFrame {
            keyframe: false,
            timestamp_us,
            data: Vec::new(),
        }
    }

    #[test]
    fn reorders_by_timestamp() {
        let mut buffer = JitterBuffer::default();
        buffer.push(frame(20));
        buffer.push(frame(10));
        assert_eq!(buffer.pop().unwrap().timestamp_us, 10);
        assert_eq!(buffer.pop().unwrap().timestamp_us, 20);
    }

    #[test]
    fn drops_frames_older_than_last_emitted() {
        let mut buffer = JitterBuffer::default();
        buffer.push(frame(30));
        let _ = buffer.pop();
        buffer.push(frame(10));
        assert!(buffer.is_empty());
    }
}
