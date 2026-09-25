//! The instrumentation manifest `wevtutil im` reads, and the access
//! descriptor each channel it creates is given.
//!
//! Channels and providers only: no event templates, no strings, and so no
//! message resource to compile and no DLL to regenerate whenever a user is
//! added. Events are TraceLogging, which carries its own field names and
//! types in every record and which Event Viewer renders without a manifest
//! having described them. What the manifest is still needed for is the part
//! TraceLogging has no say in: a channel exists only because an
//! administrator declared it, and this is the declaration.
//!
//! The manifest is also the record of what has been created. It accumulates:
//! a user who signs out keeps their channel, its `.evtx` and so their
//! history, and [`sids_in`] reads the previous manifest back so that the next
//! one is a superset. That is what makes each import additive, and what
//! leaves the uninstall a single file naming everything to remove.

use crate::{channel_name, is_sid, provider_guid, provider_name, ChannelSize, CHANNEL_VALUE};

const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- Written by `steward provision-eventlog`; edits are lost at the next logon.
     The channels are the sessions signed in then, plus every channel already
     declared here, so a user who signs out keeps theirs. -->
<instrumentationManifest
    xmlns="http://schemas.microsoft.com/win/2004/08/events"
    xmlns:win="http://manifests.microsoft.com/win/2004/08/windows/events"
    xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <instrumentation>
    <events>
"#;

const FOOTER: &str = r#"    </events>
  </instrumentation>
</instrumentationManifest>
"#;

/// The access descriptor for `sid`'s channel: that user, Administrators and
/// SYSTEM, and nobody else.
///
/// Read (0x1) and write (0x2) for the user -- write, because writing an
/// event to a channel is a right the channel grants and the shim runs as
/// them; read, because `stewardctl logs` does. Not clear (0x4): nothing in
/// steward clears a channel, and leaving it out keeps a user's own history
/// from being emptied by anything they run by accident. Administrators get
/// read, write and clear as they do on every built-in channel, and SYSTEM
/// those plus the standard rights, which is what the Application channel
/// grants it.
///
/// Anyone not named is denied by omission, which is the point: another
/// account signed in to the same machine cannot read this one's output. The
/// SID is interpolated unescaped, so it must be [`is_sid`]; a caller building
/// this from anything but `ConvertSidToStringSid` is on its own.
pub fn channel_access(sid: &str) -> String {
    debug_assert!(is_sid(sid), "{sid} is not a SID");
    format!("O:BAG:SYD:(A;;0xf0007;;;SY)(A;;0x7;;;BA)(A;;0x3;;;{sid})")
}

