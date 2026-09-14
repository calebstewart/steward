//! Where to cut a line too long for one event, so that a UTF-8 character is
//! not split between two.
//!
//! Written as is, each half of a split character would decode as a
//! replacement character in its own event, so the cut is made before it and
//! the character starts the next piece.

/// The length of the longest prefix of `bytes` that does not end partway
/// through a UTF-8 sequence. Bytes that are not UTF-8 at all are left alone:
/// at most three bytes are ever held back.
pub fn complete_prefix(bytes: &[u8]) -> usize {
    let n = bytes.len();
    for back in 1..=n.min(3) {
        let b = bytes[n - back];
        if b & 0xC0 == 0x80 {
            // A continuation byte: its lead is further back.
            continue;
        }
        let len = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 1,
        };
        return if len > back { n - back } else { n };
    }
    n
}

#[cfg(test)]
mod tests {
    use super::complete_prefix;

    #[test]
    fn whole_characters_are_kept() {
        for s in ["", "a", "hello\n", "é", "€", "😀", "a€b😀"] {
            assert_eq!(complete_prefix(s.as_bytes()), s.len(), "{s:?}");
        }
    }

    #[test]
    fn a_split_character_is_held_back() {
        for s in ["é", "€", "😀"] {
            let whole = format!("ab{s}");
            let whole = whole.as_bytes();
            for cut in 3..whole.len() {
                assert_eq!(complete_prefix(&whole[..cut]), 2, "{s:?} cut at {cut}");
            }
        }
    }

    #[test]
    fn bytes_that_are_not_utf8_are_left_alone() {
        assert_eq!(complete_prefix(&[0xFF]), 1);
        assert_eq!(complete_prefix(&[b'a', 0x80, 0x80, 0x80]), 4);
        assert_eq!(complete_prefix(&[0x80]), 1);
        // Never a lead byte in UTF-8, so nothing to wait for.
        assert_eq!(complete_prefix(&[b'a', 0xC0]), 2);
        // A code page's accented letter that is also a UTF-8 lead byte is
        // held back, since it may be one; the next read or the end of the
        // pipe lets it go.
        assert_eq!(complete_prefix(&[b'a', 0xE9]), 1);
    }
}
