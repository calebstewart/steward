//! The Task Scheduler, for the one task steward registers: the Event Log
//! provisioning that runs as SYSTEM at every logon (`crate::eventlog`).
//!
//! Through COM rather than `schtasks.exe`, for one reason that matters.
//! Registering a task and giving it a security descriptor is a single call,
//! `ITaskFolder::RegisterTask`, and `schtasks` has no option for the
//! descriptor at all. A task that runs as SYSTEM and that ordinary users may
//! *modify* is a way to become SYSTEM, so the descriptor is not a detail to
//! apply in a second step that might not happen.
//!
//! This is the only part of steward that talks COM, and it talks it only at
//! install and uninstall.

use std::io;

use windows::core::BSTR;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::TaskScheduler::{
    ITaskFolder, ITaskService, TaskScheduler, TASK_CREATE_OR_UPDATE, TASK_LOGON_SERVICE_ACCOUNT,
};
use windows::Win32::System::Variant::VARIANT;

/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`: how the Task Scheduler says a
/// task by that name is not registered.
const NOT_FOUND: i32 = -2147024894; // 0x80070002

/// Register `xml` as the task called `name`, to run as SYSTEM, with `sddl`
/// as its security descriptor.
///
/// Replaces a task of that name if there is one, rather than failing or
/// making a second: the install step and every later run of it say the same
/// thing, and saying it twice changes nothing.
pub fn register(name: &str, xml: &str, sddl: &str) -> io::Result<()> {
    let folder = root()?;
    unsafe {
        folder.RegisterTask(
            &BSTR::from(name),
            &BSTR::from(xml),
            TASK_CREATE_OR_UPDATE.0,
            // SYSTEM, named by SID so that no localised account name has to
            // be got right, and with no password because a service account
            // has none.
            &VARIANT::from("S-1-5-18"),
            &VARIANT::default(),
            TASK_LOGON_SERVICE_ACCOUNT,
            &VARIANT::from(sddl),
        )
    }
    .map(|_| ())
    .map_err(|e| failed("register the task", e))
}

/// Delete the task called `name`. `false` if there was none, which is not an
/// error: an uninstall that runs twice is an uninstall.
pub fn remove(name: &str) -> io::Result<bool> {
    let folder = root()?;
    match unsafe { folder.DeleteTask(&BSTR::from(name), 0) } {
        Ok(()) => Ok(true),
        Err(e) if e.code().0 == NOT_FOUND => Ok(false),
        Err(e) => Err(failed("delete the task", e)),
    }
}

/// The root task folder, `\`, on this machine. Tasks live there rather than
/// in a folder of steward's own: one task does not need a folder, and the
/// root is where `schtasks /query` and Task Scheduler show it without being
/// asked twice.
fn root() -> io::Result<ITaskFolder> {
    unsafe {
        // S_FALSE if this thread is already in an apartment, which is not a
        // failure and is why the result is dropped. Apartment-threaded
        // because nothing here is called from more than one thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| failed("reach the Task Scheduler", e))?;
        service
            .Connect(
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
            )
            .map_err(|e| failed("connect to the Task Scheduler", e))?;
        service
            .GetFolder(&BSTR::from("\\"))
            .map_err(|e| failed("open the task folder", e))
    }
}

/// A COM failure as an `io::Error`, keeping the HRESULT: the Task Scheduler's
/// own codes (`0x80041318` and friends) say more than "failed" does, and are
/// what a search turns up.
fn failed(what: &str, e: windows::core::Error) -> io::Error {
    io::Error::other(format!(
        "cannot {what}: {} (0x{:08x})",
        e.message().trim(),
        e.code().0
    ))
}
