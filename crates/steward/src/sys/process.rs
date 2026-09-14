//! Starting a service's processes, and learning when they end.
//!
//! A unit's process is created straight into its job
//! (`PROC_THREAD_ATTRIBUTE_JOB_LIST`, so nothing it starts in its first
//! instant escapes), inherits exactly the handles it is given
//! (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`: NUL for stdin, and for stdout and
//! stderr the unit's log, or the write ends of its pipes to a
//! `steward-cat`), gets no console window, and gets the environment it is
//! given. The `steward-cat` itself is started the same way, in no job and
//! with no console, with the pipes' read ends beside its standard three.

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, size_of_val, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, GENERIC_READ, HANDLE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_ALWAYS, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, OpenProcess, RegisterWaitForSingleObject, TerminateProcess,
    UnregisterWaitEx, UpdateProcThreadAttribute, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    DETACHED_PROCESS, EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, WT_EXECUTEONLYONCE,
};

use super::job::Job;
use super::port::Waker;
use super::{check, owned, wide};

/// A process steward started or adopted.
pub struct Child {
    pub pid: u32,
    /// Its creation time, as a FILETIME: with the PID, what identifies it
    /// across a manager restart (PIDs are reused; the pair is not).
    pub created: u64,
    handle: OwnedHandle,
}

impl Child {
    pub fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    pub fn exit_code(&self) -> io::Result<u32> {
        let mut code = 0u32;
        check(unsafe { GetExitCodeProcess(self.raw(), &mut code) })?;
        Ok(code)
    }

    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        check(unsafe { TerminateProcess(self.raw(), exit_code) })
    }

    /// Re-open a process a previous manager started, if it is still the same
    /// process.
    pub fn open(pid: u32, created: u64) -> io::Result<Child> {
        // SET_QUOTA and TERMINATE are what joining a job takes.
        let access = PROCESS_SYNCHRONIZE
            | PROCESS_QUERY_LIMITED_INFORMATION
            | PROCESS_TERMINATE
            | PROCESS_SET_QUOTA;
        let handle = unsafe { owned(OpenProcess(access, 0, pid))? };
        let actual = creation_time(handle.as_raw_handle())?;
        if actual != created {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the PID now belongs to another process",
            ));
        }
        Ok(Child {
            pid,
            created,
            handle,
        })
    }
}

fn creation_time(handle: HANDLE) -> io::Result<u64> {
    let mut times: [FILETIME; 4] = unsafe { zeroed() };
    let [created, exited, kernel, user] = &mut times;
    check(unsafe { GetProcessTimes(handle, created, exited, kernel, user) })?;
    Ok(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
}

/// A running process's creation time, by PID.
pub fn creation_time_of(pid: u32) -> Option<u64> {
    let handle = unsafe { owned(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)).ok()? };
    creation_time(handle.as_raw_handle()).ok()
}

/// The exit code of a process that has just ended, by PID, if it can still be
/// read.
pub fn exit_code_of(pid: u32) -> Option<u32> {
    let handle = unsafe { owned(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)).ok()? };
    let mut code = 0u32;
    (unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) } != 0).then_some(code)
}

fn inheritable() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    }
}

/// `path`, opened for appending, as a handle a child can inherit.
pub fn open_log(path: &Path) -> io::Result<OwnedHandle> {
    let name = wide(path);
    let security = inheritable();
    unsafe {
        owned(CreateFileW(
            name.as_ptr(),
            FILE_APPEND_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &security,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        ))
    }
}

/// The NUL device, for stdin.
pub fn open_null() -> io::Result<OwnedHandle> {
    let name = wide("NUL");
    let security = inheritable();
    unsafe {
        owned(CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &security,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        ))
    }
}

/// How big a pipe from a unit to its `steward-cat` is asked to be: what the
/// shim's stress runs used, and a comfortable margin over its one read.
const PIPE_SIZE: u32 = 64 << 10;

/// A pipe, both ends inheritable: the write end for a unit's processes, the
/// read end for the `steward-cat` that carries their output. Which child
/// gets which is the spawn's handle list, not the ends' inheritability.
pub fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let security = inheritable();
    let (mut read, mut write): (HANDLE, HANDLE) = (null_mut(), null_mut());
    check(unsafe { CreatePipe(&mut read, &mut write, &security, PIPE_SIZE) })?;
    // SAFETY: both are open handles CreatePipe made for this process.
    unsafe {
        Ok((
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        ))
    }
}

