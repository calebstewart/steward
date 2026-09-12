//! Explorer, as the built-in targets see it. `graphical-session.target` is
//! its taskbar window (`Shell_TrayWnd`) existing. `tray.target` is Explorer
//! saying the taskbar is ready: the `TaskbarCreated` broadcast, which tray
//! programs already listen for to add their icons again after Explorer
//! restarts. The window comes first, and its notification area takes an icon
//! only a second or so later (measured on gaming-windows, 2026-09-12).
//!
//! A broadcast goes to top-level windows, so hearing it takes one, hidden, and
//! a thread to pump its messages.

use std::io;
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

use windows_sys::Win32::Foundation::{FILETIME, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetMessageW,
    GetWindowThreadProcessId, RegisterClassW, RegisterWindowMessageW, MSG, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use super::port::Waker;
use super::process::creation_time_of;
use super::wide;

/// Explorer's taskbar window, if it exists.
fn taskbar() -> Option<HWND> {
    let class = wide("Shell_TrayWnd");
    let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    (!hwnd.is_null()).then_some(hwnd)
}

/// Explorer's taskbar exists: the shell is ready, and with it
/// graphical-session.target.
pub fn taskbar_exists() -> bool {
    taskbar().is_some()
}

/// How long the Explorer that owns the taskbar has been running, if there is
/// a taskbar and its process can be asked.
pub fn explorer_age() -> Option<Duration> {
    let hwnd = taskbar()?;
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    let created = creation_time_of(pid)?;
    let mut now = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    unsafe { GetSystemTimeAsFileTime(&mut now) };
    let now = u64::from(now.dwHighDateTime) << 32 | u64::from(now.dwLowDateTime);
    // FILETIMEs count 100 ns.
    Some(Duration::from_nanos(now.saturating_sub(created) * 100))
}

/// `TaskbarCreated`'s message number, and where to post when it comes.
static WATCH: OnceLock<(u32, Waker, usize)> = OnceLock::new();

/// From now on, post a packet with `key` to the port each time Explorer
/// broadcasts `TaskbarCreated`: once its taskbar is ready at sign-in, and
/// again whenever Explorer restarts. Returns once the window that hears it
/// exists, so a broadcast after this is not missed.
pub fn watch_taskbar_created(waker: Waker, key: usize) -> io::Result<()> {
    let message = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
    if message == 0 {
        return Err(io::Error::last_os_error());
    }
    if WATCH.set((message, waker, key)).is_err() {
        return Err(io::Error::other("already watching"));
    }
    let (ready, created) = mpsc::channel();
    std::thread::Builder::new()
        .name("taskbar".into())
        .spawn(move || match unsafe { create_window() } {
            Ok(_) => {
                let _ = ready.send(Ok(()));
                pump();
            }
            Err(e) => {
                let _ = ready.send(Err(e));
            }
        })?;
    created
        .recv()
        .map_err(|_| io::Error::other("the taskbar thread ended"))?
}

/// A top-level window, never shown. (A message-only window would be tidier,
/// but broadcasts do not reach those.)
unsafe fn create_window() -> io::Result<HWND> {
    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide("steward-taskbar");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..std::mem::zeroed()
    };
    if RegisterClassW(&wc) == 0 {
        return Err(io::Error::last_os_error());
    }
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        class.as_ptr(),
        std::ptr::null(),
        WS_POPUP,
        0,
        0,
        0,
        0,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(hwnd)
}

/// For as long as the process lives: broadcasts wait on the windows they
/// reach, so this thread must keep taking its messages.
fn pump() {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe { DispatchMessageW(&msg) };
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(&(taskbar_created, waker, key)) = WATCH.get() {
        if msg == taskbar_created {
            let _ = waker.post(key, 0, 0);
            return 0;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