/// The manifest declaring a provider and a channel for each of `sids`, each
/// channel `size` bytes at most.
///
/// `resource_file` is the path the manifest names as each provider's
/// resource and message file. The schema requires both, and no provider or
/// channel here carries a `message` attribute for Windows to resolve, so it
/// names the binary that did the registering rather than a resource DLL that
/// would have to be built and shipped for the sake of a required attribute.
///
/// What that costs is known from importing one of these for real
/// (2026-09-14), and it is worth stating plainly because it is visible to
/// anyone who goes looking at the channel.
///
/// `wevtutil im` prints "Failed to load resource" for a binary with no
/// resource section, and imports it anyway: the channel is created, the
/// provider is registered, events reach it and come back out with their
/// fields intact. But `wevtutil gp` on the provider then fails with "The
/// specified image file did not contain a resource section", and
/// `Get-WinEvent` writes that as a non-terminating error on every call even
/// as it returns the events and their XML. A Rust binary carries no resource
/// section, and only one built with a `WEVT_TEMPLATE` or a message resource
/// would quiet it; `mc.exe` is a Windows SDK tool, and steward cross-builds
/// on Linux.
///
/// Two separate things, often confused. The path must also be readable by
/// the Event Log service itself, which runs as `NT SERVICE\EventLog`: a path
/// under a user's profile gives "Access is denied" instead, which is a
/// different fault with the same symptom. `C:\Program Files\steward`, where
/// steward installs, settles that one and not the other.
///
/// A reader can sidestep the noise entirely, and `stewardctl logs` must. It
/// comes from opening the publisher's metadata to format a message, which
/// `EvtQuery` and `EvtRender` never do: rendering as XML or as values
/// returns the full `EventData`, and the right `RenderingInfo` besides. Only
/// `EvtOpenPublisherMetadata` fails, and nothing here needs it.
///
/// Event Viewer's General tab is fine, checked against the live channel on
/// 2026-09-15. The events are self-describing, so the message is in the
/// record rather than in publisher metadata: `EvtRender(EvtRenderEventXml)`
/// returns a `RenderingInfo Culture='zxx'` whose `Message` is `Output`
/// followed by `unit`, `stream` and `bytes`, and `EvtFormatMessage` with
/// `EvtFormatMessageEvent` returns that same text given a null publisher
/// handle -- which is the call the General tab makes, and which is why
/// `wevtutil qe /f:text` prints the line as the event's description while
/// `wevtutil gp` on the same provider fails. What has no message is the
/// publisher-metadata path itself: `Get-WinEvent` leaves `.Message` at its
/// own "Cannot retrieve event message text." (a literal in
/// `Microsoft.PowerShell.Commands.Diagnostics.dll`, not anything the Event
/// Log said), and `wevtutil qe /f:RenderedXml` appends a second
/// `RenderingInfo Culture='en-US'` reading "The operation completed
/// successfully." Both still return every field. So a compiled message
/// resource would buy formatting for those two consumers and no data, which
/// is not worth an `mc.exe` step in a cross-build.
///
/// Sorted and deduplicated, so that the same set of users gives the same
/// bytes however they were enumerated: that is what lets the caller decide
/// there is nothing to import by comparing what it would write against what
/// is already on disk. A SID that is not [`is_sid`] is dropped.
///
/// The size is in here because an import sets it: on the channels it
/// creates, and on the ones already there, whatever they had been set to
/// since. It is not a reason to import: a manifest that
/// differs from the last only in its size (see [`size_in`]) is written and
/// not imported, and the caller sets the size on each channel as it does
/// the access descriptor.
pub fn manifest(sids: &[String], resource_file: &str, size: ChannelSize) -> String {
    let mut sids: Vec<&str> = sids
        .iter()
        .map(String::as_str)
        .filter(|sid| is_sid(sid))
        .collect();
    sids.sort_unstable();
    sids.dedup();

    let file = escape(resource_file);
    let mut out = String::from(HEADER);
    for sid in sids {
        out.push_str(&format!(
            r#"      <provider name="{provider}"
                guid="{guid}"
                symbol="PROVIDER_{symbol}"
                resourceFileName="{file}"
                messageFileName="{file}">
        <channels>
          <channel name="{channel}"
                   chid="CHANNEL_{symbol}"
                   symbol="CHANNEL_{symbol}"
                   type="Operational"
                   enabled="true"
                   value="{value}"
                   isolation="Custom"
                   access="{access}">
            <logging>
              <autoBackup>false</autoBackup>
              <retention>false</retention>
              <maxSize>{size}</maxSize>
            </logging>
          </channel>
        </channels>
      </provider>
"#,
            provider = provider_name(sid),
            guid = provider_guid(sid),
            symbol = symbol(sid),
            channel = channel_name(sid),
            value = CHANNEL_VALUE,
            access = channel_access(sid),
            size = size.bytes(),
        ));
    }
    out.push_str(FOOTER);
    out
}

/// The SIDs a manifest declares channels for, in the order it declares them.
///
/// Reading the previous manifest back is how a run keeps the channels the
/// runs before it made: the users signed in now are added to these rather
/// than replacing them. A scan for the one attribute that carries a SID,
/// rather than an XML parser, because this crate wrote the file and every
/// SID in it is [`is_sid`]; anything else in there is not one of ours and is
/// ignored.
pub fn sids_in(manifest: &str) -> Vec<String> {
    const PREFIX: &str = "<channel name=\"Steward/";
    manifest
        .match_indices(PREFIX)
        .filter_map(|(at, _)| {
            let rest = &manifest[at + PREFIX.len()..];
            let sid = &rest[..rest.find('"')?];
            is_sid(sid).then(|| sid.to_string())
        })
        .collect()
}

/// The size a manifest gives its channels, if it names one that
/// [`ChannelSize`] would.
///
/// The first channel's, since a manifest [`manifest`] wrote gives every
/// channel the same. What the caller does with it is ask whether the
/// manifest on disk is what it would write now at *that* size -- if so, only
/// the size has changed, which needs no import.
pub fn size_in(manifest: &str) -> Option<ChannelSize> {
    let (_, rest) = manifest.split_once("<maxSize>")?;
    let (bytes, _) = rest.split_once("</maxSize>")?;
    ChannelSize::from_bytes(bytes.parse().ok()?).ok()
}

