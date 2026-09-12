//! Hosting by the SCM. steward is registered as a per-user service template
//! (`type= userown`); at each sign-in Windows starts an instance,
//! `steward_<suffix>`, in the user's session and with the user's token. This
//! module speaks the SCM's protocol and turns its controls into the manager's.
//!
//! - Stop (an administrator, an upgrade): detach, leaving the services for
//!   the next instance to adopt.
//! - Shutdown, pre-shutdown, and the user's own session logging off: stop
//!   every service, in order.

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType, SessionChangeReason,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;

use crate::log::{error, info};
use crate::manager::{self, Control, KEY_WAKE};
use crate::sys::port::Port;

// Ignored for an own-process service, but it may not be empty.
const DISPATCH_NAME: &str = "steward";

define_windows_service!(ffi_service_main, service_main);

pub fn run() {
    crate::log::init(false);
    if let Err(e) = service_dispatcher::start(DISPATCH_NAME, ffi_service_main) {
        error!("not started by the SCM ({e}); use --console to run in the foreground");
        eprintln!("steward: not started by the SCM ({e}); use --console to run in the foreground");
        std::process::exit(1);
    }
}

fn service_main(arguments: Vec<OsString>) {
    // The instance's name, e.g. steward_9eb32b.
    let name = arguments
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_else(|| DISPATCH_NAME.into());
    if let Err(e) = host(&name) {
        error!("service host failed: {e}");
    }
}

fn own_session() -> u32 {
    let mut session = u32::MAX;
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
    session
}

fn host(name: &str) -> windows_service::Result<()> {
    let port = Port::new().map_err(windows_service::Error::Winapi)?;
    let waker = port.waker();
    let session = own_session();
    let (controls, inbox) = mpsc::channel();
    let for_manager = controls.clone();
    let handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        let message = match control {
            ServiceControl::Interrogate => return ServiceControlHandlerResult::NoError,
            ServiceControl::Stop => Control::Detach("the SCM asked the instance to stop".into()),
            ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                Control::StopAll("the system is shutting down".into())
            }
            ServiceControl::SessionChange(change)
                if change.reason == SessionChangeReason::SessionLogoff
                    && change.notification.session_id == session =>
            {
                Control::StopAll("the user is signing out".into())
            }
            ServiceControl::SessionChange(change) => Control::Note(format!(
                "session {}: {:?}",
                change.notification.session_id, change.reason
            )),
            ServiceControl::PowerEvent(power) => Control::Note(format!("power: {power:?}")),
            _ => return ServiceControlHandlerResult::NotImplemented,
        };
        let _ = controls.send(message);
        let _ = waker.post(KEY_WAKE, 0, 0);
        ServiceControlHandlerResult::NoError
    };
    let status = service_control_handler::register(name, handler)?;
    let running = ServiceStatus {
        service_type: ServiceType::USER_OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::PRESHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE
            | ServiceControlAccept::POWER_EVENT,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status.set_service_status(running.clone())?;
    info!("running as the SCM service {name}, session {session}");

    manager::run(port, for_manager, inbox);

    status.set_service_status(ServiceStatus {
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        ..running
    })?;
    Ok(())
}
