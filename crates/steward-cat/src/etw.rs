//! The provider: registered with ETW, told when a session enables it, and
//! writing the two TraceLogging events.
//!
//! The shim registers one for the unit it carries; the manager registers one
//! for itself and writes its lines about every `StandardOutput=eventlog`
//! unit through it, as the [`Stream::Steward`] stream. Both address the same
//! provider GUID, the user's, so the unit's name is a field of each event
//! rather than a property of the registration.

use std::ffi::c_void;
use std::io;
use std::ptr::null;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use steward_eventlog::{
    EVENT_DROPPED, EVENT_OUTPUT, FIELD_BYTES, FIELD_DROPPED, FIELD_STREAM, FIELD_UNIT,
};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Diagnostics::Etw::{
    EventProviderEnabled, EventProviderSetTraits, EventRegister, EventSetInformation,
    EventUnregister, EventWriteTransfer, EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_DATA_DESCRIPTOR,
    EVENT_DATA_DESCRIPTOR_0, EVENT_DATA_DESCRIPTOR_0_0, EVENT_DATA_DESCRIPTOR_TYPE_EVENT_METADATA,
    EVENT_DATA_DESCRIPTOR_TYPE_NONE, EVENT_DATA_DESCRIPTOR_TYPE_PROVIDER_METADATA,
    EVENT_DESCRIPTOR, EVENT_FILTER_DESCRIPTOR, REGHANDLE,
};
use windows_sys::Win32::System::Threading::SetEvent;

use crate::tlg::{self, IN_CSTR16, IN_U64};
use crate::{utf16, Stream};

/// `WINEVENT_LEVEL_WARNING`: output was lost.
const LEVEL_DROPPED: u8 = 3;

/// An event's level, as Event Viewer files it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Level {
    /// `WINEVENT_LEVEL_ERROR`.
    Error = 2,
    /// `WINEVENT_LEVEL_WARNING`.
    Warning = 3,
    /// `WINEVENT_LEVEL_INFO`: a unit's output.
    Info = 4,
}

/// Where the events go, fixed at startup.
pub struct Channel<'a> {
    /// The provider's GUID.
    pub guid: u128,
    /// The provider's name, as the manifest gives it.
    pub name: &'a str,
    /// The channel's `value` in the manifest; the Event Log service routes
    /// an event by it.
    pub channel: u8,
    /// The events' keyword.
    pub keyword: u64,
}

/// A registered provider, and every blob its events point at.
pub struct Provider {
    handle: REGHANDLE,
    channel: u8,
    keyword: u64,
    traits: Vec<u8>,
    output: Vec<u8>,
    dropped: Vec<u8>,
    /// "stdout", "stderr" and "steward", nul-terminated UTF-16, by
    /// `Stream as usize`.
    streams: [Vec<u16>; 3],
    /// Writes ETW refused, and the last error it gave.
    refused: AtomicU64,
    last_error: AtomicU32,
}

