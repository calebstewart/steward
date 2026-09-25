//! Hosting by the SCM. steward is registered as a per-user service template
//! (`type= userown`); at each sign-in Windows starts an instance,
//! `steward_<suffix>`, in the user's session and with the user's token. This
//! module speaks the SCM's protocol and turns its controls into the manager's.
//!
//! - Stop, shutdown, pre-shutdown, and the user's own session logging off:
//!   stop every service, in order. Sign-out arrives as a plain Stop, a moment
//!   before Windows logs the session off (observed 2026-09-12).
//! - [`CONTROL_HAND_OVER`]: detach, leaving the services running for the next
//!   manager to adopt, and stop. This is how an upgrade replaces the manager.

use std::ffi::OsString;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType, SessionChangeReason,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{define_windows_service, service_dispatcher};

use crate::log::{error, info};
use crate::manager::{self, Control, Ending, KEY_WAKE};
use crate::sys::port::Port;

// Ignored for an own-process service, but it may not be empty.
const DISPATCH_NAME: &str = "steward";

/// The user-defined control that asks the manager to hand over to a new one:
/// detach, leaving every service running, and stop. What an upgrade sends in
/// place of Stop (winpkgs: `windows.services.<name>.restartControl`); the next
/// manager adopts the services.
///
/// Who may send it is the template's security descriptor's to say, and it
/// matters: a hand-over that no new manager follows leaves the session
/// without one -- no restarts, no timers, no `stewardctl` -- until its next
/// sign-in, since a clean stop runs none of the SCM's failure actions.
/// Windows' default descriptor lets every interactive user send a service
/// user-defined controls, so any other account signed in to the machine could
/// do that to this session. The template steward is registered with withholds
/// the right from interactive users (`nix/winpkgs/system.nix`; `sc sdset` in
/// the by-hand install), leaving it to administrators and SYSTEM, which an
/// upgrade runs as.
pub const CONTROL_HAND_OVER: u32 = 128;

/// How long the SCM is told a stop may take before it counts as hung: the
/// default `TimeoutStopSec=` and the kill that follows, with room to spare.
const STOP_WAIT_HINT: Duration = Duration::from_secs(30);

/// The service-specific exit code of a manager that could not serve its
/// control pipe, and so never ran.
const EXIT_NO_PIPE: u32 = 1;

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

fn host(name: &str) -> windows_service::Result<()> {
    let port = Port::new().map_err(windows_service::Error::Winapi)?;
    let waker = port.waker();
    let session = crate::sys::own_session();
    let (controls, inbox) = mpsc::channel();
    let for_manager = controls.clone();
    // Filled in once the service is running, for the handler to report that
    // it is stopping.
    let reporter: Arc<Mutex<Option<ServiceStatusHandle>>> = Arc::default();
    let handler_reporter = Arc::clone(&reporter);
    let handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        let message = match control {
            ServiceControl::Interrogate => return ServiceControlHandlerResult::NoError,
            ServiceControl::Stop => Control::StopAll("the SCM asked the instance to stop".into()),
            ServiceControl::UserEvent(code) if code.to_raw() == CONTROL_HAND_OVER => {
                Control::Detach("asked to hand over to a new manager".into())
            }
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
        if matches!(message, Control::StopAll(_) | Control::Detach(_)) {
            report_stopping(&handler_reporter);
        }
        let _ = controls.send(message);
        let _ = waker.post(KEY_WAKE, 0, 0);
        ServiceControlHandlerResult::NoError
    };
    let status = service_control_handler::register(name, handler)?;
    status.set_service_status(running())?;
    *reporter.lock().unwrap_or_else(|p| p.into_inner()) = Some(status);
    info!("running as the SCM service {name}, session {session}");

    let ending = manager::run(port, for_manager, inbox);

    // A manager that could not serve the pipe stops with an error, for the
    // record: the failure actions fire on a crash, not on a stop, so a
    // squatted name is waited out inside `run` rather than left to them.
    let exit_code = match ending {
        Ending::Stopped | Ending::Yielded => ServiceExitCode::NO_ERROR,
        Ending::Failed => ServiceExitCode::ServiceSpecific(EXIT_NO_PIPE),
    };
    status.set_service_status(ServiceStatus {
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code,
        ..running()
    })?;
    Ok(())
}

fn running() -> ServiceStatus {
    ServiceStatus {
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
    }
}

/// Tell the SCM the instance is on its way out, and may take a while.
fn report_stopping(reporter: &Mutex<Option<ServiceStatusHandle>>) {
    let Some(status) = *reporter.lock().unwrap_or_else(|p| p.into_inner()) else {
        return;
    };
    let _ = status.set_service_status(ServiceStatus {
        current_state: ServiceState::StopPending,
        controls_accepted: ServiceControlAccept::empty(),
        checkpoint: 1,
        wait_hint: STOP_WAIT_HINT,
        ..running()
    });
}
