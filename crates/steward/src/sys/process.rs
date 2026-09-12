//! Starting a service's processes, and learning when they end.
//!
//! A process is created straight into its job (`PROC_THREAD_ATTRIBUTE_JOB_LIST`,
//! so nothing it starts in its first instant escapes), inherits exactly two
//! handles (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`: NUL for stdin, the unit's log
//! for stdout and stderr), gets no console window, and gets the environment
//! it is given.

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, size_of_val, zeroed};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, GENERIC_READ, HANDLE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_ALWAYS, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, OpenProcess, RegisterWaitForSingleObject, TerminateProcess,
    UnregisterWaitEx, UpdateProcThreadAttribute, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, WT_EXECUTEONLYONCE,
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

/// Start `command_line` in `job`.
pub fn spawn(
    command_line: &str,
    environment: &[u16],
    directory: &Path,
    job: &Job,
    stdin: &OwnedHandle,
    output: &OwnedHandle,
) -> io::Result<Child> {
    unsafe {
        let mut size = 0usize;
        InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut size);
        let mut storage = vec![0u64; size.div_ceil(8)];
        let list = storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        check(InitializeProcThreadAttributeList(list, 2, 0, &mut size))?;
        let _cleanup = AttributeList(list);

        let jobs = [job.raw()];
        check(UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            jobs.as_ptr() as *const c_void,
            size_of_val(&jobs),
            null_mut(),
            null(),
        ))?;
        let handles = [stdin.as_raw_handle(), output.as_raw_handle()];
        check(UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr() as *const c_void,
            size_of_val(&handles),
            null_mut(),
            null(),
        ))?;

        let mut startup: STARTUPINFOEXW = zeroed();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdin.as_raw_handle();
        startup.StartupInfo.hStdOutput = output.as_raw_handle();
        startup.StartupInfo.hStdError = output.as_raw_handle();
        startup.lpAttributeList = list;

        let mut line = wide(command_line);
        let directory = wide(directory);
        let mut info: PROCESS_INFORMATION = zeroed();
        check(CreateProcessW(
            null(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            environment.as_ptr() as *const c_void,
            directory.as_ptr(),
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
