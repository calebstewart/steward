//! How large a channel's `.evtx` may grow before its oldest records are
//! overwritten: [`ChannelSize`].

use std::fmt;
use std::str::FromStr;

/// The size each channel is given, in bytes: the manifest's `<maxSize>`, and
/// what `wevtutil sl /ms:` sets when a channel has drifted from it.
///
/// Only a size Windows keeps as written can be made, which is any at all
/// from 1028 KiB (1,052,672 bytes) up. Below that Windows raises it to 1028
/// KiB -- `1MiB` included, whatever the schema's default of 1,048,576
/// suggests -- and a size it raised would never read back as the one asked
/// for, so every run would find every channel wrong and set it again. From
/// there up it keeps the very byte count it is given, the documentation's
/// talk of rounding to 64 KB notwithstanding. Both read back through
/// `wevtutil sl` and `gl` (2026-09-14).
///
/// Written and read as bytes, or as a whole number of `KiB`, `MiB` or `GiB`
/// (`256MiB`), which is how `steward provision-eventlog --channel-size` takes
/// it and how it is shown back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChannelSize(u64);

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

impl ChannelSize {
    /// The size when the install does not say.
    ///
    /// Windows' own default for a channel a manifest does not size, 1 MiB,
    /// is far too small, so steward always says.
    ///
    /// **It is a budget in lines, not in bytes.** The shim writes one event
    /// per line, and an Event Log record costs 1.2 to 1.5 KB of `.evtx`
    /// however short the line is -- measured on a real channel
    /// (2026-09-14): 1,471 bytes a line at 4,800 lines a second, 1,215 at
    /// 77,000. A longer line costs more, but not in proportion: a line of a
    /// real komorebi log, 7.2 times the length of an 80-byte one, cost
    /// 2,374 bytes against 1,330. Call it 700 to 850 short lines per MiB,
    /// or about 440 of a chatty daemon's, for all of a user's units
    /// together.
    ///
    /// So what the number buys is time, and it is chosen to buy the week
    /// the files held. Two days of one machine's units ran at 327 lines an
    /// hour, komorebi's most of them (#28), and a quieter day of the same
    /// machine's channel held 3,025 records across ten units at 1,756 bytes
    /// each, komorebi's 78% of them (2026-09-15). At the busier rate 128
    /// MiB is about seven days, where 64 MiB was three to four; at the
    /// quieter it is three weeks. Komorebi's own 8 MiB log and its `.log.1`
    /// held about a week.
    ///
    /// Time is what it can buy: the files' *bytes* are out of reach. A
    /// unit's log may reach 8 MiB (`steward::log::CAP`) with another 8 MiB
    /// set aside, and that is *per unit*, where a channel is per user;
    /// carrying four of those through a medium that costs several times the
    /// text would want gigabytes an account. The gap is the medium's, not a
    /// number that can be tuned away.
    ///
    /// The size is a ceiling and not a reservation, which is what makes the
    /// larger number cheap. A channel's `.evtx` grows as it is written and
    /// stops there, as Windows' own do: on the machine above, Kernel-WHEA
    /// is allowed 32 MiB and occupies 4, Windows Defender 16 and occupies
    /// 6, steward's own 64 and occupied 5 after a day. An account that
    /// signs in and runs little costs the 1 MiB floor whatever this says,
    /// and only an account whose history is worth keeping ever spends the
    /// rest.
    ///
    /// Which is still a judgement about a typical machine, and why an
    /// install can say otherwise. The channels are charged to the machine,
    /// one per account that has ever signed in, under
    /// `%SystemRoot%\System32\Winevt\Logs` -- where that machine already
    /// keeps 441 channels and 379 MiB nobody asked for, Application, System
    /// and Security 20 MiB apiece. A machine with many accounts on a small
    /// disk wants less; one with a daemon chattier than komorebi wants
    /// more, or wants that daemon on `StandardOutput=file`, which gives it
    /// a budget of its own again rather than letting it age out the rest.
    pub const DEFAULT: ChannelSize = ChannelSize(128 * MIB);

    /// The least Windows will make a channel.
    pub const LEAST: ChannelSize = ChannelSize(1028 * KIB);

    /// `bytes`, if it is a size Windows keeps as written.
    pub fn from_bytes(bytes: u64) -> Result<ChannelSize, String> {
        if bytes < Self::LEAST.0 {
            Err(format!(
                "{} is less than {}, the least Windows allows a channel",
                ChannelSize(bytes),
                Self::LEAST
            ))
        } else {
            Ok(ChannelSize(bytes))
        }
    }

