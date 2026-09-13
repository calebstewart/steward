//! The pipe: its name, the user and session it belongs to, and both of its
//! ends.
//!
//! A manager belongs to a session, as the desktop its services run on does,
//! so the name carries both: `\\.\pipe\steward-<user SID>-<session>`. A user
//! signed in twice has two managers, and a session signing in while the last
//! one is still stopping its services does not wait for it.
//!
//! The server end is a single instance created with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, owned by the user and with a DACL that
//! admits only the user, and it is reused client after client, so the name is
//! never free for someone else to take. That also makes it the manager's
//! lock: a second manager in the same session cannot create it, and does not
//! start.
//!
//! The name is public and machine-wide, so another account could create the
//! pipe first. The client opens it at `SecurityIdentification`, so the server
//! cannot act as the user, and before sending anything reads the pipe's
//! owner and checks that it is the user. The owner is set from the creator's
//! token, and a standard user can make nobody but themselves the owner of
//! what they create -- unlike a process ID, which is reused, and which cannot
//! be looked into across users anyway. A manager that finds the name taken
//! runs the same check (see [`probe`]) to tell a manager of the user's from
//! a squatter.

use std::ffi::{c_void, OsStr};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem::ManuallyDrop;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
};

use crate::{Request, Response, MAX_MESSAGE};

/// Where every local pipe lives.
const PREFIX: &str = r"\\.\pipe\";

/// How long a client waits for the manager to finish with the client before
/// it.
const CLIENT_PATIENCE: Duration = Duration::from_secs(10);

/// How long a manager probing a pipe it could not create waits for whoever
/// serves it to be free.
const PROBE_PATIENCE: Duration = Duration::from_secs(5);

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

/// A SID as a string, `S-1-5-21-...`.
fn sid_string(sid: PSID) -> io::Result<String> {
    unsafe {
        let mut text: *mut u16 = null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
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
        sid_string(user.User.Sid)
    }
}

/// The SID of the user this process runs as.
pub fn user_sid() -> io::Result<String> {
    token_user(unsafe { GetCurrentProcess() })
}

/// The session this process runs in.
pub fn session() -> io::Result<u32> {
    let mut session = 0;
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(session)
}

/// The owner of a kernel object, as a string SID: the user whose process
/// created it, unless an administrator says otherwise. Reading it takes
/// `READ_CONTROL`, which every read handle has.
fn owner_sid(handle: HANDLE) -> io::Result<String> {
    unsafe {
        let mut owner: PSID = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let error = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        );
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let sid = sid_string(owner);
        LocalFree(descriptor);
        sid
    }
}

/// `\\.\pipe\<name>`, for a name that is one name: Win32 normalises `\\.\`
/// paths, so a name with a separator or `..` in it would name something
/// other than a pipe.
fn path_of(name: &OsStr) -> io::Result<String> {
    let refuse = |why: &str| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("STEWARD_PIPE {name:?} {why}"),
        )
    };
    let Some(name) = name.to_str() else {
        return Err(refuse("is not Unicode"));
    };
    if name.is_empty() {
        return Err(refuse("is empty"));
    }
    if name == "." || name == ".." {
        return Err(refuse("is not a name"));
    }
    if name.contains(['\\', '/']) {
        return Err(refuse(
            "has a path separator in it; a pipe's name is one name",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(refuse("has a control character in it"));
    }
    Ok(format!("{PREFIX}{name}"))
}

/// `\\.\pipe\steward-<user SID>-<session>`: this user's manager in this
/// process's session. `STEWARD_PIPE` names another, for a second manager
/// beside the one that runs -- `steward --console` in scratch directories --
/// and the `stewctl` that talks to it; the server is still checked to be the
/// user's. It must be one name: nothing with a separator in it, and not `.`
/// or `..`.
pub fn name() -> io::Result<String> {
    if let Some(name) = std::env::var_os("STEWARD_PIPE") {
        return path_of(&name);
    }
    Ok(format!("{PREFIX}steward-{}-{}", user_sid()?, session()?))
}

/// The manager's end of the pipe.
pub struct Server {
    handle: OwnedHandle,
}

impl Server {
    /// Create the pipe. Fails with `AlreadyExists` if it exists: another
    /// manager is running for this user in this session, or someone has
    /// taken the name -- [`probe`] tells which.
    pub fn create() -> io::Result<Server> {
        Server::create_at(&name()?)
    }

    fn create_at(path: &str) -> io::Result<Server> {
        let sid = user_sid()?;
        let path = wide(path);
        // Owned by the user, in so many words, whatever the token's default
        // owner is (an elevated administrator's is the Administrators group).
        // The owner is what the client checks.
        let sddl = wide(&format!("O:{sid}D:P(A;;GA;;;{sid})"));
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
                path.as_ptr(),
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
                if error == ERROR_ACCESS_DENIED || error == ERROR_PIPE_BUSY {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "the pipe already exists: another steward is running in this session",
                    ));
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            Ok(Server {
                handle: OwnedHandle::from_raw_handle(handle),
            })
        }
    }

    /// Wait for a client, answer its one request, and hang up. A client that
    /// hangs up without asking anything -- a manager probing the pipe -- is
    /// not answered, and not an error.
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
        if line.is_empty() {
            return Ok(());
        }
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
    /// No manager is running for this user in this session.
    NotRunning,
    /// The pipe belongs to another user: whoever created it, by SID.
    Impostor(String),
    Io(io::Error),
    Protocol(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::NotRunning => write!(f, "steward is not running in this session"),
            ClientError::Impostor(who) => write!(
                f,
                "the steward pipe belongs to another user ({who}); not talking to it"
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

/// Open the pipe, waiting up to `patience` for the manager to finish with the
/// client before us.
fn connect(path: &str, patience: Duration) -> Result<File, ClientError> {
    let path = wide(path);
    let give_up = Instant::now() + patience;
    loop {
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
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
                WaitNamedPipeW(path.as_ptr(), 1000);
            },
            error => return Err(io::Error::from_raw_os_error(error as i32).into()),
        }
    }
}

