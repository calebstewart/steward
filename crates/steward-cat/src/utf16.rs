//! A line as the UTF-16 text an event carries.
//!
//! The Event Log keeps strings as UTF-16, and it is the one encoding every
//! reader of a channel decodes the same way. An 8-bit string field, even one
//! marked UTF-8, comes back from `EvtRender` -- the event's XML, Event
//! Viewer's Details tab, `Get-WinEvent`'s properties -- decoded in the
//! system's ANSI code page, and only the rendered message honours the UTF-8.
//! So output is decoded as UTF-8 here, once, and written as UTF-16.
//!
//! The fields are nul-terminated: a counted string makes the Event Log add a
//! `<field>_Length` element beside each one. A NUL ends a line before it gets
//! here ([`crate::lines`]); one that did would become U+2400 SYMBOL FOR NULL
//! rather than end the field. A `%` becomes U+FF05 FULLWIDTH PERCENT SIGN,
//! since the Event Log renders most events with a `%` in them as empty
//! ([`steward_eventlog::PERCENT_STAND_IN`]). Bytes that are not UTF-8 become
//! U+FFFD, as `String::from_utf8_lossy` would have it.

/// What a NUL in the output becomes.
pub const NUL: u16 = 0x2400;

/// What a `%` becomes: [`steward_eventlog::PERCENT_STAND_IN`], since the
/// Event Log cannot render most events with a `%` in them.
pub const PERCENT: u16 = steward_eventlog::PERCENT_STAND_IN as u16;

/// Replaces `out` with `bytes` as nul-terminated UTF-16. Each byte gives at
/// most one code unit, so `out` needs room for `bytes.len() + 1` and, given
/// that, never reallocates.
pub fn encode(bytes: &[u8], out: &mut Vec<u16>) {
    out.clear();
    for chunk in bytes.utf8_chunks() {
        for unit in chunk.valid().encode_utf16() {
            out.push(match unit {
                0 => NUL,
                0x25 => PERCENT,
                unit => unit,
            });
        }
        if !chunk.invalid().is_empty() {
            out.push(char::REPLACEMENT_CHARACTER as u16);
        }
    }
    out.push(0);
}

/// `s` as nul-terminated UTF-16, for the fields fixed at startup.
pub fn cstr(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len() + 1);
    encode(s.as_bytes(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: &[u8]) -> Vec<u16> {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        let cap = out.capacity();
        encode(bytes, &mut out);
        assert_eq!(out.capacity(), cap, "reallocated for {bytes:?}");
        out
    }

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn utf8_becomes_utf16() {
        for s in ["", "hello\n", "héllo wörld", "€ and 😀", "\r\n\t"] {
            assert_eq!(text(s.as_bytes()), utf16(s), "{s:?}");
        }
    }

    #[test]
    fn nul_is_shown_not_a_terminator() {
        assert_eq!(text(b"a\0b"), utf16("a\u{2400}b"));
    }

    /// The Event Log cannot render most events with a `%` in them, so each
    /// one goes as the fullwidth sign, one unit for one byte as before.
    #[test]
    fn percent_is_written_fullwidth() {
        assert_eq!(
            text(b"100% done, GET /a%20b%%"),
            utf16("100\u{ff05} done, GET /a\u{ff05}20b\u{ff05}\u{ff05}")
        );
        assert_eq!(cstr("%n"), utf16("\u{ff05}n"));
    }

    #[test]
    fn what_is_not_utf8_is_replaced() {
        assert_eq!(text(b"a\xffb"), utf16("a\u{fffd}b"));
        // A code page's "é", then a lone continuation byte.
        assert_eq!(text(b"caf\xe9 \x80"), utf16("caf\u{fffd} \u{fffd}"));
        // An incomplete sequence at the very end is one replacement.
        assert_eq!(text(b"x\xe2\x82"), utf16("x\u{fffd}"));
    }

    #[test]
    fn never_more_units_than_bytes() {
        let worst: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        text(&worst);
        text("😀😀😀".as_bytes());
    }
}
