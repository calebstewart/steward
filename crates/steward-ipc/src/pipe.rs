//! The pipe: its name, the user it belongs to, and both of its ends.
//!
//! The server end is a single instance created with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` and a DACL that admits only the user, and
//! it is reused client after client, so the name is never free for someone
//! else to take. That also makes it the manager's lock: a second manager for
//! the same user cannot create it, and does not start.
//!
//! The client opens it at `SecurityIdentification`, so the server cannot act
//! as the user, and checks the serving process's user before sending anything.

use std::ffi::c_void;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem::ManuallyDrop;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
    WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::{Request, Response, MAX_MESSAGE};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

/// The SID of the user a process runs as, as a string.
fn token_user(process: HANDLE) -> io::Result<String> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut buffer = vec![0u64; 64];
        let mut length = 0u32;
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr() as *mut c_void,
            (buffer.len() * 8) as u32,
            &mut length,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text: *mut u16 = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text as *mut c_void);
        Ok(sid)
    }
}

/// The SID of the user this process runs as.
pub fn user_sid() -> io::Result<String> {
    token_user(unsafe { GetCurrentProcess() })
}

/// `\\.\pipe\steward-<user SID>`.
pub fn name() -> io::Result<String> {
    Ok(format!(r"\\.\pipe\steward-{}", user_sid()?))
}

/// The manager's end of the pipe.
pub struct Server {
    handle: OwnedHandle,
}

impl Server {
    /// Create the pipe. Fails with `AlreadyExists` if it exists: another
    /// manager is running for this user (or someone has taken the name).
    pub fn create() -> io::Result<Server> {
        let sid = user_sid()?;
        let name = wide(&format!(r"\\.\pipe\steward-{sid}"));
        let sddl = wide(&format!("D:P(A;;GA;;;{sid})"));
        unsafe {
            let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let security = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let handle = CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                &security,
            );
            let error = GetLastError();
            LocalFree(descriptor);
            if handle == INVALID_HANDLE_VALUE {
                // ERROR_ACCESS_DENIED is what FIRST_PIPE_INSTANCE fails with.
                if error == 5 || error == ERROR_PIPE_BUSY {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "the pipe already exists: another steward is running for this user",
                    ));
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            Ok(Server {
                handle: OwnedHandle::from_raw_handle(handle),
            })
        }
    }

    /// Wait for a client, answer its one request, and hang up.
    pub fn serve_one(
        &self,
        answer: impl FnOnce(Result<Request, String>) -> Response,
    ) -> io::Result<()> {
        let handle = self.handle.as_raw_handle();
        if unsafe { ConnectNamedPipe(handle, null_mut()) } == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_PIPE_CONNECTED {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        let _hang_up = HangUp(handle);
        // Borrowed: the pipe outlives this client.
        let file = ManuallyDrop::new(unsafe { File::from_raw_handle(handle) });
        let mut line = Vec::new();
        BufReader::new((&*file).take(MAX_MESSAGE)).read_until(b'\n', &mut line)?;
        let request = serde_json::from_slice(&line)
            .map_err(|e| format!("a request steward cannot read: {e}"));
        let mut out = serde_json::to_vec(&answer(request))?;
        out.push(b'\n');
        (&*file).write_all(&out)?;
        unsafe { FlushFileBuffers(handle) };
        Ok(())
    }
}

struct HangUp(HANDLE);

impl Drop for HangUp {
    fn drop(&mut self) {
        unsafe { DisconnectNamedPipe(self.0) };
    }
}

#[derive(Debug)]
pub enum ClientError {
    /// No manager is running for this user.
    NotRunning,
    /// The pipe is served by a process of another user.
    Impostor(String),
    Io(io::Error),
    Protocol(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::NotRunning => write!(f, "steward is not running"),
            ClientError::Impostor(who) => write!(
                f,
                "the steward pipe is served by a process of another user ({who}); not talking to it"
            ),
            ClientError::Io(e) => write!(f, "talking to steward: {e}"),
            ClientError::Protocol(e) => write!(f, "steward's answer makes no sense: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

fn connect() -> Result<File, ClientError> {
    let name = wide(&name()?);
    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(File::from(owned(handle)?));
        }
        match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND => return Err(ClientError::NotRunning),
            // Serving someone else; the manager answers one client at a time.
            ERROR_PIPE_BUSY if Instant::now() < give_up => unsafe {
                WaitNamedPipeW(name.as_ptr(), 1000);
            },
            error => return Err(io::Error::from_raw_os_error(error as i32).into()),
        }
    }
}

fn check_server(pipe: &File) -> Result<(), ClientError> {
    let mut pid = 0u32;
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let process = owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
    let theirs = token_user(process.as_raw_handle())?;
    if theirs != user_sid()? {
        return Err(ClientError::Impostor(theirs));
    }
    Ok(())
}

/// Send one request to the manager and read its response.
pub fn request(request: &Request) -> Result<Response, ClientError> {
    let pipe = connect()?;
    check_server(&pipe)?;
    let mut out = serde_json::to_vec(request).map_err(|e| ClientError::Protocol(e.to_string()))?;
    out.push(b'\n');
    (&pipe).write_all(&out)?;
    let mut line = Vec::new();
    BufReader::new((&pipe).take(MAX_MESSAGE)).read_until(b'\n', &mut line)?;
    serde_json::from_slice(&line).map_err(|e| ClientError::Protocol(e.to_string()))
}