/// A SID as a C identifier, for the `symbol` and `chid` a manifest gives a
/// provider and a channel: neither may hold a hyphen, and a SID is otherwise
/// `S` and digits.
fn symbol(sid: &str) -> String {
    sid.replace('-', "_")
}

/// The five characters XML reserves. Only the resource file's path can hold
/// one of them -- a SID cannot -- but a path is whatever the binary was
/// installed as.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = "S-1-5-21-2571842103-1957994488-3489912835-1001";
    const TWO: &str = "S-1-5-21-2571842103-1957994488-3489912835-1002";
    const EXE: &str = r"C:\Program Files\steward\steward.exe";

    fn owned(sids: &[&str]) -> Vec<String> {
        sids.iter().map(|s| s.to_string()).collect()
    }

    fn of(sids: &[&str]) -> String {
        manifest(&owned(sids), EXE, ChannelSize::DEFAULT)
    }

    fn sized(sids: &[&str], size: &str) -> String {
        manifest(&owned(sids), EXE, size.parse().unwrap())
    }

    /// What the caller writes to disk and hands `wevtutil im`: one provider
    /// and one channel for the one user, named and numbered as the shim
    /// expects, and no event template anywhere in it.
    #[test]
    fn a_manifest_for_one_user() {
        let text = of(&[ONE]);
        assert!(text.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(text.contains(&format!("name=\"Steward-{ONE}\"")));
        assert!(text.contains(&format!("guid=\"{}\"", provider_guid(ONE))));
        assert!(text.contains(&format!("<channel name=\"Steward/{ONE}\"")));
        assert!(text.contains("value=\"16\""));
        assert!(text.contains("type=\"Operational\""));
        assert!(text.contains("isolation=\"Custom\""));
        assert!(text.contains(&format!("access=\"{}\"", channel_access(ONE))));
        assert!(text.contains("<maxSize>134217728</maxSize>"));
        assert!(text.contains(&format!("resourceFileName=\"{EXE}\"")));
        // TraceLogging: channels only.
        assert!(!text.contains("<template"));
        assert!(!text.contains("<event "));
        assert!(!text.contains("<localization"));
        assert!(text.ends_with("</instrumentationManifest>\n"));
    }

    /// `eventman.xsd` makes `<logging>` a sequence, so its three elements
    /// have one legal order and a manifest in any other is rejected by
    /// `wevtutil im`. Checked here because the schema is not, in CI.
    #[test]
    fn the_logging_elements_are_in_the_schemas_order() {
        let text = of(&[ONE]);
        let at = |tag: &str| text.find(tag).expect(tag);
        assert!(at("<autoBackup>") < at("<retention>"));
        assert!(at("<retention>") < at("<maxSize>"));
    }

    /// Symbols and channel ids are C identifiers, and differ between users:
    /// two providers in one manifest may not share either.
    #[test]
    fn symbols_are_identifiers_and_unique() {
        let text = of(&[ONE, TWO]);
        for sid in [ONE, TWO] {
            let symbol = symbol(sid);
            assert!(!symbol.contains('-'));
            assert!(text.contains(&format!("symbol=\"PROVIDER_{symbol}\"")));
            assert!(text.contains(&format!("chid=\"CHANNEL_{symbol}\"")));
        }
        assert_ne!(symbol(ONE), symbol(TWO));
    }

    /// The same users give the same bytes whatever order they arrived in,
    /// and duplicates collapse. This is the whole of the caller's
    /// idempotence: an unchanged manifest is an import that does not happen.
    #[test]
    fn the_same_users_give_the_same_bytes() {
        let expected = of(&[ONE, TWO]);
        assert_eq!(of(&[TWO, ONE]), expected);
        assert_eq!(of(&[ONE, TWO, ONE, TWO]), expected);
        assert_ne!(of(&[ONE]), expected);
    }

    /// A round trip: the SIDs read back out of a manifest are the SIDs that
    /// went in, which is how the next run keeps the channels this one made.
    #[test]
    fn the_sids_read_back() {
        assert_eq!(sids_in(&of(&[ONE, TWO])), vec![ONE, TWO]);
        assert_eq!(sids_in(&of(&[])), Vec::<String>::new());
        assert_eq!(sids_in(""), Vec::<String>::new());
        assert_eq!(sids_in("nothing of ours"), Vec::<String>::new());
    }

    /// A user who signs out keeps their channel: the next run unions what it
    /// read with the sessions it found, so the manifest only ever grows.
    #[test]
    fn a_manifest_only_grows() {
        let first = of(&[ONE]);
        let mut sids = sids_in(&first);
        sids.push(TWO.to_string());
        let second = manifest(&sids, EXE, ChannelSize::DEFAULT);
        // Additive: everything the first declared, the second still does.
        assert!(second.contains(&format!("<channel name=\"Steward/{ONE}\"")));
        assert!(second.contains(&format!("<channel name=\"Steward/{TWO}\"")));
        // And ONE alone, signed out, is still there the run after.
        assert_eq!(
            sids_in(&manifest(&sids_in(&second), EXE, ChannelSize::DEFAULT)),
            vec![ONE, TWO]
        );
    }

    /// Every channel gets the size asked for, and it is the one thing that
    /// differs between two manifests at two sizes.
    #[test]
    fn every_channel_is_the_size_asked_for() {
        let text = sized(&[ONE, TWO], "256MiB");
        assert_eq!(text.matches("<maxSize>268435456</maxSize>").count(), 2);
        assert_eq!(text.matches("<maxSize>").count(), 2);
        assert_eq!(text.replace("268435456", "134217728"), of(&[ONE, TWO]));
    }

    /// The size reads back out as it went in, and a file that names none,
    /// or one steward would not have written, reads as none.
    #[test]
    fn the_size_reads_back() {
        for size in ["1028KiB", "64MiB", "100000000", "3GiB"] {
            assert_eq!(size_in(&sized(&[ONE], size)), Some(size.parse().unwrap()));
        }
        assert_eq!(size_in(&of(&[])), None);
        assert_eq!(size_in(""), None);
        assert_eq!(size_in("<maxSize>1048576</maxSize>"), None);
        assert_eq!(size_in("<maxSize>lots</maxSize>"), None);
        assert_eq!(size_in("<maxSize>67108864"), None);
    }

    /// The question the caller asks of the file on disk -- is it what I
    /// would write now, at the size it already has? -- answered yes only
    /// when the size is all that changed.
    #[test]
    fn a_change_of_size_alone_is_told_apart() {
        let before = of(&[ONE]);
        let was = size_in(&before).unwrap();
        // Resized: the same users at the old size are the old bytes.
        assert_eq!(manifest(&owned(&[ONE]), EXE, was), before);
        assert_ne!(sized(&[ONE], "256MiB"), before);
        // A new user, or a new path, is a change whatever the size.
        assert_ne!(manifest(&owned(&[ONE, TWO]), EXE, was), before);
        assert_ne!(manifest(&owned(&[ONE]), r"D:\steward.exe", was), before);
    }

    /// Nothing that is not a SID reaches the XML or the SDDL.
    #[test]
    fn rubbish_is_dropped_not_escaped() {
        let text = manifest(
            &[
                "S-1-5-18\"/><x a=\"".to_string(),
                "CALEB".to_string(),
                ONE.to_string(),
            ],
            EXE,
            ChannelSize::DEFAULT,
        );
        assert!(!text.contains("<x "));
        assert!(!text.contains("CALEB"));
        assert_eq!(sids_in(&text), vec![ONE]);
    }

    /// The one field that can hold a reserved character is escaped.
    #[test]
    fn the_resource_path_is_escaped() {
        let text = manifest(
            &[ONE.to_string()],
            r"C:\a & b\steward.exe",
            ChannelSize::DEFAULT,
        );
        assert!(text.contains(r#"resourceFileName="C:\a &amp; b\steward.exe""#));
        assert!(!text.contains("& b"));
    }

    /// The user, Administrators and SYSTEM; not Everyone, not Users, and no
    /// clear right for the user.
    #[test]
    fn the_access_descriptor_names_three() {
        let sddl = channel_access(ONE);
        assert_eq!(
            sddl,
            format!("O:BAG:SYD:(A;;0xf0007;;;SY)(A;;0x7;;;BA)(A;;0x3;;;{ONE})")
        );
        assert!(!sddl.contains(";WD)"));
        assert!(!sddl.contains(";BU)"));
        assert!(!sddl.contains(";IU)"));
        assert_ne!(channel_access(TWO), sddl);
    }
}
