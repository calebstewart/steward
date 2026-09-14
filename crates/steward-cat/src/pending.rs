//! Output held while nobody listens.
//!
//! `EventWrite` to a provider nobody has enabled is discarded and still
//! returns success, so until the Event Log service enables the channel --
//! on an account's first sign-in the channel does not exist yet, and the
//! service may be restarting -- output waits here and is written, in order,
//! once someone listens.
//!
//! The buffer is allocated once, at its full size, and never grows. Once a
//! line does not fit, that line and every one after it is dropped until what
//! is held has been written, so what is lost is one gap at the end, and the
//! count of what fell into it is written right after what was held: the
//! first event that gets through after the gap says how much the gap was.
//! A write ETW refuses is counted the same way.

use crate::Stream;

/// A held line: its stream, then its length as a little-endian `u16`.
const HEADER: usize = 3;

/// The lines held until someone listens, and how much has been lost.
pub struct Pending {
    buf: Vec<u8>,
    /// Bytes lost from each stream, indexed by `Stream as usize`.
    dropped: [u64; 2],
    /// Something has been lost and not yet reported; hold nothing more.
    full: bool,
}

impl Pending {
    /// A buffer holding at most `capacity` bytes, lines and their headers.
    pub fn with_capacity(capacity: usize) -> Pending {
        Pending {
            buf: Vec::with_capacity(capacity),
            dropped: [0; 2],
            full: false,
        }
    }

