//! The manager. For now it loads the user's units, reports what it found, and
//! logs the events it is sent until it is told to stop; supervision is M1.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;

use steward_unit::Severity;
use windows_sys::core::BOOL;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

use crate::log::{error, info, warning};

pub enum Event {
    /// Stop everything and return; the reason is logged.
    Stop(String),
    /// A session change the SCM reported: lock, unlock, logon, ...
    Session(String),
    /// A power event the SCM reported.
    Power(String),
}

pub fn run(inbox: Receiver<Event>) {
    info!("steward {} starting", env!("CARGO_PKG_VERSION"));
    load_units();
    loop {
        match inbox.recv() {
            Ok(Event::Stop(reason)) => {
                info!("stopping: {reason}");
                break;
            }
            Ok(Event::Session(change)) => info!("session change: {change}"),
            Ok(Event::Power(event)) => info!("power event: {event}"),
            Err(_) => {
                warning!("every event source has gone; stopping");
                break;
            }
        }
    }
    info!("stopped");
}

fn load_units() {
    let Some(dir) = steward_unit::user_unit_dir() else {
        error!("APPDATA is not set; cannot find the unit directory");
        return;
    };
    let units = match steward_unit::load_dir(&dir) {
        Ok(units) => units,
        Err(e) => {
            error!("cannot read {}: {e}", dir.display());
            return;
        }
    };
    info!("{} unit(s) in {}", units.len(), dir.display());
    for unit in units {
        let name = unit.path.file_name().unwrap_or_default().to_string_lossy();
        for diagnostic in &unit.parsed.diagnostics {
            match diagnostic.severity {
                Severity::Warning => warning!("{name}: {diagnostic}"),
                Severity::Error => error!("{name}: {diagnostic}"),
            }
        }
        match &unit.parsed.service {
            Some(service) => info!("{name}: loaded, runs {}", service.exec_start[0].line),
            None => error!("{name}: not loaded"),
        }
    }
}

static CONSOLE_EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

unsafe extern "system" fn on_console_ctrl(_ctrl_type: u32) -> BOOL {
    if let Some(events) = CONSOLE_EVENTS.get() {
        let _ = events.send(Event::Stop("Ctrl+C".into()));
    }
    1
}

/// `steward --console`: the same manager, in the foreground, stopped by Ctrl+C.
pub fn run_console() {
    crate::log::init(true);
    let (events, inbox) = mpsc::channel();
    let _ = CONSOLE_EVENTS.set(events);
    unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), 1) };
    run(inbox);
}
