//! steward, the manager.
//!
//!   steward              run under the SCM, as an instance of the per-user
//!                        service template (how it runs for real)
//!   steward --console    run in the foreground until Ctrl+C (development)
//!   steward --ctrl-c PID...
//!                        (internal) deliver Ctrl+C to the consoles of PIDs
//!   steward provision-eventlog [--install | --uninstall]
//!                        create the signed-in users' Event Log channels;
//!                        run as SYSTEM by a Scheduled Task at every logon,
//!                        which `--install` registers (elevated)
//!
//! Either way the manager is the same code: `manager::run`, fed controls by
//! the SCM's control handler or the console's Ctrl+C handler.
//! `provision-eventlog` is not the manager at all: it is the administrative
//! half of the logs, and it runs and exits.

#[cfg(windows)]
mod control;
#[cfg(windows)]
mod eventlog;
#[cfg(windows)]
mod log;
#[cfg(windows)]
mod manager;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod state;
#[cfg(windows)]
mod sys;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => service::run(),
        Some("--console") if args.len() == 1 => manager::run_console(),
        Some("--version") => println!("steward {}", env!("CARGO_PKG_VERSION")),
        Some("--ctrl-c") => {
            let pids: Vec<u32> = args[1..].iter().filter_map(|a| a.parse().ok()).collect();
            std::process::exit(sys::signal::ctrl_c_helper(&pids));
        }
        Some("provision-eventlog") => {
            let done = match args.get(1).map(String::as_str) {
                None => eventlog::provision(),
                Some("--install") => eventlog::install(),
                Some("--uninstall") => eventlog::uninstall(),
                Some(other) => {
                    eprintln!("steward provision-eventlog: {other} is not one of its arguments");
                    eprintln!("usage: steward provision-eventlog [--install | --uninstall]");
                    std::process::exit(2);
                }
            };
            if let Err(e) = done {
                // The task's last-run result is how a logon-time failure is
                // noticed at all, so it must not exit 0 on one. `writeln!`
                // and not `eprintln!`, which panics when the write fails:
                // the Task Scheduler leaves a process no standard handles,
                // and the failure to report a failure should not replace it.
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "steward provision-eventlog: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: steward [--console | --version]");
            eprintln!("       steward provision-eventlog [--install | --uninstall]");
            eprintln!("  (with no arguments steward expects to be started by the SCM)");
            std::process::exit(2);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("steward manages Windows services and runs only on Windows");
    std::process::exit(1);
}