    /// Nothing held and nothing lost.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty() && self.dropped == [0; 2]
    }

    /// The bytes of output held, and lost, that have not been written.
    pub fn unwritten(&self) -> u64 {
        let mut held = 0;
        let mut rest = &self.buf[..];
        while let [_, lo, hi, tail @ ..] = rest {
            let len = u16::from_le_bytes([*lo, *hi]) as usize;
            held += len as u64;
            rest = &tail[len..];
        }
        held + self.dropped[0] + self.dropped[1]
    }

    /// Holds one line, or counts it as lost if it does not fit. A line is
    /// at most `u16::MAX` bytes.
    pub fn push(&mut self, stream: Stream, data: &[u8]) {
        let len = u16::try_from(data.len()).expect("a line is at most u16::MAX bytes");
        // The capacity never changes: `extend_from_slice` within it does not
        // reallocate.
        if !self.full && self.buf.len() + HEADER + data.len() <= self.buf.capacity() {
            self.buf.push(stream as u8);
            self.buf.extend_from_slice(&len.to_le_bytes());
            self.buf.extend_from_slice(data);
        } else {
            self.lose(stream, data.len() as u64);
        }
    }

    /// Counts `n` bytes of `stream` as lost: a gap, reported by the next
    /// drain, and nothing more held until then.
    pub fn lose(&mut self, stream: Stream, n: u64) {
        self.full = true;
        self.dropped[stream as usize] += n;
    }

    /// Writes every held line through `line`, oldest first, then each
    /// stream's lost byte count, if any, through `dropped`; each returns
    /// whether the write went. A line that did not go is counted as lost; a
    /// count that did not go is kept for the next drain. Returns whether
    /// nothing is left to write.
    pub fn drain(
        &mut self,
        mut line: impl FnMut(Stream, &[u8]) -> bool,
        mut dropped: impl FnMut(Stream, u64) -> bool,
    ) -> bool {
        let mut rest = &self.buf[..];
        while let [stream, lo, hi, tail @ ..] = rest {
            let stream = Stream::from_u8(*stream);
            let len = u16::from_le_bytes([*lo, *hi]) as usize;
            if !line(stream, &tail[..len]) {
                self.full = true;
                self.dropped[stream as usize] += len as u64;
            }
            rest = &tail[len..];
        }
        self.buf.clear();
        for stream in [Stream::Stdout, Stream::Stderr] {
            let n = self.dropped[stream as usize];
            if n != 0 && dropped(stream, n) {
                self.dropped[stream as usize] = 0;
            }
        }
        self.full = self.dropped != [0; 2];
        !self.full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    enum Out {
        Read(Stream, Vec<u8>),
        Dropped(Stream, u64),
    }

    /// Drains, writing everything.
    fn drain(pending: &mut Pending) -> Vec<Out> {
        drain_with(pending, |_| true)
    }

    /// Drains, with `goes` deciding which writes go.
    fn drain_with(pending: &mut Pending, goes: impl Fn(&Out) -> bool) -> Vec<Out> {
        let out = std::cell::RefCell::new(Vec::new());
        let write = |o: Out| {
            let went = goes(&o);
            if went {
                out.borrow_mut().push(o);
            }
            went
        };
        pending.drain(
            |s, d| write(Out::Read(s, d.to_vec())),
            |s, n| write(Out::Dropped(s, n)),
        );
        out.into_inner()
    }

    #[test]
    fn holds_lines_in_order() {
        let mut p = Pending::with_capacity(64);
        assert!(p.is_empty());
        p.push(Stream::Stdout, b"one\n");
        p.push(Stream::Stderr, b"two\n");
        p.push(Stream::Stdout, b"");
        p.push(Stream::Stdout, b"three\n");
        assert!(!p.is_empty());
        assert_eq!(p.unwritten(), 14);
        assert_eq!(
            drain(&mut p),
            [
                Out::Read(Stream::Stdout, b"one\n".to_vec()),
                Out::Read(Stream::Stderr, b"two\n".to_vec()),
                Out::Read(Stream::Stdout, b"".to_vec()),
                Out::Read(Stream::Stdout, b"three\n".to_vec()),
            ]
        );
        assert!(p.is_empty());
        assert_eq!(drain(&mut p), []);
    }

    #[test]
    fn drops_past_the_bound_and_counts_per_stream() {
        // Room for "aaaa" and "bbbb" with their headers, and no more.
        let mut p = Pending::with_capacity(2 * (HEADER + 4));
        p.push(Stream::Stdout, b"aaaa");
        p.push(Stream::Stderr, b"bbbb");
        p.push(Stream::Stdout, b"ccccc");
        p.push(Stream::Stderr, b"dd");
        p.push(Stream::Stdout, b"e");
        assert_eq!(p.unwritten(), 16);
        assert_eq!(
            drain(&mut p),
            [
                Out::Read(Stream::Stdout, b"aaaa".to_vec()),
                Out::Read(Stream::Stderr, b"bbbb".to_vec()),
                Out::Dropped(Stream::Stdout, 6),
                Out::Dropped(Stream::Stderr, 2),
            ]
        );
        assert!(p.is_empty());
    }

    #[test]
    fn a_short_line_after_a_drop_is_dropped_too() {
        // The gap stays one gap at the end: "b" would fit, but after "big"
        // was dropped it would be written before the count of what was lost.
        let mut p = Pending::with_capacity(HEADER + 2 + HEADER + 1);
        p.push(Stream::Stdout, b"aa");
        p.push(Stream::Stdout, b"big");
        p.push(Stream::Stdout, b"b");
        assert_eq!(
            drain(&mut p),
            [
                Out::Read(Stream::Stdout, b"aa".to_vec()),
                Out::Dropped(Stream::Stdout, 4),
            ]
        );
    }

    #[test]
    fn holds_again_after_a_drain() {
        let mut p = Pending::with_capacity(HEADER + 1);
        p.push(Stream::Stdout, b"a");
        p.push(Stream::Stdout, b"b");
        drain(&mut p);
        p.push(Stream::Stderr, b"c");
        assert_eq!(drain(&mut p), [Out::Read(Stream::Stderr, b"c".to_vec())]);
    }

    #[test]
    fn a_refused_write_is_counted_as_lost() {
        let mut p = Pending::with_capacity(64);
        p.push(Stream::Stdout, b"one");
        p.push(Stream::Stderr, b"huge");
        p.push(Stream::Stdout, b"two");
        let out = drain_with(&mut p, |o| {
            *o != Out::Read(Stream::Stderr, b"huge".to_vec())
        });
        assert_eq!(
            out,
            [
                Out::Read(Stream::Stdout, b"one".to_vec()),
                Out::Read(Stream::Stdout, b"two".to_vec()),
                Out::Dropped(Stream::Stderr, 4),
            ]
        );
        assert!(p.is_empty());
    }

    #[test]
    fn an_unreported_gap_stays_until_it_is_written() {
        let mut p = Pending::with_capacity(HEADER + 1);
        p.push(Stream::Stdout, b"a");
        p.push(Stream::Stdout, b"bc");
        // "a" goes, the count of the gap does not.
        let first = drain_with(&mut p, |o| matches!(o, Out::Read(..)));
        assert_eq!(first, [Out::Read(Stream::Stdout, b"a".to_vec())]);
        assert!(!p.is_empty());
        // Nothing is held behind the gap before it has been reported.
        p.push(Stream::Stdout, b"d");
        assert_eq!(drain(&mut p), [Out::Dropped(Stream::Stdout, 3)]);
        assert!(p.is_empty());
    }

    #[test]
    fn lose_is_reported_by_the_next_drain() {
        let mut p = Pending::with_capacity(64);
        p.lose(Stream::Stderr, 10);
        // After a loss nothing is held until it is reported, on any stream.
        p.push(Stream::Stdout, b"held back");
        assert_eq!(
            drain(&mut p),
            [
                Out::Dropped(Stream::Stdout, 9),
                Out::Dropped(Stream::Stderr, 10)
            ]
        );
    }

    #[test]
    fn never_grows() {
        let mut p = Pending::with_capacity(100);
        let cap = p.buf.capacity();
        for round in 0..3 {
            for _ in 0..50 {
                p.push(Stream::Stdout, &[b'x'; 7]);
            }
            drain_with(&mut p, |_| round != 1);
        }
        assert_eq!(p.buf.capacity(), cap);
    }
}
