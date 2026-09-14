//! SHA-1, because a provider's GUID is a hash of its name and Windows fixed
//! the hash. Nothing here is a security claim: SHA-1 is the algorithm
//! `TraceLoggingProvider.h` and .NET's `EventSource` use to turn a provider
//! name into a GUID, so it is the algorithm that gets the same answer they
//! do. FIPS 180-4, and no faster than it needs to be for the ~60 bytes of a
//! provider name.

pub struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    /// Bytes of `block` filled.
    filled: usize,
    /// Bytes fed in altogether, for the length the padding carries.
    length: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            block: [0; 64],
            filled: 0,
            length: 0,
        }
    }
}

impl Sha1 {
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.length += bytes.len() as u64;
        while !bytes.is_empty() {
            let take = bytes.len().min(64 - self.filled);
            self.block[self.filled..self.filled + take].copy_from_slice(&bytes[..take]);
            self.filled += take;
            bytes = &bytes[take..];
            if self.filled == 64 {
                self.compress();
                self.filled = 0;
            }
        }
    }

    /// The digest, after the 0x80 byte, the zeroes, and the length in bits.
    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.length * 8;
        self.update(&[0x80]);
        while self.filled != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        debug_assert_eq!(self.filled, 0);

        let mut digest = [0u8; 20];
        let (words, _) = digest.as_chunks_mut::<4>();
        for (word, out) in self.state.iter().zip(words) {
            *out = word.to_be_bytes();
        }
        digest
    }

    fn compress(&mut self) {
        let mut w = [0u32; 80];
        let (blocks, _) = self.block.as_chunks::<4>();
        for (word, bytes) in w.iter_mut().zip(blocks) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (s, v) in self.state.iter_mut().zip([a, b, c, d, e]) {
            *s = s.wrapping_add(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest of `bytes` in one call. Only the tests want one: the
    /// caller in this crate feeds a namespace and a name in turn.
    fn digest(bytes: &[u8]) -> [u8; 20] {
        let mut sha = Sha1::default();
        sha.update(bytes);
        sha.finish()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// FIPS 180-4's own examples, and the empty string.
    #[test]
    fn the_published_vectors() {
        assert_eq!(
            hex(&digest(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex(&digest(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    /// A million 'a's: the vector that catches a broken length or a broken
    /// carry between blocks.
    #[test]
    fn a_million_letters() {
        let mut sha = Sha1::default();
        for _ in 0..1000 {
            sha.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&sha.finish()),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    /// Fed in awkward pieces, the answer is the same: the block buffer
    /// straddles whatever the caller hands it.
    #[test]
    fn the_pieces_do_not_matter() {
        let message: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let whole = digest(&message);
        for piece in [1, 7, 63, 64, 65, 127] {
            let mut sha = Sha1::default();
            for chunk in message.chunks(piece) {
                sha.update(chunk);
            }
            assert_eq!(sha.finish(), whole, "in pieces of {piece}");
        }
    }
}