/// Whether the pipe was created by this user: its owner is what the creator's
/// token made it, and a standard user cannot make it anyone else.
fn check_server(pipe: &File) -> Result<(), ClientError> {
    let owner = owner_sid(pipe.as_raw_handle())?;
    if owner != user_sid()? {
        return Err(ClientError::Impostor(owner));
    }
    Ok(())
}

fn ask(path: &str, request: &Request) -> Result<Response, ClientError> {
    let pipe = connect(path, CLIENT_PATIENCE)?;
    check_server(&pipe)?;
    let mut out = serde_json::to_vec(request).map_err(|e| ClientError::Protocol(e.to_string()))?;
    out.push(b'\n');
    (&pipe).write_all(&out)?;
    let mut line = Vec::new();
    BufReader::new((&pipe).take(MAX_MESSAGE)).read_until(b'\n', &mut line)?;
    serde_json::from_slice(&line).map_err(|e| ClientError::Protocol(e.to_string()))
}

/// Send one request to the manager and read its response.
pub fn request(request: &Request) -> Result<Response, ClientError> {
    ask(&name()?, request)
}

/// Whose the pipe is, for a manager that could not create it: `Ok` when it
/// was created by this user (another manager of theirs serves the session),
/// `Impostor` when by someone else. `NotRunning` means it has gone since;
/// anything else could not be told apart from a squatter (one whose DACL
/// keeps this user out, say). Nothing is sent: the server sees a client that
/// hangs up without asking.
pub fn probe() -> Result<(), ClientError> {
    let pipe = connect(&name()?, PROBE_PATIENCE)?;
    check_server(&pipe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipe_name_is_one_name() {
        assert_eq!(
            path_of(OsStr::new("steward-dev")).unwrap(),
            r"\\.\pipe\steward-dev"
        );
        assert_eq!(path_of(OsStr::new("a b.c")).unwrap(), r"\\.\pipe\a b.c");
        for bad in [
            "",
            ".",
            "..",
            r"..\C:\Users\me\file",
            "../x",
            r"a\b",
            "a/b",
            "a\nb",
            "a\0b",
        ] {
            let error = path_of(OsStr::new(bad)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    #[test]
    fn the_pipe_is_its_creators_and_answers_one_request_at_a_time() {
        let path = format!("{PREFIX}steward-test-{}", std::process::id());
        let server = Server::create_at(&path).unwrap();
        // The name is held.
        let again = Server::create_at(&path).map(|_| ()).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);

        let serving = std::thread::spawn(move || {
            // A probe asks nothing, and is not answered.
            server
                .serve_one(|_| unreachable!("a probe is not a request"))
                .unwrap();
            server
                .serve_one(|request| {
                    assert_eq!(request.unwrap(), Request::Reload { apply: false });
                    Response {
                        messages: vec!["reloaded".into()],
                        ..Response::default()
                    }
                })
                .unwrap();
        });

        // The probe: the owner is this user.
        {
            let pipe = connect(&path, PROBE_PATIENCE).unwrap();
            assert_eq!(
                owner_sid(pipe.as_raw_handle()).unwrap(),
                user_sid().unwrap()
            );
            check_server(&pipe).unwrap();
        }
        // A request, waiting for the server to come round again.
        let response = ask(&path, &Request::Reload { apply: false }).unwrap();
        assert_eq!(response.messages, ["reloaded"]);
        serving.join().unwrap();

        // The server is gone with its handle.
        assert!(matches!(
            connect(&path, Duration::ZERO),
            Err(ClientError::NotRunning)
        ));
    }
}
