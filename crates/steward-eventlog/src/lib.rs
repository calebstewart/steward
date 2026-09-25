//! What steward's per-user Event Log channels are called, and the GUIDs that
//! go with them.
//!
//! One provider and one channel per user, both named after the user's SID:
//! the channel `Steward/<SID>`, written by that user's units and readable by
//! them, by Administrators and by SYSTEM, and nobody else. A provider may
//! declare at most eight channels, which is why there is a provider per user
//! rather than one for the machine -- and it is what lets the GUID be a
//! function of the SID.
//!
//! Two programs have to agree on every name and number here without being
//! able to ask each other:
//!
//! - the provisioning task ([`manifest`], `steward provision-eventlog`)
//!   writes the manifest that registers the provider and creates the
//!   channel, as SYSTEM, at logon, at the [`ChannelSize`] the install chose;
//! - the per-unit shim writes events to it, as the user, and must stamp each
//!   one with [`CHANNEL_VALUE`] and address it to the same provider GUID.
//!
//! So they share this crate. It is platform-free -- strings and a hash -- so
//! its tests run wherever the other platform-free crates' do.

mod manifest;
mod sha1;
mod size;

pub use manifest::{channel_access, manifest, sids_in, size_in};
pub use size::ChannelSize;

/// The value a channel is given in the manifest, and so the value an event's
/// descriptor must carry in its `Channel` field for the Event Log to route
/// it there.
///
/// It matters to the shim. TraceLogging events default to channel 11, which
/// is TraceLogging's own and which no manifest channel claims, so an event
/// left at the default is written to ETW and routed nowhere: collected by a
/// trace session if one is running, and otherwise dropped, with the write
/// still returning success. 0 to 15 are reserved (8, 9 and 10 are System,
/// Application and Security), so a manifest's own channels are numbered from
/// 16, which is what `mc.exe` assigns and what this declares. Each provider
/// here has exactly one channel, so every one of them is 16.
pub const CHANNEL_VALUE: u8 = 16;

/// Events carry no keyword. An event whose keyword is zero passes any ETW
/// session's `MatchAnyKeyword` filter, the Event Log's own session for the
/// channel included, so the shim needs no keyword to be collected. Named so
/// that the shim does not have to rediscover that.
pub const CHANNEL_KEYWORD: u64 = 0;

/// The events in a channel, as the shim writes them and `stewardctl logs`
/// reads them back. Named here so that the writer and the reader cannot
/// drift apart: the reader filters on [`FIELD_UNIT`] with an XPath and
/// tells the streams apart by [`FIELD_STREAM`]'s value.
///
/// [`EVENT_OUTPUT`] is one line of a unit's output, without its line end:
/// [`FIELD_UNIT`], [`FIELD_STREAM`] and [`FIELD_BYTES`], all nul-terminated
/// UTF-16 strings. [`FIELD_STREAM`] is [`STREAM_STDOUT`] or [`STREAM_STDERR`]
/// from the shim, and [`STREAM_STEWARD`] for a line the manager writes
/// about the unit -- started, exited, restarting -- which `stewardctl logs`
/// shows as `-- <time> steward: <line>`, as it shows the manager's lines in
/// a file. [`EVENT_DROPPED`] says the shim lost output while nobody was
/// listening: [`FIELD_UNIT`], [`FIELD_STREAM`] and [`FIELD_DROPPED`], the
/// bytes of line text lost, as a `u64`.
pub const EVENT_OUTPUT: &str = "Output";
/// See [`EVENT_OUTPUT`].
pub const EVENT_DROPPED: &str = "Dropped";
/// See [`EVENT_OUTPUT`].
pub const FIELD_UNIT: &str = "unit";
/// See [`EVENT_OUTPUT`].
pub const FIELD_STREAM: &str = "stream";
/// See [`EVENT_OUTPUT`].
pub const FIELD_BYTES: &str = "bytes";
/// See [`EVENT_OUTPUT`].
pub const FIELD_DROPPED: &str = "dropped";
/// See [`EVENT_OUTPUT`].
pub const STREAM_STDOUT: &str = "stdout";
/// See [`EVENT_OUTPUT`].
pub const STREAM_STDERR: &str = "stderr";
/// See [`EVENT_OUTPUT`].
pub const STREAM_STEWARD: &str = "steward";

/// What a `%` in a line becomes in the channel: U+FF05 FULLWIDTH PERCENT
/// SIGN, which `stewardctl logs` turns back into `%`.
///
/// The Event Log reads a `%` in a TraceLogging string as the start of an
/// insertion when it renders the event. `%%`, `%1` to `%99` and a `%` at
/// the very end pass; a `%` followed by anything else -- `100% done`, the
/// `%20` of a URL, `%s`, `%n`, `%100` -- makes the whole event render with
/// every field empty, through `EvtRender`, `Get-WinEvent` and Event
/// Viewer alike, although the `.evtx` holds the text intact (seen,
/// 2026-09-14, #28). Doubling it is no escape: `%%` stays `%%` in the
/// event's values and XML, and the message Event Viewer's General tab
/// shows comes back empty for any line with a `%` in it, doubled or not.
/// The fullwidth sign renders on every path, reads as a percent sign in
/// Event Viewer, and is one UTF-16 unit, so a line's length is unchanged.
/// A fullwidth sign the program itself wrote comes back from `stewardctl` as
/// `%` too; that is the one thing this costs.
pub const PERCENT_STAND_IN: char = '\u{FF05}';