impl Provider {
    /// Registers the provider. `wake`, if given, is set each time a session
    /// enables it, including from inside this call if one already has.
    pub fn register(at: &Channel, wake: Option<HANDLE>) -> io::Result<Provider> {
        let guid = GUID::from_u128(at.guid);
        let mut handle: REGHANDLE = 0;
        let wake = wake.unwrap_or(std::ptr::null_mut());
        // SAFETY: `on_enable` only uses its context as an event handle to
        // set, which the caller keeps open until the provider is dropped.
        let err = unsafe { EventRegister(&guid, Some(on_enable), wake.cast_const(), &mut handle) };
        if err != 0 {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        let provider = Provider {
            handle,
            channel: at.channel,
            keyword: at.keyword,
            traits: tlg::provider_traits(at.name),
            output: tlg::event_metadata(
                EVENT_OUTPUT,
                &[
                    (FIELD_UNIT, IN_CSTR16, 0),
                    (FIELD_STREAM, IN_CSTR16, 0),
                    (FIELD_BYTES, IN_CSTR16, 0),
                ],
            ),
            dropped: tlg::event_metadata(
                EVENT_DROPPED,
                &[
                    (FIELD_UNIT, IN_CSTR16, 0),
                    (FIELD_STREAM, IN_CSTR16, 0),
                    (FIELD_DROPPED, IN_U64, 0),
                ],
            ),
            streams: [
                utf16::cstr(Stream::Stdout.name()),
                utf16::cstr(Stream::Stderr.name()),
                utf16::cstr(Stream::Steward.name()),
            ],
            refused: AtomicU64::new(0),
            last_error: AtomicU32::new(0),
        };
        // Tells ETW the provider's name. Failing only loses that, not events.
        unsafe {
            EventSetInformation(
                handle,
                EventProviderSetTraits,
                provider.traits.as_ptr().cast(),
                provider.traits.len() as u32,
            )
        };
        Ok(provider)
    }

    /// Whether a session is listening for the events: otherwise they would be
    /// discarded, with `EventWrite` still returning success.
    pub fn listening(&self) -> bool {
        unsafe { EventProviderEnabled(self.handle, Level::Info as u8, self.keyword) }
    }

    /// Writes one line of a stream of `unit`, both as nul-terminated UTF-16
    /// ([`utf16::cstr`], [`utf16::encode`]): fields `unit`, `stream`,
    /// `bytes`. Returns whether ETW took it.
    pub fn output(&self, unit: &[u16], stream: Stream, text: &[u16]) -> bool {
        self.output_at(Level::Info, unit, stream, text)
    }

    /// [`output`](Self::output) at a level of the caller's: the manager's
    /// line saying a unit failed is an error, not information.
    pub fn output_at(&self, level: Level, unit: &[u16], stream: Stream, text: &[u16]) -> bool {
        self.write(
            level as u8,
            &self.output,
            &[
                descriptor(unit),
                descriptor(&self.streams[stream as usize]),
                descriptor(text),
            ],
        )
    }

    /// Writes how many bytes of a stream of `unit` were lost: fields `unit`,
    /// `stream`, `dropped`. Returns whether ETW took it.
    pub fn dropped(&self, unit: &[u16], stream: Stream, bytes: u64) -> bool {
        self.write(
            LEVEL_DROPPED,
            &self.dropped,
            &[
                descriptor(unit),
                descriptor(&self.streams[stream as usize]),
                descriptor(&[bytes]),
            ],
        )
    }

    /// Writes an event: the provider traits and the event's metadata, then
    /// its three fields, all pointing at memory that already exists.
    ///
    /// Neither ETW nor the Event Log says when it loses an event: when the
    /// session runs out of buffers ETW counts it lost and the write still
    /// succeeds, and the service drops a record too big for a chunk of the
    /// `.evtx` after the write has returned (see [`crate::LINE_MAX`]). So
    /// the only refusals seen here are for good, such as an event too big
    /// for ETW itself.
    fn write(&self, level: u8, meta: &[u8], fields: &[EVENT_DATA_DESCRIPTOR; 3]) -> bool {
        let descriptors = [
            raw_descriptor(&self.traits, EVENT_DATA_DESCRIPTOR_TYPE_PROVIDER_METADATA),
            raw_descriptor(meta, EVENT_DATA_DESCRIPTOR_TYPE_EVENT_METADATA),
            fields[0],
            fields[1],
            fields[2],
        ];
        let event = EVENT_DESCRIPTOR {
            Channel: self.channel,
            Level: level,
            Keyword: self.keyword,
            ..Default::default()
        };
        // SAFETY: every descriptor points at a live slice of its size.
        let err = unsafe {
            EventWriteTransfer(
                self.handle,
                &event,
                null(),
                null(),
                descriptors.len() as u32,
                descriptors.as_ptr(),
            )
        };
        if err != 0 {
            self.refused.fetch_add(1, Relaxed);
            self.last_error.store(err, Relaxed);
        }
        err == 0
    }

    /// How many writes ETW refused, and the last error it gave.
    pub fn refused(&self) -> (u64, u32) {
        (self.refused.load(Relaxed), self.last_error.load(Relaxed))
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        unsafe { EventUnregister(self.handle) };
    }
}

/// A field's data: any slice of plain values, as its bytes.
fn descriptor<T: Copy>(data: &[T]) -> EVENT_DATA_DESCRIPTOR {
    let bytes = std::mem::size_of_val(data);
    // SAFETY: `T` is plain data (`u16` or `u64` here), so its bytes are too.
    let data = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), bytes) };
    raw_descriptor(data, EVENT_DATA_DESCRIPTOR_TYPE_NONE)
}

fn raw_descriptor(data: &[u8], kind: u32) -> EVENT_DATA_DESCRIPTOR {
    EVENT_DATA_DESCRIPTOR {
        Ptr: data.as_ptr() as u64,
        Size: data.len() as u32,
        Anonymous: EVENT_DATA_DESCRIPTOR_0 {
            Anonymous: EVENT_DATA_DESCRIPTOR_0_0 {
                Type: kind as u8,
                Reserved1: 0,
                Reserved2: 0,
            },
        },
    }
}

/// The enable callback: ETW calls it when a session enables or disables the
/// provider. It only wakes main, which writes what is held; writing from
/// here would hold up ETW's notification thread.
unsafe extern "system" fn on_enable(
    _source: *const GUID,
    state: u32,
    _level: u8,
    _any: u64,
    _all: u64,
    _filter: *const EVENT_FILTER_DESCRIPTOR,
    wake: *mut c_void,
) {
    if state == EVENT_CONTROL_CODE_ENABLE_PROVIDER && !wake.is_null() {
        unsafe { SetEvent(wake as HANDLE) };
    }
}
