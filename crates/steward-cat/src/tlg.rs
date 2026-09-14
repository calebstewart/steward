//! TraceLogging's encoding, for the two events steward-cat writes.
//!
//! A TraceLogging event describes itself: beside its data it carries the
//! provider's name (the provider traits) and the event's name and field
//! types (the event metadata), as two extra data descriptors ETW recognises
//! by type, so no manifest templates or message DLL are needed to decode
//! it. The format is the one `TraceLoggingProvider.h` and Microsoft's
//! `tracelogging` crates write; that crate is not used because it links
//! `OneCore_apiset` by that exact name, which the mingw cross build cannot
//! find (mingw ships `libonecore_apiset.a`).
//!
//! Every blob here is built once, at startup, so writing an event only points
//! descriptors at memory that already exists.

/// `TlgInUNICODESTRING`: nul-terminated UTF-16.
pub const IN_CSTR16: u8 = 1;
/// `TlgInUINT64`.
pub const IN_U64: u8 = 10;

/// Set on an in-type when an out-type follows it.
const CHAIN: u8 = 0x80;

/// The provider traits: their size as a `u16`, then the provider's name,
/// nul-terminated. Given to `EventSetInformation` and to every event.
pub fn provider_traits(name: &str) -> Vec<u8> {
    assert!(!name.contains('\0'));
    let mut traits = vec![0, 0];
    traits.extend_from_slice(name.as_bytes());
    traits.push(0);
    set_size(&mut traits);
    traits
}

/// An event's metadata: its size as a `u16`, no tags, its name, then each
/// field's name, in-type and, if not 0, out-type.
pub fn event_metadata(name: &str, fields: &[(&str, u8, u8)]) -> Vec<u8> {
    for s in std::iter::once(name).chain(fields.iter().map(|f| f.0)) {
        assert!(!s.contains('\0'));
    }
    // Size, then a single tag byte of zero.
    let mut meta = vec![0, 0, 0];
    meta.extend_from_slice(name.as_bytes());
    meta.push(0);
    for &(field, in_type, out_type) in fields {
        meta.extend_from_slice(field.as_bytes());
        meta.push(0);
        if out_type == 0 {
            meta.push(in_type);
        } else {
            meta.push(in_type | CHAIN);
            meta.push(out_type);
        }
    }
    set_size(&mut meta);
    meta
}

fn set_size(blob: &mut [u8]) {
    let size = u16::try_from(blob.len()).expect("TraceLogging metadata is at most 64 KiB");
    blob[..2].copy_from_slice(&size.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_traits_are_size_and_name() {
        assert_eq!(provider_traits("Ab"), [5, 0, b'A', b'b', 0]);
    }

    #[test]
    fn event_metadata_matches_tracelogging() {
        // What tracelogging_dynamic's EventBuilder produces for
        // reset("Ev", ..., 0), add_cstr16("s", _, Default, 0) and
        // add_u64("n", _, Hex, 0).
        let meta = event_metadata("Ev", &[("s", IN_CSTR16, 0), ("n", IN_U64, 4)]);
        let expected: &[&[u8]] = &[
            &[13, 0], // size
            &[0],     // tags
            b"Ev\0",  // event name
            b"s\0",   // nul-terminated UTF-16, no out-type
            &[1],
            b"n\0", // a u64, shown as hex
            &[10 | 0x80, 4],
        ];
        assert_eq!(meta, expected.concat());
    }
}
