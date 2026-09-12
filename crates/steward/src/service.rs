//! Hosting by the SCM. steward is registered as a per-user service template
//! (`type= userown`); at each sign-in Windows starts an instance,
//! `steward_<suffix>`, in the user's session and with the user's token. This
//! module speaks the SCM's protocol and turns its controls into manager events.

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::log::{error, info};
use crate::manager::{self, Event};

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

fn host(name: &str) -> windows_service::Result<()> {
    let (events, inbox) = mpsc::channel();
    let handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        let event = match control {
            ServiceControl::Interrogate => return ServiceControlHandlerResult::NoError,
            ServiceControl::Stop => Event::Stop("stop requested by the SCM".into()),
            ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                Event::Stop("system shutdown".into())
            }
            ServiceControl::SessionChange(change) => Event::Session(format!("{:?}", change.reason)),
            ServiceControl::PowerEvent(power) => Event::Power(format!("{power:?}")),
            _ => return ServiceControlHandlerResult::NotImplemented,
        };
        let _ = events.send(event);
        ServiceControlHandlerResult::NoError
    };
    let status = service_control_handler::register(name, handler)?;
    let running = ServiceStatus {
        service_type: ServiceType::USER_OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE
            | ServiceControlAccept::POWER_EVENT,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status.set_service_status(running.clone())?;
    info!("running as the SCM service {name}");

    manager::run(inbox);

    status.set_service_status(ServiceStatus {
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        ..running
    })?;
    Ok(())
}
