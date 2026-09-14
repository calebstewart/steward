//! steward-cat: reads a unit's stdout and stderr and writes them to the
//! user's Event Log channel, in the spirit of `systemd-cat`.
//!
//! The manager starts one per unit whose output goes to the Event Log, in the
//! unit's job, with the read ends of the unit's two pipes. It outlives a
//! manager crash or hand-over and dies with the unit, and it is the one thing
//! whose own crash loses a unit's output, so it is small: two threads each
//! reading one pipe, one event per line, and nothing allocated once they run.
//!
//! The pieces that are not Windows -- where a line ends, what is held while
//! nobody listens, the text an event carries, TraceLogging's encoding --
//! are here so their tests run anywhere; the ETW provider is [`etw`], and the
//! program is `main.rs`.

#[cfg(windows)]
pub mod etw;
pub mod lines;
pub mod pending;
pub mod tlg;
pub mod utf16;
pub mod utf8;

/// The longest line one event carries; a longer one is cut into pieces this
/// long, as journald cuts at `LineMax=`. It is also what one read of a pipe
/// fills, and small enough that the Event Log keeps every event.
///
/// It keeps an event only if its record fits one 64 KiB chunk of the
/// channel's `.evtx`, and a TraceLogging record holds the event's data twice,
/// as written and as rendered. Measured on a real channel (2026-09-14): an
/// event whose text was 15,500 UTF-16 characters, 31,110 bytes of data in
/// all, was kept; one of 16,000, 32,110 bytes, was dropped by the service
/// without a word to the writer -- `EventWrite` succeeds either way. A line
/// is at most one UTF-16 unit a byte, so this is at most 24 KiB of text, well
/// clear of that with the unit's name beside it ([`UNIT_MAX`]).
pub const LINE_MAX: usize = 12 * 1024;

/// The longest unit name, in bytes, an event may carry beside [`LINE_MAX`]
/// of text: longer than any unit file's name.
pub const UNIT_MAX: usize = 256;

/// The most output held, per unit, while nobody listens: a great deal of
/// startup chatter, less three bytes a line. Allocated once; untouched pages
/// cost commit charge but no memory.
pub const HOLD_MAX: usize = 1 << 20;

/// One of a unit's two output streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Stream {
    Stdout = 0,
    Stderr = 1,
}

impl Stream {
    /// The `stream` field's value.
    pub fn name(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }

    fn from_u8(b: u8) -> Stream {
        if b == Stream::Stdout as u8 {
            Stream::Stdout
        } else {
            Stream::Stderr
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest event steward-cat can write stays under what the Event
    /// Log was seen to keep (31,110 bytes of data), with room to spare for
    /// the record's own overhead.
    #[test]
    fn the_largest_event_is_one_the_event_log_keeps() {
        let text = 2 * (LINE_MAX + 1);
        let unit = 2 * (UNIT_MAX + 1);
        let stream = 2 * ("stdout".len() + 1);
        assert!(
            text + unit + stream <= 26 * 1024,
            "{}",
            text + unit + stream
        );
    }

    /// A line fits the `u16` length a held line is stored with.
    #[test]
    fn a_line_fits_a_held_length() {
        assert!(LINE_MAX <= u16::MAX as usize);
    }
}
