//! What can be told about a pipe from one end of it: what it is called,
//! whether it is a pipe at all, and whether anything can still write to it.
//!
//! All of this is for one thing. A manager that adopts a unit from a
//! manager that crashed or handed over takes the read ends of the unit's
//! pipes back out of the `steward-cat` that holds them, by the handle
//! numbers the last manager recorded ([`super::process::Child::duplicate`]),
//! so that it can replace the shim as the manager that made the pipes
//! could. A number alone is not enough: the shim may have closed that
//! handle and opened something else, and the number would then name that
//! instead. `GetFileType` says only "a pipe"; the pipe's own name says
//! *which*, and is what the two managers compare.
//!
//! An anonymous pipe has a name because it is not really anonymous: Win32
//! makes one by creating a named pipe called
//! `\Device\NamedPipe\Win32Pipes.<process>.<counter>` and never telling
//! anybody. `NtQueryObject(ObjectNameInformation)` reads it back. That is
//! `ntdll`, and undocumented in the sense that the object information
//! classes are: it is the call Process Explorer and `handle.exe` are built
//! on, it has behaved the same since NT 3.1, and nothing here is fatal if a
//! future Windows stops answering -- a pipe that cannot be named is one no
//! later manager takes back.

use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::null_mut;

use windows_sys::Wdk::Foundation::{NtQueryObject, OBJECT_INFORMATION_CLASS};
use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, UNICODE_STRING};
use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_PIPE};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

/// `ObjectNameInformation`. windows-sys names the two classes the DDK
/// documents and not this one, which is the only one with a name in it.
const OBJECT_NAME_INFORMATION: OBJECT_INFORMATION_CLASS = 1;

/// The buffer `NtQueryObject` is asked for a name in first. A generated
/// pipe name is 44 characters and every other object here is a path, so
/// this is answered in one call; the second is for the day it is not.
const NAME_BYTES: usize = 1 << 10;

/// The buffer was too small, and `needed` says how small.
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;

/// What the object behind `handle` is called:
/// `\Device\NamedPipe\Win32Pipes.0000000000000fa8.00000003` for a pipe
/// `CreatePipe` made, and a device path for most other things.
///
/// The name is the whole identity check a recovered handle gets, so an
/// error here is a refusal to recover rather than something to paper over:
/// the caller records no name, and no later manager takes the pipe back.
pub fn name_of(handle: &OwnedHandle) -> io::Result<String> {
    // u64 and not u8: the buffer holds a UNICODE_STRING, which must be
    // aligned as one.
    let mut buffer = vec![0u64; NAME_BYTES / size_of::<u64>()];
    for _ in 0..2 {
        let length = buffer.len() * size_of::<u64>();
        let mut needed = 0u32;
        let status = unsafe {
            NtQueryObject(
                handle.as_raw_handle(),
                OBJECT_NAME_INFORMATION,
                buffer.as_mut_ptr().cast(),
                length as u32,
                &mut needed,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH && needed as usize > length {
            buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
            continue;
        }
        if status < 0 {
            return Err(io::Error::other(format!("NtQueryObject: {status:#010x}")));
        }
        // SAFETY: the call filled the buffer with an OBJECT_NAME_INFORMATION,
        // whose one field is a UNICODE_STRING pointing into the rest of it.
        let name = unsafe { &*(buffer.as_ptr() as *const UNICODE_STRING) };
        let units = name.Length as usize / size_of::<u16>();
        if name.Buffer.is_null() || units == 0 {
            return Err(io::Error::other("the handle names nothing"));
        }
        // SAFETY: Length is the name's length in bytes, inside the buffer.
        let name = unsafe { std::slice::from_raw_parts(name.Buffer, units) };
        return Ok(String::from_utf16_lossy(name));
    }
    Err(io::Error::other(
        "NtQueryObject: the name outgrew two tries",
    ))
}

/// Whether the handle is a pipe at all. Cheap, and asked first so that the
/// name of something that could not be one is never compared.
pub fn is_pipe(handle: &OwnedHandle) -> bool {
    unsafe { GetFileType(handle.as_raw_handle()) == FILE_TYPE_PIPE }
}

/// Whether anything can still write to the pipe `read` is the read end of,
/// asked without reading a byte or waiting: `PeekNamedPipe` fails with
/// `ERROR_BROKEN_PIPE` once every write end is closed, and an anonymous
/// pipe is a named one underneath.
///
/// Any other failure is taken as "yes". The question is only ever asked to
/// rule a thing out -- whether a shim that exited did so because there was
/// nothing left to read -- and a manager that cannot tell should do what it
/// did before it could ask.
pub fn has_writer(read: &OwnedHandle) -> bool {
    let mut available = 0u32;
    let told = unsafe {
        PeekNamedPipe(
            read.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            &mut available,
            null_mut(),
        )
    };
    told != 0 || io::Error::last_os_error().raw_os_error() != Some(ERROR_BROKEN_PIPE as i32)
}
