//! steward, the manager.
//!
//!   steward              run under the SCM, as an instance of the per-user
//!                        service template (how it runs for real)
//!   steward --console    run in the foreground until Ctrl+C (development)
//!   steward --ctrl-c PID...
//!                        (internal) deliver Ctrl+C to the consoles of PIDs
//!   steward provision-eventlog [--channel-size SIZE] [--account NAME]...
//!   steward provision-eventlog --uninstall
//!                        create the Event Log channels of the signed-in
//!                        users and of the accounts named, each SIZE at
//!                        most, or remove every channel it has made. Run as
//!                        SYSTEM by a Scheduled Task at every logon, which
//!                        the install declares rather than this registering
//!                        it.
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
            let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            let done = match eventlog::Job::parse(&rest) {
                Ok(eventlog::Job::Provision { size, accounts }) => {
                    eventlog::provision(size, &accounts)
                }
                Ok(eventlog::Job::Uninstall) => eventlog::uninstall(),
                Err(e) => fail(
                    2,
                    &format!("steward provision-eventlog: {e}\n{}", eventlog::USAGE),
                ),
            };
            if let Err(e) = done {
                fail(1, &format!("steward provision-eventlog: {e}"));
            }
        }
        _ => {
            eprintln!("usage: steward [--console | --version]");
            eprintln!(
                "       steward provision-eventlog [--channel-size SIZE] [--account NAME]..."
            );
            eprintln!("       steward provision-eventlog --uninstall");
            eprintln!("  (with no arguments steward expects to be started by the SCM)");
            std::process::exit(2);
        }
    }
}

/// `provision-eventlog`'s way out when it has failed, or was asked wrongly:
/// said, and a non-zero exit, because the task's last-run result is how a
/// logon-time failure is noticed at all.
///
/// `writeln!` and not `eprintln!`, which panics when the write fails: the
/// Task Scheduler leaves a process no standard handles, and the failure to
/// report a failure should not replace it. A task declared with a size
/// steward refuses gets 2 for its last result rather than a panic's 101.
#[cfg(windows)]
fn fail(code: i32, message: &str) -> ! {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{message}");
    std::process::exit(code)
}

#[cfg(not(windows))]
fn main() {
    eprintln!("steward manages Windows services and runs only on Windows");
    std::process::exit(1);
}
