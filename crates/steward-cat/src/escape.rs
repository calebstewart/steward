//! Escape sequences taken out of a unit's output: the colours, cursor
//! movements and window titles a program writes for a terminal.
//!
//! The Event Log keeps them as written, and nothing that reads a channel is
//! a terminal: Event Viewer and `Get-WinEvent` show the codes as text among
//! the words, `[31mred[0m`, and `stewardctl logs` would hand them to whatever
//! terminal it runs in, which lets a unit's output set the reader's window
//! title or write their clipboard. So they are taken out as the output is
//! read, before a line is cut or held, and a line carries its text alone.
//!
//! What is taken out is ECMA-48's 7-bit forms, as a terminal reads them:
//!
//! - a control sequence, `ESC [` then parameter and intermediate bytes up to
//!   a final byte: colours (`ESC [ 1 ; 31 m`), cursor movement, erasing;
//! - a control string, `ESC ]`, `ESC P`, `ESC X`, `ESC ^` or `ESC _` up to
//!   ST (`ESC \`) or BEL: window titles, hyperlinks (`ESC ] 8`);
//! - any other escape, `ESC`, intermediate bytes, then a final byte:
//!   `ESC ( B`, `ESC =`, `ESC 7`.
//!
//! A byte that cannot continue a sequence ends it, and is kept: an ESC
//! before a letter of text loses only the ESC. A line end ends a control
//! string too, so one never finished costs the rest of its line and no
//! more. Other control characters -- a tab, a lone `\r`, BEL outside a
//! string -- are not escape sequences and are kept. The 8-bit C1 forms are
//! not UTF-8, and are left to become U+FFFD with any other such byte.
//!
//! The state is carried from one read to the next, so a sequence split
//! between two reads, or between the pieces of a line too long for one
//! event, is taken out whole.

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;

/// Where the last byte left the stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum State {
    /// Text.
    #[default]
    Text,
    /// After an ESC.
    Escape,
    /// After an ESC and an intermediate byte, as in `ESC ( B`.
    Intermediate,
    /// In a control sequence, after `ESC [`.
    Control,
    /// In a control string, after `ESC ]` and the like.
    String,
    /// After an ESC in a control string, which a `\` makes ST.
    StringEscape,
}

/// Takes escape sequences out of one stream, read by read; the default is
/// a stream that has not begun one.
#[derive(Debug, Default)]
pub struct Stripper {
    state: State,
}

impl Stripper {
    /// Takes the escape sequences out of `buf`, the next bytes of the
    /// stream, moving what is left to its front, and returns how many bytes
    /// that is.
    pub fn strip(&mut self, buf: &mut [u8]) -> usize {
        let mut kept = 0;
        for i in 0..buf.len() {
            let (state, dropped) = step(self.state, buf[i]);
            self.state = state;
            if !dropped {
                buf[kept] = buf[i];
                kept += 1;
            }
        }
        kept
    }
}