/// The provider that owns `sid`'s channel: `Steward-<SID>`.
///
/// A name rather than only a GUID because Windows' own tools take a provider
/// by name -- `logman`, `tracelog` and `wpr` accept `*Steward-S-1-5-...` and
/// derive the GUID from it exactly as [`Guid::from_provider_name`] does --
/// which is how a unit's output can be watched live before the channel it
/// belongs to has been created at all.
pub fn provider_name(sid: &str) -> String {
    format!("Steward-{sid}")
}

/// The channel `sid`'s units write to: `Steward/<SID>`.
pub fn channel_name(sid: &str) -> String {
    format!("Steward/{sid}")
}

/// Where Windows keeps `sid`'s channel's configuration, under
/// `HKEY_LOCAL_MACHINE`: the key exists exactly while the channel is
/// registered -- an import creates it, `wevtutil um` removes it -- and any
/// user may read it. So it is how a program that is not an administrator,
/// the manager or `stewardctl`, asks whether the channel is there, without the
/// Event Log API and without opening the channel.
pub fn channel_key(sid: &str) -> String {
    format!(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\WINEVT\Channels\{}",
        channel_name(sid)
    )
}

/// The Scheduled Task that runs `steward provision-eventlog` as SYSTEM at
/// every logon, as the install declares it: `windows.scheduledTasks` in
/// `nix/winpkgs/system.nix`, or the README's by-hand steps. Its descriptor
/// lets any signed-in user run it, and the manager does, when it finds its
/// user's channel missing, rather than wait for the logon trigger; so the
/// name is shared between whatever declares the task and the manager that
/// runs it.
pub const PROVISION_TASK: &str = "steward-provision-eventlog";

/// The provider GUID for `sid`, derived from [`provider_name`].
pub fn provider_guid(sid: &str) -> Guid {
    Guid::from_provider_name(&provider_name(sid))
}

/// Whether `sid` is a SID in the form this crate will put in a manifest:
/// `S-1-`, then sub-authorities, digits and hyphens throughout.
///
/// Everything here interpolates a SID into XML and into SDDL without
/// escaping either, so this is what makes that safe rather than lucky. The
/// SIDs come from `ConvertSidToStringSid`, which produces nothing else; a
/// caller that got one from somewhere else is refused rather than trusted.
pub fn is_sid(sid: &str) -> bool {
    let Some(rest) = sid.strip_prefix("S-1-") else {
        return false;
    };
    !rest.is_empty()
        && rest.split('-').all(|part| {
            !part.is_empty() && part.len() <= 20 && part.bytes().all(|b| b.is_ascii_digit())
        })
}

/// A Windows GUID, in Windows' own field order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    /// The GUID Windows derives from an ETW provider's name: a version-5
    /// UUID of the name over the namespace ETW reserved for exactly this,
    /// `{482C2DB2-C390-47C8-87F8-1A15BFC130FB}`.
    ///
    /// This is `TraceLoggingProvider.h`'s `*Name` form and .NET's
    /// `EventSource.GenerateGuidFromName`, to the letter: the name upper
    /// cased, encoded as big-endian UTF-16, hashed after the namespace's
    /// sixteen bytes, and the first sixteen bytes of the digest read as a
    /// GUID with the version nibble forced to 5. Note what it does *not* do
    /// -- it leaves RFC 4122's variant bits alone, so the result is not
    /// quite a conforming v5 UUID. Matching Windows matters more than
    /// matching the RFC: because this is the derivation Windows uses,
    /// `logman -p "*Steward-S-1-5-..."` finds the provider by name with
    /// nothing registered at all.
    pub fn from_provider_name(name: &str) -> Guid {
        const NAMESPACE: [u8; 16] = [
            0x48, 0x2C, 0x2D, 0xB2, 0xC3, 0x90, 0x47, 0xC8, 0x87, 0xF8, 0x1A, 0x15, 0xBF, 0xC1,
            0x30, 0xFB,
        ];
        let mut sha = sha1::Sha1::default();
        sha.update(&NAMESPACE);
        // Upper cased as `ToUpperInvariant` would, which for a provider name
        // -- ASCII, here a SID -- is ASCII casing, and which unlike
        // `to_uppercase` cannot change the name's length.
        for unit in name.to_ascii_uppercase().encode_utf16() {
            sha.update(&unit.to_be_bytes());
        }
        let digest = sha.finish();

        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[7] = (bytes[7] & 0x0F) | 0x50;
        Guid {
            data1: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            data2: u16::from_le_bytes([bytes[4], bytes[5]]),
            data3: u16::from_le_bytes([bytes[6], bytes[7]]),
            data4: [
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ],
        }
    }

    /// The GUID as one number, its bytes in the order they are written:
    /// `Uuid::as_u128`'s order, for a caller that would rather hold an
    /// integer than four fields.
    pub fn to_u128(self) -> u128 {
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&self.data1.to_be_bytes());
        bytes[4..6].copy_from_slice(&self.data2.to_be_bytes());
        bytes[6..8].copy_from_slice(&self.data3.to_be_bytes());
        bytes[8..].copy_from_slice(&self.data4);
        u128::from_be_bytes(bytes)
    }
}

