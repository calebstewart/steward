//! Whether the user's Event Log channel exists: the one fact about the
//! machine that steward and `stewctl` must read alike.
//!
//! A unit that does not say `StandardOutput=` sends its output to the
//! channel where there is one and to its log file where there is not. The
//! manager decides that when it starts the unit, and `stewctl` has to reach
//! the same answer when no manager is running to ask, so both ask here.

use std::io;

use steward_eventlog::channel_key;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE,
};

/// Whether `sid`'s channel, `Steward/<SID>`, is registered on this machine.
///
/// Read off the registry key Windows keeps the channel's configuration in
/// ([`channel_key`]), which exists exactly while the channel does and which
/// any user may read. A key that cannot be opened for any other reason is
/// an error rather than a no: the caller says so, where "not there" would
/// quietly send the output somewhere else.
pub fn registered(sid: &str) -> io::Result<bool> {
    let key: Vec<u16> = channel_key(sid).encode_utf16().chain([0]).collect();
    let mut handle: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut handle,
        )
    };
    match status {
        ERROR_SUCCESS => {
            unsafe { RegCloseKey(handle) };
            Ok(true)
        }
        ERROR_FILE_NOT_FOUND => Ok(false),
        other => Err(io::Error::from_raw_os_error(other as i32)),
    }
}
