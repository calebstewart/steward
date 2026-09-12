//! steward, the manager.
//!
//!   steward              run under the SCM, as an instance of the per-user
//!                        service template (how it runs for real)
//!   steward --console    run in the foreground until Ctrl+C (development)
//!   steward --ctrl-c PID...
//!                        (internal) deliver Ctrl+C to the consoles of PIDs
//!
//! Either way the manager is the same code: `manager::run`, fed controls by
//! the SCM's control handler or the console's Ctrl+C handler.

#[cfg(windows)]
mod control;
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
        _ => {
            eprintln!("usage: steward [--console | --version]");
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
