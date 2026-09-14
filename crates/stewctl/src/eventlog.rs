//! A unit's output read back out of the user's Event Log channel: what
//! `logs` and `status` do for a unit whose `StandardOutput=eventlog`.
//!
//! The channel is `Steward/<SID>` for the SID this process runs as, so no
//! manager is asked anything. The events are the shim's ([`steward_cat`] is
//! the writer; `steward_eventlog` names the fields): one per line, filtered
//! to the unit with an XPath on its `unit` field, read newest first for a
//! tail and subscribed to from the end for `-f`.
//!
//! Everything is `EvtQuery`, `EvtSubscribe` and `EvtRender` -- never
//! `EvtFormatMessage` or `EvtOpenPublisherMetadata`. The provider steward
//! registers has no message resources (see `steward_eventlog::manifest`),
//! and those two fail on every event, loudly; rendering the event's values
//! never touches the publisher, and comes back with the fields intact.

use std::io;
use std::ptr::null;

use steward_eventlog::{
    channel_name, FIELD_BYTES, FIELD_DROPPED, FIELD_STREAM, FIELD_UNIT, STREAM_STEWARD,
};
use windows_sys::core::PCWSTR;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_EVT_CHANNEL_NOT_FOUND,
    ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, FILETIME, HANDLE, SYSTEMTIME,
};
use windows_sys::Win32::System::EventLog::{
    EvtClose, EvtCreateBookmark, EvtCreateRenderContext, EvtNext, EvtQuery, EvtQueryChannelPath,
    EvtQueryReverseDirection, EvtRender, EvtRenderContextValues, EvtRenderEventValues,
    EvtSubscribe, EvtSubscribeStartAfterBookmark, EvtSubscribeToFutureEvents, EvtUpdateBookmark,
    EvtVarTypeFileTime, EvtVarTypeString, EvtVarTypeUInt64, EVT_HANDLE, EVT_VARIANT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

/// How many events are asked for at a time.
const BATCH: usize = 64;

/// What went wrong reading the channel, said in the caller's terms.
pub fn describe(channel: &str, e: &io::Error) -> String {
    match e.raw_os_error().map(|code| code as u32) {
        Some(ERROR_EVT_CHANNEL_NOT_FOUND) => format!(
            "no channel {channel} on this machine yet: the steward-provision-eventlog task creates \
             it at sign-in (see the README's install section)"
        ),
        Some(ERROR_ACCESS_DENIED) => format!("cannot read {channel}: access is denied"),
        _ => format!("cannot read {channel}: {e}"),
    }
}

/// A handle from `wevtapi`, closed when dropped.
struct Evt(EVT_HANDLE);

impl Evt {
    /// Wraps a handle a call just returned, or gives that call's error.
    fn from_call(handle: EVT_HANDLE) -> io::Result<Evt> {
        if handle == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Evt(handle))
    }
}

impl Drop for Evt {
    fn drop(&mut self) {
        unsafe { EvtClose(self.0) };
    }
}

/// An event handle, if any, marking where a tail ended and a follow begins.
pub struct Bookmark(Evt);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `text` as an XPath string literal. XPath 1.0 has no escaping: a literal
/// is delimited by whichever quote it does not contain, and a text holding
/// both cannot be written. A unit's name is a file name and holds neither,
/// or almost never; the last is refused rather than guessed at.
pub fn xpath_literal(text: &str) -> Option<String> {
    match (text.contains('\''), text.contains('"')) {
        (false, _) => Some(format!("'{text}'")),
        (true, false) => Some(format!("\"{text}\"")),
        (true, true) => None,
    }
}

/// The events that are `unit`'s: those whose `unit` field says so.
fn query_for(unit: &str) -> Option<String> {
    Some(format!(
        "*[EventData[Data[@Name='{FIELD_UNIT}']={}]]",
        xpath_literal(unit)?
    ))
}

/// One unit's events in the caller's channel.
pub struct Reader {
    channel: String,
    path: Vec<u16>,
    query: Vec<u16>,
    /// Renders an event's time, stream, text and lost byte count, in that
    /// order, as values.
    context: Evt,
}