/// Where `b` leaves a stream that was at `state`, and whether `b` is part of
/// an escape sequence.
fn step(state: State, b: u8) -> (State, bool) {
    use State::*;
    match state {
        Text if b == ESC => (Escape, true),
        Text => (Text, false),
        Escape => match b {
            b'[' => (Control, true),
            b']' | b'P' | b'X' | b'^' | b'_' => (String, true),
            0x20..=0x2F => (Intermediate, true),
            0x30..=0x7E => (Text, true),
            // Another ESC begins again; anything else is text.
            _ => step(Text, b),
        },
        Intermediate => match b {
            0x20..=0x2F => (Intermediate, true),
            0x30..=0x7E => (Text, true),
            _ => step(Text, b),
        },
        Control => match b {
            // Parameter bytes, then intermediate bytes.
            0x20..=0x3F => (Control, true),
            0x40..=0x7E => (Text, true),
            _ => step(Text, b),
        },
        String => match b {
            BEL => (Text, true),
            ESC => (StringEscape, true),
            b'\n' | 0 => (Text, false),
            _ => (String, true),
        },
        StringEscape => match b {
            b'\\' => (Text, true),
            // The ESC ended the string and begins what follows.
            _ => step(Escape, b),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `bytes` stripped in one read, checked against stripping them a byte
    /// at a time and at every split into two reads.
    fn strip(bytes: &[u8]) -> String {
        let mut whole = bytes.to_vec();
        let n = Stripper::default().strip(&mut whole);
        whole.truncate(n);

        let mut stripper = Stripper::default();
        let mut single = Vec::new();
        for &b in bytes {
            let mut one = [b];
            let n = stripper.strip(&mut one);
            single.extend_from_slice(&one[..n]);
        }
        assert_eq!(single, whole, "a byte at a time: {bytes:?}");

        for cut in 0..=bytes.len() {
            let mut stripper = Stripper::default();
            let (mut a, mut b) = (bytes[..cut].to_vec(), bytes[cut..].to_vec());
            let n = stripper.strip(&mut a);
            a.truncate(n);
            let n = stripper.strip(&mut b);
            a.extend_from_slice(&b[..n]);
            assert_eq!(a, whole, "split at {cut}: {bytes:?}");
        }
        String::from_utf8(whole).expect("stripping keeps UTF-8 whole")
    }

    #[test]
    fn text_is_untouched() {
        for s in [
            "",
            "plain\n",
            "héllo wörld € 😀",
            "100% done",
            "tab\there\r\n",
            "10%\r50%\r100%",
            "a [31m that is only text",
        ] {
            assert_eq!(strip(s.as_bytes()), s, "{s:?}");
        }
    }

    #[test]
    fn colours_are_taken_out() {
        assert_eq!(
            strip(b"\x1b[31mred\x1b[0m and \x1b[1;32mbold green\x1b[m"),
            "red and bold green"
        );
        // 256 colours and truecolor, with `:` separators.
        assert_eq!(strip(b"\x1b[38;5;208ma\x1b[38:2::1:2:3mb"), "ab");
    }

    #[test]
    fn cursor_movement_and_erasing_are_taken_out() {
        assert_eq!(strip(b"\x1b[2K\x1b[1Gprogress\x1b[?25l\x1b[3A"), "progress");
        // An intermediate byte before the final one.
        assert_eq!(strip(b"a\x1b[2 qb"), "ab");
    }

    #[test]
    fn control_strings_are_taken_out() {
        // A window title, ended by BEL.
        assert_eq!(strip(b"\x1b]0;title\x07after"), "after");
        // A hyperlink, ended by ST, around its text.
        assert_eq!(
            strip(b"\x1b]8;;https://example.com/a%20b\x1b\\link\x1b]8;;\x1b\\ done"),
            "link done"
        );
        // The other strings, with UTF-8 in them.
        assert_eq!(
            strip("a\x1bPq€\x1b\\b\x1bXé\x07c\x1b^x\x1b\\d\x1b_y\x1b\\e".as_bytes()),
            "abcde"
        );
    }

    #[test]
    fn other_escapes_are_taken_out() {
        assert_eq!(
            strip(b"\x1b(Bcharset\x1b=keypad\x1b7\x1bMx"),
            "charsetkeypadx"
        );
        // ST on its own.
        assert_eq!(strip(b"a\x1b\\b"), "ab");
    }

    /// A byte that cannot continue a sequence ends it and is kept, so text
    /// after a broken sequence is not lost.
    #[test]
    fn a_broken_sequence_keeps_what_follows() {
        assert_eq!(strip("\x1bé".as_bytes()), "é");
        assert_eq!(strip(b"a\x1b\tb"), "a\tb");
        assert_eq!(strip(b"\x1b[31\nnext"), "\nnext");
        assert_eq!(strip("\x1b[1;€".as_bytes()), "€");
        assert_eq!(strip(b"\x1b(\rx"), "\rx");
        // An ESC begins again.
        assert_eq!(strip(b"\x1b\x1b[1mx"), "x");
        assert_eq!(strip(b"\x1b[1\x1b[2mx"), "x");
    }

    /// A control string never finished costs the rest of its line, not the
    /// lines after it.
    #[test]
    fn a_line_end_ends_a_control_string() {
        assert_eq!(strip(b"\x1b]0;title\nnext"), "\nnext");
        assert_eq!(strip(b"\x1b]0;title\x00next"), "\x00next");
        // An ESC in a string that is not ST begins the next sequence.
        assert_eq!(strip(b"\x1b]0;t\x1b[31mred"), "red");
        assert_eq!(strip(b"\x1b]0;t\x1b\nnext"), "\nnext");
    }

    #[test]
    fn a_sequence_at_the_very_end_is_taken_out() {
        for s in [
            "a\x1b",
            "a\x1b[",
            "a\x1b[31",
            "a\x1b]0;tit",
            "a\x1b]0;t\x1b",
            "a\x1b(",
        ] {
            assert_eq!(strip(s.as_bytes()), "a", "{s:?}");
        }
    }

    /// The state goes with the stream: a sequence begun in one read is
    /// taken out of the next, and a finished one leaves the next alone.
    #[test]
    fn the_state_carries_between_reads() {
        let mut stripper = Stripper::default();
        let mut first = *b"red\x1b[3";
        assert_eq!(stripper.strip(&mut first), 3);
        let mut second = *b"1mtext";
        let n = stripper.strip(&mut second);
        assert_eq!(&second[..n], b"text");
        let mut third = *b"[31m";
        let n = stripper.strip(&mut third);
        assert_eq!(&third[..n], b"[31m");
    }
}