/// `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`, braces and all, which is how a
/// manifest and every Windows tool write a GUID.
impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{{:08x}-{:04x}-{:04x}-{:02x}{:02x}-",
            self.data1, self.data2, self.data3, self.data4[0], self.data4[1]
        )?;
        for byte in &self.data4[2..] {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Microsoft's own implementation's answers. `System.Runtime` is the
    /// .NET runtime counters' `EventSource`, whose published GUID is
    /// `{49592c0f-5a05-516d-aa4b-a64e02026c89}` precisely because it is
    /// derived from the name; the rest were read off
    /// `[System.Diagnostics.Tracing.EventSource]::new(name).Guid` on Windows
    /// (2026-09-14), which is `GenerateGuidFromName` itself. While these
    /// hold, a Windows tool handed `*Steward-<SID>` reaches the provider
    /// this crate names.
    ///
    /// Note the shape of the answers: the third group begins with `5`, the
    /// version nibble, and the fourth does not begin with `8`, `9`, `a` or
    /// `b` -- the variant bits an RFC 4122 v5 UUID would have and this
    /// deliberately does not.
    #[test]
    fn the_derivation_is_windows_own() {
        for (name, guid) in [
            ("System.Runtime", "{49592c0f-5a05-516d-aa4b-a64e02026c89}"),
            ("abc", "{d6a32a83-a4b5-5669-8026-1705883ec518}"),
            (
                "Microsoft-Windows-DotNETRuntime",
                "{5e5bb766-bbfc-5662-0548-1d44fad9bb56}",
            ),
        ] {
            assert_eq!(
                Guid::from_provider_name(name).to_string(),
                guid,
                "for {name}"
            );
        }
    }

    /// And the names this crate actually makes, against the same authority.
    #[test]
    fn a_users_provider_is_the_guid_windows_derives() {
        assert_eq!(
            provider_guid("S-1-5-21-2571842103-1957994488-3489912835-1001").to_string(),
            "{bc89313e-1c9a-57d5-23e4-4897be4c66e4}"
        );
        assert_eq!(
            provider_guid("S-1-5-21-2571842103-1957994488-3489912835-1002").to_string(),
            "{8a2c379e-89ef-5634-3ade-bf56b7076699}"
        );
    }

    /// The name is upper cased before it is hashed, so the case a caller
    /// writes cannot change the provider it reaches.
    #[test]
    fn case_does_not_change_the_guid() {
        let guid = Guid::from_provider_name("Microsoft-Windows-DotNETRuntime");
        assert_eq!(
            Guid::from_provider_name("microsoft-windows-dotnetruntime"),
            guid
        );
        assert_eq!(
            Guid::from_provider_name("MICROSOFT-WINDOWS-DOTNETRUNTIME"),
            guid
        );
    }

    /// Two users get two providers, and a user gets the same one every time.
    #[test]
    fn a_provider_per_user() {
        let one = "S-1-5-21-2571842103-1957994488-3489912835-1001";
        let two = "S-1-5-21-2571842103-1957994488-3489912835-1002";
        assert_eq!(provider_name(one), format!("Steward-{one}"));
        assert_eq!(channel_name(one), format!("Steward/{one}"));
        assert_eq!(provider_guid(one), provider_guid(one));
        assert_ne!(provider_guid(one), provider_guid(two));
    }

    #[test]
    fn a_channel_is_found_where_windows_keeps_it() {
        let sid = "S-1-5-21-2571842103-1957994488-3489912835-1001";
        assert_eq!(
            channel_key(sid),
            format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\WINEVT\Channels\Steward/{sid}")
        );
    }

    /// `to_u128` reads the bytes in the order `Display` writes them.
    #[test]
    fn the_number_matches_the_text() {
        let guid = Guid::from_provider_name("Microsoft-Windows-DotNETRuntime");
        assert_eq!(guid.to_u128(), 0x5e5bb766_bbfc_5662_0548_1d44fad9bb56);
    }

    #[test]
    fn sids_and_things_that_are_not() {
        assert!(is_sid("S-1-5-18"));
        assert!(is_sid("S-1-5-21-2571842103-1957994488-3489912835-1001"));
        assert!(!is_sid("S-1-5-"));
        assert!(!is_sid("S-1-"));
        assert!(!is_sid("S-1-5-21-x"));
        assert!(!is_sid("s-1-5-18"));
        assert!(!is_sid(""));
        assert!(!is_sid("CALEB"));
        // What a manifest and an SDDL with no escaping depend on refusing.
        assert!(!is_sid("S-1-5-18\"/><script>"));
        assert!(!is_sid("S-1-5-18)(A;;0x7;;;WD"));
    }
}