    pub fn bytes(self) -> u64 {
        self.0
    }
}

impl Default for ChannelSize {
    fn default() -> ChannelSize {
        ChannelSize::DEFAULT
    }
}

impl FromStr for ChannelSize {
    type Err = String;

    fn from_str(text: &str) -> Result<ChannelSize, String> {
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        let (number, unit) = text.split_at(digits);
        let scale = match unit {
            "" => 1,
            "KiB" => KIB,
            "MiB" => MIB,
            "GiB" => GIB,
            _ => 0,
        };
        if number.is_empty() || scale == 0 {
            return Err(format!(
                "`{text}` is not a size: give bytes, or a whole number of KiB, MiB or GiB, as in 256MiB"
            ));
        }
        let bytes = number
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .ok_or_else(|| format!("{text} is too large"))?;
        ChannelSize::from_bytes(bytes)
    }
}

/// In the largest unit that divides it exactly, and so as the flag would be
/// given: `64MiB`, `1028KiB`, `100000000`.
impl fmt::Display for ChannelSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        match [(GIB, "GiB"), (MIB, "MiB"), (KIB, "KiB")]
            .into_iter()
            .find(|(unit, _)| n != 0 && n.is_multiple_of(*unit))
        {
            Some((unit, name)) => write!(f, "{}{name}", n / unit),
            None => write!(f, "{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(text: &str) -> Result<u64, String> {
        text.parse::<ChannelSize>().map(ChannelSize::bytes)
    }

    #[test]
    fn the_default_is_128_mib() {
        assert_eq!(ChannelSize::default().bytes(), 128 << 20);
        assert_eq!(ChannelSize::DEFAULT.to_string(), "128MiB");
    }

    #[test]
    fn bytes_and_the_binary_units() {
        assert_eq!(size("67108864"), Ok(64 << 20));
        assert_eq!(size("65536KiB"), Ok(64 << 20));
        assert_eq!(size("64MiB"), Ok(64 << 20));
        assert_eq!(size("1GiB"), Ok(1 << 30));
        assert_eq!(size("1088KiB"), Ok(1088 << 10));
        assert_eq!(size("2MiB"), Ok(2 << 20));
    }

    /// Only the one spelling of each unit: `MB` is a thousand thousand to
    /// some readers, and a size that is almost what was meant is worse than
    /// one refused.
    #[test]
    fn nothing_else_is_a_size() {
        for text in [
            "",
            "MiB",
            "64 MiB",
            "64mib",
            "64MB",
            "64M",
            "64TiB",
            "-64MiB",
            "+64MiB",
            "64.5MiB",
            " 64MiB",
            "64MiB ",
            "0x4000000",
        ] {
            let e = size(text).expect_err(text);
            assert!(e.contains("is not a size"), "{text}: {e}");
        }
    }

    /// What Windows would raise, refused rather than sent to be raised: 1
    /// MiB comes back from `wevtutil gl` as 1,052,672. Anything from there
    /// up comes back as it went, however unround.
    #[test]
    fn only_sizes_windows_keeps() {
        for text in ["0", "1000000", "1MiB", "1048576", "1027KiB", "1052671"] {
            let e = size(text).expect_err(text);
            assert!(e.contains("less than 1028KiB"), "{text}: {e}");
        }
        assert_eq!(size("1028KiB"), Ok(1_052_672));
        assert_eq!(size("1052672"), Ok(1_052_672));
        assert_eq!(size("1052673"), Ok(1_052_673));
        assert_eq!(size("100000000"), Ok(100_000_000));
        assert_eq!(ChannelSize::LEAST.bytes(), 1_052_672);
    }

    #[test]
    fn too_many_bytes_is_an_error_not_a_wrap() {
        assert!(size("17179869184GiB").unwrap_err().contains("too large"));
        assert!(size("99999999999999999999999")
            .unwrap_err()
            .contains("too large"));
    }

    /// Shown back as it would be given, so that what a run reports can be
    /// pasted into the flag.
    #[test]
    fn it_reads_back_as_it_is_written() {
        for text in [
            "1028KiB",
            "1088KiB",
            "64MiB",
            "1GiB",
            "3GiB",
            "1536MiB",
            "100000000",
        ] {
            assert_eq!(
                size(text).map(|b| ChannelSize(b).to_string()),
                Ok(text.to_string())
            );
        }
        assert_eq!(ChannelSize(1024 << 20).to_string(), "1GiB");
    }
}
