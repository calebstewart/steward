//! The Windows half of supervision: jobs, processes, what can be told about
//! a pipe from one end of it, the completion port the manager waits on,
//! environments, asking programs to exit, Explorer's readiness, the local
//! clock timers keep, and -- for the Event Log provisioning rather than for
//! supervision -- who is signed in and what an account name is called in
//! SIDs. Each module is a thin, safe wrapper over the Win32 calls it names.

pub mod account;
pub mod clock;
pub mod env;
pub mod job;
pub mod pipe;
pub mod port;
pub mod process;
pub mod session;
pub mod shell;
pub mod signal;

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{LocalFree, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::PSID;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;

/// A NUL-terminated UTF-16 copy of `s`.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn check(result: BOOL) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Take ownership of a handle a Win32 call returned, or its error.
unsafe fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedHandle::from_raw_handle(handle))
    }
}

/// A SID as `S-1-5-21-...`, the only form the manifest and the access
/// descriptors are written in, and the only form `steward_eventlog::is_sid`
/// lets through to either.
///
/// Shared by the two ways the provisioning finds a SID: a session's token
/// ([`session`]) and an account's name ([`account`]).
fn string_sid(sid: PSID) -> io::Result<String> {
    let mut text = std::ptr::null_mut();
    check(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
    let length = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    unsafe { LocalFree(text.cast()) };
    Ok(sid)
}

/// The session a process runs in, if it can be told.
pub fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0;
    (unsafe { ProcessIdToSessionId(pid, &mut session) } != 0).then_some(session)
}

/// The session this process runs in.
pub fn own_session() -> u32 {
    session_of(std::process::id()).unwrap_or(u32::MAX)
}
