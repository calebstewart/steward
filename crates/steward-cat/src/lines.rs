//! Cutting a stream into lines: one event per line, as journald makes one
//! entry per line.
//!
//! A line ends at `\n` or at a NUL (journald's `_LINE_BREAK=nul`), and the
//! terminator is not part of it, nor is a `\r` before the `\n`, so a Windows
//! program's CRLF reads the same as a LF. A line longer than the buffer is
//! cut where it fills (journald's `LineMax=`), at a character boundary. What
//! follows the last terminator waits for the rest of its line, and is
//! written as it is when the stream ends.

use crate::utf8;

/// Hands each complete line in `buf` to `line`, without its terminator, and
/// returns how many bytes of `buf` that used; the rest is the start of a line
/// still to come. If there is no complete line and `full` says `buf` can take
/// no more, the front of it is handed over as a line of its own instead, cut
/// where no character is split.
pub fn split(buf: &[u8], full: bool, mut line: impl FnMut(&[u8])) -> usize {
    let mut start = 0;
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' || b == 0 {
            let mut end = i;
            if b == b'\n' && end > start && buf[end - 1] == b'\r' {
                end -= 1;
            }
            line(&buf[start..end]);
            start = i + 1;
        }
    }
    if start == 0 && full && !buf.is_empty() {
        let mut cut = match utf8::complete_prefix(buf) {
            0 => buf.len(),
            n => n,
        };
        // Keep a CR with the LF that may follow it, so the next piece does
        // not begin with an empty line.
        if cut > 1 && buf[cut - 1] == b'\r' {
            cut -= 1;
        }
        line(&buf[..cut]);
        return cut;
    }
    start
}

/// The last of a stream, which no terminator ended: `rest` as one line, less
/// a trailing `\r`. Nothing, if it is empty.
pub fn last(rest: &[u8]) -> Option<&[u8]> {
    match rest {
        [] => None,
        [line @ .., b'\r'] => Some(line),
        line => Some(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(buf: &[u8], full: bool) -> (Vec<String>, usize) {
        let mut out = Vec::new();
        let used = split(buf, full, |l| {
            out.push(String::from_utf8_lossy(l).into_owned())
        });
        (out, used)
    }

    #[test]
    fn each_line_without_its_terminator() {
        assert_eq!(
            lines(b"one\ntwo\n", false),
            (vec!["one".into(), "two".into()], 8)
        );
    }

    #[test]
    fn crlf_is_a_line_end() {
        assert_eq!(
            lines(b"one\r\ntwo\r\n", false),
            (vec!["one".into(), "two".into()], 10)
        );
    }

    #[test]
    fn a_lone_cr_stays_in_the_line() {
        // A progress bar redrawing itself is one line.
        assert_eq!(
            lines(b"10%\r50%\r100%\n", false),
            (vec!["10%\r50%\r100%".into()], 13)
        );
    }

    #[test]
    fn empty_lines_are_lines() {
        assert_eq!(
            lines(b"a\n\n\nb\n", false),
            (vec!["a".into(), "".into(), "".into(), "b".into()], 6)
        );
    }

    #[test]
    fn nul_ends_a_line() {
        assert_eq!(
            lines(b"one\0two\n", false),
            (vec!["one".into(), "two".into()], 8)
        );
    }

    #[test]
    fn the_start_of_a_line_waits() {
        assert_eq!(lines(b"one\ntw", false), (vec!["one".into()], 4));
        assert_eq!(lines(b"partial", false), (vec![], 0));
        // A CR waits too: its LF may be in the next read.
        assert_eq!(lines(b"one\r", false), (vec![], 0));
    }

    #[test]
    fn a_full_buffer_with_no_line_end_is_cut() {
        assert_eq!(lines(b"abcdef", true), (vec!["abcdef".into()], 6));
        // Not partway through a character: "€" is three bytes.
        let buf = "abc€".as_bytes();
        assert_eq!(lines(&buf[..5], true), (vec!["abc".into()], 3));
        // Not between a CR and the LF that may follow.
        assert_eq!(lines(b"abc\r", true), (vec!["abc".into()], 3));
    }

    #[test]
    fn a_full_buffer_with_lines_in_it_is_not_cut() {
        assert_eq!(lines(b"one\ntwo", true), (vec!["one".into()], 4));
    }

    #[test]
    fn the_last_line_needs_no_terminator() {
        assert_eq!(last(b""), None);
        assert_eq!(last(b"bye"), Some(&b"bye"[..]));
        assert_eq!(last(b"bye\r"), Some(&b"bye"[..]));
    }
}