/// The handles a process is started with: exactly these, and nothing else
/// of the manager's.
pub struct Handles<'a> {
    pub stdin: &'a OwnedHandle,
    pub stdout: &'a OwnedHandle,
    pub stderr: &'a OwnedHandle,
    /// Inherited beside the standard three, for a program that takes handles
    /// by number: a `steward-cat`'s two read ends.
    pub also: &'a [&'a OwnedHandle],
}

/// Start `command_line`: in `job` if one is given; with `environment` and in
/// `directory`, or the manager's own where not given; with a hidden console
/// (`CREATE_NO_WINDOW`) or none at all (`DETACHED_PROCESS`).
pub fn spawn(
    command_line: &str,
    environment: Option<&[u16]>,
    directory: Option<&Path>,
    job: Option<&Job>,
    handles: &Handles,
    console: bool,
) -> io::Result<Child> {
    unsafe {
        let attributes = 1 + u32::from(job.is_some());
        let mut size = 0usize;
        InitializeProcThreadAttributeList(null_mut(), attributes, 0, &mut size);
        let mut storage = vec![0u64; size.div_ceil(8)];
        let list = storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        check(InitializeProcThreadAttributeList(
            list, attributes, 0, &mut size,
        ))?;
        let _cleanup = AttributeList(list);

        let jobs = job.map(|job| [job.raw()]);
        if let Some(jobs) = &jobs {
            check(UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr() as *const c_void,
                size_of_val(jobs),
                null_mut(),
                null(),
            ))?;
        }
        // Each handle once: stdout and stderr are usually the same one.
        let mut inherited: Vec<HANDLE> = Vec::new();
        for handle in [handles.stdin, handles.stdout, handles.stderr]
            .into_iter()
            .chain(handles.also.iter().copied())
        {
            let raw = handle.as_raw_handle();
            if !inherited.contains(&raw) {
                inherited.push(raw);
            }
        }
        check(UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherited.as_ptr() as *const c_void,
            inherited.len() * size_of::<HANDLE>(),
            null_mut(),
            null(),
        ))?;

        let mut startup: STARTUPINFOEXW = zeroed();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles.stdin.as_raw_handle();
        startup.StartupInfo.hStdOutput = handles.stdout.as_raw_handle();
        startup.StartupInfo.hStdError = handles.stderr.as_raw_handle();
        startup.lpAttributeList = list;

        let mut line = wide(command_line);
        let directory = directory.map(wide);
        let window = if console {
            CREATE_NO_WINDOW
        } else {
            DETACHED_PROCESS
        };
        let mut info: PROCESS_INFORMATION = zeroed();
        check(CreateProcessW(
            null(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | window,
            environment.map_or(null(), |e| e.as_ptr() as *const c_void),
            directory.as_ref().map_or(null(), |d| d.as_ptr()),
            &startup.StartupInfo,
            &mut info,
        ))?;
        CloseHandle(info.hThread);
        let handle = owned(info.hProcess)?;
        let created = creation_time(handle.as_raw_handle()).unwrap_or(0);
        Ok(Child {
            pid: info.dwProcessId,
            created,
            handle,
        })
    }
}

struct AttributeList(LPPROC_THREAD_ATTRIBUTE_LIST);

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.0) };
    }
}

/// A thread-pool wait that posts a packet to the port when a process exits.
pub struct ExitWatch(HANDLE);

struct WatchContext {
    waker: Waker,
    key: usize,
    token: usize,
}

unsafe extern "system" fn on_exit(context: *mut c_void, _timed_out: bool) {
    let context = Box::from_raw(context as *mut WatchContext);
    let _ = context.waker.post(context.key, 0, context.token);
}

impl ExitWatch {
    /// Post (`key`, `token`) through `waker` once `child` has exited.
    pub fn new(child: &Child, waker: Waker, key: usize, token: usize) -> io::Result<ExitWatch> {
        let context = Box::into_raw(Box::new(WatchContext { waker, key, token }));
        let mut wait: HANDLE = null_mut();
        let ok = unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                child.raw(),
                Some(on_exit),
                context as *const c_void,
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        if ok == 0 {
            drop(unsafe { Box::from_raw(context) });
            return Err(io::Error::last_os_error());
        }
        Ok(ExitWatch(wait))
    }
}

impl Drop for ExitWatch {
    fn drop(&mut self) {
        // Does not wait for a callback in flight; the callback owns its context.
        unsafe { UnregisterWaitEx(self.0, null_mut()) };
    }
}