impl Reader {
    /// For `unit`'s events in `sid`'s channel.
    pub fn new(sid: &str, unit: &str) -> Result<Reader, String> {
        let channel = channel_name(sid);
        let query = query_for(unit)
            .ok_or_else(|| format!("{unit}: a unit name cannot hold both kinds of quote"))?;
        let paths: Vec<Vec<u16>> = [
            "Event/System/TimeCreated/@SystemTime".to_string(),
            format!("Event/EventData/Data[@Name='{FIELD_STREAM}']"),
            format!("Event/EventData/Data[@Name='{FIELD_BYTES}']"),
            format!("Event/EventData/Data[@Name='{FIELD_DROPPED}']"),
        ]
        .iter()
        .map(|p| wide(p))
        .collect();
        let pointers: Vec<PCWSTR> = paths.iter().map(|p| p.as_ptr()).collect();
        let context = Evt::from_call(unsafe {
            EvtCreateRenderContext(
                pointers.len() as u32,
                pointers.as_ptr(),
                EvtRenderContextValues,
            )
        })
        .map_err(|e| describe(&channel, &e))?;
        Ok(Reader {
            path: wide(&channel),
            query: wide(&query),
            channel,
            context,
        })
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// The last `n` lines of the unit's log, oldest first, and a bookmark on
    /// the newest event if there was one: what a follow continues after.
    ///
    /// Read newest first (`EvtQueryReverseDirection`), so that a tail costs
    /// what it prints and not the whole of the channel.
    pub fn tail(&self, n: usize) -> io::Result<(Vec<String>, Option<Bookmark>)> {
        let query = Evt::from_call(unsafe {
            EvtQuery(
                0,
                self.path.as_ptr(),
                self.query.as_ptr(),
                EvtQueryChannelPath | EvtQueryReverseDirection,
            )
        })?;
        let mut lines = Vec::new();
        let mut newest = None;
        if n == 0 {
            return Ok((lines, newest));
        }
        let mut buffer = Vec::new();
        'batches: loop {
            let mut events = [0 as EVT_HANDLE; BATCH];
            let mut returned = 0u32;
            let ok = unsafe {
                EvtNext(
                    query.0,
                    events.len() as u32,
                    events.as_mut_ptr(),
                    INFINITE,
                    0,
                    &mut returned,
                )
            };
            if ok == 0 {
                return match unsafe { GetLastError() } {
                    ERROR_NO_MORE_ITEMS => Ok((finish(lines), newest)),
                    code => Err(io::Error::from_raw_os_error(code as i32)),
                };
            }
            let events: Vec<Evt> = events[..returned as usize]
                .iter()
                .map(|&h| Evt(h))
                .collect();
            for event in &events {
                if newest.is_none() {
                    let bookmark = Evt::from_call(unsafe { EvtCreateBookmark(null()) })?;
                    if unsafe { EvtUpdateBookmark(bookmark.0, event.0) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    newest = Some(Bookmark(bookmark));
                }
                if let Some(line) = self.render(event, &mut buffer)? {
                    lines.push(line);
                    if lines.len() == n {
                        break 'batches;
                    }
                }
            }
        }
        Ok((finish(lines), newest))
    }

    /// Prints every event of the unit's from `after` -- or from now, with
    /// no bookmark -- as it arrives, and does not return.
    ///
    /// A pull subscription: the service sets `signal` when there is
    /// something to read, and it is read here, in order, on this thread. A
    /// callback would run on the service's thread, where a stalled stdout
    /// (a pager, say) would hold the subscription up too.
    pub fn follow(
        &self,
        after: Option<Bookmark>,
        mut print: impl FnMut(&str),
    ) -> io::Result<std::convert::Infallible> {
        // Manual reset, set at the start: the first wait asks at once.
        let signal = unsafe { CreateEventW(null(), 1, 1, null()) };
        if signal.is_null() {
            return Err(io::Error::last_os_error());
        }
        let signal = Signal(signal);
        let (bookmark, flags) = match &after {
            Some(Bookmark(bookmark)) => (bookmark.0, EvtSubscribeStartAfterBookmark),
            None => (0, EvtSubscribeToFutureEvents),
        };
        let subscription = Evt::from_call(unsafe {
            EvtSubscribe(
                0,
                signal.0,
                self.path.as_ptr(),
                self.query.as_ptr(),
                bookmark,
                null(),
                None,
                flags,
            )
        })?;
        let mut buffer = Vec::new();
        loop {
            unsafe { WaitForSingleObject(signal.0, INFINITE) };
            loop {
                let mut events = [0 as EVT_HANDLE; BATCH];
                let mut returned = 0u32;
                let ok = unsafe {
                    EvtNext(
                        subscription.0,
                        events.len() as u32,
                        events.as_mut_ptr(),
                        INFINITE,
                        0,
                        &mut returned,
                    )
                };
                if ok == 0 {
                    match unsafe { GetLastError() } {
                        ERROR_NO_MORE_ITEMS => break,
                        code => return Err(io::Error::from_raw_os_error(code as i32)),
                    }
                }
                let events: Vec<Evt> = events[..returned as usize]
                    .iter()
                    .map(|&h| Evt(h))
                    .collect();
                for event in &events {
                    if let Some(line) = self.render(event, &mut buffer)? {
                        print(&line);
                    }
                }
            }
            unsafe { ResetEvent(signal.0) };
        }
    }

    /// One event as the line `logs` prints for it: a line of output as
    /// itself; one of the manager's as `-- <time> steward: <line>`, in the
    /// form the manager writes into a file; a lost-output event as a line
    /// saying how much. `None` for an event of some other shape.
    fn render(&self, event: &Evt, buffer: &mut Vec<u64>) -> io::Result<Option<String>> {
        let values = render_values(&self.context, event, buffer)?;
        let [time, stream, bytes, dropped] = values.as_slice() else {
            return Ok(None);
        };
        let Value::String(stream) = stream else {
            return Ok(None);
        };
        let time = match time {
            Value::FileTime(t) => local_time(*t),
            _ => String::new(),
        };
        Ok(match (bytes, dropped) {
            (Value::String(text), _) if stream == STREAM_STEWARD => {
                Some(format!("-- {time} steward: {text}"))
            }
            (Value::String(text), _) => Some(text.clone()),
            (_, Value::UInt64(lost)) => Some(format!(
                "-- {time} steward-cat: {lost} bytes of {stream} were lost while no session was \
                 listening to the channel"
            )),
            _ => None,
        })
    }
}

/// Newest first is how they were read; oldest first is how they are shown.
fn finish(mut lines: Vec<String>) -> Vec<String> {
    lines.reverse();
    lines
}

/// The signal event a subscription sets.
struct Signal(HANDLE);

impl Drop for Signal {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// A rendered value, of the kinds an event of ours holds.
#[derive(Debug, PartialEq)]
enum Value {
    Null,
    String(String),
    UInt64(u64),
    FileTime(u64),
    Other,
}

/// The values `context` selects from `event`, in its order. `buffer` is
/// reused between events; the variants and the strings they point into are
/// both in it, which is why the values are copied out before it is next
/// written.
fn render_values(context: &Evt, event: &Evt, buffer: &mut Vec<u64>) -> io::Result<Vec<Value>> {
    let mut used = 0u32;
    let mut count = 0u32;
    loop {
        let ok = unsafe {
            EvtRender(
                context.0,
                event.0,
                EvtRenderEventValues,
                (buffer.len() * 8) as u32,
                buffer.as_mut_ptr().cast(),
                &mut used,
                &mut count,
            )
        };
        if ok != 0 {
            break;
        }
        match unsafe { GetLastError() } {
            ERROR_INSUFFICIENT_BUFFER => buffer.resize((used as usize).div_ceil(8), 0),
            code => return Err(io::Error::from_raw_os_error(code as i32)),
        }
    }
    // SAFETY: EvtRender wrote `count` EVT_VARIANTs at the start of the
    // buffer, and every pointer in them points into the same buffer, which
    // lives until this returns.
    let variants: &[EVT_VARIANT] =
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), count as usize) };
    Ok(variants.iter().map(|v| unsafe { value(v) }).collect())
}

