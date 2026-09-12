//! Asking programs to exit, since Windows has no SIGTERM: WM_CLOSE to their
//! top-level windows, and Ctrl+C to their consoles.
//!
//! Ctrl+C can only be sent by a process attached to the target's console, and
//! attaching would cost the manager its own (in `--console` mode) and its
//! ignorance of Ctrl+C. So a helper does it: `steward --ctrl-c <pid>...`
//! detaches from any console, ignores Ctrl+C itself, and attaches to each
//! target's console in turn. Every process on a console hears one Ctrl+C,
//! however many of them were named.

use std::collections::BTreeSet;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleProcessList,
    SetConsoleCtrlHandler, CTRL_C_EVENT,
};
use windows_sys::Win32::System::Threading::DETACHED_PROCESS;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};

/// Ask `pids` to exit, both ways; neither waits.
pub fn ask_to_exit(pids: &[u32]) -> std::io::Result<()> {
    close_windows(pids);
    if pids.is_empty() {
        return Ok(());
    }
    Command::new(std::env::current_exe()?)
        .arg("--ctrl-c")
        .args(pids.iter().map(u32::to_string))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .map(drop)
}

struct Targets<'a> {
    pids: &'a [u32],
}

unsafe extern "system" fn close_window(window: HWND, context: LPARAM) -> BOOL {
    let targets = &*(context as *const Targets);
    let mut pid = 0u32;
    GetWindowThreadProcessId(window, &mut pid);
    if targets.pids.contains(&pid) {
        PostMessageW(window, WM_CLOSE, 0, 0);
    }
    1
}

/// Post WM_CLOSE to every top-level window the processes own.
pub fn close_windows(pids: &[u32]) {
    let targets = Targets { pids };
    unsafe { EnumWindows(Some(close_window), &targets as *const Targets as LPARAM) };
}

/// The helper process: `steward --ctrl-c <pid>...`.
pub fn ctrl_c_helper(pids: &[u32]) -> i32 {
    let mut signalled: BTreeSet<u32> = BTreeSet::new();
    unsafe {
        FreeConsole();
        SetConsoleCtrlHandler(None, 1);
        for &pid in pids {
            if signalled.contains(&pid) || AttachConsole(pid) == 0 {
                // Already told, or no console: a GUI program.
                continue;
            }
            let mut attached = [0u32; 256];
            let count =
                GetConsoleProcessList(attached.as_mut_ptr(), attached.len() as u32) as usize;
            signalled.extend(&attached[..count.min(attached.len())]);
            GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0);
            FreeConsole();
        }
    }
    0
}
