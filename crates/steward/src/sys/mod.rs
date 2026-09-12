//! The Windows half of supervision: jobs, processes, the completion port the
//! manager waits on, environments, asking programs to exit, Explorer's
//! readiness, and the local clock timers keep. Each module is a thin, safe
//! wrapper over the Win32 calls it names.

pub mod clock;
pub mod env;
pub mod job;
pub mod port;
pub mod process;
pub mod shell;
pub mod signal;

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
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

/// The session a process runs in, if it can be told.
pub fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0;
    (unsafe { ProcessIdToSessionId(pid, &mut session) } != 0).then_some(session)
}

/// The session this process runs in.
pub fn own_session() -> u32 {
    session_of(std::process::id()).unwrap_or(u32::MAX)
}
