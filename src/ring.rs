//! Fixed-size byte ring holding the encoded stream, indexed by keyframe.
//!
//! The buffer is allocated once. Each segment starts at a keyframe, so any
//! segment start is a valid place to begin a clip without re-encoding.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
struct Segment {
    /// Absolute stream offset (total bytes written before this segment).
    start: u64,
    at: Instant,
}

pub struct Ring {
    buf: Box<[u8]>,
    /// Total bytes ever written; `head % cap` is the write position.
    head: u64,
    segments: VecDeque<Segment>,
    max_age: Duration,
}

pub struct Stats {
    pub buffered: Duration,
    pub bytes: u64,
    pub capacity: u64,
    pub segments: usize,
    pub total_written: u64,
}

impl Ring {
    pub fn new(capacity: usize, max_age: Duration) -> Self {
        Ring {
            buf: vec![0u8; capacity.max(1)].into_boxed_slice(),
            head: 0,
            segments: VecDeque::new(),
            max_age,
        }
    }

    fn cap(&self) -> u64 {
        self.buf.len() as u64
    }

    /// Mark that the next bytes pushed begin a new keyframe segment.
    pub fn mark(&mut self, at: Instant) {
        // Drop segments nobody can ask for any more: the oldest one is only
        // needed while the one after it is still younger than max_age.
        while self.segments.len() > 1 && at.duration_since(self.segments[1].at) >= self.max_age {
            self.segments.pop_front();
        }
        self.segments.push_back(Segment { start: self.head, at });
    }

    pub fn push(&mut self, data: &[u8]) {
        let cap = self.buf.len();
        // Only the tail can survive if a single write exceeds the capacity.
        let data = if data.len() > cap {
            self.head += (data.len() - cap) as u64;
            &data[data.len() - cap..]
        } else {
            data
        };
        let pos = (self.head % cap as u64) as usize;
        let first = data.len().min(cap - pos);
        self.buf[pos..pos + first].copy_from_slice(&data[..first]);
        self.buf[..data.len() - first].copy_from_slice(&data[first..]);
        self.head += data.len() as u64;

        // Segments whose first bytes were overwritten are no longer usable.
        let oldest_valid = self.head.saturating_sub(self.cap());
        while self.segments.front().is_some_and(|s| s.start < oldest_valid) {
            self.segments.pop_front();
        }
    }

    /// Copy out everything from the keyframe at or before `since` up to now.
    /// Falls back to the oldest keyframe if the buffer is shorter than asked.
    /// Returns the bytes and the time the clip starts at.
    pub fn snapshot(&self, since: Instant) -> Option<(Vec<u8>, Instant)> {
        let seg = self
            .segments
            .iter()
            .rev()
            .find(|s| s.at <= since)
            .or_else(|| self.segments.front())?;
        let len = (self.head - seg.start) as usize;
        if len == 0 {
            return None;
        }
        let cap = self.buf.len();
        let pos = (seg.start % cap as u64) as usize;
        let first = len.min(cap - pos);
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&self.buf[pos..pos + first]);
        out.extend_from_slice(&self.buf[..len - first]);
        Some((out, seg.at))
    }

    pub fn stats(&self, now: Instant) -> Stats {
        let (buffered, bytes) = match self.segments.front() {
            Some(s) => (now.duration_since(s.at), self.head - s.start),
            None => (Duration::ZERO, 0),
        };
        Stats {
            buffered,
            bytes,
            capacity: self.cap(),
            segments: self.segments.len(),
            total_written: self.head,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn snapshot_starts_at_keyframe_before_cutoff() {
        let t0 = Instant::now();
        let mut r = Ring::new(1024, secs(60));
        for i in 0..5u8 {
            r.mark(t0 + secs(i as u64 * 2));
            r.push(&[i; 10]);
        }
        // Cutoff at t=5s falls inside the segment that began at t=4s.
        let (data, at) = r.snapshot(t0 + secs(5)).unwrap();
        assert_eq!(at, t0 + secs(4));
        assert_eq!(data, [[2u8; 10], [3u8; 10], [4u8; 10]].concat());
    }

    #[test]
    fn snapshot_falls_back_to_oldest() {
        let t0 = Instant::now() + secs(100);
        let mut r = Ring::new(1024, secs(60));
        r.mark(t0);
        r.push(&[7; 4]);
        let (data, _) = r.snapshot(t0 - secs(50)).unwrap();
        assert_eq!(data, vec![7; 4]);
    }

    #[test]
    fn wraps_and_evicts_overwritten_segments() {
        let t0 = Instant::now();
        let mut r = Ring::new(32, secs(60));
        for i in 0..10u8 {
            r.mark(t0 + secs(i as u64));
            r.push(&[i; 10]);
        }
        // 32 bytes hold the last three 10-byte segments in full.
        let (data, at) = r.snapshot(t0).unwrap();
        assert_eq!(at, t0 + secs(7));
        assert_eq!(data, [[7u8; 10], [8u8; 10], [9u8; 10]].concat());
        assert_eq!(r.stats(t0 + secs(10)).segments, 3);
    }

    #[test]
    fn evicts_by_age_but_keeps_covering_segment() {
        let t0 = Instant::now();
        let mut r = Ring::new(4096, secs(10));
        for i in 0..20u64 {
            r.mark(t0 + secs(i * 3));
            r.push(&[i as u8; 8]);
        }
        // Newest mark at 57s, window reaches back to 47s, which lies inside
        // the segment that began at 45s, so that one must be kept.
        let (_, at) = r.snapshot(t0 + secs(47)).unwrap();
        assert_eq!(at, t0 + secs(45));
        assert_eq!(r.stats(t0 + secs(57)).segments, 5);
    }

    #[test]
    fn oversized_write_keeps_tail() {
        let t0 = Instant::now();
        let mut r = Ring::new(8, secs(60));
        r.mark(t0);
        r.push(&(0..20u8).collect::<Vec<_>>());
        assert!(r.snapshot(t0).is_none());
        r.mark(t0);
        r.push(&[1, 2]);
        assert_eq!(r.snapshot(t0).unwrap().0, vec![1, 2]);
    }
}