/// # Safety
/// `v` was written by `EvtRender`, and whatever it points at is alive.
unsafe fn value(v: &EVT_VARIANT) -> Value {
    // The high bit marks an array; none of ours is one.
    match (v.Type & 0x7f) as i32 {
        0 => Value::Null,
        t if t == EvtVarTypeString => {
            let text = unsafe { v.Anonymous.StringVal };
            if text.is_null() {
                return Value::String(String::new());
            }
            let length = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
            Value::String(String::from_utf16_lossy(unsafe {
                std::slice::from_raw_parts(text, length)
            }))
        }
        t if t == EvtVarTypeUInt64 => Value::UInt64(unsafe { v.Anonymous.UInt64Val }),
        t if t == EvtVarTypeFileTime => Value::FileTime(unsafe { v.Anonymous.FileTimeVal }),
        _ => Value::Other,
    }
}

/// A `FILETIME` as the local time the manager stamps its lines with:
/// `2026-09-13 14:29:54.387`.
fn local_time(filetime: u64) -> String {
    let filetime = FILETIME {
        dwLowDateTime: filetime as u32,
        dwHighDateTime: (filetime >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    let ok = unsafe {
        FileTimeToSystemTime(&filetime, &mut utc) != 0
            && SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) != 0
    };
    if !ok {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        local.wYear,
        local.wMonth,
        local.wDay,
        local.wHour,
        local.wMinute,
        local.wSecond,
        local.wMilliseconds
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xpath_literals() {
        assert_eq!(
            xpath_literal("whkd.service").as_deref(),
            Some("'whkd.service'")
        );
        assert_eq!(
            xpath_literal("it's.service").as_deref(),
            Some("\"it's.service\"")
        );
        assert_eq!(xpath_literal("a'b\"c"), None);
        assert_eq!(
            query_for("whkd.service").as_deref(),
            Some("*[EventData[Data[@Name='unit']='whkd.service']]")
        );
    }

    /// A known instant, in whatever zone this runs in: the shape, the
    /// millisecond and the date are asserted, and the hour is not.
    #[test]
    fn a_filetime_is_a_local_timestamp() {
        // 2026-09-14 20:28:43.386 UTC, as `DateTime::ToFileTimeUtc` gives it.
        let text = local_time(134_338_913_233_860_000);
        assert_eq!(text.len(), "2026-09-14 20:28:43.386".len(), "{text}");
        assert!(text.starts_with("2026-09-1"), "{text}");
        assert!(text.ends_with(":28:43.386"), "{text}");
    }
}
